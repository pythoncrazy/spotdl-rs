mod download;
mod search;
mod spotify;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, fs, path::PathBuf, sync::Mutex};

pub type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DEFAULTS: &str = r#"{"output":"{artists} - {title}.{output-ext}","format":"mp3","bitrate":"128k","overwrite":"skip","threads":16,"meta_threads":32,"ffmpeg":"ffmpeg","yt_dlp":"yt-dlp","audio_providers":["youtube-music"],"lyrics_providers":["genius","azlyrics","musixmatch"],"search_query":null,"cookie_file":null,"proxy":null,"yt_dlp_args":null,"ffmpeg_args":null,"restrict":null,"playlist_numbering":false,"playlist_retain_track_cover":false,"m3u":null,"archive":null,"save_file":null,"filter_results":true,"album_type":null,"print_errors":false,"save_errors":null,"sponsor_block":false,"preload":false,"id3_separator":"/","generate_lrc":false,"force_update_metadata":false,"only_verified_results":false,"sync_without_deleting":false,"sync_remove_lrc":false,"max_filename_length":0,"detect_formats":[],"ignore_albums":[],"skip_explicit":false,"redownload":false,"skip_album_art":false,"create_skip_file":false,"respect_skip_file":false,"auth_token":null,"client_id":"","client_secret":"","no_cache":false}"#;

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
    pub no_cache: bool,
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
    pub artist_id: String,
    pub album_type: String,
}

pub fn home() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default());
    [home.join(".config/spotdl"), home.join(".spotdl")].into_iter().find(|p| p.exists()).unwrap_or(home.join(".spotdl"))
}

fn settings() -> R<(String, Vec<String>, Settings)> {
    let mut raw: Value = serde_json::from_str(DEFAULTS)?;
    let file = fs::read(home().join("config.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    file.iter().flat_map(|v| v.as_object()).flatten().for_each(|(k, v)| raw[k] = v.clone());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let key = args[i].strip_prefix("--").unwrap_or_default().replace('-', "_");
        let next = args.get(i + 1).filter(|n| !n.starts_with("--")).cloned();
        let take = next.is_some() && !matches!(raw.get(&key), Some(Value::Bool(_)) | None);
        let value = match (raw.get(&key), next) {
            (None, _) if key.is_empty() => {
                positional.push(args[i].clone());
                None
            }
            (Some(Value::Bool(_)), _) | (None, _) => Some(json!(true)),
            (Some(Value::Number(_)), Some(n)) => Some(json!(n.parse::<usize>()?)),
            (Some(Value::Array(_)), Some(n)) => Some(json!(n.split(',').map(str::trim).collect::<Vec<_>>())),
            (_, Some(n)) => Some(json!(n)),
            (_, None) => Some(json!("")),
        };
        i += 1 + usize::from(take);
        value.into_iter().for_each(|v| raw[key.as_str()] = v);
    }
    let op = match positional.first().map(String::as_str) {
        Some("download" | "save" | "sync" | "url" | "meta") => positional.remove(0),
        _ => "download".into(),
    };
    Ok((op, positional, serde_json::from_value(raw)?))
}

fn number(mut songs: Vec<Song>, cfg: &Settings) -> Vec<Song> {
    let len = songs.len();
    songs.iter_mut().enumerate().filter(|_| cfg.playlist_numbering).for_each(|(i, s)| {
        s.album_name = s.list_name.clone().unwrap_or_default();
        s.album_artist = s.list_name.clone().unwrap_or_default();
        s.track_number = i as u32 + 1;
        s.tracks_count = len as u32;
        s.disc_number = 1;
        s.disc_count = 1;
        s.cover_url = s.cover_url.clone().filter(|_| cfg.playlist_retain_track_cover);
    });
    songs
        .into_iter()
        .filter(|s| !cfg.skip_explicit || !s.explicit)
        .filter(|s| cfg.album_type.as_ref().is_none_or(|t| &s.album_type == t))
        .filter(|s| !cfg.ignore_albums.iter().any(|a| s.album_name.to_lowercase().contains(&a.to_lowercase())))
        .collect()
}

fn write_json(path: &str, value: &impl Serialize) -> R<()> {
    fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

fn run() -> R<()> {
    let (op, original, cfg) = settings()?;
    if std::env::args().any(|a| a == "--version") {
        println!("spotdl {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--generate-config") {
        fs::create_dir_all(home())?;
        return fs::write(
            home().join("config.json"),
            serde_json::to_string_pretty(&serde_json::from_str::<Value>(DEFAULTS)?)?,
        )
        .map_err(Into::into);
    }
    if original.is_empty() {
        return Err("usage: spotdl [download|save|sync|url|meta] <query>... [--option value]".into());
    }
    rayon::ThreadPoolBuilder::new().num_threads(cfg.meta_threads.max(1)).build_global()?;
    if op == "meta" {
        return download::meta(&original, &cfg);
    }
    let sync = (op == "sync" && original.len() == 1 && original[0].ends_with(".spotdl"))
        .then(|| serde_json::from_slice::<Value>(&fs::read(&original[0]).ok()?).ok())
        .flatten();
    let queries: Vec<String> =
        sync.as_ref().map_or(original.clone(), |s| serde_json::from_value(s["query"].clone()).unwrap_or_default());
    let seen = Mutex::new(HashSet::new());
    let errors = Mutex::new(Vec::new());
    let lists: Vec<Vec<Song>> = queries
        .par_iter()
        .map(|q| {
            spotify::resolve(q, &cfg).map_err(|e| errors.lock().unwrap().push(format!("{q}: {e}"))).unwrap_or_default()
        })
        .map(|songs| number(songs, &cfg).into_iter().filter(|s| seen.lock().unwrap().insert(s.url.clone())).collect())
        .collect();
    let songs: Vec<Song> = lists.into_iter().flatten().collect();
    if let Some(old) = sync.as_ref().filter(|_| !cfg.sync_without_deleting) {
        download::prune(&serde_json::from_value::<Vec<Song>>(old["songs"].clone())?, &songs, &cfg);
    }
    if op == "sync" {
        let path = sync.as_ref().map(|_| original[0].clone()).or(cfg.save_file.clone());
        let path = path.unwrap_or_else(|| format!("{}.spotdl", download::clean(&queries[0])));
        write_json(&path, &json!({"type": "sync", "query": queries, "songs": songs}))?;
    }
    let done: Vec<(Song, R<PathBuf>)> = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads.max(1))
        .build()?
        .install(|| songs.into_par_iter().with_max_len(1).map(|s| download::process(s, &op, &cfg)).collect());
    let paths: Vec<String> =
        done.iter().filter_map(|(_, r)| r.as_ref().ok()).map(|p| p.display().to_string()).collect();
    errors
        .lock()
        .unwrap()
        .extend(done.iter().filter_map(|(s, r)| r.as_ref().err().map(|e| format!("{} - {}: {e}", s.artist, s.name))));
    if op == "save" {
        let saved: Vec<&Song> = done.iter().map(|(s, _)| s).collect();
        write_json(cfg.save_file.as_deref().unwrap_or("saved.spotdl"), &saved)?;
    }
    if let Some(m3u) = &cfg.m3u {
        let name = done.first().and_then(|(s, _)| s.list_name.clone()).unwrap_or_else(|| "playlist".into());
        let file = if m3u.is_empty() || m3u == "true" { "{list[0]}.m3u8".into() } else { m3u.clone() };
        fs::write(file.replace("{list[0]}", &download::clean(&name)), format!("#EXTM3U\n{}\n", paths.join("\n")))?;
    }
    let errors = errors.into_inner().unwrap();
    errors.iter().filter(|_| cfg.print_errors).for_each(|e| eprintln!("{e}"));
    cfg.save_errors.iter().try_for_each(|p| fs::write(p, errors.join("\n")))?;
    eprintln!("{} succeeded, {} failed", paths.len(), errors.len());
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
