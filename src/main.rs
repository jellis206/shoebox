use std::path::PathBuf;
use std::process::ExitCode;

use chrono::NaiveDateTime;
use clap::{Parser, Subcommand};
use serde_json::{Value as Json, json};
use shoebox::{Album, Error, ExportOptions, Folder, Photo, Photos, Result};

const DATE_FMT: &str = "%Y-%m-%dT%H:%M:%S";

/// Script Apple Photos from the shell.
///
/// Photos are addressed by uuid. Lists print one uuid (or path) per line;
/// details print one JSON object per line, ready for jq.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// App and library summary as JSON
    Info,
    /// Exit 0 if Photos is running, 1 if not (never launches it)
    Running,
    /// Bring Photos to the front
    Activate,
    /// Quit Photos
    Quit,
    /// Hide the Photos window
    Hide,
    /// Quit Photos and reopen it on another library
    Open {
        library: PathBuf,
        /// Seconds to wait for you to acknowledge the switch in Photos
        #[arg(long, default_value_t = 10)]
        delay: i64,
    },
    /// Uuids of the photos selected in Photos
    Selection,
    /// List photo uuids (all of them if no filter; slow, ~1/sec)
    Photos {
        /// Photos search text
        #[arg(long, group = "filter")]
        search: Option<String>,
        /// Library positions START:STOP (stop exclusive) or STOP
        #[arg(long, group = "filter")]
        range: Option<String>,
        /// The Favorites album
        #[arg(long, group = "filter")]
        favorites: bool,
    },
    /// Photo details as JSON lines
    Show { uuids: Vec<String> },
    /// Edit photo metadata
    Set {
        #[arg(required = true)]
        uuids: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Comma-separated; replaces existing keywords ("" clears)
        #[arg(long)]
        keywords: Option<String>,
        #[arg(long)]
        favorite: Option<bool>,
        /// LAT,LON or "none" to clear
        #[arg(long)]
        location: Option<String>,
        /// Local time, YYYY-MM-DDTHH:MM:SS
        #[arg(long)]
        date: Option<String>,
    },
    /// Import files; prints the new uuids
    Import {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Album name or uuid to import into
        #[arg(long)]
        album: Option<String>,
        /// Don't let Photos stop on duplicates (otherwise it blocks on a dialog)
        #[arg(long)]
        skip_duplicate_check: bool,
    },
    /// Export photos; prints the written paths
    Export {
        #[arg(required = true)]
        uuids: Vec<String>,
        #[command(flatten)]
        opts: ExportArgs,
    },
    /// Duplicate a photo; prints the new uuid
    Duplicate { uuid: String },
    /// List albums as JSON lines {uuid, path}
    Albums {
        #[arg(long)]
        top_level: bool,
    },
    /// Work with one album
    #[command(subcommand)]
    Album(AlbumCmd),
    /// List folder paths
    Folders {
        #[arg(long)]
        top_level: bool,
    },
    /// Work with one folder (addressed by path, e.g. "Trips/2026")
    #[command(subcommand)]
    Folder(FolderCmd),
    /// Reveal a photo in Photos
    Spotlight { uuid: String },
}

#[derive(clap::Args)]
struct ExportArgs {
    /// Destination directory
    #[arg(long)]
    to: PathBuf,
    /// Export originals (all burst frames, live photo movies) instead of current JPEGs
    #[arg(long)]
    original: bool,
    /// Overwrite instead of adding " (n)" to clashing names
    #[arg(long)]
    overwrite: bool,
    /// Seconds to wait for Photos per photo
    #[arg(long, default_value_t = 120)]
    timeout: i64,
    /// Reveal exported files in Finder
    #[arg(long)]
    reveal: bool,
}

impl From<&ExportArgs> for ExportOptions {
    fn from(a: &ExportArgs) -> Self {
        ExportOptions { original: a.original, overwrite: a.overwrite, timeout: a.timeout, reveal_in_finder: a.reveal }
    }
}

/// ALBUM arguments take a name or a uuid.
#[derive(Subcommand)]
enum AlbumCmd {
    /// Album details as JSON
    Show { album: String },
    /// Uuids of the photos in the album
    Photos { album: String },
    /// Create an album (inside FOLDER path if given, creating folders as needed)
    Create {
        name: String,
        #[arg(long)]
        folder: Option<String>,
    },
    /// Delete the album (its photos stay in the library)
    Delete { album: String },
    Rename { album: String, name: String },
    /// Add library photos to the album
    Add {
        album: String,
        #[arg(required = true)]
        uuids: Vec<String>,
    },
    /// Remove photos from the album (recreates the album; its uuid changes)
    Remove {
        album: String,
        #[arg(required = true)]
        uuids: Vec<String>,
    },
    /// Move the album to a folder path, or top level if omitted (recreates the album)
    Move { album: String, folder: Option<String> },
    /// Export every photo in the album
    Export {
        album: String,
        #[command(flatten)]
        opts: ExportArgs,
    },
    /// Reveal the album in Photos
    Spotlight { album: String },
}

#[derive(Subcommand)]
enum FolderCmd {
    /// Folder details as JSON
    Show { path: String },
    /// Create a folder path, like mkdir -p
    Create { path: String },
    /// Delete a top-level folder and everything in it
    Delete { path: String },
    Rename { path: String, name: String },
    /// Reveal the folder in Photos
    Spotlight { path: String },
}

fn main() -> ExitCode {
    // Die quietly on a closed pipe (`shoebox photos | head`) instead of panicking.
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    unsafe { signal(13 /* SIGPIPE */, 0 /* SIG_DFL */) };

    let cli = Cli::parse();
    match Photos::new().and_then(|lib| run(&lib, cli.cmd)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("shoebox: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(lib: &Photos, cmd: Cmd) -> Result<ExitCode> {
    match cmd {
        Cmd::Info => {
            let running = lib.running()?;
            println!(
                "{}",
                json!({
                    "name": lib.name()?,
                    "version": lib.version()?,
                    "running": running,
                    "hidden": lib.hidden()?,
                    "frontmost": lib.frontmost()?,
                    "count": lib.count()?,
                })
            );
        }
        Cmd::Running => {
            let running = lib.running()?;
            println!("{running}");
            return Ok(if running { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Cmd::Activate => lib.activate()?,
        Cmd::Quit => lib.quit()?,
        Cmd::Hide => lib.hide()?,
        Cmd::Open { library, delay } => lib.open(&library, delay)?,
        Cmd::Selection => lib.selection()?.iter().for_each(|p| println!("{}", p.uuid)),
        Cmd::Photos { search, range, favorites } => {
            let ids = if let Some(text) = search {
                lib.search(&text)?
            } else if let Some(range) = range {
                let (start, stop) = parse_range(&range)?;
                lib.photo_ids_in_range(start, stop)?
            } else if favorites {
                lib.favorites()?.photo_ids()?
            } else {
                for photo in lib.all_photos()? {
                    println!("{}", photo?.uuid);
                }
                return Ok(ExitCode::SUCCESS);
            };
            ids.iter().for_each(|id| println!("{}", shoebox::uuid_to_id(id, "").0));
        }
        Cmd::Show { uuids } => {
            for uuid in uuids {
                println!("{}", photo_json(&lib.photo(&uuid)?)?);
            }
        }
        Cmd::Set { uuids, title, description, keywords, favorite, location, date } => {
            let location = location.map(|l| parse_location(&l)).transpose()?;
            let date = date.map(|d| parse_date(&d)).transpose()?;
            let keywords = keywords.map(|k| split_list(&k));
            for uuid in uuids {
                let photo = lib.photo(&uuid)?;
                if let Some(t) = &title {
                    photo.set_title(t)?;
                }
                if let Some(d) = &description {
                    photo.set_description(d)?;
                }
                if let Some(k) = &keywords {
                    photo.set_keywords(k)?;
                }
                if let Some(f) = favorite {
                    photo.set_favorite(f)?;
                }
                if let Some((lat, lon)) = location {
                    photo.set_location(lat, lon)?;
                }
                if let Some(d) = date {
                    photo.set_date(d)?;
                }
            }
        }
        Cmd::Import { paths, album, skip_duplicate_check } => {
            let album = album.map(|a| find_album(lib, &a)).transpose()?;
            for photo in lib.import(&paths, album.as_ref(), skip_duplicate_check)? {
                println!("{}", photo.uuid);
            }
        }
        Cmd::Export { uuids, opts: args } => {
            let per_photo = ExportOptions { reveal_in_finder: false, ..ExportOptions::from(&args) };
            let mut written = vec![];
            for uuid in uuids {
                for path in lib.photo(&uuid)?.export(&args.to, &per_photo)? {
                    println!("{}", path.display());
                    written.push(path);
                }
            }
            if args.reveal && !written.is_empty() {
                lib.reveal_in_finder(&written)?;
            }
        }
        Cmd::Duplicate { uuid } => println!("{}", lib.photo(&uuid)?.duplicate()?.uuid),
        Cmd::Albums { top_level } => {
            for album in lib.albums(top_level)? {
                println!("{}", json!({ "uuid": album.uuid, "path": album.path_str('/')? }));
            }
        }
        Cmd::Album(cmd) => album_cmd(lib, cmd)?,
        Cmd::Folders { top_level } => {
            for folder in lib.folders(top_level)? {
                println!("{}", folder.path_str('/')?);
            }
        }
        Cmd::Folder(cmd) => folder_cmd(lib, cmd)?,
        Cmd::Spotlight { uuid } => lib.photo(&uuid)?.spotlight()?,
    }
    Ok(ExitCode::SUCCESS)
}

fn album_cmd(lib: &Photos, cmd: AlbumCmd) -> Result<()> {
    match cmd {
        AlbumCmd::Show { album } => {
            let a = find_album(lib, &album)?;
            println!(
                "{}",
                json!({
                    "uuid": a.uuid,
                    "name": a.name()?,
                    "path": a.path_str('/')?,
                    "count": a.count()?,
                })
            );
        }
        AlbumCmd::Photos { album } => {
            for id in find_album(lib, &album)?.photo_ids()? {
                println!("{}", shoebox::uuid_to_id(&id, "").0);
            }
        }
        AlbumCmd::Create { name, folder } => {
            let album = match folder {
                Some(path) => lib.make_album_folders(&name, &split_path(&path))?,
                None => lib.create_album(&name, None)?,
            };
            println!("{}", album.uuid);
        }
        AlbumCmd::Delete { album } => lib.delete_album(&find_album(lib, &album)?)?,
        AlbumCmd::Rename { album, name } => find_album(lib, &album)?.set_name(&name)?,
        AlbumCmd::Add { album, uuids } => {
            let photos = uuids.iter().map(|u| lib.photo(u)).collect::<Result<Vec<Photo>>>()?;
            find_album(lib, &album)?.add(&photos)?;
        }
        AlbumCmd::Remove { album, uuids } => {
            let mut a = find_album(lib, &album)?;
            let ids: Vec<String> = uuids.iter().map(|u| shoebox::uuid_to_id(u, "/L0/001").1).collect();
            a.remove_by_id(&ids)?;
            println!("{}", a.uuid);
        }
        AlbumCmd::Move { album, folder } => {
            let mut a = find_album(lib, &album)?;
            let folder = folder.map(|p| find_folder(lib, &p)).transpose()?;
            a.move_to(folder.as_ref())?;
            println!("{}", a.uuid);
        }
        AlbumCmd::Export { album, opts } => {
            for path in find_album(lib, &album)?.export(&opts.to, &ExportOptions::from(&opts))? {
                println!("{}", path.display());
            }
        }
        AlbumCmd::Spotlight { album } => find_album(lib, &album)?.spotlight()?,
    }
    Ok(())
}

fn folder_cmd(lib: &Photos, cmd: FolderCmd) -> Result<()> {
    match cmd {
        FolderCmd::Show { path } => {
            let f = find_folder(lib, &path)?;
            let albums = f.albums()?.iter().map(Album::name).collect::<Result<Vec<_>>>()?;
            let subfolders = f.subfolders()?.iter().map(Folder::name).collect::<Result<Vec<_>>>()?;
            println!(
                "{}",
                json!({
                    "uuid": f.uuid()?,
                    "name": f.name()?,
                    "path": f.path_str('/')?,
                    "albums": albums,
                    "folders": subfolders,
                })
            );
        }
        FolderCmd::Create { path } => println!("{}", lib.make_folders(&split_path(&path))?.path_str('/')?),
        FolderCmd::Delete { path } => lib.delete_folder(&find_folder(lib, &path)?)?,
        FolderCmd::Rename { path, name } => find_folder(lib, &path)?.set_name(&name)?,
        FolderCmd::Spotlight { path } => find_folder(lib, &path)?.spotlight()?,
    }
    Ok(())
}

fn photo_json(p: &Photo) -> Result<Json> {
    let (lat, lon) = p.location()?;
    Ok(json!({
        "uuid": p.uuid,
        "filename": p.filename()?,
        "title": p.title()?,
        "description": p.description()?,
        "keywords": p.keywords()?,
        "favorite": p.favorite()?,
        "date": p.date()?.map(|d| d.format(DATE_FMT).to_string()),
        "width": p.width()?,
        "height": p.height()?,
        "latitude": lat,
        "longitude": lon,
        "altitude": p.altitude()?,
    }))
}

fn looks_like_uuid(s: &str) -> bool {
    let bare = s.split('/').next().unwrap_or(s);
    bare.len() == 36 && bare.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn find_album<'a>(lib: &'a Photos, name_or_uuid: &str) -> Result<Album<'a>> {
    if looks_like_uuid(name_or_uuid) {
        return lib.album(name_or_uuid);
    }
    lib.album_by_name(name_or_uuid, false)?.ok_or_else(|| Error::new(format!("no album named {name_or_uuid:?}")))
}

fn find_folder<'a>(lib: &'a Photos, path: &str) -> Result<Folder<'a>> {
    lib.folder_by_path(&split_path(path))?.ok_or_else(|| Error::new(format!("no folder at {path:?}")))
}

// ponytail: folder names containing "/" can't be addressed from the CLI; the library API takes a Vec.
fn split_path(path: &str) -> Vec<String> {
    path.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

fn parse_range(s: &str) -> Result<(i64, i64)> {
    let num = |n: &str| n.trim().parse::<i64>().map_err(|_| Error::new(format!("bad range {s:?}: expected START:STOP or STOP")));
    match s.split_once(':') {
        Some((a, b)) => Ok((num(a)?, num(b)?)),
        None => Ok((0, num(s)?)),
    }
}

fn parse_location(s: &str) -> Result<(Option<f64>, Option<f64>)> {
    if s.eq_ignore_ascii_case("none") {
        return Ok((None, None));
    }
    let bad = || Error::new(format!("bad location {s:?}: expected LAT,LON or none"));
    let (lat, lon) = s.split_once(',').ok_or_else(bad)?;
    Ok((Some(lat.trim().parse().map_err(|_| bad())?), Some(lon.trim().parse().map_err(|_| bad())?)))
}

fn parse_date(s: &str) -> Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, DATE_FMT).map_err(|e| Error::new(format!("bad date {s:?} ({e}): expected YYYY-MM-DDTHH:MM:SS")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parsers() {
        assert_eq!(parse_range("10").unwrap(), (0, 10));
        assert_eq!(parse_range("2:5").unwrap(), (2, 5));
        assert!(parse_range("x").is_err());
        assert_eq!(parse_location("1.5, -2").unwrap(), (Some(1.5), Some(-2.0)));
        assert_eq!(parse_location("None").unwrap(), (None, None));
        assert_eq!(split_path("/Trips//2026/"), vec!["Trips", "2026"]);
        assert_eq!(split_list("a, b,,c"), vec!["a", "b", "c"]);
        assert!(looks_like_uuid("EC19EA1A-FC91-449C-8925-B13D863E2EDB/L0/040"));
        assert!(!looks_like_uuid("Vacation"));
        Cli::command().debug_assert();
    }
}
