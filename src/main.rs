mod download;
mod files;
mod genius;
mod html;
mod http;
mod lrclib;
mod lyrics;
mod meta;
mod replaygain;
mod search;
mod settings;
mod spotify;
mod tags;

use crate::settings::settings;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::UNIX_EPOCH,
};

pub(crate) type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    PathBuf::from(home.unwrap_or_default()).join(".spotdl")
});

/// One track, in the `.spotdl` save-file format shared with Python spotDL.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Song {
    pub name: String,
    pub artists: Vec<String>,
    pub artist: String,
    pub genres: Vec<String>,
    pub disc_number: u32,
    pub disc_count: u32,
    pub album_name: String,
    pub album_artist: String,
    pub duration: f64,
    pub year: u32,
    pub date: String,
    pub track_number: u32,
    pub tracks_count: u32,
    pub song_id: String,
    pub explicit: bool,
    pub publisher: String,
    pub url: String,
    pub isrc: Option<String>,
    pub cover_url: Option<String>,
    pub copyright_text: Option<String>,
    pub download_url: Option<String>,
    pub lyrics: Option<String>,
    pub popularity: Option<u64>,
    pub album_id: String,
    pub artist_id: String,
    pub album_type: String,
    pub list_name: Option<String>,
    pub list_url: Option<String>,
    pub list_position: Option<usize>,
    pub list_length: Option<usize>,
    #[serde(skip)]
    pub extra: Extra,
}

/// Run-time data that the save-file format has no field for.
#[derive(Clone, Debug, Default)]
pub(crate) struct Extra {
    pub list_author: Option<String>,
    pub list_cover_url: Option<String>,
    pub original_date: String,
    pub upc: String,
    pub bpm: f64,
    pub gain: f64,
    /// The song as it was before `--playlist-numbering` or `--playlist-retain-track-cover` gave it the playlist's
    /// album, album artist, disc and track: Deezer's album is found, cached and read by the real ones.
    pub release: Option<Box<Song>>,
}

pub(crate) fn now() -> u64 {
    UNIX_EPOCH.elapsed().unwrap_or_default().as_secs()
}

/// The longest time a [`cached`] entry is read, but for those kept for good.
const CACHE_DAYS: u64 = 7;

/// `fetch()`, remembered in `~/.spotdl/cache` for `days` (0 means not at all, at most [`CACHE_DAYS`] or forever).
pub(crate) fn cached(key: &str, days: u64, fetch: impl FnOnce() -> R<Value>) -> R<Value> {
    // Entries kept for good go to their own folder, which [`prune_cache`] leaves alone: the names are hashes.
    let dir = if days == u64::MAX { HOME.join("cache/keep") } else { HOME.join("cache") };
    let path = dir.join(&format!("{:x}", Sha1::digest(key))[..16]);
    let days = if settings().no_cache { 0 } else { days };
    let age = fs::metadata(&path).and_then(|meta| meta.modified()).ok().and_then(|time| time.elapsed().ok());
    if age.is_some_and(|age| age.as_secs() / 86_400 < days) {
        if let Some(value) = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()) {
            return Ok(value);
        }
    }
    let value = fetch()?;
    if days > 0 {
        let _ = fs::create_dir_all(dir).and_then(|_| fs::write(&path, value.to_string()));
    }
    Ok(value)
}

/// Deletes the files in `dir` older than `days`. An expired entry is otherwise only replaced when its key is asked for
/// again, so the cache (mostly YouTube Music replies of up to 300 KB) grew with every new query, without end.
fn prune_cache(dir: &Path, days: u64) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let meta = entry.metadata().ok().filter(|meta| meta.is_file());
        let age = meta.and_then(|meta| meta.modified().ok()?.elapsed().ok());
        if age.is_some_and(|age| age.as_secs() / 86_400 >= days) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn url_encode(value: &str) -> String {
    const UNRESERVED: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
    percent_encoding::utf8_percent_encode(value, UNRESERVED).to_string()
}

pub(crate) fn log(level: &str, message: &str) {
    let rank = |name: &str| match name.to_ascii_uppercase().as_str() {
        "DEBUG" => 0,
        "WARNING" | "WARN" => 2,
        "ERROR" | "CRITICAL" => 3,
        _ => 1,
    };
    if rank(level) < rank(settings().log_level.as_str()) {
        return;
    }
    let line = settings().log_format.as_deref().map_or_else(
        || format!("{level}: {message}"),
        |format| {
            let format = format.replace("%(asctime)s", &now().to_string()).replace("%(name)s", "spotdl");
            format.replace("%(levelname)s", level).replace("%(message)s", message)
        },
    );
    eprintln!("{line}");
}

/// Playlist numbering, and the album, album type and explicit filters.
fn arrange(mut songs: Vec<Song>) -> Vec<Song> {
    let (numbering, retain) = (settings().playlist_numbering, settings().playlist_retain_track_cover);
    for song in songs.iter_mut().filter(|song| (numbering || retain) && song.list_name.is_some()) {
        song.extra.release = Some(Box::new(song.clone()));
        song.album_name = song.list_name.clone().unwrap_or_default();
        song.track_number = song.list_position.map_or(song.track_number, |position| position as u32);
        song.tracks_count = song.list_length.map_or(song.tracks_count, |length| length as u32);
        (song.disc_number, song.disc_count) = (1, 1);
        if numbering && song.list_url.as_deref().is_some_and(|url| url.contains("/playlist/")) {
            song.album_artist = song.extra.list_author.clone().unwrap_or_else(|| song.album_artist.clone());
            if !retain {
                song.cover_url.clone_from(&song.extra.list_cover_url);
            }
        }
    }
    let ignored: Vec<String> = settings().ignore_albums.iter().map(String::as_str).map(str::to_lowercase).collect();
    songs.retain(|song| {
        let album = song.album_name.to_lowercase();
        let wrong_type = settings().album_type.as_deref().is_some_and(|kind| kind != song.album_type);
        !(settings().skip_explicit && song.explicit) && !wrong_type && !ignored.iter().any(|name| album.contains(name))
    });
    songs
}

fn resolve(queries: &[String], albums: bool) -> (Vec<Song>, Vec<String>) {
    let results: Vec<_> = queries.par_iter().map(|query| (query, spotify::resolve(query))).collect();
    let (mut songs, mut errors) = (Vec::new(), Vec::new());
    for (query, result) in results {
        match result {
            Ok(found) => songs.extend(arrange(found)),
            Err(error) => errors.push(format!("{query}: {error}")),
        }
    }
    if albums {
        let mut ids: Vec<String> = songs.iter().map(|song| format!("spotify:album:{}", song.album_id)).collect();
        ids.retain(|id| id.len() > 14);
        ids.sort();
        ids.dedup();
        let (more, failed) = resolve(&ids, false);
        songs.extend(more);
        errors.extend(failed);
    }
    (songs, errors)
}

fn relative(file: &Path, list: &Path) -> R<String> {
    let file = std::path::absolute(file)?;
    let dir = std::path::absolute(list)?.parent().map(PathBuf::from).unwrap_or_default();
    let shared = file.components().zip(dir.components()).take_while(|(a, b)| a == b).count();
    let up = std::iter::repeat_n(std::path::Component::ParentDir, dir.components().count() - shared);
    Ok(up.chain(file.components().skip(shared)).collect::<PathBuf>().display().to_string())
}

/// One `.m3u8` per list name, with every song of every query, including duplicates that were downloaded once.
fn write_m3u(listed: &[(&Song, &R<PathBuf>)], op: &str) -> R<()> {
    let Some(pattern) = settings().m3u.as_deref().filter(|_| op != "url") else {
        return Ok(());
    };
    let pattern = match pattern {
        "" | "true" => "{list[0]}.m3u8".into(),
        name if name.ends_with('/') => format!("{name}{{list[0]}}.m3u8"),
        name if name.ends_with(".m3u") || name.ends_with(".m3u8") => name.into(),
        name => format!("{name}.m3u8"),
    };
    let first = listed.first().map_or_else(|| "playlist".into(), |(song, _)| files::fill("{list}", song));
    let mut files = BTreeMap::<String, String>::new();
    for (song, result) in listed {
        if !(op == "save" || settings().add_unavailable || result.as_ref().is_ok_and(|path| path.is_file())) {
            continue;
        }
        let path = result.as_ref().map_or_else(|_| files::path(song), Clone::clone);
        let name = files::fill(&pattern, song).replace("{list[0]}", &first);
        let line = files::fill("#EXTINF:{duration},{album-artist} - {title}", song);
        let entry = relative(&path, Path::new(&name))?;
        files.entry(name).or_insert_with(|| "#EXTM3U\n".into()).push_str(&format!("{line}\n{entry}\n"));
    }
    for (name, text) in files {
        fs::create_dir_all(
            Path::new(&name).parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new(".")),
        )?;
        fs::write(name, text)?;
    }
    Ok(())
}

fn report(done: &[(Song, R<PathBuf>)], mut errors: Vec<String>) -> R<()> {
    let failed = done.iter().filter_map(|(song, result)| Some((song, result.as_ref().err()?)));
    errors.extend(failed.map(|(song, error)| format!("{} ({} - {}): {error}", song.url, song.artist, song.name)));
    if settings().print_errors {
        errors.iter().for_each(|error| eprintln!("{error}"));
    }
    if let Some(path) = settings().save_errors.as_deref().filter(|_| !errors.is_empty()) {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        std::io::Write::write_all(&mut file, format!("{}\n{}\n", now(), errors.join("\n")).as_bytes())?;
    }
    let failures = done.iter().filter(|(_, result)| result.is_err()).count();
    eprintln!("{} succeeded, {} failed", done.len() - failures, errors.len());
    match errors.len() {
        0 => Ok(()),
        count => Err(format!("{count} downloads failed").into()),
    }
}

fn run() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|arg| arg == name);
    if flag("--version") {
        println!("spotdl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if flag("--generate-config") {
        fs::create_dir_all(&*HOME)?;
        fs::write(
            HOME.join("config.json"),
            serde_json::to_vec_pretty(&serde_json::from_str::<Value>(settings::DEFAULTS)?)?,
        )?;
        println!("Config file generated at {}", HOME.join("config.json").display());
        return Ok(());
    }
    let (op, queries) = if flag("--help") || flag("-h") { Default::default() } else { settings::load(&args)? };
    if queries.is_empty() {
        println!("Usage: spotdl [download|save|sync|url|meta] <query>... [--option value]...");
        return Ok(());
    }
    let save_file = settings().save_file.as_deref();
    if save_file.is_some_and(|path| path != "-" && !path.ends_with(".spotdl")) {
        return Err("save file must end with .spotdl or be '-'".into());
    }
    if op == "save" && save_file.is_none() {
        return Err("save file is not specified".into());
    }
    let meta_threads = settings().meta_threads.max(1) as usize;
    rayon::ThreadPoolBuilder::new().num_threads(meta_threads).build_global()?;
    prune_cache(&HOME.join("cache"), CACHE_DAYS);
    if op == "meta" {
        return meta::meta(&queries);
    }
    let sync_file = queries.first().filter(|query| op == "sync" && queries.len() == 1 && query.ends_with(".spotdl"));
    let read = |path: &String| -> R<Value> { Ok(serde_json::from_slice(&fs::read(path)?)?) };
    let synced = sync_file.map(read).transpose()?;
    if synced.as_ref().is_some_and(|file| file["type"] != "sync" || !file["songs"].is_array()) {
        return Err("sync file is not valid".into());
    }
    let sync_file = sync_file.cloned();
    let queries: Vec<String> = match &synced {
        Some(file) => serde_json::from_value(file["query"].clone())?,
        None => queries,
    };
    let downloads = matches!(op.as_str(), "download" | "sync");
    let (songs, errors) = resolve(&queries, downloads && settings().fetch_albums);
    if let Some(file) = synced.filter(|_| errors.is_empty() && !settings().sync_without_deleting) {
        download::prune(&serde_json::from_value::<Vec<Song>>(file["songs"].clone())?, &songs);
    }
    // One job per output file (per URL when nothing is written); a pinned YouTube URL beats a search.
    let (mut jobs, mut slots, mut index) = (Vec::<Song>::new(), HashMap::new(), Vec::new());
    for song in &songs {
        let key = if downloads { files::path(song) } else { PathBuf::from(&song.url) };
        let slot = *slots.entry(key).or_insert_with(|| {
            jobs.push(song.clone());
            jobs.len() - 1
        });
        if jobs[slot].download_url.is_none() {
            jobs[slot].download_url.clone_from(&song.download_url);
        }
        index.push(slot);
    }
    let threads = if op == "save" { meta_threads } else { settings().threads.max(1) as usize };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
    let done: Vec<(Song, R<PathBuf>)> =
        pool.install(|| jobs.into_par_iter().with_max_len(1).map(|song| download::process(song, &op)).collect());
    replaygain::album_gains(&done.iter().filter_map(|(_, result)| result.as_ref().ok().cloned()).collect::<Vec<_>>());
    lyrics::report_unchecked();
    let saved: Vec<&Song> = done.iter().map(|(song, _)| song).collect();
    let sync_path = sync_file.as_deref().or(save_file).filter(|_| op == "sync" && errors.is_empty());
    if let Some(path) = sync_path {
        let file = json!({"type": "sync", "query": queries, "songs": saved});
        fs::write(path, serde_json::to_string_pretty(&file)?)?;
    } else if let Some(path) = save_file.filter(|_| op == "save" || op == "download") {
        let text = serde_json::to_string_pretty(&saved)?;
        if path == "-" {
            println!("{text}");
        } else {
            fs::write(path, text)?;
        }
    }
    let listed: Vec<_> = songs.iter().zip(index).map(|(song, slot)| (song, &done[slot].1)).collect();
    write_m3u(&listed, &op)?;
    report(&done, errors)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "fuzz/gen.rs"]
mod gen;

#[cfg(test)]
#[path = "fuzz/main.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::prune_cache;
    use std::{
        fs,
        time::{Duration, SystemTime},
    };

    #[test]
    fn the_cache_drops_expired_entries_but_keeps_the_kept_ones() {
        let dir = std::env::temp_dir().join(format!("spotdl-cache-{}", std::process::id()));
        fs::create_dir_all(dir.join("keep")).unwrap();
        let week_ago = SystemTime::now() - Duration::from_secs(7 * 86_400 + 60);
        for (name, old) in [("old", true), ("new", false), ("keep/old", true)] {
            let file = fs::File::create(dir.join(name)).unwrap();
            if old {
                file.set_modified(week_ago).unwrap();
            }
        }
        prune_cache(&dir, 7);
        let left = ["old", "new", "keep/old"].map(|name| dir.join(name).exists());
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(left, [false, true, true]);
    }
}
