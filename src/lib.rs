//! Automate Apple Photos via AppleScript. A Rust port of PhotoScript
//! (https://github.com/RhetTbull/PhotoScript); the AppleScript handlers in
//! `photoscript.applescript` are PhotoScript's, embedded verbatim.

pub mod osa;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::NaiveDateTime;

pub use osa::{Error, Result, Value};

const SCRIPT: &str = include_str!("photoscript.applescript");

// Photos AppleScript ids carry a suffix that the library database uuid doesn't.
const SUFFIX_PHOTO: &str = "/L0/001";
const SUFFIX_ALBUM: &str = "/L0/040";
const SUFFIX_FOLDER: &str = "/L0/020";

const OPEN_SCRIPT: &str = r#"
on openLibrary(libraryPath, theDelay)
    set tries to 0
    repeat while tries < 5
        try
            tell application "Photos" to quit
            delay 2
            do shell script "open -a Photos " & quoted form of libraryPath
            delay theDelay
            set tries to 99
        on error
            set tries to tries + 1
        end try
    end repeat
    return tries
end openLibrary
"#;

/// Returns `(uuid, id)`: the bare database uuid and the suffixed AppleScript id.
pub fn uuid_to_id(uuid: &str, suffix: &str) -> (String, String) {
    match uuid.split_once('/') {
        Some((bare, _)) => (bare.to_owned(), uuid.to_owned()),
        None => (uuid.to_owned(), format!("{uuid}{suffix}")),
    }
}

/// Validates a python-style `[start, stop)` range against the library size and
/// returns the 1-based inclusive bounds AppleScript wants.
pub fn range_bounds(start: i64, stop: i64, count: i64) -> Result<(i64, i64)> {
    if start > stop {
        return Err(Error::new("start range must be <= stop range"));
    }
    if !((0..count).contains(&start) && (1..=count).contains(&stop)) {
        return Err(Error::new(format!(
            "invalid range: valid range is start: 0 to {}, stop: 1 to {count}",
            count - 1
        )));
    }
    Ok((start + 1, stop))
}

/// Picks a stem that doesn't collide (case-insensitively) with any existing file
/// stem starting with `stem`: "IMG", "IMG (1)", "IMG (2)", ...
pub fn unique_stem(stem: &str, existing_stems: &[String]) -> String {
    let taken: Vec<String> = existing_stems.iter().map(|s| s.to_lowercase()).collect();
    let mut candidate = stem.to_owned();
    let mut n = 1;
    while taken.contains(&candidate.to_lowercase()) {
        candidate = format!("{stem} ({n})");
        n += 1;
    }
    candidate
}

/// Pulls a photo uuid out of errors like
/// `Can’t get media item id "EC19.../L0/001" of album id ...`.
pub fn uuid_from_error_str(error: &str) -> Option<String> {
    let rest = error.split_once("media item id \"")?.1;
    let id = rest.split_once('"')?.0;
    Some(id.split('/').next()?.to_owned())
}

fn stem_and_ext(path: &Path) -> (String, String) {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (stem, ext)
}

pub struct Photos {
    script: osa::Script,
}

impl Photos {
    /// Compiles the AppleScript. Photos is launched on demand by the first call that needs it.
    pub fn new() -> Result<Self> {
        Ok(Photos {
            script: osa::Script::compile(SCRIPT)?,
        })
    }

    /// Runs a handler; on an AppleEvent timeout kills Photos and retries once.
    pub fn run(&self, handler: &str, args: &[Value]) -> Result<Value> {
        match self.script.call(handler, args) {
            Err(e) if e.number == Some(osa::ERR_TIMEOUT) => {
                eprintln!("shoebox: {handler} timed out; restarting Photos and retrying");
                let _ = Command::new("killall").arg("Photos").status();
                thread::sleep(Duration::from_secs(5));
                self.script.call(handler, args)
            }
            r => r,
        }
        .map_err(|e| Error {
            message: format!("{handler}: {}", e.message),
            number: e.number,
        })
    }

    pub fn activate(&self) -> Result<()> {
        self.run("photosLibraryActivate", &[]).map(drop)
    }

    pub fn quit(&self) -> Result<()> {
        self.run("photosLibraryQuit", &[]).map(drop)
    }

    pub fn hide(&self) -> Result<()> {
        self.run("photosLibraryHide", &[]).map(drop)
    }

    /// Quits Photos and reopens it on `library_path`, waiting `delay` seconds for the user to acknowledge.
    pub fn open(&self, library_path: &Path, delay: i64) -> Result<()> {
        if !library_path.is_dir() {
            return Err(Error::new(format!(
                "{} does not appear to be a Photos library",
                library_path.display()
            )));
        }
        self.activate()?;
        let path = library_path.to_string_lossy().into_owned();
        let tries = osa::Script::compile(OPEN_SCRIPT)?
            .call("openLibrary", &[path.into(), delay.into()])?
            .as_i64()?;
        if tries != 99 {
            return Err(Error::new(format!(
                "Photos failed to open {} after {tries} tries",
                library_path.display()
            )));
        }
        Ok(())
    }

    /// Doesn't launch Photos.
    pub fn running(&self) -> Result<bool> {
        self.run("photosLibraryIsRunning", &[])?.as_bool()
    }

    pub fn hidden(&self) -> Result<bool> {
        self.run("photosLibraryIsHidden", &[])?.as_bool()
    }

    pub fn frontmost(&self) -> Result<bool> {
        self.run("photosLibraryIsFrontMost", &[])?.as_bool()
    }

    pub fn name(&self) -> Result<String> {
        self.run("photosLibraryName", &[])?.into_text()
    }

    pub fn version(&self) -> Result<String> {
        self.run("photosLibraryVersion", &[])?.into_text()
    }

    pub fn count(&self) -> Result<i64> {
        self.run("photosLibraryCount", &[])?.as_i64()
    }

    pub fn selection(&self) -> Result<Vec<Photo<'_>>> {
        match self.run("photosLibraryGetSelection", &[]) {
            Ok(ids) => ids
                .into_text_list()?
                .iter()
                .map(|id| self.photo(id))
                .collect(),
            // In full-screen view Photos errors but names the photo in the message.
            Err(e) => match uuid_from_error_str(&e.message) {
                Some(uuid) => Ok(self.photo(&uuid).into_iter().collect()),
                None => Ok(vec![]),
            },
        }
    }

    pub fn favorites(&self) -> Result<Album<'_>> {
        let id = self.run("photosLibraryFavorites", &[])?.into_text()?;
        self.album(&id)
    }

    pub fn photo(&self, uuid: &str) -> Result<Photo<'_>> {
        Photo::new(self, uuid)
    }

    /// Every photo id in library order. One bulk Apple event: ~1s for 13k photos,
    /// where indexed lookups (`media items 1 thru n`) take ~0.1s per photo.
    pub fn all_photo_ids(&self) -> Result<Vec<String>> {
        self.run("photosLibraryGetAllPhotos", &[])?.into_text_list()
    }

    /// Every photo in the library; ids are fetched up front, photos validated lazily.
    pub fn all_photos(&self) -> Result<impl Iterator<Item = Result<Photo<'_>>>> {
        Ok(self
            .all_photo_ids()?
            .into_iter()
            .map(move |id| self.photo(&id)))
    }

    /// Photo ids matching a Photos search string.
    pub fn search(&self, text: &str) -> Result<Vec<String>> {
        self.run("photosLibrarySearchPhotos", &[text.into()])?
            .into_text_list()
    }

    /// Photo ids for library positions `[start, stop)`.
    pub fn photo_ids_in_range(&self, start: i64, stop: i64) -> Result<Vec<String>> {
        let mut ids = self.all_photo_ids()?;
        let (first, last) = range_bounds(start, stop, ids.len() as i64)?;
        ids.truncate(last as usize);
        Ok(ids.split_off(first as usize - 1))
    }

    /// Imports files, optionally straight into `album`.
    ///
    /// Without `skip_duplicate_check`, importing a duplicate blocks on a Photos dialog.
    pub fn import(
        &self,
        paths: &[PathBuf],
        album: Option<&Album>,
        skip_duplicate_check: bool,
    ) -> Result<Vec<Photo<'_>>> {
        let paths: Vec<Value> = paths
            .iter()
            .map(|p| fs::canonicalize(p).map(|p| Value::from(p.to_string_lossy().into_owned())))
            .collect::<std::io::Result<_>>()
            .map_err(|e| Error::new(format!("import: {e}")))?;
        let ids = match album {
            Some(album) => self.run(
                "photosLibraryImportToAlbum",
                &[
                    Value::List(paths),
                    album.id.clone().into(),
                    skip_duplicate_check.into(),
                ],
            )?,
            None => self.run(
                "photosLibraryImport",
                &[Value::List(paths), skip_duplicate_check.into()],
            )?,
        };
        ids.into_text_list()?
            .iter()
            .map(|id| self.photo(id))
            .collect()
    }

    pub fn album_names(&self, top_level: bool) -> Result<Vec<String>> {
        self.run("photosLibraryAlbumNames", &[top_level.into()])?
            .into_text_list()
    }

    pub fn folder_names(&self, top_level: bool) -> Result<Vec<String>> {
        self.run("photosLibraryFolderNames", &[top_level.into()])?
            .into_text_list()
    }

    pub fn album(&self, uuid: &str) -> Result<Album<'_>> {
        Album::new(self, uuid)
    }

    /// First album named `name`, or None.
    pub fn album_by_name(&self, name: &str, top_level: bool) -> Result<Option<Album<'_>>> {
        match self.run("albumByName", &[name.into(), top_level.into()])? {
            Value::Text(id) => self.album(&id).map(Some),
            _ => Ok(None),
        }
    }

    pub fn albums(&self, top_level: bool) -> Result<Vec<Album<'_>>> {
        let ids = self
            .run("photosLibraryAlbumIDs", &[top_level.into()])?
            .into_text_list()?;
        ids.iter().map(|id| self.album(id)).collect()
    }

    pub fn create_album(&self, name: &str, folder: Option<&Folder>) -> Result<Album<'_>> {
        let id = match folder {
            Some(f) => self.run(
                "photosLibraryCreateAlbumAtFolder",
                &[name.into(), f.idstring.clone().into()],
            )?,
            None => self.run("photosLibraryCreateAlbum", &[name.into()])?,
        };
        match id.into_opt_text()? {
            Some(id) => self.album(&id),
            None => Err(Error::new(format!("Could not create album {name}"))),
        }
    }

    /// Deletes the album but not its photos.
    pub fn delete_album(&self, album: &Album) -> Result<()> {
        self.run("photosLibraryDeleteAlbum", &[album.id.clone().into()])
            .map(drop)
    }

    fn folder_from_idstring(&self, idstring: Value) -> Result<Option<Folder<'_>>> {
        match idstring.into_opt_text()? {
            Some(s) => Folder::new(self, s).map(Some),
            None => Ok(None),
        }
    }

    pub fn folder_by_name(&self, name: &str, top_level: bool) -> Result<Option<Folder<'_>>> {
        self.folder_from_idstring(self.run(
            "photosLibraryGetFolderIDStringForName",
            &[name.into(), top_level.into()],
        )?)
    }

    /// Folder at `path`, e.g. `["Folder", "SubFolder"]`.
    pub fn folder_by_path(&self, path: &[String]) -> Result<Option<Folder<'_>>> {
        self.folder_from_idstring(self.run("folderGetIDStringFromPath", &[path.to_vec().into()])?)
    }

    pub fn folder_by_uuid(&self, uuid: &str) -> Result<Option<Folder<'_>>> {
        let (_, id) = uuid_to_id(uuid, SUFFIX_FOLDER);
        self.folder_from_idstring(self.run(
            "photosLibraryGetFolderIDStringForID",
            &[id.into(), false.into()],
        )?)
    }

    pub fn folders(&self, top_level: bool) -> Result<Vec<Folder<'_>>> {
        let ids = self
            .run("photosLibraryFolderIDs", &[top_level.into()])?
            .into_text_list()?;
        ids.iter()
            .map(|id| {
                self.folder_by_uuid(id)?
                    .ok_or_else(|| Error::new(format!("folder {id} vanished")))
            })
            .collect()
    }

    pub fn create_folder(&self, name: &str, parent: Option<&Folder>) -> Result<Folder<'_>> {
        let idstring = match parent {
            Some(p) => self.run(
                "photosLibraryCreateFolderAtFolder",
                &[name.into(), p.idstring.clone().into()],
            )?,
            None => self.run("photosLibraryCreateFolder", &[name.into()])?,
        };
        self.folder_from_idstring(idstring)?
            .ok_or_else(|| Error::new(format!("Could not create folder {name}")))
    }

    /// Like `mkdir -p`: creates any missing folders along `path` and returns the last.
    pub fn make_folders(&self, path: &[String]) -> Result<Folder<'_>> {
        let (first, rest) = path
            .split_first()
            .ok_or_else(|| Error::new("no values in folder path"))?;
        let mut folder = match self.folder_by_name(first, true)? {
            Some(f) => f,
            None => self.create_folder(first, None)?,
        };
        for name in rest {
            folder = match folder.folder(name)? {
                Some(f) => f,
                None => folder.create_folder(name)?,
            };
        }
        Ok(folder)
    }

    /// Returns album `name` inside folder `path`, creating the folders and album if missing.
    pub fn make_album_folders(&self, name: &str, path: &[String]) -> Result<Album<'_>> {
        if name.is_empty() {
            return Err(Error::new("album name must not be empty"));
        }
        let folder = self.make_folders(path)?;
        match folder.album(name)? {
            Some(a) => Ok(a),
            None => folder.create_album(name),
        }
    }

    /// Deletes a folder and everything in it. Photos only supports this for top-level folders.
    pub fn delete_folder(&self, folder: &Folder) -> Result<()> {
        self.run(
            "photosLibraryDeleteFolder",
            &[folder.idstring.clone().into()],
        )
        .map(drop)
    }

    fn temp_album_name(&self) -> Result<String> {
        loop {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let name = format!("shoebox_{nanos}");
            if self.album_by_name(&name, false)?.is_none() {
                return Ok(name);
            }
        }
    }

    pub fn reveal_in_finder(&self, paths: &[PathBuf]) -> Result<()> {
        let paths: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.run("revealInFinder", &[paths.into()]).map(drop)
    }

    fn export_photo(
        &self,
        photo: &Photo,
        dest: &Path,
        opts: &ExportOptions,
    ) -> Result<Vec<PathBuf>> {
        if !dest.is_dir() {
            return Err(Error::new(format!(
                "export path {} must be a directory",
                dest.display()
            )));
        }
        let io = |e: std::io::Error| Error::new(format!("export: {e}"));
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let tmp = std::env::temp_dir().join(format!("shoebox_{}_{nanos}", std::process::id()));
        fs::create_dir_all(&tmp).map_err(io)?;
        let result = (|| {
            let tmp_str = tmp.to_string_lossy().into_owned();
            let args = [
                photo.id.clone().into(),
                tmp_str.into(),
                opts.original.into(),
                (!opts.original).into(),
                opts.timeout.into(),
            ];
            self.run("photoExport", &args)?;

            // Photos may rename on export (JPG -> jpeg) and live photos export two files,
            // so take whatever landed; files sharing a stem share the de-duplicated name.
            let mut exported = vec![];
            let mut seen: HashMap<String, String> = HashMap::new();
            for (n, entry) in fs::read_dir(&tmp).map_err(io)?.enumerate() {
                let src = entry.map_err(io)?.path();
                let (stem, ext) = stem_and_ext(&src);
                // ditto keeps extended attributes and resource forks; copy to a private
                // name in dest first so the final name can be claimed atomically below.
                let staged = dest.join(format!(".shoebox-{}-{nanos}-{n}{ext}", std::process::id()));
                let ok = Command::new("/usr/bin/ditto")
                    .arg(&src)
                    .arg(&staged)
                    .status()
                    .map_err(io)?
                    .success();
                if !ok {
                    let _ = fs::remove_file(&staged);
                    return Err(Error::new(format!(
                        "ditto failed copying to {}",
                        staged.display()
                    )));
                }
                if opts.overwrite {
                    let target = dest.join(format!("{stem}{ext}"));
                    fs::rename(&staged, &target).map_err(io)?;
                    exported.push(target);
                    continue;
                }
                // Several shoebox processes may export into dest at once, so a name is only
                // ours once hard_link (which fails if it exists) succeeds.
                let mut taken: Vec<String> = fs::read_dir(dest)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| stem_and_ext(&e.path()).0)
                    .filter(|s| s.to_lowercase().starts_with(&stem.to_lowercase()))
                    .collect();
                let mut new_stem = seen
                    .get(&stem)
                    .cloned()
                    .unwrap_or_else(|| unique_stem(&stem, &taken));
                let target = loop {
                    let target = dest.join(format!("{new_stem}{ext}"));
                    match fs::hard_link(&staged, &target) {
                        Ok(()) => break target,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            taken.push(new_stem);
                            new_stem = unique_stem(&stem, &taken);
                        }
                        Err(e) => {
                            let _ = fs::remove_file(&staged);
                            return Err(io(e));
                        }
                    }
                };
                fs::remove_file(&staged).map_err(io)?;
                seen.insert(stem, new_stem);
                exported.push(target);
            }
            Ok(exported)
        })();
        let _ = fs::remove_dir_all(&tmp);
        let exported = result?;
        if opts.reveal_in_finder && !exported.is_empty() {
            self.reveal_in_finder(&exported)?;
        }
        Ok(exported)
    }
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Export the original file (all burst frames, live photo movie) instead of the current JPEG.
    pub original: bool,
    /// Overwrite same-named files in the destination instead of adding " (n)".
    pub overwrite: bool,
    /// Seconds to wait for Photos per photo.
    pub timeout: i64,
    pub reveal_in_finder: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            original: false,
            overwrite: false,
            timeout: 120,
            reveal_in_finder: false,
        }
    }
}

pub struct Album<'a> {
    lib: &'a Photos,
    pub uuid: String,
    pub id: String,
}

impl<'a> Album<'a> {
    fn new(lib: &'a Photos, uuid: &str) -> Result<Self> {
        let (uuid, id) = uuid_to_id(uuid, SUFFIX_ALBUM);
        if !lib.run("albumExists", &[id.clone().into()])?.as_bool()? {
            return Err(Error::new(format!("Invalid album id: {uuid}")));
        }
        Ok(Album { lib, uuid, id })
    }

    fn arg(&self) -> Value {
        self.id.clone().into()
    }

    pub fn name(&self) -> Result<String> {
        self.lib.run("albumName", &[self.arg()])?.into_text()
    }

    pub fn set_name(&self, name: &str) -> Result<()> {
        self.lib
            .run("albumSetName", &[self.arg(), name.into()])
            .map(drop)
    }

    /// Containing folder, or None for a top-level album.
    pub fn parent(&self) -> Result<Option<Folder<'a>>> {
        match self.lib.run("albumParent", &[self.arg()])? {
            Value::Text(id) => self.lib.folder_by_uuid(&id),
            _ => Ok(None),
        }
    }

    fn parent_id(&self) -> Result<Option<String>> {
        match self.lib.run("albumParent", &[self.arg()])? {
            Value::Text(id) => Ok(Some(id)),
            _ => Ok(None),
        }
    }

    /// Library path, e.g. "Folder/SubFolder/Album".
    pub fn path_str(&self, delim: char) -> Result<String> {
        self.lib
            .run("albumGetPath", &[self.arg(), delim.to_string().into()])?
            .into_text()
    }

    pub fn photo_ids(&self) -> Result<Vec<String>> {
        self.lib.run("albumPhotes", &[self.arg()])?.into_text_list()
    }

    pub fn photos(&self) -> Result<Vec<Photo<'a>>> {
        self.photo_ids()?
            .iter()
            .map(|id| self.lib.photo(id))
            .collect()
    }

    pub fn count(&self) -> Result<i64> {
        self.lib.run("albumCount", &[self.arg()])?.as_i64()
    }

    /// Adds library photos to the album; returns the photos added.
    pub fn add(&self, photos: &[Photo]) -> Result<Vec<Photo<'a>>> {
        let ids: Vec<String> = photos.iter().map(|p| p.id.clone()).collect();
        let added = self
            .lib
            .run("albumAdd", &[self.arg(), ids.into()])?
            .into_text_list()?;
        added.iter().map(|id| self.lib.photo(id)).collect()
    }

    pub fn import(&self, paths: &[PathBuf], skip_duplicate_check: bool) -> Result<Vec<Photo<'a>>> {
        self.lib.import(paths, Some(self), skip_duplicate_check)
    }

    pub fn export(&self, dest: &Path, opts: &ExportOptions) -> Result<Vec<PathBuf>> {
        let per_photo = ExportOptions {
            reveal_in_finder: false,
            ..opts.clone()
        };
        let mut exported = vec![];
        for photo in self.photos()? {
            exported.extend(photo.export(dest, &per_photo)?);
        }
        if opts.reveal_in_finder && !exported.is_empty() {
            self.lib.reveal_in_finder(&exported)?;
        }
        Ok(exported)
    }

    /// Removes photos by id. AppleScript can't remove from an album, so this
    /// builds a same-named replacement album without them and deletes this one.
    pub fn remove_by_id(&mut self, photo_ids: &[String]) -> Result<()> {
        let parent = self.parent()?;
        let new = self
            .lib
            .create_album(&self.lib.temp_album_name()?, parent.as_ref())?;
        let keep: Vec<Photo> = self
            .photos()?
            .into_iter()
            .filter(|p| !photo_ids.contains(&p.id) && !photo_ids.contains(&p.uuid))
            .collect();
        if !keep.is_empty() {
            new.add(&keep)?;
        }
        self.replace_with(new)
    }

    pub fn remove(&mut self, photos: &[Photo]) -> Result<()> {
        let ids: Vec<String> = photos.iter().map(|p| p.id.clone()).collect();
        self.remove_by_id(&ids)
    }

    /// Moves the album into `folder` (None = top level) by recreating it there.
    pub fn move_to(&mut self, folder: Option<&Folder>) -> Result<()> {
        let current = self.parent_id()?;
        let target = match folder {
            Some(f) => Some(f.id()?),
            None => None,
        };
        if current == target {
            return Ok(());
        }
        let new = self
            .lib
            .create_album(&self.lib.temp_album_name()?, folder)?;
        new.add(&self.photos()?)?;
        self.replace_with(new)
    }

    fn replace_with(&mut self, new: Album<'a>) -> Result<()> {
        let name = self.name()?;
        self.lib.delete_album(self)?;
        new.set_name(&name)?;
        *self = new;
        Ok(())
    }

    pub fn spotlight(&self) -> Result<()> {
        self.lib.run("albumSpotlight", &[self.arg()]).map(drop)
    }
}

pub struct Folder<'a> {
    lib: &'a Photos,
    /// AppleScript object specifier, e.g. `folder id("…/L0/020") of folder id("…/L0/020")`.
    pub idstring: String,
}

impl<'a> Folder<'a> {
    fn new(lib: &'a Photos, idstring: String) -> Result<Self> {
        if !lib
            .run("folderExists", &[idstring.clone().into()])?
            .as_bool()?
        {
            return Err(Error::new(format!("Folder {idstring} does not exist")));
        }
        Ok(Folder { lib, idstring })
    }

    fn arg(&self) -> Value {
        self.idstring.clone().into()
    }

    /// Bare uuid (no `/L0/020` suffix).
    pub fn uuid(&self) -> Result<String> {
        Ok(uuid_to_id(
            &self.lib.run("folderUUID", &[self.arg()])?.into_text()?,
            SUFFIX_FOLDER,
        )
        .0)
    }

    /// Suffixed AppleScript id.
    pub fn id(&self) -> Result<String> {
        Ok(uuid_to_id(
            &self.lib.run("folderUUID", &[self.arg()])?.into_text()?,
            SUFFIX_FOLDER,
        )
        .1)
    }

    pub fn name(&self) -> Result<String> {
        self.lib.run("folderName", &[self.arg()])?.into_text()
    }

    pub fn set_name(&self, name: &str) -> Result<()> {
        self.lib
            .run("folderSetName", &[self.arg(), name.into()])
            .map(drop)
    }

    pub fn parent(&self) -> Result<Option<Folder<'a>>> {
        self.lib
            .folder_from_idstring(self.lib.run("folderParent", &[self.arg()])?)
    }

    /// Library path, e.g. "Folder/SubFolder".
    pub fn path_str(&self, delim: char) -> Result<String> {
        self.lib
            .run("folderGetPath", &[self.arg(), delim.to_string().into()])?
            .into_text()
    }

    /// Ancestor folders, top-level first.
    pub fn path(&self) -> Result<Vec<Folder<'a>>> {
        let ids = self
            .lib
            .run("folderGetPathFolderIDScript", &[self.arg()])?
            .into_text_list()?;
        ids.into_iter().map(|s| Folder::new(self.lib, s)).collect()
    }

    pub fn albums(&self) -> Result<Vec<Album<'a>>> {
        let ids = self
            .lib
            .run("folderAlbums", &[self.arg()])?
            .into_text_list()?;
        ids.iter().map(|id| self.lib.album(id)).collect()
    }

    pub fn album(&self, name: &str) -> Result<Option<Album<'a>>> {
        for album in self.albums()? {
            if album.name()? == name {
                return Ok(Some(album));
            }
        }
        Ok(None)
    }

    pub fn subfolders(&self) -> Result<Vec<Folder<'a>>> {
        let ids = self
            .lib
            .run("folderFolders", &[self.arg()])?
            .into_text_list()?;
        ids.into_iter().map(|s| Folder::new(self.lib, s)).collect()
    }

    pub fn folder(&self, name: &str) -> Result<Option<Folder<'a>>> {
        for folder in self.subfolders()? {
            if folder.name()? == name {
                return Ok(Some(folder));
            }
        }
        Ok(None)
    }

    pub fn create_album(&self, name: &str) -> Result<Album<'a>> {
        self.lib.create_album(name, Some(self))
    }

    pub fn create_folder(&self, name: &str) -> Result<Folder<'a>> {
        self.lib.create_folder(name, Some(self))
    }

    /// Number of albums and folders directly inside.
    pub fn count(&self) -> Result<i64> {
        self.lib.run("folderCount", &[self.arg()])?.as_i64()
    }

    pub fn spotlight(&self) -> Result<()> {
        self.lib.run("folderSpotlight", &[self.arg()]).map(drop)
    }
}

pub struct Photo<'a> {
    lib: &'a Photos,
    pub uuid: String,
    pub id: String,
}

impl<'a> Photo<'a> {
    fn new(lib: &'a Photos, uuid: &str) -> Result<Self> {
        let (uuid, id) = uuid_to_id(uuid, SUFFIX_PHOTO);
        if !lib.run("photoExists", &[uuid.clone().into()])?.as_bool()? {
            return Err(Error::new(format!("Invalid photo id: {uuid}")));
        }
        Ok(Photo { lib, uuid, id })
    }

    fn arg(&self) -> Value {
        self.id.clone().into()
    }

    /// Title (Photos calls it "name").
    pub fn title(&self) -> Result<String> {
        self.lib.run("photoName", &[self.arg()])?.into_text()
    }

    pub fn set_title(&self, title: &str) -> Result<()> {
        self.lib
            .run("photoSetName", &[self.arg(), title.into()])
            .map(drop)
    }

    pub fn description(&self) -> Result<String> {
        self.lib.run("photoDescription", &[self.arg()])?.into_text()
    }

    pub fn set_description(&self, description: &str) -> Result<()> {
        self.lib
            .run("photoSetDescription", &[self.arg(), description.into()])
            .map(drop)
    }

    pub fn keywords(&self) -> Result<Vec<String>> {
        self.lib
            .run("photoKeywords", &[self.arg()])?
            .into_text_list()
    }

    pub fn set_keywords(&self, keywords: &[String]) -> Result<()> {
        self.lib
            .run("photoSetKeywords", &[self.arg(), keywords.to_vec().into()])
            .map(drop)
    }

    pub fn favorite(&self) -> Result<bool> {
        self.lib.run("photoFavorite", &[self.arg()])?.as_bool()
    }

    pub fn set_favorite(&self, favorite: bool) -> Result<()> {
        self.lib
            .run("photoSetFavorite", &[self.arg(), favorite.into()])
            .map(drop)
    }

    pub fn height(&self) -> Result<i64> {
        self.lib.run("photoHeight", &[self.arg()])?.as_i64()
    }

    pub fn width(&self) -> Result<i64> {
        self.lib.run("photoWidth", &[self.arg()])?.as_i64()
    }

    /// GPS altitude in meters.
    pub fn altitude(&self) -> Result<Option<f64>> {
        self.lib.run("photoAltitude", &[self.arg()])?.as_opt_f64()
    }

    /// `(latitude, longitude)`, either possibly unset.
    pub fn location(&self) -> Result<(Option<f64>, Option<f64>)> {
        match self.lib.run("photoLocation", &[self.arg()])? {
            Value::List(v) if v.len() == 2 => Ok((v[0].as_opt_f64()?, v[1].as_opt_f64()?)),
            Value::Missing => Ok((None, None)),
            other => Err(Error::new(format!("unexpected location {other:?}"))),
        }
    }

    pub fn set_location(&self, latitude: Option<f64>, longitude: Option<f64>) -> Result<()> {
        if latitude.is_some_and(|l| !(-90.0..=90.0).contains(&l)) {
            return Err(Error::new("latitude must be in range -90.0 to 90.0"));
        }
        if longitude.is_some_and(|l| !(-180.0..=180.0).contains(&l)) {
            return Err(Error::new("longitude must be in range -180.0 to 180.0"));
        }
        let v = |x: Option<f64>| x.map_or(Value::Missing, Value::Float);
        self.lib
            .run(
                "photoSetLocation",
                &[self.arg(), Value::List(vec![v(latitude), v(longitude)])],
            )
            .map(drop)
    }

    /// Capture date, timezone-naive local time.
    pub fn date(&self) -> Result<Option<NaiveDateTime>> {
        match self.lib.run("photoDate", &[self.arg()])? {
            Value::Date(d) => Ok(Some(d)),
            Value::Missing => Ok(None),
            other => Err(Error::new(format!("expected date, got {other:?}"))),
        }
    }

    pub fn set_date(&self, date: NaiveDateTime) -> Result<()> {
        self.lib
            .run("photoSetDate", &[self.arg(), Value::Date(date)])
            .map(drop)
    }

    /// Original filename.
    pub fn filename(&self) -> Result<String> {
        self.lib.run("photoFilename", &[self.arg()])?.into_text()
    }

    pub fn albums(&self) -> Result<Vec<Album<'a>>> {
        let ids = self
            .lib
            .run("photoAlbums", &[self.arg()])?
            .into_text_list()?;
        ids.iter().map(|id| self.lib.album(id)).collect()
    }

    /// Exports into directory `dest`; returns every file written (live photos and bursts can produce several).
    pub fn export(&self, dest: &Path, opts: &ExportOptions) -> Result<Vec<PathBuf>> {
        self.lib.export_photo(self, dest, opts)
    }

    pub fn duplicate(&self) -> Result<Photo<'a>> {
        let id = self.lib.run("photoDuplicate", &[self.arg()])?.into_text()?;
        self.lib.photo(&id)
    }

    pub fn spotlight(&self) -> Result<()> {
        self.lib.run("photoSpotlight", &[self.arg()]).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_suffixing() {
        assert_eq!(
            uuid_to_id("ABC", SUFFIX_PHOTO),
            ("ABC".into(), "ABC/L0/001".into())
        );
        assert_eq!(
            uuid_to_id("ABC/L0/040", SUFFIX_ALBUM),
            ("ABC".into(), "ABC/L0/040".into())
        );
    }

    #[test]
    fn ranges() {
        assert_eq!(range_bounds(0, 4, 10).unwrap(), (1, 4));
        assert_eq!(range_bounds(9, 10, 10).unwrap(), (10, 10));
        assert!(range_bounds(5, 4, 10).is_err());
        assert!(range_bounds(0, 11, 10).is_err());
        assert!(range_bounds(10, 10, 10).is_err());
    }

    #[test]
    fn collision_names() {
        assert_eq!(unique_stem("IMG_1", &[]), "IMG_1");
        assert_eq!(unique_stem("IMG_1", &["img_1".into()]), "IMG_1 (1)");
        assert_eq!(
            unique_stem(
                "IMG_1",
                &["IMG_1".into(), "IMG_1 (1)".into(), "IMG_10".into()]
            ),
            "IMG_1 (2)"
        );
    }

    #[test]
    fn uuid_in_error() {
        let e = r#"Photos got an error: Can’t get media item id "EC19EA1A-FC91/L0/001" of album id "4DE5/L0/040". (-1728)"#;
        assert_eq!(uuid_from_error_str(e).as_deref(), Some("EC19EA1A-FC91"));
        assert_eq!(uuid_from_error_str("nope"), None);
    }
}
