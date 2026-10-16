mod download;
mod search;
mod spotify;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub(crate) fn retry<T>(
    attempts: usize,
    mut run: impl FnMut(usize) -> R<T>,
    wait: impl Fn(&(dyn std::error::Error + Send + Sync)) -> Option<Duration>,
) -> R<T> {
    for attempt in 0..attempts.max(1) {
        match run(attempt) {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt + 1 == attempts.max(1) {
                    return Err(error);
                }
                let Some(delay) = wait(error.as_ref()) else {
                    return Err(error);
                };
                std::thread::sleep(delay);
            }
        }
    }
    Err("retry limit reached".into())
}

const DEFAULTS: &str = r#"{"output":"{artists} - {title}.{output-ext}","format":"mp3","bitrate":"128k","overwrite":"skip","threads":16,"meta_threads":32,"ffmpeg":"ffmpeg","yt_dlp":"yt-dlp","audio_providers":["youtube-music"],"lyrics_providers":["genius","azlyrics","musixmatch"],"search_query":null,"cookie_file":null,"proxy":null,"yt_dlp_args":null,"ffmpeg_args":null,"restrict":null,"playlist_numbering":false,"playlist_retain_track_cover":false,"m3u":null,"archive":null,"save_file":null,"filter_results":true,"album_type":null,"print_errors":false,"save_errors":null,"sponsor_block":false,"preload":false,"id3_separator":"/","generate_lrc":false,"force_update_metadata":false,"only_verified_results":false,"sync_without_deleting":false,"sync_remove_lrc":false,"max_filename_length":0,"detect_formats":[],"ignore_albums":[],"skip_explicit":false,"redownload":false,"skip_album_art":false,"create_skip_file":false,"respect_skip_file":false,"auth_token":null,"client_id":"","client_secret":"","no_cache":false,"genius_token":"","log_level":"INFO","log_format":null,"cache_path":null,"use_cache_file":false,"max_retries":3,"headless":false,"simple_tui":false,"ytm_data":false,"add_unavailable":false,"fetch_albums":false,"scan_for_songs":false,"user_auth":false,"use_official_api":false,"load_config":true}"#;

const ALIASES: [(&str, &str); 4] = [
    ("audio", "audio_providers"),
    ("lyrics", "lyrics_providers"),
    ("genius_access_token", "genius_token"),
    ("dont_filter_results", "filter_results"),
];

#[derive(Clone, Deserialize)]
pub struct Settings {
    pub output: String,
    pub format: String,
    pub bitrate: String,
    pub overwrite: String,
    pub threads: usize,
    pub meta_threads: usize,
    pub ffmpeg: String,
    pub yt_dlp: String,
    pub audio_providers: Vec<String>,
    pub lyrics_providers: Vec<String>,
    pub search_query: Option<String>,
    pub cookie_file: Option<String>,
    pub proxy: Option<String>,
    pub yt_dlp_args: Option<String>,
    pub ffmpeg_args: Option<String>,
    pub restrict: Option<String>,
    pub playlist_numbering: bool,
    pub playlist_retain_track_cover: bool,
    pub m3u: Option<String>,
    pub archive: Option<String>,
    pub save_file: Option<String>,
    pub filter_results: bool,
    pub album_type: Option<String>,
    pub print_errors: bool,
    pub save_errors: Option<String>,
    pub sponsor_block: bool,
    pub preload: bool,
    pub id3_separator: String,
    pub generate_lrc: bool,
    pub force_update_metadata: bool,
    pub only_verified_results: bool,
    pub sync_without_deleting: bool,
    pub sync_remove_lrc: bool,
    pub max_filename_length: usize,
    pub detect_formats: Vec<String>,
    pub ignore_albums: Vec<String>,
    pub skip_explicit: bool,
    pub redownload: bool,
    pub skip_album_art: bool,
    pub create_skip_file: bool,
    pub respect_skip_file: bool,
    pub auth_token: Option<String>,
    pub client_id: String,
    pub client_secret: String,
    pub genius_token: String,
    pub no_cache: bool,
    pub max_retries: usize,
    pub fetch_albums: bool,
    pub add_unavailable: bool,
    pub scan_for_songs: bool,
    pub user_auth: bool,
    pub use_official_api: bool,
    pub cache_path: Option<String>,
    pub use_cache_file: bool,
    pub log_level: String,
    pub log_format: Option<String>,
    pub ytm_data: bool,
    pub headless: bool,
    pub simple_tui: bool,
    pub load_config: bool,
}

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
    let home = PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default());
    home.join(".spotdl")
}

fn settings() -> R<(String, Vec<String>, Settings)> {
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
            (None, _) => return Err(format!("unknown option --{raw_key}").into()),
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
    let cfg: Settings = serde_json::from_value(raw)?;
    let audio = ["youtube-music", "youtube", "soundcloud", "bandcamp", "piped"];
    if let Some(provider) = cfg.audio_providers.iter().find(|provider| !audio.contains(&provider.as_str())) {
        return Err(format!("unknown audio provider: {provider}").into());
    }
    let lyrics = ["genius", "azlyrics", "musixmatch", "synced"];
    if let Some(provider) = cfg.lyrics_providers.iter().find(|provider| !lyrics.contains(&provider.as_str())) {
        return Err(format!("unknown lyrics provider: {provider}").into());
    }
    Ok((op, positional, cfg))
}

fn write_default_config() -> R<()> {
    fs::create_dir_all(home())?;
    let mut defaults: Value = serde_json::from_str(DEFAULTS)?;
    defaults["cache_path"] = json!(home().join(".spotipy").to_string_lossy());
    fs::write(home().join("config.json"), serde_json::to_vec_pretty(&defaults)?)?;
    println!("Config file generated at {}", home().join("config.json").display());
    Ok(())
}

fn set_executable(path: &Path) -> R<()> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn download_ffmpeg() -> R<PathBuf> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let url = match (os, arch) {
        ("windows", "x86_64") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/win32-x64",
        ("windows", "x86") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/win32-ia32",
        ("linux", "x86_64") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/linux-x64",
        ("linux", "x86") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/linux-ia32",
        ("linux", "arm") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/linux-arm",
        ("linux", "aarch64") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/linux-arm64",
        ("macos", "x86_64") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/darwin-x64",
        ("macos", "aarch64") => "https://github.com/eugeneware/ffmpeg-static/releases/download/b4.4/darwin-arm64",
        _ => return Err("FFmpeg binary is not available for your system".into()),
    };
    fs::create_dir_all(home())?;
    let path = home().join(if os == "windows" { "ffmpeg.exe" } else { "ffmpeg" });
    let bytes = spotify::HTTP.get(url).call()?.body_mut().with_config().limit(1 << 30).read_to_vec()?;
    fs::write(&path, bytes)?;
    set_executable(&path)?;
    Ok(path)
}

fn download_deno() -> R<PathBuf> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let target = match (os, arch) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => return Err("Deno binary is not available for your system".into()),
    };
    let latest = spotify::HTTP.get("https://dl.deno.land/release-latest.txt").call()?.body_mut().read_to_string()?;
    let url = format!("https://dl.deno.land/release/{}/deno-{target}.zip", latest.trim());
    fs::create_dir_all(home())?;
    let archive = home().join(format!("deno-{}.zip", std::process::id()));
    let bytes = spotify::HTTP.get(&url).call()?.body_mut().with_config().limit(1 << 30).read_to_vec()?;
    fs::write(&archive, bytes)?;
    let filename = if os == "windows" { "deno.exe" } else { "deno" };
    let binary = if os == "windows" {
        let result = Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(format!("Expand-Archive -Path '{}' -DestinationPath '{}' -Force", archive.display(), home().display()))
            .output()?;
        if !result.status.success() {
            return Err("Could not extract Deno archive".into());
        }
        fs::read(home().join(filename))?
    } else {
        let result = Command::new("unzip").args(["-p"]).arg(&archive).arg(filename).output()?;
        if !result.status.success() {
            return Err("Could not extract Deno archive".into());
        }
        result.stdout
    };
    let _ = fs::remove_file(&archive);
    if binary.is_empty() {
        return Err(format!("Could not extract Deno from {}", url).into());
    }
    let path = home().join(filename);
    fs::write(&path, binary)?;
    set_executable(&path)?;
    Ok(path)
}

fn version_parts(version: &str) -> Vec<u64> {
    version.trim_start_matches('v').split('.').filter_map(|part| part.parse().ok()).collect()
}

fn check_for_updates() -> R<()> {
    let release: Value = spotify::HTTP
        .get("https://api.github.com/repos/pythoncrazy/spotdl-rs/releases/latest")
        .header("User-Agent", "spotdl-rs")
        .call()?
        .body_mut()
        .read_json()?;
    let latest = release["tag_name"].as_str().ok_or("GitHub release has no tag_name")?;
    let current = env!("CARGO_PKG_VERSION");
    if version_parts(latest) > version_parts(current) {
        println!("New version available: {latest}. Current version: {current}.");
    } else {
        println!("No updates available. Current version: {current}.");
    }
    Ok(())
}

pub(crate) fn log(cfg: &Settings, level: &str, message: &str) {
    let severity = |name: &str| match name.to_ascii_uppercase().as_str() {
        "DEBUG" => 10,
        "INFO" => 20,
        "WARNING" | "WARN" => 30,
        "ERROR" | "CRITICAL" => 40,
        _ => 20,
    };
    if severity(level) < severity(&cfg.log_level) {
        return;
    }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |value| value.as_secs());
    let formatted = cfg.log_format.as_deref().map_or_else(
        || format!("{level}: {message}"),
        |format| {
            format
                .replace("%(asctime)s", &timestamp.to_string())
                .replace("%(name)s", "spotdl")
                .replace("%(levelname)s", level)
                .replace("%(message)s", message)
        },
    );
    eprintln!("{formatted}");
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn url_decode(value: &str) -> R<String> {
    let mut bytes = Vec::new();
    let mut chars = value.bytes();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hex = [chars.next().ok_or("invalid URL escape")?, chars.next().ok_or("invalid URL escape")?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex)?, 16)?);
        } else {
            bytes.push(if byte == b'+' { b' ' } else { byte });
        }
    }
    Ok(String::from_utf8(bytes)?)
}

fn base64(value: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in value.chunks(3) {
        let a = chunk[0] as usize;
        let b = chunk.get(1).copied().unwrap_or_default() as usize;
        let c = chunk.get(2).copied().unwrap_or_default() as usize;
        encoded.push(TABLE[a >> 2] as char);
        encoded.push(TABLE[((a & 3) << 4) | (b >> 4)] as char);
        encoded.push(if chunk.len() > 1 { TABLE[((b & 15) << 2) | (c >> 6)] as char } else { '=' });
        encoded.push(if chunk.len() > 2 { TABLE[c & 63] as char } else { '=' });
    }
    encoded
}

fn user_token(cfg: &Settings) -> R<String> {
    let path = cfg.cache_path.as_ref().map(PathBuf::from).unwrap_or_else(|| home().join(".spotipy"));
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let cache = (!cfg.no_cache)
        .then(|| fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()))
        .flatten();
    if let Some(token) = cache
        .as_ref()
        .filter(|cache| cache["expires_at"].as_u64().is_some_and(|expiry| expiry > now + 30))
        .and_then(|cache| cache["access_token"].as_str())
    {
        return Ok(token.into());
    }
    if let Some(refresh) = cache.as_ref().and_then(|cache| cache["refresh_token"].as_str()) {
        let credentials = base64(format!("{}:{}", cfg.client_id, cfg.client_secret).as_bytes());
        let fields = [("grant_type", "refresh_token"), ("refresh_token", refresh)];
        let token: Value = spotify::HTTP
            .post("https://accounts.spotify.com/api/token")
            .header("Authorization", format!("Basic {credentials}"))
            .send_form(fields)?
            .body_mut()
            .read_json()?;
        let access = token["access_token"].as_str().ok_or("Spotify token refresh failed")?;
        if !cfg.no_cache {
            fs::write(
                &path,
                serde_json::to_vec(&json!({
                    "access_token": access,
                    "refresh_token": token["refresh_token"].as_str().unwrap_or(refresh),
                    "expires_at": now + token["expires_in"].as_u64().unwrap_or(3600)
                }))?,
            )?;
        }
        return Ok(access.into());
    }
    let listener = TcpListener::bind("127.0.0.1:9900")?;
    let callback = "http://127.0.0.1:9900/";
    let state = format!("{now:x}");
    let scope = "user-library-read user-follow-read playlist-read-private";
    let url = format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&state={}",
        url_encode(&cfg.client_id),
        url_encode(callback),
        url_encode(scope),
        state
    );
    if cfg.headless {
        println!("{url}");
    } else {
        let mut browser = if cfg!(target_os = "macos") {
            Command::new("open").arg(&url).spawn()
        } else if cfg!(windows) {
            Command::new("cmd").args(["/C", "start", ""]).arg(&url).spawn()
        } else {
            Command::new("xdg-open").arg(&url).spawn()
        };
        if browser.as_mut().is_err() {
            println!("{url}");
        }
    }
    log(cfg, "INFO", "Waiting for Spotify authorization at 127.0.0.1:9900");
    let (mut stream, _) = listener.accept()?;
    let mut request = [0; 8192];
    let length = stream.read(&mut request)?;
    let request = String::from_utf8_lossy(&request[..length]);
    let route =
        request.lines().next().and_then(|line| line.split_whitespace().nth(1)).ok_or("invalid OAuth callback")?;
    let values = route
        .split_once('?')
        .map(|(_, query)| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, value)| Ok((url_decode(key)?, url_decode(value)?)))
                .collect::<R<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let value = |key: &str| values.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
    if value("state") != Some(state.as_str()) {
        let body = b"Invalid Spotify OAuth state";
        write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n", body.len())?;
        stream.write_all(body)?;
        return Err("Spotify OAuth state did not match".into());
    }
    let code = value("code").ok_or("Spotify did not return an authorization code")?;
    let body = b"Spotify login complete. You can close this tab.";
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())?;
    stream.write_all(body)?;
    let credentials = base64(format!("{}:{}", cfg.client_id, cfg.client_secret).as_bytes());
    let fields = [("grant_type", "authorization_code"), ("code", code), ("redirect_uri", callback)];
    let token: Value = spotify::HTTP
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", format!("Basic {credentials}"))
        .send_form(fields)?
        .body_mut()
        .read_json()?;
    let access_token = token["access_token"].as_str().ok_or("Spotify did not return an access token")?;
    if !cfg.no_cache {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            path,
            serde_json::to_vec(&json!({
                "access_token": access_token,
                "refresh_token": token["refresh_token"],
                "expires_at": now + token["expires_in"].as_u64().unwrap_or(3600)
            }))?,
        )?;
    }
    Ok(access_token.into())
}

fn number(mut songs: Vec<Song>, cfg: &Settings) -> Vec<Song> {
    songs
        .iter_mut()
        .filter(|song| (cfg.playlist_numbering || cfg.playlist_retain_track_cover) && song.list_name.is_some())
        .for_each(|song| {
            song.album_name = song.list_name.clone().unwrap_or_default();
            song.track_number = song.list_position.unwrap_or(song.track_number as usize) as u32;
            song.tracks_count = song.list_length.unwrap_or(song.tracks_count as usize) as u32;
            song.disc_number = 1;
            song.disc_count = 1;
            if cfg.playlist_numbering && song.list_url.as_deref().is_some_and(|url| url.contains("/playlist/")) {
                if let Some(author) = &song.list_author {
                    song.album_artist.clone_from(author);
                }
                if !cfg.playlist_retain_track_cover {
                    song.cover_url.clone_from(&song.list_cover_url);
                }
            }
        });
    let keep =
        |s: &Song| (!cfg.skip_explicit || !s.explicit) && cfg.album_type.as_ref().is_none_or(|t| &s.album_type == t);
    songs
        .into_iter()
        .filter(keep)
        .filter(|s| !cfg.ignore_albums.iter().any(|a| s.album_name.to_lowercase().contains(&a.to_lowercase())))
        .collect()
}

fn resolve(queries: &[String], op: &str, cfg: &Settings, sync: Option<&Value>) -> R<(Vec<Song>, Vec<String>)> {
    let errors = Mutex::new(Vec::new());
    let mut songs: Vec<Song> = queries
        .par_iter()
        .flat_map_iter(|q| {
            number(
                spotify::resolve(q, cfg, op != "save" && op != "url")
                    .inspect_err(|e| errors.lock().unwrap().push(format!("{q}: {e}")))
                    .unwrap_or_default(),
                cfg,
            )
        })
        .collect();
    if cfg.fetch_albums && matches!(op, "download" | "sync") {
        let ids: HashSet<_> = songs.iter().map(|s| s.album_id.as_str()).filter(|id| !id.is_empty()).collect();
        let albums = ids
            .par_iter()
            .flat_map_iter(|id| {
                let query = format!("spotify:album:{id}");
                match spotify::resolve(&query, cfg, true) {
                    Ok(albums) => number(albums, cfg).into_iter(),
                    Err(error) => {
                        errors.lock().unwrap().push(format!("{query}: {error}"));
                        Vec::new().into_iter()
                    }
                }
            })
            .collect::<Vec<_>>();
        songs.extend(albums);
    }
    let mut seen = HashSet::new();
    songs.retain(|s| {
        seen.insert(if op == "download" || op == "sync" { download::path(s, cfg) } else { PathBuf::from(&s.url) })
    });
    if let Some(old) = sync.filter(|_| !cfg.sync_without_deleting) {
        let previous = serde_json::from_value::<Vec<Song>>(old["songs"].clone())?;
        download::prune(&previous, &songs, cfg);
    }
    Ok((songs, errors.into_inner().unwrap()))
}

fn download_songs(songs: Vec<Song>, op: &str, cfg: &Settings) -> R<Vec<(Song, R<PathBuf>)>> {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cfg.threads.max(1)).build()?;
    Ok(pool.install(|| songs.into_par_iter().with_max_len(1).map(|s| download::process(s, op, cfg)).collect()))
}

fn write_outputs(
    done: &[(Song, R<PathBuf>)],
    op: &str,
    queries: Vec<String>,
    sync_path: Option<String>,
    cfg: &Settings,
    mut errors: Vec<String>,
) -> R<()> {
    errors.extend(done.iter().filter_map(|(s, r)| r.as_ref().err().map(|e| format!("{} - {}: {e}", s.artist, s.name))));
    let saved: Vec<&Song> = done.iter().map(|(s, _)| s).collect();
    if op == "sync" {
        if let Some(path) = sync_path.or(cfg.save_file.clone()) {
            fs::write(
                &path,
                serde_json::to_string_pretty(&json!({"type": "sync", "query": queries, "songs": &saved}))?,
            )?;
        }
    }
    if op == "save" {
        let output = serde_json::to_string_pretty(&saved)?;
        match cfg.save_file.as_deref() {
            Some("-") => println!("{output}"),
            Some(path) if path.ends_with(".spotdl") => fs::write(path, output)?,
            Some(_) => return Err("save file must end with .spotdl or be '-'".into()),
            None => return Err("save file is not specified".into()),
        }
    }
    if op == "download" {
        if let Some(path) = cfg.save_file.as_deref() {
            fs::write(path, serde_json::to_string_pretty(&saved)?)?;
        }
    }
    if let Some(m3u) = cfg.m3u.as_deref().filter(|_| op != "url") {
        let n = if m3u.is_empty() || m3u == "true" { "{list[0]}.m3u8" } else { m3u };
        let suffix = if n.ends_with('/') {
            "{list[0]}.m3u8"
        } else if n.ends_with(".m3u") || n.ends_with(".m3u8") {
            ""
        } else {
            ".m3u8"
        };
        let name = format!("{n}{suffix}");
        let first = done.first().map_or("playlist", |(s, _)| s.list_name.as_deref().unwrap_or(s.album_name.as_str()));
        let mut files = BTreeMap::<String, String>::new();
        done.iter()
            .filter(|(_, result)| {
                op != "download" || cfg.add_unavailable || result.as_ref().is_ok_and(|path| path.is_file())
            })
            .for_each(|(s, r)| {
                let path = r.as_ref().ok().cloned().unwrap_or_else(|| download::path(s, cfg));
                let list = download::clean(s.list_name.as_deref().unwrap_or(s.album_name.as_str()));
                let file = name.replace("{list}", &list).replace("{list[0]}", &download::clean(first));
                let line = download::fill("#EXTINF:{duration},{album-artist} - {title}", s, cfg);
                files.entry(file).or_insert("#EXTM3U\n".into()).push_str(&format!("{line}\n{}\n", path.display()));
            });
        files.into_iter().try_for_each(|(f, v)| {
            fs::create_dir_all(Path::new(&f).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))
                .and_then(|_| fs::write(f, v))
        })?;
    }
    errors.iter().filter(|_| cfg.print_errors).for_each(|e| eprintln!("{e}"));
    if !errors.is_empty() {
        cfg.save_errors.iter().try_for_each(|path| {
            let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(std::io::Error::other)?;
            writeln!(file, "{}", now.as_secs())?;
            errors.iter().try_for_each(|error| writeln!(file, "{error}"))
        })?;
    }
    eprintln!("{} succeeded, {} failed", done.iter().filter(|(_, r)| r.is_ok()).count(), errors.len());
    errors.is_empty().then_some(()).ok_or_else(|| format!("{} downloads failed", errors.len()).into())
}

fn run() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().any(|arg| arg == name);
    if flag("--version") {
        println!("spotdl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if flag("--help") || flag("-h") {
        println!("Usage: spotdl [download|save|sync|url|meta] <query>... [options]");
        println!("Options: --config, --generate-config, --download-ffmpeg, --download-deno, --check-for-updates");
        return Ok(());
    }
    if flag("--generate-config") {
        return write_default_config();
    }
    if flag("--download-ffmpeg") {
        println!("FFmpeg successfully downloaded to {}", download_ffmpeg()?.display());
        return Ok(());
    }
    if flag("--download-deno") {
        println!("Deno successfully downloaded to {}", download_deno()?.display());
        return Ok(());
    }
    if flag("--check-for-updates") {
        return check_for_updates();
    }
    let (op, original, mut cfg) = settings()?;
    if cfg.save_file.as_deref().is_some_and(|path| path != "-" && !path.ends_with(".spotdl")) {
        return Err("save file must end with .spotdl or be '-'".into());
    }
    if cfg.user_auth && cfg.auth_token.is_none() {
        cfg.auth_token = Some(user_token(&cfg)?);
    }
    if cfg.user_auth || cfg.auth_token.is_some() {
        cfg.use_official_api = true;
    }
    if original.is_empty() {
        println!("Usage: spotdl [download|save|sync|url|meta] <query>...");
        return Ok(());
    }
    rayon::ThreadPoolBuilder::new().num_threads(cfg.meta_threads.max(1)).build_global()?;
    if op == "meta" {
        return download::meta(&original, &cfg);
    }
    if op == "sync" && cfg.save_file.is_some() && original.iter().any(|query| query.ends_with(".spotdl")) {
        return Err("cannot create a sync file from a .spotdl file".into());
    }
    let sync = if op == "sync" && original.len() == 1 && original[0].ends_with(".spotdl") {
        let value: Value = serde_json::from_slice(&fs::read(&original[0])?)?;
        if value["type"] != "sync" || !value["songs"].is_array() || !value["query"].is_array() {
            return Err("sync file is not valid".into());
        }
        Some(value)
    } else {
        None
    };
    let sync_path = sync.as_ref().map(|_| original[0].clone());
    let queries: Vec<String> = match sync.as_ref() {
        Some(value) => serde_json::from_value(value["query"].clone())?,
        None => original,
    };
    let (songs, errors) = resolve(&queries, &op, &cfg, sync.as_ref())?;
    if op == "sync" && sync.is_none() {
        if let Some(path) = &cfg.save_file {
            fs::write(
                path,
                serde_json::to_string_pretty(&json!({"type": "sync", "query": &queries, "songs": &songs}))?,
            )?;
        }
    }
    let done = download_songs(songs, &op, &cfg)?;
    write_outputs(&done, &op, queries, sync_path, &cfg, errors)
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
