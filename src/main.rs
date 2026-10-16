mod download;

mod search;

mod spotify;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DEFAULTS: &str = r#"{"output":"{artists} - {title}.{output-ext}","format":"mp3","bitrate":"128k","overwrite":"skip","threads":16,"meta_threads":32,"ffmpeg":"ffmpeg","yt_dlp":"yt-dlp","audio_providers":["youtube-music"],"lyrics_providers":["genius","azlyrics","musixmatch"],"search_query":null,"cookie_file":null,"proxy":null,"yt_dlp_args":null,"ffmpeg_args":null,"restrict":null,"playlist_numbering":false,"playlist_retain_track_cover":false,"m3u":null,"archive":null,"save_file":null,"filter_results":true,"album_type":null,"print_errors":false,"save_errors":null,"sponsor_block":false,"preload":false,"id3_separator":"/","generate_lrc":false,"force_update_metadata":false,"only_verified_results":false,"sync_without_deleting":false,"sync_remove_lrc":false,"max_filename_length":0,"detect_formats":[],"ignore_albums":[],"skip_explicit":false,"redownload":false,"skip_album_art":false,"create_skip_file":false,"respect_skip_file":false,"auth_token":null,"client_id":"","client_secret":"","no_cache":false,"genius_token":"","log_level":"INFO","log_format":null,"cache_path":null,"use_cache_file":false,"max_retries":3,"headless":false,"keep_sessions":false,"simple_tui":false,"ytm_data":false,"add_unavailable":false,"fetch_albums":false,"scan_for_songs":false,"user_auth":false,"use_official_api":false,"load_config":true}"#;

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
    pub no_cache: bool,
    pub max_retries: usize,
    pub fetch_albums: bool,
    pub add_unavailable: bool,
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
    #[serde(skip)]
    pub original_date: String,
    #[serde(skip)]
    pub upc: String,
    #[serde(skip)]
    pub bpm: f64,
    #[serde(skip)]
    pub gain: f64,
    #[serde(skip)]
    pub deezer_track_id: String,
}

pub fn home() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default());
    [home.join(".config/spotdl"), home.join(".spotdl")].into_iter().find(|p| p.exists()).unwrap_or(home.join(".spotdl"))
}

fn settings() -> R<(String, Vec<String>, Settings)> {
    let mut raw: Value = serde_json::from_str(DEFAULTS)?;
    let file = fs::read(home().join("config.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    file.iter()
        .filter_map(Value::as_object)
        .flatten()
        .filter(|(_, v)| !v.is_null())
        .for_each(|(k, v)| raw[k] = v.clone());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let Some(raw_key) = args[i].strip_prefix("--").map(|s| s.replace('-', "_")) else {
            positional.push(args[i].clone());
            i += 1;
            continue;
        };
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
            (None, _) => (0, json!(true)),
        };
        raw[key] = value;
        i += take + 1;
    }
    let known = ["download", "save", "sync", "url", "meta"].contains(&positional.first().map_or("", String::as_str));
    let op = if known { positional.remove(0) } else { "download".into() };
    Ok((op, positional, serde_json::from_value(raw)?))
}

fn number(mut songs: Vec<Song>, cfg: &Settings) -> Vec<Song> {
    let len = songs.len();
    songs.iter_mut().enumerate().filter(|(_, s)| cfg.playlist_numbering && s.list_name.is_some()).for_each(|(i, s)| {
        (s.album_name, s.album_artist) =
            (s.list_name.clone().unwrap_or_default(), s.list_name.clone().unwrap_or_default());
        (s.track_number, s.tracks_count, s.disc_number, s.disc_count) = (i as u32 + 1, len as u32, 1, 1);
        s.cover_url = s.cover_url.clone().filter(|_| cfg.playlist_retain_track_cover);
    });
    let keep =
        |s: &Song| (!cfg.skip_explicit || !s.explicit) && cfg.album_type.as_ref().is_none_or(|t| &s.album_type == t);
    songs
        .into_iter()
        .filter(keep)
        .filter(|s| !cfg.ignore_albums.iter().any(|a| s.album_name.to_lowercase().contains(&a.to_lowercase())))
        .collect()
}

fn run() -> R<()> {
    let (op, original, cfg) = settings()?;
    let flag = |f: &str| std::env::args().any(|a| a == f);
    if flag("--version") || flag("--generate-config") || original.is_empty() {
        let created = flag("--generate-config")
            && fs::create_dir_all(home()).and(fs::write(home().join("config.json"), DEFAULTS)).is_ok();
        println!(
            "spotdl {} {}",
            env!("CARGO_PKG_VERSION"),
            if created { "config written" } else { "[download|save|sync|url|meta] <query>..." }
        );
        return Ok(());
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
    let errors = Mutex::new(Vec::new());
    let mut songs: Vec<Song> = queries
        .par_iter()
        .flat_map_iter(|q| {
            number(
                spotify::resolve(q, &cfg, op != "save" && op != "url")
                    .inspect_err(|e| errors.lock().unwrap().push(format!("{q}: {e}")))
                    .unwrap_or_default(),
                &cfg,
            )
        })
        .collect();
    if cfg.fetch_albums {
        let ids: HashSet<_> = songs.iter().map(|s| s.album_id.clone()).filter(|id| !id.is_empty()).collect();
        let album = |id| {
            number(
                spotify::resolve(&format!("spotify:album:{id}"), &cfg, op != "save" && op != "url").unwrap_or_default(),
                &cfg,
            )
        };
        songs.extend(ids.par_iter().flat_map_iter(album).collect::<Vec<_>>());
    }
    let mut seen = HashSet::new();
    songs.retain(|s| {
        seen.insert(if op == "download" || op == "sync" { download::path(s, &cfg) } else { PathBuf::from(&s.url) })
    });
    if let Some(old) = sync.as_ref().filter(|_| !cfg.sync_without_deleting) {
        download::prune(&serde_json::from_value::<Vec<Song>>(old["songs"].clone())?, &songs, &cfg);
    }
    let done: Vec<(Song, R<PathBuf>)> = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads.max(1))
        .build()?
        .install(|| songs.into_par_iter().with_max_len(1).map(|s| download::process(s, &op, &cfg)).collect());
    errors
        .lock()
        .unwrap()
        .extend(done.iter().filter_map(|(s, r)| r.as_ref().err().map(|e| format!("{} - {}: {e}", s.artist, s.name))));
    let saved: Vec<&Song> = done.iter().map(|(s, _)| s).collect();
    if op == "sync" {
        let path = sync.as_ref().map(|_| original[0].clone()).or(cfg.save_file.clone());
        let path = path.unwrap_or_else(|| format!("{}.spotdl", download::clean(&queries[0])));
        fs::write(&path, serde_json::to_string_pretty(&json!({"type": "sync", "query": queries, "songs": &saved}))?)?;
    }
    if op == "save" {
        fs::write(cfg.save_file.as_deref().unwrap_or("saved.spotdl"), serde_json::to_string_pretty(&saved)?)?;
    }
    if let Some(m3u) = &cfg.m3u {
        let n = if m3u.is_empty() || m3u == "true" { "{list[0]}.m3u8" } else { m3u.as_str() };
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
        done.iter().filter(|(_, r)| cfg.add_unavailable || r.as_ref().is_ok_and(|p| p.is_file())).for_each(|(s, r)| {
            let path = r.as_ref().ok().cloned().unwrap_or_else(|| download::path(s, &cfg));
            let list = download::clean(s.list_name.as_deref().unwrap_or(s.album_name.as_str()));
            let file = name.replace("{list}", &list).replace("{list[0]}", &download::clean(first));
            let line = download::fill("#EXTINF:{duration},{album-artist} - {title}", s, &cfg);
            files.entry(file).or_insert("#EXTM3U\n".into()).push_str(&format!("{line}\n{}\n", path.display()));
        });
        files.into_iter().try_for_each(|(f, v)| {
            fs::create_dir_all(Path::new(&f).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")))
                .and_then(|_| fs::write(f, v))
        })?;
    }
    let errors = errors.into_inner().unwrap();
    errors.iter().filter(|_| cfg.print_errors).for_each(|e| eprintln!("{e}"));
    cfg.save_errors.iter().try_for_each(|p| fs::write(p, errors.join("\n")))?;
    eprintln!("{} succeeded, {} failed", done.iter().filter(|(_, r)| r.is_ok()).count(), errors.len());
    errors.is_empty().then_some(()).ok_or_else(|| format!("{} downloads failed", errors.len()).into())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
