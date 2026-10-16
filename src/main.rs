mod download;
mod search;
mod spotify;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

pub type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

const DEFAULTS: &str = r#"{"output":"{artists} - {title}.{output-ext}","format":"mp3","bitrate":"128k","overwrite":"skip","threads":16,"meta_threads":32,"ffmpeg":"ffmpeg","yt_dlp":"yt-dlp","audio_providers":["youtube-music"],"lyrics_providers":["genius","azlyrics","musixmatch"],"search_query":null,"cookie_file":null,"proxy":null,"yt_dlp_args":null,"ffmpeg_args":null,"restrict":null,"playlist_numbering":false,"playlist_retain_track_cover":false,"m3u":null,"archive":null,"save_file":null,"filter_results":true,"album_type":null,"print_errors":false,"save_errors":null,"sponsor_block":false,"preload":false,"id3_separator":"/","generate_lrc":false,"force_update_metadata":false,"only_verified_results":false,"sync_without_deleting":false,"sync_remove_lrc":false,"max_filename_length":0,"detect_formats":[],"ignore_albums":[],"skip_explicit":false,"redownload":false,"skip_album_art":false,"create_skip_file":false,"respect_skip_file":false,"auth_token":null,"client_id":"","client_secret":"","no_cache":false,"genius_token":"","log_level":"INFO","log_format":null,"cache_path":null,"use_cache_file":false,"max_retries":3,"headless":false,"simple_tui":false,"ytm_data":false,"add_unavailable":false,"fetch_albums":false,"scan_for_songs":false,"user_auth":false,"use_official_api":false,"load_config":true}"#;

const ALIASES: [(&str, &str); 4] = [
    ("audio", "audio_providers"),
    ("lyrics", "lyrics_providers"),
    ("genius_access_token", "genius_token"),
    ("dont_filter_results", "filter_results"),
];

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
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
    pub list_name: Option<String>,
    pub list_url: Option<String>,
    pub list_position: Option<usize>,
    pub list_length: Option<usize>,
    #[serde(skip)]
    pub list_author: Option<String>,
    #[serde(skip)]
    pub list_cover_url: Option<String>,
    pub artist_id: String,
    pub album_type: String,
    #[serde(skip)]
    pub original_date: String,
    #[serde(skip)]
    pub upc: String,
    #[serde(skip)]
    pub bpm: f64,
    #[serde(skip)]
    pub gain: f64,
}

pub fn home() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    PathBuf::from(home.unwrap_or_default()).join(".spotdl")
}

pub fn http(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>, cfg: &Value) -> R<Vec<u8>> {
    let agent = AGENT.get_or_init(|| {
        let proxy = cfg["proxy"].as_str().and_then(|proxy| ureq::Proxy::new(proxy).ok());
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .proxy(proxy)
            .build();
        ureq::Agent::new_with_config(config)
    });
    let attempts = cfg["max_retries"].as_u64().unwrap_or(3).max(1);
    let mut attempt = 1;
    loop {
        let mut request = ureq::http::Request::builder().method(method).uri(url);
        for (key, value) in headers {
            request = request.header(*key, *value);
        }
        let response = agent.run(request.body(body.unwrap_or_default().to_vec())?);
        let retry = match &response {
            Ok(reply) => reply.status() == 429 || reply.status().is_server_error(),
            Err(_) => true,
        };
        if retry && attempt < attempts {
            std::thread::sleep(Duration::from_secs(1 << attempt));
            attempt += 1;
            continue;
        }
        let mut reply = response?;
        if !reply.status().is_success() {
            return Err(format!("HTTP {} for {url}", reply.status()).into());
        }
        return Ok(reply.body_mut().with_config().limit(1 << 28).read_to_vec()?);
    }
}

pub fn json(url: &str, headers: &[(&str, &str)], cfg: &Value) -> R<Value> {
    let bytes = http("GET", url, headers, None, cfg)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn settings() -> R<(String, Vec<String>, Value)> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config_flag = args.iter().position(|arg| arg == "--config");
    let config_path = match config_flag {
        Some(index) => PathBuf::from(
            args.get(index + 1).filter(|arg| !arg.starts_with("--")).ok_or("--config requires a file path")?,
        ),
        None => home().join("config.json"),
    };
    if config_flag.is_some() && !config_path.is_file() {
        return Err(format!("config file not found: {}", config_path.display()).into());
    }
    let mut raw: Value = serde_json::from_str(DEFAULTS)?;
    if config_path.is_file() {
        let config: Value = serde_json::from_slice(&fs::read(&config_path)?)?;
        if config_flag.is_some() || config["load_config"].as_bool().unwrap_or(true) {
            config.as_object().into_iter().flatten().filter(|(_, v)| !v.is_null()).for_each(|(key, value)| {
                raw[key] = value.clone();
            });
        }
    }
    if raw["cache_path"].is_null() {
        raw["cache_path"] = json!(home().join(".spotipy").to_string_lossy());
    }
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let Some(raw_key) = args[i].strip_prefix("--").map(|s| s.replace('-', "_")) else {
            positional.push(args[i].clone());
            i += 1;
            continue;
        };
        if raw_key == "config" {
            i += usize::from(args.get(i + 1).is_some_and(|value| !value.starts_with("--"))) + 1;
            continue;
        }
        let key = ALIASES.iter().find(|a| a.0 == raw_key.as_str()).map_or(raw_key.as_str(), |a| a.1);
        let rest = args[i + 1..].iter().map(String::as_str).take_while(|v| !v.starts_with("--")).collect::<Vec<_>>();
        let values =
            rest.iter().flat_map(|v| v.split(',')).map(str::trim).filter(|v| !v.is_empty()).collect::<Vec<_>>();
        let (take, value) = match (raw.get(key), rest.first().copied()) {
            (Some(Value::Array(_)), _) => (rest.len(), json!(values)),
            (Some(Value::Bool(_)), v) => (
                usize::from(matches!(v, Some("true" | "false"))),
                json!(raw_key != "dont_filter_results" && v != Some("false")),
            ),
            (Some(Value::Number(_)), Some(v)) => (1, json!(v.parse::<usize>()?)),
            (Some(_), Some(v)) => (1, json!(v)),
            (Some(_), _) => (0, json!("")),
            (None, _) if !["help", "check_for_updates", "download_ffmpeg", "download_deno"].contains(&key) => {
                return Err(format!("unknown option --{raw_key}").into());
            }
            (None, _) => (0, json!(true)),
        };
        raw[key] = value;
        i += take + 1;
    }
    if raw["m3u"].is_boolean() {
        let enabled = raw["m3u"] == true;
        raw["m3u"] = if enabled { json!("true") } else { Value::Null };
    }
    let explicit_ffmpeg = args.iter().any(|arg| arg == "--ffmpeg");
    let local_ffmpeg = home().join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    if !explicit_ffmpeg && raw["ffmpeg"] == "ffmpeg" && local_ffmpeg.is_file() {
        raw["ffmpeg"] = json!(local_ffmpeg.to_string_lossy());
    }
    let known = ["download", "save", "sync", "url", "meta"].contains(&positional.first().map_or("", String::as_str));
    let op = if known { positional.remove(0) } else { "download".into() };
    let providers = [
        ("audio_providers", "audio", &["youtube-music", "youtube", "soundcloud"][..]),
        ("lyrics_providers", "lyrics", &["genius", "azlyrics", "musixmatch", "synced"][..]),
    ];
    for (key, label, allowed) in providers {
        if let Some(provider) = raw[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find(|provider| !allowed.contains(provider))
        {
            return Err(format!("unknown {label} provider: {provider}").into());
        }
    }
    Ok((op, positional, raw))
}

fn write_default_config() -> R<()> {
    let mut defaults: Value = serde_json::from_str(DEFAULTS)?;
    defaults["cache_path"] = json!(home().join(".spotipy").to_string_lossy());
    fs::create_dir_all(home())?;
    fs::write(home().join("config.json"), serde_json::to_vec_pretty(&defaults)?)?;
    println!("Config file generated at {}", home().join("config.json").display());
    Ok(())
}

fn install(tool: &str, cfg: &Value) -> R<()> {
    let table: &[(&str, &str, Option<&str>)] = &[
        ("windows-x86_64", "win32-x64", Some("x86_64-pc-windows-msvc")),
        ("windows-x86", "win32-ia32", None),
        ("windows-aarch64", "", Some("aarch64-pc-windows-msvc")),
        ("linux-x86_64", "linux-x64", Some("x86_64-unknown-linux-gnu")),
        ("linux-x86", "linux-ia32", None),
        ("linux-arm", "linux-arm", None),
        ("linux-aarch64", "linux-arm64", Some("aarch64-unknown-linux-gnu")),
        ("macos-x86_64", "darwin-x64", Some("x86_64-apple-darwin")),
        ("macos-aarch64", "darwin-arm64", Some("aarch64-apple-darwin")),
    ];
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let name = table.iter().find(|entry| entry.0 == target).and_then(|entry| {
        if tool == "ffmpeg" {
            (!entry.1.is_empty()).then_some(entry.1)
        } else {
            entry.2
        }
    });
    let name = name.ok_or(format!("{tool} binary is not available for your system"))?;
    let member = if cfg!(windows) { format!("{tool}.exe") } else { tool.to_string() };
    let file = home().join(&member);
    fs::create_dir_all(home())?;
    let binary = if tool == "ffmpeg" {
        let url = format!("https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/{name}");
        http("GET", &url, &[], None, cfg)?
    } else {
        let latest = String::from_utf8(http("GET", "https://dl.deno.land/release-latest.txt", &[], None, cfg)?)?;
        let url = format!("https://dl.deno.land/release/{}/deno-{name}.zip", latest.trim());
        let archive = home().join("deno.zip");
        fs::write(&archive, http("GET", &url, &[], None, cfg)?)?;
        let mut unpack = Command::new(if cfg!(windows) { "tar" } else { "unzip" });
        unpack.arg(if cfg!(windows) { "-xOf" } else { "-p" }).arg(&archive).arg(&member);
        let output = unpack.output()?;
        let _ = fs::remove_file(&archive);
        output.stdout
    };
    if binary.is_empty() {
        return Err(format!("Could not install {tool}").into());
    }
    fs::write(&file, binary)?;
    #[cfg(unix)]
    fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    println!("{tool} installed at {}", file.display());
    Ok(())
}

fn check_for_updates(cfg: &Value) -> R<()> {
    let url = "https://api.github.com/repos/pythoncrazy/spotdl-rs/releases/latest";
    let release = match json(url, &[("User-Agent", "spotdl-rs")], cfg) {
        Ok(release) => release,
        Err(error) if error.to_string().starts_with("HTTP 404") => {
            println!("No release found.");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let latest = release["tag_name"].as_str().ok_or("GitHub release has no tag_name")?;
    let current = env!("CARGO_PKG_VERSION");
    let parts = |version: &str| {
        let numbers = version.trim_start_matches('v').split('.').map(|part| part.parse().unwrap_or(0));
        numbers.collect::<Vec<u64>>()
    };
    if parts(latest) > parts(current) {
        println!("New version available: {latest}. Current version: {current}.");
    } else {
        println!("No updates available. Current version: {current}.");
    }
    Ok(())
}

pub fn log(cfg: &Value, level: &str, message: &str) {
    let severity = |name: &str| match name.to_ascii_uppercase().as_str() {
        "DEBUG" => 10,
        "WARNING" | "WARN" => 30,
        "ERROR" | "CRITICAL" => 40,
        _ => 20,
    };
    if severity(level) < severity(cfg["log_level"].as_str().unwrap_or("INFO")) {
        return;
    }
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_secs());
    let line = cfg["log_format"].as_str().map_or_else(
        || format!("{level}: {message}"),
        |format| {
            let format = format.replace("%(asctime)s", &time.to_string()).replace("%(name)s", "spotdl");
            format.replace("%(levelname)s", level).replace("%(message)s", message)
        },
    );
    eprintln!("{line}");
}

pub fn url_encode(value: &str) -> String {
    let encode = |byte: u8| match byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
        true => (byte as char).to_string(),
        false => format!("%{byte:02X}"),
    };
    value.bytes().map(encode).collect()
}

fn number(mut songs: Vec<Song>, cfg: &Value) -> Vec<Song> {
    let numbering = cfg["playlist_numbering"] == true;
    let retain = cfg["playlist_retain_track_cover"] == true;
    for song in songs.iter_mut().filter(|song| (numbering || retain) && song.list_name.is_some()) {
        song.album_name = song.list_name.clone().unwrap_or_default();
        song.track_number = song.list_position.unwrap_or(song.track_number as usize) as u32;
        song.tracks_count = song.list_length.unwrap_or(song.tracks_count as usize) as u32;
        song.disc_number = 1;
        song.disc_count = 1;
        if numbering && song.list_url.as_deref().is_some_and(|url| url.contains("/playlist/")) {
            song.album_artist = song.list_author.clone().unwrap_or_else(|| song.album_artist.clone());
            if !retain {
                song.cover_url.clone_from(&song.list_cover_url);
            }
        }
    }
    let ignored = cfg["ignore_albums"].as_array().into_iter().flatten().filter_map(Value::as_str);
    let ignored = ignored.map(str::to_lowercase).collect::<Vec<_>>();
    songs.retain(|song| {
        let album = song.album_name.to_lowercase();
        let explicit = cfg["skip_explicit"] == true && song.explicit;
        let kind = cfg["album_type"].as_str().is_some_and(|kind| kind != song.album_type);
        !explicit && !kind && !ignored.iter().any(|name| album.contains(name))
    });
    songs
}

fn resolve(queries: &[String], op: &str, cfg: &Value) -> (Vec<Song>, Vec<String>) {
    let enrich = op != "save" && op != "url";
    let fetch = |query: &String| match spotify::resolve(query, cfg, enrich) {
        Ok(songs) => (number(songs, cfg), Vec::new()),
        Err(error) => (Vec::new(), vec![format!("{query}: {error}")]),
    };
    let (mut songs, mut errors): (Vec<Song>, Vec<String>) =
        queries.par_iter().map(fetch).reduce(Default::default, |mut all, part| {
            all.0.extend(part.0);
            all.1.extend(part.1);
            all
        });
    if cfg["fetch_albums"] == true && matches!(op, "download" | "sync") {
        let ids = songs.iter().map(|song| format!("spotify:album:{}", song.album_id));
        let ids = ids.filter(|id| id.len() > 14).collect::<HashSet<_>>().into_iter().collect::<Vec<_>>();
        let (albums, failed) = resolve(&ids, "fetch", cfg);
        songs.extend(albums);
        errors.extend(failed);
    }
    let mut seen = HashSet::new();
    songs.retain(|song| match op {
        "download" | "sync" => seen.insert(download::path(song, cfg)),
        _ => seen.insert(PathBuf::from(&song.url)),
    });
    (songs, errors)
}

fn write_m3u(done: &[(Song, R<PathBuf>)], op: &str, cfg: &Value) -> R<()> {
    let Some(pattern) = cfg["m3u"].as_str().filter(|_| op != "url") else {
        return Ok(());
    };
    let pattern = if pattern.is_empty() || pattern == "true" { "{list[0]}.m3u8" } else { pattern };
    let suffix = match pattern {
        name if name.ends_with('/') => "{list[0]}.m3u8",
        name if name.ends_with(".m3u") || name.ends_with(".m3u8") => "",
        _ => ".m3u8",
    };
    let list = |song: &Song| download::clean(song.list_name.as_deref().unwrap_or(&song.album_name));
    let first = done.first().map_or("playlist".into(), |(song, _)| list(song));
    let mut files = std::collections::BTreeMap::<String, String>::new();
    let keep = |result: &R<PathBuf>| op != "download" || cfg["add_unavailable"] == true || result.is_ok();
    for (song, result) in done.iter().filter(|(_, result)| keep(result)) {
        let path = result.as_ref().map_or_else(|_| download::path(song, cfg), Clone::clone);
        let name = format!("{pattern}{suffix}").replace("{list}", &list(song)).replace("{list[0]}", &first);
        let line = download::fill("#EXTINF:{duration},{album-artist} - {title}", song, cfg);
        let file = files.entry(name).or_insert_with(|| "#EXTM3U\n".into());
        file.push_str(&format!("{line}\n{}\n", path.display()));
    }
    for (name, text) in files {
        let parent = Path::new(&name).parent().filter(|parent| !parent.as_os_str().is_empty());
        fs::create_dir_all(parent.unwrap_or(Path::new(".")))?;
        fs::write(name, text)?;
    }
    Ok(())
}

fn report(done: &[(Song, R<PathBuf>)], mut errors: Vec<String>, cfg: &Value) -> R<()> {
    let failed = done.iter().filter_map(|(song, result)| result.as_ref().err().map(|error| (song, error)));
    errors.extend(failed.map(|(song, error)| format!("{} - {}: {error}", song.artist, song.name)));
    if cfg["print_errors"] == true {
        errors.iter().for_each(|error| eprintln!("{error}"));
    }
    if let Some(path) = cfg["save_errors"].as_str().filter(|_| !errors.is_empty()) {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        std::io::Write::write_all(&mut file, format!("{time}\n{}\n", errors.join("\n")).as_bytes())?;
    }
    eprintln!(
        "{} succeeded, {} failed",
        done.len() - done.iter().filter(|(_, result)| result.is_err()).count(),
        errors.len()
    );
    match errors.len() {
        0 => Ok(()),
        count => Err(format!("{count} downloads failed").into()),
    }
}

fn sync_file(queries: &[String], songs: &[Song]) -> R<String> {
    Ok(serde_json::to_string_pretty(&json!({"type": "sync", "query": queries, "songs": songs}))?)
}

fn run() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|arg| arg == name);
    if flag("--version") {
        println!("spotdl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if flag("--generate-config") {
        return write_default_config();
    }
    let (op, original, mut cfg) = settings()?;
    if let Some(tool) = ["ffmpeg", "deno"].into_iter().find(|tool| flag(&format!("--download-{tool}"))) {
        return install(tool, &cfg);
    }
    if flag("--check-for-updates") {
        return check_for_updates(&cfg);
    }
    let save_file = cfg["save_file"].as_str().map(String::from);
    if save_file.as_deref().is_some_and(|path| path != "-" && !path.ends_with(".spotdl")) {
        return Err("save file must end with .spotdl or be '-'".into());
    }
    if original.is_empty() || flag("--help") || flag("-h") {
        println!("Usage: spotdl [download|save|sync|url|meta] <query>... [options]");
        return Ok(());
    }
    if cfg["user_auth"] == true && cfg["auth_token"].is_null() {
        cfg["auth_token"] = json!(spotify::token(&cfg, true)?);
    }
    if !cfg["auth_token"].is_null() {
        cfg["use_official_api"] = json!(true);
    }
    let meta_threads = cfg["meta_threads"].as_u64().unwrap_or(32).max(1) as usize;
    rayon::ThreadPoolBuilder::new().num_threads(meta_threads).build_global()?;
    if op == "meta" {
        return download::meta(&original, &cfg);
    }
    let sync = (op == "sync" && original.len() == 1 && original[0].ends_with(".spotdl"))
        .then(|| -> R<_> { Ok(serde_json::from_slice::<Value>(&fs::read(&original[0])?)?) })
        .transpose()?;
    if sync.as_ref().is_some_and(|file| file["type"] != "sync" || !file["songs"].is_array()) {
        return Err("sync file is not valid".into());
    }
    let queries: Vec<String> = match &sync {
        Some(file) => serde_json::from_value(file["query"].clone())?,
        None => original.clone(),
    };
    let (songs, errors) = resolve(&queries, &op, &cfg);
    if let Some(file) = sync.as_ref().filter(|_| cfg["sync_without_deleting"] != true) {
        download::prune(&serde_json::from_value::<Vec<Song>>(file["songs"].clone())?, &songs, &cfg);
    }
    let sync_path = sync.as_ref().map(|_| original[0].clone()).or(save_file.clone()).filter(|_| op == "sync");
    if let Some(path) = sync_path.as_ref().filter(|_| sync.is_none()) {
        fs::write(path, sync_file(&queries, &songs)?)?;
    }
    let threads = if op == "save" { meta_threads } else { cfg["threads"].as_u64().unwrap_or(16).max(1) as usize };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
    let process = |song: Song| download::process(song, &op, &cfg);
    let done: Vec<(Song, R<PathBuf>)> = pool.install(|| songs.into_par_iter().with_max_len(1).map(process).collect());
    let saved = done.iter().map(|(song, _)| song.clone()).collect::<Vec<_>>();
    match (op.as_str(), save_file.as_deref(), sync_path) {
        ("sync", _, Some(path)) => fs::write(path, sync_file(&queries, &saved)?)?,
        ("save", Some("-"), _) => println!("{}", serde_json::to_string_pretty(&saved)?),
        ("save" | "download", Some(path), _) => fs::write(path, serde_json::to_string_pretty(&saved)?)?,
        ("save", None, _) => return Err("save file is not specified".into()),
        _ => {}
    }
    write_m3u(&done, &op, &cfg)?;
    report(&done, errors, &cfg)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
