use crate::{spotify::HTTP, Settings, Song, R};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[derive(Default)]
pub struct Hit {
    url: String,
    name: String,
    artists: Vec<String>,
    album: String,
    duration: f64,
    verified: bool,
    isrc: bool,
}

const FORBIDDEN: &str = "bassboosted remix remastered remaster reverb bassboost live acoustic 8daudio concert acapella slowed instrumental cover nightcore";

static YT_BLOCKED: AtomicU64 = AtomicU64::new(0);

static YT_ANNOUNCED: AtomicU64 = AtomicU64::new(0);

static YT_ATTEMPT: AtomicU64 = AtomicU64::new(0);

fn blocked(output: &Output) -> bool {
    let error = String::from_utf8_lossy(&output.stderr);
    error.contains("Sign in to confirm")
        || error.contains("HTTP Error 429")
        || error.contains("HTTP 429")
        || error.contains("Too Many Requests")
}

pub fn yt_output(cmd: &mut Command, cfg: &Settings) -> R<Output> {
    crate::retry(
        cfg.max_retries.saturating_add(1),
        |_| {
            let deadline = YT_BLOCKED.load(Ordering::Relaxed);
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let seconds = deadline.saturating_sub(now);
            if seconds > 0 && YT_ANNOUNCED.swap(deadline, Ordering::Relaxed) != deadline {
                eprintln!("YouTube blocked downloads. Waiting {seconds} s.");
            }
            std::thread::sleep(Duration::from_secs(seconds));
            let output = cmd.output()?;
            if blocked(&output) {
                return Err(std::io::Error::other(String::from_utf8_lossy(&output.stderr).to_string()).into());
            }
            YT_ATTEMPT.store(0, Ordering::Relaxed);
            Ok(output)
        },
        |error| {
            let message = error.to_string();
            if !["Sign in to confirm", "HTTP Error 429", "HTTP 429", "Too Many Requests"]
                .iter()
                .any(|text| message.contains(text))
            {
                return None;
            }
            let attempt = YT_ATTEMPT.fetch_add(1, Ordering::Relaxed) as usize;
            let seconds = std::env::var("SPOTDL_TEST_BACKOFF_SECONDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or([30, 60, 120][attempt.min(2)]);
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            YT_BLOCKED.fetch_max(now + seconds, Ordering::Relaxed);
            Some(Duration::ZERO)
        },
    )
}

pub fn deno_args(cmd: &mut Command) {
    let deno = crate::home().join(if cfg!(windows) { "deno.exe" } else { "deno" });
    if deno.is_file() {
        cmd.args(["--js-runtimes", &format!("deno:{}", deno.display())]);
    }
}

pub fn slug(s: &str) -> String {
    let s = deunicode::deunicode(s).to_lowercase();
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-")
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

fn ytm(query: &str, songs: bool, isrc: bool) -> R<Vec<Hit>> {
    let params = if songs { "EgWKAQIIAWoMEAMQBBAJEA4QChAF" } else { "EgWKAQIQAWoMEA4QChADEAQQCRAF" };
    let client = json!({"clientName": "WEB_REMIX", "clientVersion": "1.20251001.01.00", "hl": "en", "gl": "US"});
    let body = json!({"context": {"client": client}, "query": query, "params": params});
    let v: Value = HTTP
        .post("https://music.youtube.com/youtubei/v1/search?prettyPrint=false")
        .send_json(body)?
        .body_mut()
        .read_json()?;
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
            let artists = Some(kind("ARTIST"))
                .filter(|a| !a.is_empty())
                .or(Some(vec![meta.first()?["text"].as_str()?.into()]))?;
            Some(Hit {
                url: format!("https://music.youtube.com/watch?v={}", r["playlistItemData"]["videoId"].as_str()?),
                name: columns[0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"][0]["text"]
                    .as_str()?
                    .into(),
                album: kind("ALBUM").first().cloned().unwrap_or_default(),
                artists,
                duration: time.ok()?,
                verified: songs,
                isrc,
            })
        })
        .collect())
}

fn ytdlp(search: &str, cfg: &Settings) -> R<Vec<Hit>> {
    let mut cmd = Command::new(&cfg.yt_dlp);
    cmd.args(["--flat-playlist", "-J", "--no-warnings"]);
    deno_args(&mut cmd);
    cmd.arg(search);
    let out = yt_output(&mut cmd, cfg)?;
    if !out.status.success() {
        return Err(format!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let mut v: Value = serde_json::from_slice(&out.stdout)?;
    let entries = match v["entries"].take() {
        Value::Array(entries) => entries,
        _ => vec![v],
    };
    Ok(entries
        .iter()
        .filter_map(|e| {
            Some(Hit {
                url: e["webpage_url"].as_str().or(e["url"].as_str())?.into(),
                name: e["track"].as_str().or(e["title"].as_str())?.into(),
                artists: vec![["artist", "uploader", "channel"]
                    .iter()
                    .find_map(|k| e[k].as_str())
                    .unwrap_or_default()
                    .into()],
                album: e["album"].as_str().unwrap_or_default().into(),
                duration: e["duration"].as_f64().unwrap_or_default(),
                ..Hit::default()
            })
        })
        .collect())
}

fn score(s: &Song, h: &Hit) -> f64 {
    let (rname, sname) = (slug(&h.name), slug(&s.name));
    let found = |a: &String| h.artists.iter().any(|r| ratio(&slug(a), &slug(r)) > 85.0) || rname.contains(&slug(a));
    let count = s.artists.len().max(1) as f64;
    let artists = 100.0 * s.artists.iter().filter(|a| found(a)).count() as f64 / count;
    let main = h.artists.iter().chain([&h.name]).map(|r| ratio(&slug(&s.artist), &slug(r))).fold(0.0, f64::max);
    let artists = if found(&s.artist) { artists.max(main).max(160.0 / count).min(100.0) } else { main.min(artists) };
    let fwords = FORBIDDEN.split(' ').filter(|w| rname.contains(w) && !sname.contains(w)).count() as f64;
    let stripped = s.artists.iter().fold(slug(&h.name), |n, a| n.replace(&slug(a), ""));
    let name = ratio(&sname, &rname).max(ratio(&sname, stripped.trim_matches('-'))) - 15.0 * fwords;
    let time = (-0.1 * (s.duration - h.duration).abs()).exp() * 100.0;
    let album = if h.album.is_empty() { 0.0 } else { ratio(&slug(&s.album_name), &slug(&h.album)) };
    let avg = (artists + name) / 2.0;
    let avg = if h.verified && !h.isrc && !h.album.is_empty() && album <= 80.0 { (avg + album) / 2.0 } else { avg };
    let skip = name <= 60.0 || artists < 70.0 || time < 25.0 || (time < 50.0 && avg < 75.0);
    let avg = if !h.isrc && avg <= 85.0 { (avg + time) / 2.0 } else { avg };
    avg.min(100.0) * f64::from(u8::from(!skip))
}

pub fn find(s: &Song, cfg: &Settings) -> R<Vec<String>> {
    let q = crate::download::fill(cfg.search_query.as_deref().unwrap_or("{artists} - {title}"), s, cfg);
    let rank = |h: Hit| (if cfg.filter_results { score(s, &h) } else { 100.0 }, h.url);
    let isrc = s.isrc.as_ref().filter(|_| cfg.audio_providers.iter().any(|p| p == "youtube-music"));
    let mut found: Vec<_> = isrc
        .into_iter()
        .flat_map(|i| ytm(i, true, true).unwrap_or_default())
        .filter(|h| (h.duration - s.duration).abs() < 10.0)
        .filter(|h| !cfg.only_verified_results || h.verified)
        .map(&rank)
        .filter(|x| x.0 > 0.0)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .into_iter()
        .map(|x| (f64::INFINITY, x.1))
        .collect();
    for p in &cfg.audio_providers {
        let mut hits = match p.as_str() {
            "youtube-music" => ytm(&q, true, false),
            "youtube" => ytdlp(&format!("ytsearch10:{q}"), cfg),
            "soundcloud" => ytdlp(&format!("scsearch10:{q}"), cfg),
            _ => Ok(Vec::new()),
        }
        .unwrap_or_default();
        let best = hits.iter().map(|h| if cfg.filter_results { score(s, h) } else { 100.0 }).max_by(f64::total_cmp);
        if p == "youtube-music" && !cfg.only_verified_results && best.is_none_or(|n| n < 80.0) {
            hits.extend(ytm(&q, false, false).unwrap_or_default());
        }
        found.extend(
            hits.into_iter()
                .filter(|hit| !cfg.only_verified_results || hit.verified)
                .map(&rank)
                .filter(|candidate| candidate.0 > 0.0),
        );
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut seen = HashSet::new();
    let found: Vec<String> = found.into_iter().filter(|x| seen.insert(x.1.clone())).take(3).map(|x| x.1).collect();
    (!found.is_empty()).then_some(found).ok_or_else(|| format!("no match for {q}").into())
}

pub fn youtube(url: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    let n = if url.contains("list=") && !url.contains("watch?") { usize::MAX } else { 1 };
    let music = url.contains("music.youtube.com");
    let songs = ytdlp(url, cfg)?
        .into_iter()
        .take(n)
        .filter_map(|hit| {
            let source_url = hit.url.clone();
            if music {
                let mut song =
                    crate::spotify::resolve(&format!("{} - {}", hit.artists.join(", "), hit.name), cfg, enrich)
                        .ok()
                        .and_then(|mut songs| songs.drain(..).next())?;
                if cfg.ytm_data {
                    song.name = hit.name;
                    song.artist = hit.artists.first().cloned().unwrap_or_default();
                    song.artists = hit.artists;
                    song.duration = hit.duration;
                    song.album_name = hit.album;
                }
                song.download_url = Some(source_url);
                return Some(song);
            }
            Some(from_hit(hit))
        })
        .collect();
    Ok(songs)
}

pub fn source(url: &str, cfg: &Settings) -> R<Vec<Song>> {
    Ok(ytdlp(url, cfg)?.into_iter().map(from_hit).collect())
}

fn from_hit(hit: Hit) -> Song {
    let artist = hit.artists.first().cloned().unwrap_or_default();
    let song_id = hit.url.rsplit(['=', '/']).next().unwrap_or_default().to_string();
    Song {
        name: hit.name,
        artist,
        artists: hit.artists,
        album_name: hit.album,
        duration: hit.duration,
        song_id,
        url: hit.url.clone(),
        download_url: Some(hit.url),
        ..Song::default()
    }
}
