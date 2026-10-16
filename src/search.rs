use crate::{spotify::HTTP, Settings, Song, R};
use serde_json::{json, Value};
use std::process::Command;

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
    let shelves = tab["sectionListRenderer"]["contents"].as_array().cloned().unwrap_or_default();
    let items =
        shelves.into_iter().flat_map(|s| s["musicShelfRenderer"]["contents"].as_array().cloned().unwrap_or_default());
    Ok(items
        .filter_map(|i| {
            let r = &i["musicResponsiveListItemRenderer"];
            let runs =
                |c: usize| r["flexColumns"][c]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"].clone();
            let meta = runs(1).as_array().cloned().unwrap_or_default();
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
                name: runs(0)[0]["text"].as_str()?.into(),
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
    let out = Command::new(&cfg.yt_dlp).args(["--flat-playlist", "-J", "--no-warnings", search]).output()?;
    let v: Value = serde_json::from_slice(&out.stdout)?;
    let entries = v["entries"].as_array().cloned().unwrap_or_else(|| vec![v.clone()]);
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
    let stripped = s.artists.iter().fold(rname.clone(), |n, a| n.replace(&slug(a), ""));
    let name = ratio(&sname, &rname).max(ratio(&sname, stripped.trim_matches('-'))) - 15.0 * fwords;
    let time = (-0.1 * (s.duration - h.duration).abs()).exp() * 100.0;
    let album = if h.album.is_empty() { 0.0 } else { ratio(&slug(&s.album_name), &slug(&h.album)) };
    let avg = (artists + name) / 2.0;
    let avg = if h.verified && !h.isrc && !h.album.is_empty() && album <= 80.0 { (avg + album) / 2.0 } else { avg };
    let skip = name <= 60.0 || artists < 70.0 || time < 25.0 || (time < 50.0 && avg < 75.0);
    let avg = if !h.isrc && avg <= 85.0 { (avg + time) / 2.0 } else { avg };
    avg.min(100.0) * f64::from(u8::from(!skip))
}

pub fn find(s: &Song, cfg: &Settings) -> R<String> {
    let q = crate::download::fill(cfg.search_query.as_deref().unwrap_or("{artists} - {title}"), s, cfg);
    let best = |hits: Vec<Hit>| -> Option<(f64, String)> {
        let first = hits.first().filter(|_| !cfg.filter_results).map(|h| (100.0, h.url.clone()));
        first.or_else(|| {
            hits.iter().map(|h| (score(s, h), h.url.clone())).filter(|x| x.0 > 0.0).max_by(|a, b| a.0.total_cmp(&b.0))
        })
    };
    let isrc = s.isrc.as_ref().filter(|_| cfg.audio_providers.iter().any(|p| p == "youtube-music"));
    let close = |h: Vec<Hit>| h.into_iter().filter(|h| (h.duration - s.duration).abs() < 10.0).take(1).collect();
    let by_isrc = isrc.and_then(|i| ytm(i, true, true).ok()).and_then(|h| best(close(h)));
    let search = |p: &String| -> Option<(f64, String)> {
        let top = match p.as_str() {
            "youtube-music" => ytm(&q, true, false),
            "youtube" => ytdlp(&format!("ytsearch10:{q}"), cfg),
            _ => ytdlp(&format!("scsearch10:{q}"), cfg),
        };
        let top = top.ok().and_then(best);
        let more = p == "youtube-music" && !cfg.only_verified_results && top.as_ref().is_none_or(|t| t.0 < 80.0);
        let videos = more.then(|| ytm(&q, false, false).ok().and_then(best)).flatten();
        top.into_iter().chain(videos).max_by(|a, b| a.0.total_cmp(&b.0))
    };
    let found = by_isrc.or_else(|| cfg.audio_providers.iter().find_map(search));
    found.map(|b| b.1).ok_or_else(|| format!("no match for {q}").into())
}

pub fn youtube(url: &str, cfg: &Settings) -> R<Vec<Song>> {
    let n = if url.contains("list=") && !url.contains("watch?") { usize::MAX } else { 1 };
    let hits = ytdlp(url, cfg)?.into_iter().take(n);
    Ok(hits
        .map(|h| Song {
            name: h.name,
            artist: h.artists[0].clone(),
            artists: h.artists,
            album_name: h.album,
            duration: h.duration,
            song_id: h.url.rsplit('=').next().unwrap_or_default().into(),
            url: h.url.clone(),
            download_url: Some(h.url),
            ..Song::default()
        })
        .collect())
}
