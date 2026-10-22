use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;

use chrono::{Days, Local, NaiveDate, NaiveDateTime, NaiveTime};
use clap::{Parser, Subcommand};
use serde_json::{Value as Json, json};
use shoebox::dupes::{self, Exact, Merge};
use shoebox::library::{Asset, CORE_DATA_EPOCH, Library, REVIEW_PREFIX, SHOEBOX_FOLDER};
use shoebox::{Album, Error, ExportOptions, Folder, Photo, Photos, Result};

const DATE_FMT: &str = "%Y-%m-%dT%H:%M:%S";

/// Script Apple Photos from the shell.
///
/// Photos are addressed by uuid. Lists print one uuid (or path) per line;
/// details print one JSON object per line, ready for jq.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Parallel shoebox processes for multi-photo show/set/export (Photos serves them concurrently)
    #[arg(short, long, global = true, default_value_t = 4)]
    jobs: usize,
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
    /// Find duplicates and queue them into week-stamped albums in the Shoebox folder
    ///
    /// exact: byte-identical originals. The copy with the most on it survives (the
    /// others' albums, keywords, favorite and title are merged onto it first); the rest
    /// go to "Shoebox Delete <week>". near/similar: look-alike thumbnails among capture-time
    /// neighbours; whole groups go to "Shoebox Review <week> #n" for you to choose.
    /// Nothing is deleted: do that in Photos. Every run ends by removing empty Shoebox
    /// albums from earlier weeks. Group JSON lines on stdout, a summary on stderr.
    Dupes(DupesArgs),
}

#[derive(clap::Args)]
struct DupesArgs {
    #[arg(long = "match", value_enum, default_value_t = Match::Exact)]
    mode: Match,
    /// Max thumbnail difference in bits, 0-64 (near = 4, similar = 10)
    #[arg(long)]
    threshold: Option<u32>,
    /// Capture-time neighbours compared on each side (near/similar)
    #[arg(long, default_value_t = 20)]
    window: usize,
    /// Album name or uuid to clean; exact matches still look library-wide
    #[arg(long, conflicts_with_all = ["from", "to"])]
    album: Option<String>,
    /// First capture date, YYYY-MM-DD
    #[arg(long)]
    from: Option<NaiveDate>,
    /// Last capture date, YYYY-MM-DD (inclusive)
    #[arg(long)]
    to: Option<NaiveDate>,
    /// Assets per review album; a group is never split
    #[arg(long, default_value_t = 32)]
    review_size: usize,
    /// Print groups and the summary; change nothing in Photos
    #[arg(long)]
    dry_run: bool,
    /// Photos library package
    #[arg(long, default_value_os_t = Library::default_path())]
    library: PathBuf,
}

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
enum Match {
    Exact,
    Near,
    Similar,
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
        ExportOptions {
            original: a.original,
            overwrite: a.overwrite,
            timeout: a.timeout,
            reveal_in_finder: a.reveal,
        }
    }
}

/// ALBUM arguments take a name or a uuid.
#[derive(Subcommand)]
enum AlbumCmd {
    /// Album details as JSON
    Show {
        album: String,
    },
    /// Uuids of the photos in the album
    Photos {
        album: String,
    },
    /// Create an album (inside FOLDER path if given, creating folders as needed)
    Create {
        name: String,
        #[arg(long)]
        folder: Option<String>,
    },
    /// Delete the album (its photos stay in the library)
    Delete {
        album: String,
    },
    Rename {
        album: String,
        name: String,
    },
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
    Move {
        album: String,
        folder: Option<String>,
    },
    /// Export every photo in the album
    Export {
        album: String,
        #[command(flatten)]
        opts: ExportArgs,
    },
    /// Reveal the album in Photos
    Spotlight {
        album: String,
    },
}

#[derive(Subcommand)]
enum FolderCmd {
    /// Folder details as JSON
    Show {
        path: String,
    },
    /// Create a folder path, like mkdir -p
    Create {
        path: String,
    },
    /// Delete a top-level folder and everything in it
    Delete {
        path: String,
    },
    Rename {
        path: String,
        name: String,
    },
    /// Reveal the folder in Photos
    Spotlight {
        path: String,
    },
}

fn main() -> ExitCode {
    // Die quietly on a closed pipe (`shoebox photos | head`) instead of panicking.
    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    unsafe {
        signal(13 /* SIGPIPE */, 0 /* SIG_DFL */)
    };

    let cli = Cli::parse();
    match Photos::new().and_then(|lib| run(&lib, cli.cmd, cli.jobs)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("shoebox: {e}");
            ExitCode::FAILURE
        }
    }
}

const CHUNK_ENV: &str = "SHOEBOX_CHUNK";

/// Contiguous slice `i` of `n` over `len` items.
fn chunk_range(len: usize, i: usize, n: usize) -> Range<usize> {
    let size = len.div_ceil(n);
    (i * size).min(len)..((i + 1) * size).min(len)
}

/// In a worker spawned by `fan_out`, the chunk of `items` it owns; otherwise all of them.
fn my_chunk(mut items: Vec<String>) -> Vec<String> {
    let chunk = std::env::var(CHUNK_ENV).ok().and_then(|c| {
        let (i, n) = c.split_once('/')?;
        Some((i.parse().ok()?, n.parse().ok()?))
    });
    match chunk {
        Some((i, n)) => {
            let r = chunk_range(items.len(), i, n);
            items.truncate(r.end);
            items.split_off(r.start)
        }
        None => items,
    }
}

fn is_worker() -> bool {
    std::env::var_os(CHUNK_ENV).is_some()
}

/// Re-runs this exact command line as up to `jobs` workers, each taking a contiguous
/// chunk of the `count` items via `my_chunk`, and relays their stdout in order.
/// Returns the relayed lines, or None when the work is too small to split.
fn fan_out(count: usize, jobs: usize) -> Result<Option<Vec<String>>> {
    let n = jobs.min(count);
    if n < 2 || is_worker() {
        return Ok(None);
    }
    let exe = std::env::current_exe().map_err(|e| Error::new(format!("fan out: {e}")))?;
    let workers = (0..n)
        .map(|i| {
            let child = Command::new(&exe)
                .args(std::env::args_os().skip(1))
                .env(CHUNK_ENV, format!("{i}/{n}"))
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| Error::new(format!("fan out: {e}")))?;
            // drain each pipe on its own thread so no worker blocks on a full pipe
            Ok(thread::spawn(move || child.wait_with_output()))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut lines = vec![];
    let mut failed = 0;
    for worker in workers {
        let out = worker
            .join()
            .expect("worker thread")
            .map_err(|e| Error::new(format!("fan out: {e}")))?;
        let text = String::from_utf8_lossy(&out.stdout);
        print!("{text}");
        lines.extend(text.lines().map(str::to_owned));
        failed += usize::from(!out.status.success());
    }
    if failed > 0 {
        return Err(Error::new(format!("{failed} of {n} workers failed")));
    }
    Ok(Some(lines))
}

fn export_cmd(lib: &Photos, uuids: Vec<String>, args: &ExportArgs, jobs: usize) -> Result<()> {
    let written: Vec<PathBuf> = match fan_out(uuids.len(), jobs)? {
        Some(lines) => lines.into_iter().map(PathBuf::from).collect(),
        None => {
            let per_photo = ExportOptions {
                reveal_in_finder: false,
                ..ExportOptions::from(args)
            };
            let mut written = vec![];
            for uuid in my_chunk(uuids) {
                for path in lib.photo(&uuid)?.export(&args.to, &per_photo)? {
                    println!("{}", path.display());
                    written.push(path);
                }
            }
            written
        }
    };
    if args.reveal && !is_worker() && !written.is_empty() {
        lib.reveal_in_finder(&written)?;
    }
    Ok(())
}

fn run(lib: &Photos, cmd: Cmd, jobs: usize) -> Result<ExitCode> {
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
            return Ok(if running {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            });
        }
        Cmd::Activate => lib.activate()?,
        Cmd::Quit => lib.quit()?,
        Cmd::Hide => lib.hide()?,
        Cmd::Open { library, delay } => lib.open(&library, delay)?,
        Cmd::Selection => lib.selection()?.iter().for_each(|p| println!("{}", p.uuid)),
        Cmd::Photos {
            search,
            range,
            favorites,
        } => {
            let ids = if let Some(text) = search {
                lib.search(&text)?
            } else if let Some(range) = range {
                let (start, stop) = parse_range(&range)?;
                lib.photo_ids_in_range(start, stop)?
            } else if favorites {
                lib.favorites()?.photo_ids()?
            } else {
                lib.all_photo_ids()?
            };
            ids.iter()
                .for_each(|id| println!("{}", shoebox::uuid_to_id(id, "").0));
        }
        Cmd::Show { uuids } => {
            if fan_out(uuids.len(), jobs)?.is_some() {
                return Ok(ExitCode::SUCCESS);
            }
            for uuid in my_chunk(uuids) {
                println!("{}", photo_json(&lib.photo(&uuid)?)?);
            }
        }
        Cmd::Set {
            uuids,
            title,
            description,
            keywords,
            favorite,
            location,
            date,
        } => {
            let location = location.map(|l| parse_location(&l)).transpose()?;
            let date = date.map(|d| parse_date(&d)).transpose()?;
            let keywords = keywords.map(|k| split_list(&k));
            if fan_out(uuids.len(), jobs)?.is_some() {
                return Ok(ExitCode::SUCCESS);
            }
            for uuid in my_chunk(uuids) {
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
        Cmd::Import {
            paths,
            album,
            skip_duplicate_check,
        } => {
            let album = album.map(|a| find_album(lib, &a)).transpose()?;
            for photo in lib.import(&paths, album.as_ref(), skip_duplicate_check)? {
                println!("{}", photo.uuid);
            }
        }
        Cmd::Export { uuids, opts } => export_cmd(lib, uuids, &opts, jobs)?,
        Cmd::Duplicate { uuid } => println!("{}", lib.photo(&uuid)?.duplicate()?.uuid),
        Cmd::Albums { top_level } => {
            for album in lib.albums(top_level)? {
                println!(
                    "{}",
                    json!({ "uuid": album.uuid, "path": album.path_str('/')? })
                );
            }
        }
        Cmd::Album(cmd) => album_cmd(lib, cmd, jobs)?,
        Cmd::Folders { top_level } => {
            for folder in lib.folders(top_level)? {
                println!("{}", folder.path_str('/')?);
            }
        }
        Cmd::Folder(cmd) => folder_cmd(lib, cmd)?,
        Cmd::Spotlight { uuid } => lib.photo(&uuid)?.spotlight()?,
        Cmd::Dupes(args) => dupes_cmd(lib, &args)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn dupes_cmd(lib: &Photos, a: &DupesArgs) -> Result<()> {
    let db = Library::new(&a.library);
    let week = dupes::iso_week_label(Local::now().date_naive());

    // Only what AppleScript can address, minus anything already queued by an earlier run.
    let visible: HashSet<String> = lib
        .all_photo_ids()?
        .iter()
        .map(|id| shoebox::uuid_to_id(id, "").0)
        .collect();
    let shoebox_albums = db.shoebox_albums()?;
    let queued: HashSet<&str> = shoebox_albums.iter().map(|s| s.uuid.as_str()).collect();
    let mut assets: Vec<Asset> = db
        .assets()?
        .into_iter()
        .filter(|x| visible.contains(&x.uuid) && !x.albums.iter().any(|u| queued.contains(u.as_str())))
        .collect();
    assets.sort_by(|x, y| x.created.total_cmp(&y.created));

    let members: Option<HashSet<String>> = match &a.album {
        Some(album) => Some(db.album_members(album)?.into_iter().collect()),
        None => None,
    };
    // ponytail: dates are UTC days; a few hours' skew at slice edges, which --window padding absorbs
    let secs = |d: NaiveDate| (d.and_time(NaiveTime::MIN).and_utc().timestamp() - CORE_DATA_EPOCH) as f64;
    let lo = a.from.map_or(f64::MIN, secs);
    let hi = a.to.map_or(f64::MAX, |d| secs(d + Days::new(1)));
    let in_scope = |x: &Asset| match &members {
        Some(m) => m.contains(&x.uuid),
        None => (lo..hi).contains(&x.created),
    };

    let mut groups = 0;
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut planned: Vec<(String, Merge, Vec<String>)> = vec![];
    let mut review: Vec<Vec<String>> = vec![];
    let mut no_thumbnail = 0;
    let uuids = |xs: &[&Asset]| xs.iter().map(|x| x.uuid.clone()).collect::<Vec<_>>();

    if a.mode == Match::Exact {
        for g in dupes::exact_groups(&assets) {
            if !g.members().iter().any(|x| in_scope(x)) {
                continue;
            }
            groups += 1;
            match g {
                Exact::Queue {
                    survivor,
                    losers,
                    merge,
                } => {
                    println!(
                        "{}",
                        json!({
                            "match": "exact",
                            "survivor": survivor.uuid,
                            "delete": uuids(&losers),
                            "merge": (!merge.is_empty()).then(|| merge_json(&merge)),
                        })
                    );
                    planned.push((survivor.uuid.clone(), merge, uuids(&losers)));
                }
                Exact::Skip { members, reason } => {
                    println!(
                        "{}",
                        json!({ "match": "exact", "skipped": reason, "members": uuids(&members) })
                    );
                    *skipped.entry(reason).or_default() += 1;
                }
            }
        }
    } else {
        let preset = if a.mode == Match::Near { 4 } else { 10 };
        let max_dist = a.threshold.unwrap_or(preset);
        // album scope compares album members; a date slice reads --window past each edge
        let candidates: Vec<&Asset> = match &members {
            Some(_) => assets.iter().filter(|x| in_scope(x)).collect(),
            None => {
                let first = assets.partition_point(|x| x.created < lo);
                let last = assets.partition_point(|x| x.created < hi);
                assets[first.saturating_sub(a.window)..(last + a.window).min(assets.len())]
                    .iter()
                    .collect()
            }
        };
        let hashed = thumbnail_hashes(&db, &candidates);
        no_thumbnail = candidates.len() - hashed.len();
        for g in dupes::similar_groups(&hashed, a.window, max_dist) {
            if !g.iter().any(|&i| in_scope(hashed[i].0)) {
                continue;
            }
            groups += 1;
            let members: Vec<String> = g.iter().map(|&i| hashed[i].0.uuid.clone()).collect();
            let name = if a.mode == Match::Near { "near" } else { "similar" };
            println!("{}", json!({ "match": name, "members": members }));
            review.push(members);
        }
    }

    let to_delete: usize = planned.iter().map(|p| p.2.len()).sum();
    let to_review: usize = review.iter().map(Vec::len).sum();
    let mut written = vec![];
    if !a.dry_run && (!planned.is_empty() || !review.is_empty()) {
        let folder = lib.make_folders(&[SHOEBOX_FOLDER.to_owned()])?;
        let album_named = |name: &str| match folder.album(name)? {
            Some(album) => Ok(album),
            None => folder.create_album(name),
        };

        let mut losers = vec![];
        for (survivor, merge, ls) in planned {
            match apply_merge(lib, &survivor, &merge) {
                Ok(()) => losers.extend(ls),
                Err(e) => {
                    eprintln!("shoebox: merging onto {survivor} failed, its copies stay put: {e}");
                    *skipped.entry("merge-failed").or_default() += 1;
                }
            }
        }
        if !losers.is_empty() {
            let name = dupes::delete_album_name(&week);
            album_named(&name)?.add_uuids(&losers)?;
            written.push(name);
        }

        let open = format!("{REVIEW_PREFIX}{week} #");
        let last = shoebox_albums
            .iter()
            .filter_map(|s| Some((s.title.strip_prefix(&open)?.parse::<usize>().ok()?, s.members)))
            .max();
        let sizes: Vec<usize> = review.iter().map(Vec::len).collect();
        let last_n = last.map_or(0, |l| l.0);
        for (k, bucket) in dupes::pack_review(&sizes, a.review_size, last.map(|l| l.1))
            .iter()
            .enumerate()
        {
            if bucket.is_empty() {
                continue;
            }
            let name = dupes::review_album_name(&week, last_n + k);
            let assets: Vec<String> = bucket.iter().flat_map(|&g| review[g].clone()).collect();
            album_named(&name)?.add_uuids(&assets)?;
            written.push(name);
        }
    }

    // Final cleanup: earlier weeks' Shoebox albums you've emptied.
    let mut removed = vec![];
    for s in &shoebox_albums {
        if s.members == 0 && dupes::album_week(&s.title).is_some_and(|w| w != week) {
            if !a.dry_run {
                lib.delete_album(&lib.album(&s.uuid)?)?;
            }
            removed.push(s.title.clone());
        }
    }

    eprintln!(
        "{}",
        json!({
            "dry_run": a.dry_run,
            "groups": groups,
            "delete": to_delete,
            "review": to_review,
            "skipped": skipped,
            "no_thumbnail": no_thumbnail,
            "albums": written,
            "removed_albums": removed,
        })
    );
    Ok(())
}

/// dHash of each asset's thumbnail across all cores; assets without one are dropped.
fn thumbnail_hashes<'a>(db: &Library, assets: &[&'a Asset]) -> Vec<(&'a Asset, u64)> {
    let n = thread::available_parallelism().map_or(4, |n| n.get());
    let size = assets.len().div_ceil(n).max(1);
    let hashes: Vec<Option<u64>> = thread::scope(|s| {
        let workers: Vec<_> = assets
            .chunks(size)
            .map(|chunk| {
                s.spawn(move || {
                    chunk
                        .iter()
                        .map(|x| db.thumbnail(&x.uuid).and_then(|p| dupes::dhash(&p).ok()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("hash worker"))
            .collect()
    });
    assets
        .iter()
        .zip(hashes)
        .filter_map(|(x, h)| Some((*x, h?)))
        .collect()
}

fn apply_merge(lib: &Photos, survivor: &str, m: &Merge) -> Result<()> {
    if m.is_empty() {
        return Ok(());
    }
    let photo = lib.photo(survivor)?;
    for album in &m.albums {
        lib.album(album)?.add_uuids(&[survivor.to_owned()])?;
    }
    if let Some(k) = &m.keywords {
        photo.set_keywords(k)?;
    }
    if m.favorite {
        photo.set_favorite(true)?;
    }
    if let Some(t) = &m.title {
        photo.set_title(t)?;
    }
    if let Some(d) = &m.description {
        photo.set_description(d)?;
    }
    Ok(())
}

fn merge_json(m: &Merge) -> Json {
    json!({
        "albums": m.albums,
        "keywords": m.keywords,
        "favorite": m.favorite,
        "title": m.title,
        "description": m.description,
    })
}

fn album_cmd(lib: &Photos, cmd: AlbumCmd, jobs: usize) -> Result<()> {
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
            let photos = uuids
                .iter()
                .map(|u| lib.photo(u))
                .collect::<Result<Vec<Photo>>>()?;
            find_album(lib, &album)?.add(&photos)?;
        }
        AlbumCmd::Remove { album, uuids } => {
            let mut a = find_album(lib, &album)?;
            let ids: Vec<String> = uuids
                .iter()
                .map(|u| shoebox::uuid_to_id(u, "/L0/001").1)
                .collect();
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
            let ids = find_album(lib, &album)?.photo_ids()?;
            export_cmd(lib, ids, &opts, jobs)?;
        }
        AlbumCmd::Spotlight { album } => find_album(lib, &album)?.spotlight()?,
    }
    Ok(())
}

fn folder_cmd(lib: &Photos, cmd: FolderCmd) -> Result<()> {
    match cmd {
        FolderCmd::Show { path } => {
            let f = find_folder(lib, &path)?;
            let albums = f
                .albums()?
                .iter()
                .map(Album::name)
                .collect::<Result<Vec<_>>>()?;
            let subfolders = f
                .subfolders()?
                .iter()
                .map(Folder::name)
                .collect::<Result<Vec<_>>>()?;
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
        FolderCmd::Create { path } => {
            println!("{}", lib.make_folders(&split_path(&path))?.path_str('/')?)
        }
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
    lib.album_by_name(name_or_uuid, false)?
        .ok_or_else(|| Error::new(format!("no album named {name_or_uuid:?}")))
}

fn find_folder<'a>(lib: &'a Photos, path: &str) -> Result<Folder<'a>> {
    lib.folder_by_path(&split_path(path))?
        .ok_or_else(|| Error::new(format!("no folder at {path:?}")))
}

// ponytail: folder names containing "/" can't be addressed from the CLI; the library API takes a Vec.
fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_range(s: &str) -> Result<(i64, i64)> {
    let num = |n: &str| {
        n.trim()
            .parse::<i64>()
            .map_err(|_| Error::new(format!("bad range {s:?}: expected START:STOP or STOP")))
    };
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
    Ok((
        Some(lat.trim().parse().map_err(|_| bad())?),
        Some(lon.trim().parse().map_err(|_| bad())?),
    ))
}

fn parse_date(s: &str) -> Result<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, DATE_FMT).map_err(|e| {
        Error::new(format!(
            "bad date {s:?} ({e}): expected YYYY-MM-DDTHH:MM:SS"
        ))
    })
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
        assert!(looks_like_uuid(
            "EC19EA1A-FC91-449C-8925-B13D863E2EDB/L0/040"
        ));
        assert!(!looks_like_uuid("Vacation"));
        Cli::command().debug_assert();
    }

    #[test]
    fn chunks_cover_everything_once_in_order() {
        for len in 0..20 {
            for n in 1..6 {
                let all: Vec<usize> = (0..n).flat_map(|i| chunk_range(len, i, n)).collect();
                assert_eq!(all, (0..len).collect::<Vec<_>>(), "len={len} n={n}");
            }
        }
    }
}
