mod download;
mod search;
mod spotify;

use rayon::prelude::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant, UNIX_EPOCH},
};

pub type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub static HOME: LazyLock<PathBuf> = LazyLock::new(|| {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    PathBuf::from(home.unwrap_or_default()).join(".spotdl")
});

const DEFAULTS: &str = r#"{"output":"{artists} - {title}.{output-ext}","format":"opus","bitrate":"disable","overwrite":"skip","threads":16,"meta_threads":128,"ffmpeg":"ffmpeg","yt_dlp":"yt-dlp","audio_providers":["youtube-music"],"lyrics_providers":["genius","synced","azlyrics","musixmatch"],"search_query":null,"cookie_file":null,"proxy":null,"yt_dlp_args":null,"ffmpeg_args":null,"lyrics_aligner":null,"lyrics_aligner_jobs":1,"restrict":null,"playlist_numbering":false,"playlist_retain_track_cover":false,"m3u":null,"archive":null,"save_file":null,"filter_results":true,"album_type":null,"print_errors":false,"save_errors":null,"sponsor_block":false,"preload":false,"id3_separator":"/","generate_lrc":false,"force_update_metadata":false,"only_verified_results":false,"sync_without_deleting":false,"sync_remove_lrc":false,"max_filename_length":0,"detect_formats":[],"ignore_albums":[],"skip_explicit":false,"redownload":false,"skip_album_art":false,"replaygain":false,"folder_art":false,"create_skip_file":false,"respect_skip_file":false,"no_cache":false,"genius_token":"","log_level":"INFO","log_format":null,"use_cache_file":false,"max_retries":3,"simple_tui":false,"ytm_data":false,"add_unavailable":false,"fetch_albums":false,"scan_for_songs":false,"load_config":true}"#;
const ALIASES: [(&str, &str); 4] = [
    ("audio", "audio_providers"),
    ("lyrics", "lyrics_providers"),
    ("genius_access_token", "genius_token"),
    ("dont_filter_results", "filter_results"),
];
const PROVIDERS: [(&str, &[&str]); 2] = [
    ("audio_providers", &["youtube-music", "youtube", "soundcloud"]),
    ("lyrics_providers", &["genius", "synced", "azlyrics", "musixmatch"]),
];

/// One track, in the `.spotdl` save-file format shared with Python spotDL.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Song {
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
pub struct Extra {
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

static CONFIG: OnceLock<Value> = OnceLock::new();

/// Settings: the defaults, then `config.json`, then the command line.
pub fn cfg() -> &'static Value {
    CONFIG.get().expect("settings are loaded before use")
}

pub fn on(key: &str) -> bool {
    cfg()[key] == true
}

pub fn text(key: &str) -> Option<&'static str> {
    cfg()[key].as_str()
}

pub fn list(key: &str) -> impl Iterator<Item = &'static str> {
    cfg()[key].as_array().into_iter().flatten().filter_map(Value::as_str)
}

pub fn number(key: &str) -> u64 {
    cfg()[key].as_u64().unwrap_or_default()
}

pub fn now() -> u64 {
    UNIX_EPOCH.elapsed().unwrap_or_default().as_secs()
}

/// Spaces the requests to one site `ms` apart, across all threads.
pub struct Pace(Mutex<Option<Instant>>, u64);

impl Pace {
    pub const fn new(ms: u64) -> Self {
        Self(Mutex::new(None), ms)
    }

    pub fn wait(&self) {
        let slot = {
            let mut next = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let slot = next.map_or(Instant::now(), |next| next.max(Instant::now()));
            *next = Some(slot + Duration::from_millis(self.1));
            slot
        };
        std::thread::sleep(slot.saturating_duration_since(Instant::now()));
    }

    /// Makes every caller wait `pause` before the next request, for a service that asked us to slow down.
    pub fn hold(&self, pause: Duration) {
        let mut next = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *next = Some(next.unwrap_or_else(Instant::now).max(Instant::now() + pause));
    }
}

pub static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .max_idle_connections_per_host(64)
        .proxy(text("proxy").and_then(|proxy| ureq::Proxy::new(proxy).ok()))
        .build();
    ureq::Agent::new_with_config(config)
});

/// GET (or POST, given a body), retrying connection errors, 429 and 5xx with exponential backoff.
pub fn http(url: &str, headers: &[(&str, &str)], body: Option<&str>) -> R<Vec<u8>> {
    http_paced(url, headers, body, number("max_retries"), None)
}

/// [`http`] with `retries` retries; 0 for a caller that paces its own retries.
pub fn http_retrying(url: &str, headers: &[(&str, &str)], body: Option<&str>, retries: u64) -> R<Vec<u8>> {
    http_paced(url, headers, body, retries, None)
}

/// The pause before retry `attempt`: the server's Retry-After, else doubling. Capped, so that a dead host or a long
/// ban under a high `--max-retries` fails within minutes rather than hanging each request for half an hour.
fn backoff(attempt: u64, retry_after: Option<u64>) -> Duration {
    Duration::from_secs(retry_after.unwrap_or(1 << attempt.min(5)).min(60))
}

/// [`http_retrying`], spaced by `pace`: a 429 holds it, so that every thread waits the pause out together rather
/// than each asking again at its own time and keeping the limit on.
pub fn http_paced(
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
    retries: u64,
    pace: Option<&Pace>,
) -> R<Vec<u8>> {
    let mut attempt = 0;
    loop {
        if let Some(pace) = pace {
            pace.wait();
        }
        let method = if body.is_some() { "POST" } else { "GET" };
        let request = ureq::http::Request::builder().method(method).uri(url);
        let request = headers.iter().fold(request, |request, (key, value)| request.header(*key, *value));
        let reply = AGENT.run(request.body(body.unwrap_or_default().as_bytes().to_vec())?);
        let status = reply.as_ref().map(|reply| reply.status()).ok();
        let retry = status.is_none_or(|status| status == 429 || status.is_server_error());
        if retry && attempt < retries {
            attempt += 1;
            let after = reply.as_ref().ok().and_then(|reply| reply.headers().get("retry-after"));
            let pause = backoff(attempt, after.and_then(|after| after.to_str().ok()?.trim().parse().ok()));
            match pace {
                Some(pace) if status.is_some_and(|status| status == 429) => pace.hold(pause),
                _ => std::thread::sleep(pause),
            }
            continue;
        }
        let mut reply = reply?;
        if !reply.status().is_success() {
            return Err(format!("HTTP {} for {url}", reply.status()).into());
        }
        return Ok(reply.body_mut().with_config().limit(1 << 28).read_to_vec()?);
    }
}

pub fn http_json<T: DeserializeOwned>(url: &str, headers: &[(&str, &str)], body: Option<&str>) -> R<T> {
    Ok(serde_json::from_slice(&http(url, headers, body)?)?)
}

/// `fetch()`, remembered in `~/.spotdl/cache` for `days` (0 means not at all).
pub fn cached(key: &str, days: u64, fetch: impl FnOnce() -> R<Value>) -> R<Value> {
    let path = HOME.join("cache").join(&format!("{:x}", Sha1::digest(key))[..16]);
    let days = if on("no_cache") { 0 } else { days };
    let age = fs::metadata(&path).and_then(|meta| meta.modified()).ok().and_then(|time| time.elapsed().ok());
    if age.is_some_and(|age| age.as_secs() / 86_400 < days) {
        if let Some(value) = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()) {
            return Ok(value);
        }
    }
    let value = fetch()?;
    if days > 0 {
        let _ = fs::create_dir_all(HOME.join("cache")).and_then(|_| fs::write(&path, value.to_string()));
    }
    Ok(value)
}

pub fn url_encode(value: &str) -> String {
    const UNRESERVED: &percent_encoding::AsciiSet =
        &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
    percent_encoding::utf8_percent_encode(value, UNRESERVED).to_string()
}

pub fn log(level: &str, message: &str) {
    let rank = |name: &str| match name.to_ascii_uppercase().as_str() {
        "DEBUG" => 0,
        "WARNING" | "WARN" => 2,
        "ERROR" | "CRITICAL" => 3,
        _ => 1,
    };
    if rank(level) < rank(text("log_level").unwrap_or("INFO")) {
        return;
    }
    let line = text("log_format").map_or_else(
        || format!("{level}: {message}"),
        |format| {
            let format = format.replace("%(asctime)s", &now().to_string()).replace("%(name)s", "spotdl");
            format.replace("%(levelname)s", level).replace("%(message)s", message)
        },
    );
    eprintln!("{line}");
}

/// Loads the settings into `cfg()` and returns the operation and its queries.
fn settings(args: &[String]) -> R<(String, Vec<String>)> {
    let mut cfg: Value = serde_json::from_str(DEFAULTS)?;
    let (mut cli, mut queries, mut config) = (serde_json::Map::new(), Vec::new(), None);
    let mut i = 0;
    while i < args.len() {
        i += 1;
        let Some(option) = args[i - 1].strip_prefix("--") else {
            queries.push(args[i - 1].clone());
            continue;
        };
        let (raw, inline) = option.split_once('=').map_or((option, None), |(name, value)| (name, Some(value)));
        let name = raw.replace('-', "_");
        let key = ALIASES.iter().find(|alias| alias.0 == name).map_or(name.as_str(), |alias| alias.1);
        let rest: Vec<&str> = match inline {
            Some(value) => vec![value],
            None => args[i..].iter().map(String::as_str).take_while(|arg| !arg.starts_with("--")).collect(),
        };
        let (taken, value) = match (cfg.get(key), rest.first().copied()) {
            _ if key == "config" => {
                config = Some(PathBuf::from(rest.first().ok_or("--config requires a file path")?));
                i += usize::from(inline.is_none());
                continue;
            }
            (Some(Value::Array(_)), _) => {
                let items = rest.iter().flat_map(|value| value.split(',')).map(str::trim).filter(|v| !v.is_empty());
                (rest.len(), json!(items.collect::<Vec<_>>()))
            }
            (Some(Value::Bool(_)), value) => {
                let on = name != "dont_filter_results" && value != Some("false");
                (usize::from(matches!(value, Some("true" | "false"))), json!(on))
            }
            (Some(Value::Number(_)), Some(value)) => (1, json!(value.parse::<u64>()?)),
            (Some(Value::String(_) | Value::Number(_)), None) => {
                return Err(format!("missing value for --{raw}").into());
            }
            (Some(_), value) => (usize::from(value.is_some()), json!(value.unwrap_or_default())),
            (None, _) => return Err(format!("unknown option --{raw}").into()),
        };
        cli.insert(key.to_string(), value);
        i += if inline.is_some() { 0 } else { taken };
    }
    let path = config.clone().unwrap_or_else(|| HOME.join("config.json"));
    if config.is_some() && !path.is_file() {
        return Err(format!("config file not found: {}", path.display()).into());
    }
    if path.is_file() {
        let file: serde_json::Map<String, Value> = serde_json::from_slice(&fs::read(&path)?)?;
        if config.is_some() || file.get("load_config") != Some(&json!(false)) {
            file.into_iter().filter(|(_, value)| !value.is_null()).for_each(|(key, value)| cfg[key] = value);
        }
    }
    cli.into_iter().for_each(|(key, value)| cfg[key] = value);
    for (key, known) in PROVIDERS {
        let names = cfg[key].as_array().into_iter().flatten();
        if let Some(name) = names.filter_map(Value::as_str).find(|name| !known.contains(name)) {
            return Err(format!("unknown {} provider: {name}", key.trim_end_matches("_providers")).into());
        }
    }
    CONFIG.set(cfg).map_err(|_| "settings were already loaded")?;
    let op =
        ["download", "save", "sync", "url", "meta"].into_iter().find(|op| queries.first().is_some_and(|q| q == op));
    Ok((op.map_or_else(|| "download".into(), |_| queries.remove(0)), queries))
}

/// Playlist numbering, and the album, album type and explicit filters.
fn arrange(mut songs: Vec<Song>) -> Vec<Song> {
    let (numbering, retain) = (on("playlist_numbering"), on("playlist_retain_track_cover"));
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
    let ignored: Vec<String> = list("ignore_albums").map(str::to_lowercase).collect();
    songs.retain(|song| {
        let album = song.album_name.to_lowercase();
        let wrong_type = text("album_type").is_some_and(|kind| kind != song.album_type);
        !(on("skip_explicit") && song.explicit) && !wrong_type && !ignored.iter().any(|name| album.contains(name))
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
    let pattern = match &cfg()["m3u"] {
        Value::String(pattern) if op != "url" => pattern.as_str(),
        Value::Bool(true) if op != "url" => "",
        _ => return Ok(()),
    };
    let pattern = match pattern {
        "" | "true" => "{list[0]}.m3u8".into(),
        name if name.ends_with('/') => format!("{name}{{list[0]}}.m3u8"),
        name if name.ends_with(".m3u") || name.ends_with(".m3u8") => name.into(),
        name => format!("{name}.m3u8"),
    };
    let first = listed.first().map_or_else(|| "playlist".into(), |(song, _)| download::fill("{list}", song));
    let mut files = BTreeMap::<String, String>::new();
    for (song, result) in listed {
        if !(op == "save" || on("add_unavailable") || result.as_ref().is_ok_and(|path| path.is_file())) {
            continue;
        }
        let path = result.as_ref().map_or_else(|_| download::path(song), Clone::clone);
        let name = download::fill(&pattern, song).replace("{list[0]}", &first);
        let line = download::fill("#EXTINF:{duration},{album-artist} - {title}", song);
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
    if on("print_errors") {
        errors.iter().for_each(|error| eprintln!("{error}"));
    }
    if let Some(path) = text("save_errors").filter(|_| !errors.is_empty()) {
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
        fs::write(HOME.join("config.json"), serde_json::to_vec_pretty(&serde_json::from_str::<Value>(DEFAULTS)?)?)?;
        println!("Config file generated at {}", HOME.join("config.json").display());
        return Ok(());
    }
    let (op, queries) = if flag("--help") || flag("-h") { Default::default() } else { settings(&args)? };
    if queries.is_empty() {
        println!("Usage: spotdl [download|save|sync|url|meta] <query>... [--option value]...");
        return Ok(());
    }
    let save_file = text("save_file");
    if save_file.is_some_and(|path| path != "-" && !path.ends_with(".spotdl")) {
        return Err("save file must end with .spotdl or be '-'".into());
    }
    if op == "save" && save_file.is_none() {
        return Err("save file is not specified".into());
    }
    let meta_threads = number("meta_threads").max(1) as usize;
    rayon::ThreadPoolBuilder::new().num_threads(meta_threads).build_global()?;
    if op == "meta" {
        return download::meta(&queries);
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
    let (songs, errors) = resolve(&queries, downloads && on("fetch_albums"));
    if let Some(file) = synced.filter(|_| errors.is_empty() && !on("sync_without_deleting")) {
        download::prune(&serde_json::from_value::<Vec<Song>>(file["songs"].clone())?, &songs);
    }
    // One job per output file (per URL when nothing is written); a pinned YouTube URL beats a search.
    let (mut jobs, mut slots, mut index) = (Vec::<Song>::new(), HashMap::new(), Vec::new());
    for song in &songs {
        let key = if downloads { download::path(song) } else { PathBuf::from(&song.url) };
        let slot = *slots.entry(key).or_insert_with(|| {
            jobs.push(song.clone());
            jobs.len() - 1
        });
        if jobs[slot].download_url.is_none() {
            jobs[slot].download_url.clone_from(&song.download_url);
        }
        index.push(slot);
    }
    let threads = if op == "save" { meta_threads } else { number("threads").max(1) as usize };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
    let done: Vec<(Song, R<PathBuf>)> =
        pool.install(|| jobs.into_par_iter().with_max_len(1).map(|song| download::process(song, &op)).collect());
    download::album_gains(&done.iter().filter_map(|(_, result)| result.as_ref().ok().cloned()).collect::<Vec<_>>());
    download::report_unchecked();
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
mod tests {
    use super::{backoff, http_paced, Pace};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };

    #[test]
    fn retry_pauses_are_capped() {
        assert_eq!(backoff(1, None), Duration::from_secs(2));
        // `--max-retries 10` against a dead host: not 1024 s for the last retry.
        assert_eq!(backoff(10, None), Duration::from_secs(32));
        assert_eq!(backoff(1, Some(5)), Duration::from_secs(5));
        assert_eq!(backoff(1, Some(86_400)), Duration::from_secs(60));
    }

    #[test]
    fn a_429_holds_every_thread_for_its_retry_after() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", server.local_addr().unwrap());
        std::thread::spawn(move || {
            for (i, stream) in server.incoming().enumerate() {
                let mut stream = stream.unwrap();
                let _ = stream.read(&mut [0; 4096]);
                let reply = if i == 0 { "429 Too Many Requests\r\nRetry-After: 1" } else { "200 OK" };
                let _ = write!(stream, "HTTP/1.1 {reply}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        static PACE: Pace = Pace::new(0);
        let start = Instant::now();
        let other = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            PACE.wait();
            start.elapsed()
        });
        assert_eq!(http_paced(&url, &[], None, 3, Some(&PACE)).unwrap(), b"ok");
        // One second as the server asked, not the two of the doubling pause.
        assert!((900..1900).contains(&start.elapsed().as_millis()), "{:?}", start.elapsed());
        // Another thread that asks during the hold waits it out too.
        assert!(other.join().unwrap() >= Duration::from_millis(900));
    }
}
