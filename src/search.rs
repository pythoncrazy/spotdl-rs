use crate::{Song, R};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
pub struct Hit {
    song: Song,
    verified: bool,
    isrc: bool,
}

fn hit_song(url: String, name: String, artists: Vec<String>, album: String, duration: f64) -> Song {
    let artist = artists.first().cloned().unwrap_or_default();
    let song_id = url.rsplit(['=', '/']).next().unwrap_or_default().to_string();
    Song {
        name,
        artist,
        artists,
        album_name: album,
        duration,
        song_id,
        url: url.clone(),
        download_url: Some(url),
        ..Song::default()
    }
}

const FORBIDDEN: &str = "bassboosted remix remastered remaster reverb bassboost live acoustic 8daudio concert acapella slowed instrumental cover nightcore";
const VERSION_SUFFIXES: &str = "remaster remastered mono stereo mix version edit single radio deluxe anniversary";

pub(crate) struct Gate {
    name: &'static str,
    until: AtomicU64,
    strikes: AtomicU64,
}

impl Gate {
    const fn new(name: &'static str) -> Self {
        Self { name, until: AtomicU64::new(0), strikes: AtomicU64::new(0) }
    }
}

pub(crate) static SEARCH_GATE: Gate = Gate::new("searches");
pub(crate) static DOWNLOAD_GATE: Gate = Gate::new("downloads");

fn short_retry_delay() -> Duration {
    let jitter = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos() % 3;
    Duration::from_secs(3 + u64::from(jitter))
}

fn with_backoff<T>(
    gate: &Gate,
    cfg: &Value,
    mut attempt: impl FnMut() -> Result<T, String>,
    retry: impl Fn(&str) -> bool,
) -> R<T> {
    let attempts = cfg["max_retries"].as_u64().unwrap_or(3).saturating_add(1);
    for number in 1..=attempts {
        let until = gate.until.load(Relaxed);
        let wait = until.saturating_sub(UNIX_EPOCH.elapsed().unwrap_or_default().as_secs());
        if wait > 0 {
            eprintln!("YouTube blocked {}. Waiting {wait} s.", gate.name);
        }
        std::thread::sleep(Duration::from_secs(wait));
        match attempt() {
            Ok(value) => {
                gate.strikes.store(0, Relaxed);
                return Ok(value);
            }
            Err(error) if !retry(&error) => return Err(error.into()),
            Err(error) if number == attempts => {
                if number > 1 && !transient(&error) {
                    strike(gate);
                }
                return Err(error.into());
            }
            Err(error) if transient(&error) || number == 1 => std::thread::sleep(short_retry_delay()),
            Err(_) => strike(gate),
        }
    }
    Err(format!("YouTube blocked {}", gate.name).into())
}

fn strike(gate: &Gate) {
    let strike = gate.strikes.fetch_add(1, Relaxed).min(2) as usize;
    gate.until.fetch_max(UNIX_EPOCH.elapsed().unwrap_or_default().as_secs() + [30, 60, 120][strike], Relaxed);
}

fn transient(error: &str) -> bool {
    error.contains("Requested format is not available")
}

fn youtube_retryable(error: &str) -> bool {
    transient(error)
        || ["not a bot", "HTTP Error 403", "HTTP Error 429", "HTTP 429", "Too Many Requests"]
            .iter()
            .any(|pattern| error.contains(pattern))
}

pub(crate) fn yt(cmd: &mut Command, cfg: &Value, gate: &Gate) -> R<Output> {
    with_backoff(
        gate,
        cfg,
        || {
            let output = cmd.output().map_err(|error| error.to_string())?;
            let error = String::from_utf8_lossy(&output.stderr);
            if youtube_retryable(&error) {
                Err(error.trim().to_string())
            } else {
                Ok(output)
            }
        },
        youtube_retryable,
    )
}

fn js_runtime_args(cmd: &mut Command) {
    let deno = crate::HOME.join(if cfg!(windows) { "deno.exe" } else { "deno" });
    if deno.is_file() {
        cmd.args(["--js-runtimes", &format!("deno:{}", deno.display())]);
        return;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    if let Some((name, path)) = ["deno", "node", "bun"].into_iter().find_map(|name| {
        let executable = format!("{name}{suffix}");
        std::env::split_paths(&path)
            .map(|dir| dir.join(&executable))
            .find(|path| path.is_file())
            .map(|path| (name, path))
    }) {
        cmd.args(["--js-runtimes", &format!("{name}:{}", path.display())]);
    }
}

pub(crate) fn yt_dlp(cfg: &Value) -> Command {
    let mut command = Command::new(cfg["yt_dlp"].as_str().unwrap_or("yt-dlp"));
    for (key, flag) in [("cookie_file", "--cookies"), ("proxy", "--proxy")] {
        if let Some(value) = cfg[key].as_str() {
            command.args([flag, value]);
        }
    }
    command.args(cfg["yt_dlp_args"].as_str().unwrap_or_default().split_whitespace());
    js_runtime_args(&mut command);
    command
}

pub fn slug(s: &str, sep: &str) -> String {
    let s = deunicode::deunicode(s).to_lowercase();
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(sep)
}

pub fn ratio(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let row = a.iter().fold(vec![0usize; b.len() + 1], |row, x| {
        b.iter().enumerate().fold(vec![0], |mut next, (j, y)| {
            next.push(if x == y { row[j] + 1 } else { row[j + 1].max(next[j]) });
            next
        })
    });
    let n = (a.len() + b.len()) as f64;
    if n == 0.0 {
        100.0
    } else {
        200.0 * row[b.len()] as f64 / n
    }
}

fn is_version_suffix(suffix: &str) -> bool {
    let words = slug(suffix, "-");
    !words.is_empty()
        && words.split('-').all(|word| {
            word.bytes().all(|byte| byte.is_ascii_digit()) || VERSION_SUFFIXES.split(' ').any(|w| w == word)
        })
}

pub(crate) fn versionless(name: &str) -> &str {
    name.rsplit_once(" - ").filter(|(_, suffix)| is_version_suffix(suffix)).map_or(name, |(title, _)| title)
}

fn ytm_retryable(error: &str) -> bool {
    let status = error
        .strip_prefix("HTTP ")
        .and_then(|error| error.split_whitespace().next())
        .and_then(|status| status.parse::<u16>().ok());
    status.is_some_and(|status| status == 403 || status == 429 || status >= 500)
        || ["timeout", "timed out", "connection", "connect", "network", "transport"]
            .iter()
            .any(|sign| error.to_ascii_lowercase().contains(sign))
}

fn ytm_hits(v: &Value, songs: bool, isrc: bool) -> R<Vec<Hit>> {
    if v.get("contents").is_none() {
        return Err("unexpected YouTube Music response".into());
    }
    let tab = &v["contents"]["tabbedSearchResultsRenderer"]["tabs"][0]["tabRenderer"]["content"];
    let shelves = tab["sectionListRenderer"]["contents"].as_array().into_iter().flatten();
    let items = shelves.flat_map(|s| s["musicShelfRenderer"]["contents"].as_array().into_iter().flatten());
    Ok(items
        .filter_map(|i| {
            let r = &i["musicResponsiveListItemRenderer"];
            let columns = &r["flexColumns"];
            let meta = columns[1]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"].as_array()?;
            let kind = |t: &str| -> Vec<String> {
                let typed = meta.iter().filter(|x| x["navigationEndpoint"]["browseEndpoint"].to_string().contains(t));
                typed.filter_map(|x| x["text"].as_str().map(String::from)).collect()
            };
            let time =
                meta.last()?["text"].as_str()?.split(':').try_fold(0.0, |a, p| p.parse::<f64>().map(|p| a * 60.0 + p));
            let artists = kind("ARTIST");
            let artists = match artists.first() {
                Some(_) => artists,
                None => vec![meta.first()?["text"].as_str()?.into()],
            };
            Some(Hit {
                song: hit_song(
                    format!("https://music.youtube.com/watch?v={}", r["playlistItemData"]["videoId"].as_str()?),
                    columns[0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"][0]["text"].as_str()?.into(),
                    artists,
                    kind("ALBUM").first().cloned().unwrap_or_default(),
                    time.ok()?,
                ),
                verified: songs,
                isrc,
            })
        })
        .collect())
}

fn ytm(query: &str, songs: bool, isrc: bool, cfg: &Value) -> R<Vec<Hit>> {
    let params = if songs { "EgWKAQIIAWoMEAMQBBAJEA4QChAF" } else { "EgWKAQIQAWoMEA4QChADEAQQCRAF" };
    let client = json!({"clientName": "WEB_REMIX", "clientVersion": "1.20251001.01.00", "hl": "en", "gl": "US"});
    let body = json!({"context": {"client": client}, "query": query, "params": params}).to_string();
    let url = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
    let v = with_backoff(
        &SEARCH_GATE,
        cfg,
        || {
            crate::http_json("POST", url, &[("Content-Type", "application/json")], Some(body.as_bytes()), cfg)
                .map_err(|error| error.to_string())
        },
        ytm_retryable,
    )?;
    ytm_hits(&v, songs, isrc)
}

fn hits_or_note(first_error: &mut Option<String>, result: R<Vec<Hit>>) -> Vec<Hit> {
    result.unwrap_or_else(|error| {
        first_error.get_or_insert(error.to_string());
        Vec::new()
    })
}

pub fn isrc_hit(song: &Song, cfg: &Value) -> R<Option<String>> {
    let Some(isrc) = song.isrc.as_ref().filter(|_| audio_providers(cfg).any(|provider| provider == "youtube-music"))
    else {
        return Ok(None);
    };
    let filter = cfg["filter_results"] != false;
    let rank = |hit: &Hit| if filter { score(song, hit) } else { 100.0 };
    Ok(ytm(isrc, true, true, cfg)?
        .into_iter()
        .filter(|hit| (hit.song.duration - song.duration).abs() < 10.0 && rank(hit) > 0.0)
        .max_by(|a, b| rank(a).total_cmp(&rank(b)))
        .map(|hit| hit.song.url))
}

pub(crate) fn first_hit(song: &Song, cfg: &Value) -> R<String> {
    let isrc = isrc_hit(song, cfg);
    if let Ok(Some(url)) = isrc.as_ref() {
        return Ok(url.clone());
    }
    find(song, cfg, isrc.err().map(|error| error.to_string()))?.into_iter().next().ok_or("no match".into())
}

fn ytdlp(search: &str, cfg: &Value) -> R<Vec<Hit>> {
    let mut cmd = yt_dlp(cfg);
    cmd.args(["--flat-playlist", "-J", "--no-warnings"]);
    let out = yt(cmd.arg(search), cfg, &SEARCH_GATE)?;
    if !out.status.success() {
        return Err(format!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let mut v: Value = serde_json::from_slice(&out.stdout)?;
    let entries = match v["entries"].take() {
        Value::Array(entries) => entries,
        _ => vec![v],
    };
    let hit = |entry: &Value| {
        let artist = ["artist", "uploader", "channel"].iter().find_map(|key| entry[key].as_str());
        Some(Hit {
            song: hit_song(
                entry["webpage_url"].as_str().or(entry["url"].as_str())?.into(),
                entry["track"].as_str().or(entry["title"].as_str())?.into(),
                vec![artist.unwrap_or_default().into()],
                entry["album"].as_str().unwrap_or_default().into(),
                entry["duration"].as_f64().unwrap_or_default(),
            ),
            ..Hit::default()
        })
    };
    Ok(entries.iter().filter_map(hit).collect())
}

fn score(s: &Song, h: &Hit) -> f64 {
    let hit = &h.song;
    let (rname, sname) = (slug(&hit.name, "-"), slug(&s.name, "-"));
    let versionless = slug(versionless(&s.name), "-");
    let found = |a: &String| {
        hit.artists.iter().any(|r| ratio(&slug(a, "-"), &slug(r, "-")) > 85.0) || rname.contains(&slug(a, "-"))
    };
    let count = s.artists.len().max(1) as f64;
    let artists = 100.0 * s.artists.iter().filter(|a| found(a)).count() as f64 / count;
    let main =
        hit.artists.iter().chain([&hit.name]).map(|r| ratio(&slug(&s.artist, "-"), &slug(r, "-"))).fold(0.0, f64::max);
    let artists = if found(&s.artist) { artists.max(main).max(160.0 / count).min(100.0) } else { main.min(artists) };
    let fwords = FORBIDDEN.split(' ').filter(|w| rname.contains(w) && !sname.contains(w)).count() as f64;
    let stripped = s.artists.iter().fold(slug(&hit.name, "-"), |n, a| n.replace(&slug(a, "-"), ""));
    let stripped = stripped.trim_matches('-');
    let name = ratio(&sname, &rname)
        .max(ratio(&sname, stripped))
        .max(ratio(&versionless, &rname))
        .max(ratio(&versionless, stripped))
        - 15.0 * fwords;
    let time = (-0.1 * (s.duration - hit.duration).abs()).exp() * 100.0;
    let album =
        if hit.album_name.is_empty() { 0.0 } else { ratio(&slug(&s.album_name, "-"), &slug(&hit.album_name, "-")) };
    let avg = (artists + name) / 2.0;
    let avg =
        if h.verified && !h.isrc && !hit.album_name.is_empty() && album <= 80.0 { (avg + album) / 2.0 } else { avg };
    let skip = name <= 60.0 || artists < 70.0 || time < 25.0 || (time < 50.0 && avg < 75.0);
    let avg = if !h.isrc && avg <= 85.0 { (avg + time) / 2.0 } else { avg };
    avg.min(100.0) * f64::from(u8::from(!skip))
}

fn audio_providers(cfg: &Value) -> impl Iterator<Item = &str> {
    cfg["audio_providers"].as_array().into_iter().flatten().filter_map(Value::as_str)
}

pub fn find(song: &Song, cfg: &Value, prior_error: Option<String>) -> R<Vec<String>> {
    let query = crate::download::fill(cfg["search_query"].as_str().unwrap_or("{artists} - {title}"), song, cfg);
    let filter = cfg["filter_results"] != false;
    let verified = cfg["only_verified_results"] == true;
    let rank = |hit: &Hit| if filter { score(song, hit) } else { 100.0 };
    let mut found: Vec<(f64, String)> = Vec::new();
    let mut first_error = prior_error;
    for provider in audio_providers(cfg) {
        let hits = match provider {
            "youtube-music" => ytm(&query, true, false, cfg),
            "youtube" => ytdlp(&format!("ytsearch10:{query}"), cfg),
            _ => ytdlp(&format!("scsearch10:{query}"), cfg),
        };
        let mut hits = hits_or_note(&mut first_error, hits);
        let best = hits.iter().map(rank).max_by(f64::total_cmp);
        if provider == "youtube-music" && !verified && best.is_none_or(|best| best < 80.0) {
            hits.extend(hits_or_note(&mut first_error, ytm(&query, false, false, cfg)));
        }
        let hits = hits.into_iter().filter(|hit| !verified || hit.verified);
        found.extend(hits.map(|hit| (rank(&hit), hit.song.url)).filter(|candidate| candidate.0 > 0.0));
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut seen = HashSet::new();
    let found: Vec<String> =
        found.into_iter().map(|candidate| candidate.1).filter(|url| seen.insert(url.clone())).take(3).collect();
    if found.is_empty() {
        return Err(first_error
            .map_or_else(|| format!("no match for {query}"), |error| format!("search failed for {query}: {error}"))
            .into());
    }
    Ok(found)
}

pub(crate) fn apply_ytm_data(song: &mut Song, source: Song, copy: bool) {
    if copy {
        song.name = source.name;
        song.artist = source.artist;
        song.artists = source.artists;
        song.duration = source.duration;
        song.album_name = source.album_name;
    }
    song.download_url = source.download_url;
}

pub fn source(url: &str, cfg: &Value, lookup: bool) -> R<Vec<Song>> {
    let count =
        if url.contains("youtu") && !(url.contains("list=") && !url.contains("watch?")) { 1 } else { usize::MAX };
    let music = url.contains("music.youtube.com");
    let song = |hit: Hit| {
        if !lookup || !music {
            return Some(hit.song);
        }
        let query = format!("{} - {}", hit.song.artists.join(", "), hit.song.name);
        let mut song = crate::spotify::resolve(&query, cfg).ok()?.into_iter().next()?;
        apply_ytm_data(&mut song, hit.song, cfg["ytm_data"] == true);
        Some(song)
    };
    Ok(ytdlp(url, cfg)?.into_iter().take(count).filter_map(song).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yt_dlp_command_includes_proxy_and_custom_args() {
        let cfg = serde_json::json!({
            "yt_dlp": "yt-dlp-test",
            "cookie_file": "/tmp/cookies.txt",
            "proxy": "http://proxy.example:8080",
            "yt_dlp_args": "--no-warnings --verbose"
        });
        let args = yt_dlp(&cfg).get_args().map(|arg| arg.to_string_lossy().into_owned()).collect::<Vec<_>>();
        assert!(args.starts_with(&[
            "--cookies".into(),
            "/tmp/cookies.txt".into(),
            "--proxy".into(),
            "http://proxy.example:8080".into(),
        ]));
        assert!(args.windows(2).any(|pair| pair[0] == "--no-warnings" && pair[1] == "--verbose"));
    }

    fn song(name: &str) -> Song {
        Song {
            name: name.into(),
            artist: "The Beatles".into(),
            artists: vec!["The Beatles".into()],
            duration: 140.0,
            ..Song::default()
        }
    }

    fn hit(name: &str) -> Hit {
        Hit {
            song: hit_song(
                "https://youtube.com/watch?v=test".into(),
                name.into(),
                vec!["The Beatles".into()],
                "".into(),
                140.0,
            ),
            ..Hit::default()
        }
    }

    #[test]
    fn version_suffixes_allow_a_remastered_title_match() {
        assert!(score(&song("I Feel Fine - Remastered 2015"), &hit("I Feel Fine")) > 0.0);
    }

    #[test]
    fn only_recognized_version_suffix_words_are_allowed() {
        for suffix in [
            "Remastered 2009",
            "2009 Remaster",
            "Mono",
            "Stereo",
            "2018 Mix",
            "Single Version",
            "Radio Edit",
            "Deluxe Anniversary",
        ] {
            assert!(is_version_suffix(suffix), "{suffix}");
        }
        for suffix in ["Take 2", "Live At The BBC 1964", "Instrumental"] {
            assert!(!is_version_suffix(suffix), "{suffix}");
        }
    }

    #[test]
    fn versionless_strips_only_recognized_suffixes() {
        assert_eq!(versionless("Taxman - Remastered 2009"), "Taxman");
        assert_eq!(versionless("Love Me Do - Mono"), "Love Me Do");
        assert_eq!(versionless("Bad - Live at Wembley"), "Bad - Live at Wembley");
    }

    #[test]
    fn failed_search_reports_provider_error() {
        SEARCH_GATE.until.store(0, Relaxed);
        SEARCH_GATE.strikes.store(0, Relaxed);
        DOWNLOAD_GATE.until.store(0, Relaxed);
        DOWNLOAD_GATE.strikes.store(0, Relaxed);
        let mut cfg: Value = serde_json::from_str(crate::DEFAULTS).unwrap();
        cfg["audio_providers"] = serde_json::json!(["youtube-music"]);
        cfg["max_retries"] = serde_json::json!(0);
        cfg["proxy"] = serde_json::json!("http://127.0.0.1:9");
        let isrc_song = Song { isrc: Some("USRC17607839".into()), ..song("Test") };
        let error = first_hit(&isrc_song, &cfg).unwrap_err().to_string();
        assert!(error.contains("search failed") && !error.contains("no match"));
    }

    #[test]
    fn backoff_delays_a_first_retry_and_strikes_on_a_second_failure() {
        for message in ["Sign in to confirm you're not a bot", "Sign in to confirm you’re not a bot"] {
            assert!(youtube_retryable(message));
        }
        let gate = Gate::new("test");
        let cfg = serde_json::json!({"max_retries": 1});
        let mut attempts = 0;
        let result = with_backoff(
            &gate,
            &cfg,
            || {
                attempts += 1;
                if attempts == 1 {
                    Err("HTTP Error 403".into())
                } else {
                    Ok(())
                }
            },
            youtube_retryable,
        );
        assert!(result.is_ok());
        assert_eq!(gate.until.load(Relaxed), 0);

        let result = with_backoff(&gate, &cfg, || Err::<(), _>("HTTP Error 403".into()), youtube_retryable);
        assert!(result.is_err());
        assert_eq!(gate.strikes.load(Relaxed), 1);
        assert!(gate.until.load(Relaxed) > 0);

        let gate = Gate::new("downloads");
        let age_restricted = "Sign in to confirm your age. This video may be inappropriate.";
        assert!(!youtube_retryable(age_restricted));
        let result = with_backoff(&gate, &cfg, || Err::<(), _>(age_restricted.into()), youtube_retryable);
        assert!(result.is_err());
        assert_eq!(gate.until.load(Relaxed), 0);
        assert_eq!(gate.strikes.load(Relaxed), 0);
    }

    #[test]
    fn unavailable_download_format_retries_without_striking() {
        let error = "ERROR: Requested format is not available";
        assert!(youtube_retryable(error));
        let gate = Gate::new("downloads");
        let cfg = serde_json::json!({"max_retries": 1});
        let mut attempts = 0;
        let result = with_backoff(
            &gate,
            &cfg,
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(error.into())
                } else {
                    Ok(())
                }
            },
            youtube_retryable,
        );
        assert!(result.is_ok());
        assert_eq!(gate.until.load(Relaxed), 0);
        assert_eq!(gate.strikes.load(Relaxed), 0);

        let gate = Gate::new("downloads");
        let mut attempts = 0;
        let result = with_backoff(
            &gate,
            &cfg,
            || {
                attempts += 1;
                Err::<(), _>(error.into())
            },
            youtube_retryable,
        );
        assert!(result.is_err());
        assert_eq!(attempts, 2);
        assert_eq!(gate.until.load(Relaxed), 0);
        assert_eq!(gate.strikes.load(Relaxed), 0);
    }

    #[test]
    fn missing_ytm_contents_is_an_error_but_empty_results_are_not() {
        assert!(matches!(
            ytm_hits(&serde_json::json!({}), true, false),
            Err(error) if error.to_string() == "unexpected YouTube Music response"
        ));
        assert!(ytm_hits(&serde_json::json!({"contents": {}}), true, false).unwrap().is_empty());
    }
}
