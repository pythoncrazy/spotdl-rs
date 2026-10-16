use crate::{http, Song, R};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering::Relaxed},
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

static BLOCKED_UNTIL: AtomicU64 = AtomicU64::new(0);

static STRIKES: AtomicU64 = AtomicU64::new(0);

fn now() -> u64 {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH);
    elapsed.map_or(0, |elapsed| elapsed.as_secs())
}

pub fn yt(cmd: &mut Command, cfg: &Value) -> R<Output> {
    let attempts = cfg["max_retries"].as_u64().unwrap_or(3) + 1;
    let mut announced = 0;
    for attempt in 1..=attempts {
        let until = BLOCKED_UNTIL.load(Relaxed);
        let wait = until.saturating_sub(now());
        if wait > 0 && announced != until {
            eprintln!("YouTube blocked downloads. Waiting {wait} s.");
            announced = until;
        }
        std::thread::sleep(Duration::from_secs(wait));
        let output = cmd.output()?;
        let error = String::from_utf8_lossy(&output.stderr);
        let signs = ["Sign in to confirm", "HTTP Error 429", "HTTP 429", "Too Many Requests"];
        if !signs.iter().any(|sign| error.contains(sign)) {
            STRIKES.store(0, Relaxed);
            return Ok(output);
        }
        if attempt == attempts {
            return Err(error.trim().to_string().into());
        }
        let strike = STRIKES.fetch_add(1, Relaxed).min(2) as usize;
        let test = std::env::var("SPOTDL_TEST_BACKOFF_SECONDS").ok().and_then(|value| value.parse().ok());
        BLOCKED_UNTIL.fetch_max(now() + test.unwrap_or([30, 60, 120][strike]), Relaxed);
    }
    Err("YouTube blocked downloads".into())
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

fn ytm(query: &str, songs: bool, isrc: bool, cfg: &Value) -> R<Vec<Hit>> {
    let params = if songs { "EgWKAQIIAWoMEAMQBBAJEA4QChAF" } else { "EgWKAQIQAWoMEA4QChADEAQQCRAF" };
    let client = json!({"clientName": "WEB_REMIX", "clientVersion": "1.20251001.01.00", "hl": "en", "gl": "US"});
    let body = json!({"context": {"client": client}, "query": query, "params": params}).to_string();
    let url = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
    let reply = http("POST", url, &[("Content-Type", "application/json")], Some(body.as_bytes()), cfg)?;
    let v: Value = serde_json::from_slice(&reply)?;
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

fn ytdlp(search: &str, cfg: &Value) -> R<Vec<Hit>> {
    let mut cmd = Command::new(cfg["yt_dlp"].as_str().unwrap_or("yt-dlp"));
    cmd.args(["--flat-playlist", "-J", "--no-warnings"]);
    deno_args(&mut cmd);
    let out = yt(cmd.arg(search), cfg)?;
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
            url: entry["webpage_url"].as_str().or(entry["url"].as_str())?.into(),
            name: entry["track"].as_str().or(entry["title"].as_str())?.into(),
            artists: vec![artist.unwrap_or_default().into()],
            album: entry["album"].as_str().unwrap_or_default().into(),
            duration: entry["duration"].as_f64().unwrap_or_default(),
            ..Hit::default()
        })
    };
    Ok(entries.iter().filter_map(hit).collect())
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

pub fn find(song: &Song, cfg: &Value) -> R<Vec<String>> {
    let query = crate::download::fill(cfg["search_query"].as_str().unwrap_or("{artists} - {title}"), song, cfg);
    let filter = cfg["filter_results"] != false;
    let verified = cfg["only_verified_results"] == true;
    let providers = cfg["audio_providers"].as_array().into_iter().flatten().filter_map(Value::as_str);
    let providers: Vec<&str> = providers.collect();
    let rank = |hit: &Hit| if filter { score(song, hit) } else { 100.0 };
    let mut found: Vec<(f64, String)> = Vec::new();
    if let Some(isrc) = song.isrc.as_ref().filter(|_| providers.contains(&"youtube-music")) {
        let hits = ytm(isrc, true, true, cfg).unwrap_or_default().into_iter();
        let hits = hits.filter(|hit| (hit.duration - song.duration).abs() < 10.0 && rank(hit) > 0.0);
        let best = hits.max_by(|a, b| rank(a).total_cmp(&rank(b)));
        found.extend(best.map(|hit| (f64::INFINITY, hit.url)));
    }
    for provider in &providers {
        let hits = match *provider {
            "youtube-music" => ytm(&query, true, false, cfg),
            "youtube" => ytdlp(&format!("ytsearch10:{query}"), cfg),
            _ => ytdlp(&format!("scsearch10:{query}"), cfg),
        };
        let mut hits = hits.unwrap_or_default();
        let best = hits.iter().map(rank).max_by(f64::total_cmp);
        if *provider == "youtube-music" && !verified && best.is_none_or(|best| best < 80.0) {
            hits.extend(ytm(&query, false, false, cfg).unwrap_or_default());
        }
        let hits = hits.into_iter().filter(|hit| !verified || hit.verified);
        found.extend(hits.map(|hit| (rank(&hit), hit.url)).filter(|candidate| candidate.0 > 0.0));
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut seen = HashSet::new();
    let found: Vec<String> =
        found.into_iter().map(|candidate| candidate.1).filter(|url| seen.insert(url.clone())).take(3).collect();
    if found.is_empty() {
        return Err(format!("no match for {query}").into());
    }
    Ok(found)
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

pub fn source(url: &str, cfg: &Value) -> R<Vec<Song>> {
    Ok(ytdlp(url, cfg)?.into_iter().map(from_hit).collect())
}

pub fn youtube(url: &str, cfg: &Value, enrich: bool) -> R<Vec<Song>> {
    let count = if url.contains("list=") && !url.contains("watch?") { usize::MAX } else { 1 };
    let music = url.contains("music.youtube.com");
    let song = |hit: Hit| {
        if !music {
            return Some(from_hit(hit));
        }
        let query = format!("{} - {}", hit.artists.join(", "), hit.name);
        let mut song = crate::spotify::resolve(&query, cfg, enrich).ok()?.into_iter().next()?;
        song.download_url = Some(hit.url.clone());
        if cfg["ytm_data"] == true {
            song.artist = hit.artists.first().cloned().unwrap_or_default();
            song.name = hit.name;
            song.artists = hit.artists;
            song.duration = hit.duration;
            song.album_name = hit.album;
        }
        Some(song)
    };
    Ok(ytdlp(url, cfg)?.into_iter().take(count).filter_map(song).collect())
}
