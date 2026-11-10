//! Settings: the defaults, then `config.json`, then the command line.

use crate::{Value, HOME, R};
use serde::Deserialize;
use serde_json::json;
use std::{fs, path::PathBuf, sync::OnceLock};

pub(crate) const DEFAULTS: &str = r#"
{"output":"{artists} - {title}.{output-ext}","format":"opus","bitrate":"disable",
"overwrite":"skip","threads":16,"meta_threads":128,"ffmpeg":"ffmpeg","yt_dlp":"yt-dlp",
"audio_providers":["youtube-music"],"lyrics_providers":["genius","synced","azlyrics",
"musixmatch"],"search_query":null,"cookie_file":null,"proxy":null,"yt_dlp_args":null,
"ffmpeg_args":null,"restrict":null,"playlist_numbering":false,
"playlist_retain_track_cover":false,"m3u":null,"archive":null,"save_file":null,
"filter_results":true,"album_type":null,"print_errors":false,"save_errors":null,
"sponsor_block":false,"preload":false,"id3_separator":"/","generate_lrc":false,
"force_update_metadata":false,"only_verified_results":false,"sync_without_deleting":false,
"sync_remove_lrc":false,"max_filename_length":0,"detect_formats":[],"ignore_albums":[],
"skip_explicit":false,"redownload":false,"skip_album_art":false,"replaygain":false,
"folder_art":false,"create_skip_file":false,"respect_skip_file":false,"no_cache":false,
"genius_token":"","log_level":"INFO","log_format":null,"use_cache_file":false,"max_retries":3,
"simple_tui":false,"ytm_data":false,"add_unavailable":false,"fetch_albums":false,
"scan_for_songs":false,"load_config":true}
"#;
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

/// What a run does. `Redownload` is the download of `meta --redownload`, which replaces the local file.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Op {
    #[default]
    Download,
    Save,
    Sync,
    Url,
    Meta,
    Redownload,
}

const OPS: [(&str, Op); 5] =
    [("download", Op::Download), ("save", Op::Save), ("sync", Op::Sync), ("url", Op::Url), ("meta", Op::Meta)];

/// What happens to a song whose file is there: it is left, downloaded again, or only tagged again.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Overwrite {
    Skip,
    Force,
    Metadata,
}

impl Overwrite {
    /// Whether the tags, lyrics and `.lrc` of a file that is there are written again.
    pub fn replaces(self) -> bool {
        self != Self::Skip
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Format {
    Mp3,
    Flac,
    Ogg,
    Opus,
    M4a,
    Wav,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
            Self::M4a => "m4a",
            Self::Wav => "wav",
        }
    }
}

/// Every setting. A key that is not a setting is an error on the command line and ignored in `config.json`.
#[derive(Debug, Deserialize)]
pub(crate) struct Settings {
    // Where songs go and how they are encoded.
    pub output: String,
    pub format: Format,
    pub bitrate: String,
    pub overwrite: Overwrite,
    pub restrict: Option<String>,
    pub max_filename_length: u64,
    pub archive: Option<String>,
    pub save_file: Option<String>,
    pub save_errors: Option<String>,
    pub m3u: Option<String>,
    pub create_skip_file: bool,
    pub respect_skip_file: bool,
    pub scan_for_songs: bool,
    pub sync_without_deleting: bool,
    pub sync_remove_lrc: bool,
    pub detect_formats: Vec<String>,
    pub folder_art: bool,
    pub skip_album_art: bool,
    pub id3_separator: String,

    // Which songs.
    pub ignore_albums: Vec<String>,
    pub album_type: Option<String>,
    pub skip_explicit: bool,
    pub playlist_numbering: bool,
    pub playlist_retain_track_cover: bool,
    pub fetch_albums: bool,
    pub add_unavailable: bool,

    // Audio sources.
    pub audio_providers: Vec<String>,
    pub search_query: Option<String>,
    pub only_verified_results: bool,
    pub ytm_data: bool,
    pub yt_dlp: String,
    pub yt_dlp_args: Option<String>,
    pub ffmpeg: String,
    pub ffmpeg_args: Option<String>,
    pub cookie_file: Option<String>,
    pub sponsor_block: bool,
    pub preload: bool,
    pub redownload: bool,

    // Lyrics and tags.
    pub lyrics_providers: Vec<String>,
    pub generate_lrc: bool,
    pub genius_token: String,
    pub force_update_metadata: bool,
    pub replaygain: bool,

    // Network and caches.
    pub threads: u64,
    pub meta_threads: u64,
    pub max_retries: u64,
    pub proxy: Option<String>,
    pub no_cache: bool,
    pub use_cache_file: bool,
    pub filter_results: bool,

    // Output of the program.
    pub print_errors: bool,
    pub simple_tui: bool,
    pub log_level: String,
    pub log_format: Option<String>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

pub(crate) fn settings() -> &'static Settings {
    SETTINGS.get().expect("settings are loaded before use")
}

/// The defaults as the settings, for tests.
#[cfg(test)]
pub(crate) fn use_defaults() {
    let _ = SETTINGS.set(serde_json::from_str(DEFAULTS).unwrap());
}

/// Loads the settings and returns the operation and its queries.
pub(crate) fn load(args: &[String]) -> R<(Op, Vec<String>)> {
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
    // `--m3u` as a flag, or `"m3u": true` in the file, is the default name.
    match cfg["m3u"] {
        Value::Bool(true) => cfg["m3u"] = json!(""),
        Value::Bool(false) => cfg["m3u"] = Value::Null,
        _ => {}
    }
    SETTINGS.set(serde_json::from_value(cfg)?).map_err(|_| "settings were already loaded")?;
    let op = OPS.iter().find(|(name, _)| queries.first().is_some_and(|query| query == name)).map(|(_, op)| *op);
    if op.is_some() {
        queries.remove(0);
    }
    Ok((op.unwrap_or_default(), queries))
}

#[cfg(test)]
#[path = "fuzz/settings.rs"]
mod fuzz;
