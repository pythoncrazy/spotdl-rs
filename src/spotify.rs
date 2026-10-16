use crate::{home, Settings, Song, R};
use rayon::prelude::*;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    fs,
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

pub static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36")
        .build()
        .into()
});
static STATE: Mutex<Option<Value>> = Mutex::new(None);
static ALBUMS: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);
const PAGE: usize = 343;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn text(url: &str) -> R<String> {
    Ok(HTTP.get(url).call()?.body_mut().with_config().limit(1 << 26).read_to_string()?)
}

fn totp(secret: &[u8]) -> String {
    let key: Vec<u8> = secret
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ ((i % 33 + 9) as u8))
        .flat_map(|b| b.to_string().into_bytes())
        .collect();
    let pad = |x: u8| (0..64).map(|i| key.get(i).unwrap_or(&0) ^ x).collect::<Vec<u8>>();
    let inner = Sha1::new().chain_update(pad(0x36)).chain_update((now() / 30).to_be_bytes()).finalize();
    let d = Sha1::new().chain_update(pad(0x5c)).chain_update(inner).finalize();
    let o = (d[19] & 15) as usize;
    format!("{:06}", (u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) & 0x7fff_ffff) % 1_000_000)
}

fn hashes(ops: &[&str]) -> R<Value> {
    let html = text("https://open.spotify.com")?;
    let main = html
        .split('"')
        .find(|s| s.contains("/web-player/web-player.") && s.ends_with(".js"))
        .ok_or("Spotify web-player bundle not found")?;
    let js = text(main)?;
    let maps: Vec<HashMap<String, String>> = js
        .match_indices("{")
        .filter_map(|(i, _)| js[i..].find('}').map(|j| &js[i..=i + j]))
        .filter(|m| m.len() > 200 && m[1..].starts_with(|c: char| c.is_ascii_digit()) && m.contains(":\""))
        .map(|m| {
            m[1..m.len() - 1]
                .split(',')
                .filter_map(|kv| kv.split_once(':'))
                .map(|(k, v)| (k.into(), v.trim_matches('"').into()))
                .collect()
        })
        .collect();
    let chunks: Vec<String> = maps
        .iter()
        .flat_map(|names| names.iter().filter(|(_, n)| n.contains("routes-search") || n.contains("routes-artist")))
        .flat_map(|(k, n)| maps.iter().filter_map(move |h| h.get(k).filter(|h| *h != n).map(|h| format!("{n}.{h}.js"))))
        .filter_map(|c| text(&format!("https://open.spotifycdn.com/cdn/build/web-player/{c}")).ok())
        .collect();
    let all = std::iter::once(js).chain(chunks).collect::<String>();
    Ok(ops
        .iter()
        .filter_map(|op| {
            let rest = all.split_once(&format!("\"{op}\",\"query\",\""))?.1;
            Some((op.to_string(), json!(rest.get(..64)?)))
        })
        .collect::<serde_json::Map<_, _>>()
        .into())
}

fn state(refresh: bool) -> R<Value> {
    let mut guard = STATE.lock().map_err(|e| e.to_string())?;
    let path = home().join("anon.json");
    let cached = guard.clone().or_else(|| serde_json::from_slice::<Value>(&fs::read(&path).ok()?).ok());
    if let Some(v) = cached.filter(|v| !refresh && v["exp"].as_u64().unwrap_or(0) > now() * 1000 + 60_000) {
        *guard = Some(v.clone());
        return Ok(v);
    }
    let secrets: HashMap<String, Vec<u8>> = serde_json::from_str(&text(
        "https://code.thetadev.de/ThetaDev/spotify-secrets/raw/branch/main/secrets/secretDict.json",
    )?)?;
    let (ver, secret) = secrets.iter().max_by_key(|(k, _)| k.parse::<u32>().unwrap_or(0)).ok_or("no TOTP secret")?;
    let code = totp(secret);
    let url = format!("https://open.spotify.com/api/token?reason=init&productType=web-player&totp={code}&totpVer={ver}&totpServer={code}");
    let tok: Value = HTTP.get(&url).call()?.body_mut().read_json()?;
    let v = json!({
        "token": tok["accessToken"].as_str().ok_or("Spotify token missing")?,
        "exp": tok["accessTokenExpirationTimestampMs"],
        "hashes": hashes(&["getTrack", "getAlbum", "fetchPlaylist", "queryArtistDiscographyAll", "searchDesktop"])?,
    });
    fs::create_dir_all(home())?;
    fs::write(path, v.to_string())?;
    *guard = Some(v.clone());
    Ok(v)
}

fn query(op: &str, vars: Value, cfg: &Settings) -> R<Value> {
    let key = format!("{:x}", Sha1::digest(format!("{op}{vars}")));
    let path = home().join("cache").join(&key[..16]);
    if let Some(v) = fs::read(&path)
        .ok()
        .filter(|_| !cfg.no_cache && op != "searchDesktop")
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        return Ok(v);
    }
    let attempt = |refresh: bool| -> R<Value> {
        let s = state(refresh)?;
        let ext = json!({"persistedQuery": {"version": 1, "sha256Hash": s["hashes"][op].as_str().ok_or("missing query hash")?}});
        let v: Value = HTTP
            .get("https://api-partner.spotify.com/pathfinder/v1/query")
            .query("operationName", op)
            .query("variables", vars.to_string())
            .query("extensions", ext.to_string())
            .header("Authorization", format!("Bearer {}", s["token"].as_str().unwrap_or_default()))
            .call()?
            .body_mut()
            .with_config()
            .limit(1 << 26)
            .read_json()?;
        v.get("data").map(|_| v.clone()).ok_or_else(|| format!("{op}: {}", v["errors"]).into())
    };
    let v = attempt(false).or_else(|_| attempt(true))?;
    fs::create_dir_all(path.parent().unwrap_or(&path))?;
    fs::write(&path, v.to_string())?;
    Ok(v)
}

fn official(url: &str, cfg: &Settings) -> R<Vec<Value>> {
    let token = match &cfg.auth_token {
        Some(t) => t.clone(),
        None => HTTP
            .post("https://accounts.spotify.com/api/token")
            .send_form([
                ("grant_type", "client_credentials"),
                ("client_id", &cfg.client_id),
                ("client_secret", &cfg.client_secret),
            ])?
            .body_mut()
            .read_json::<Value>()?["access_token"]
            .as_str()
            .ok_or("official Spotify login failed")?
            .to_string(),
    };
    let mut next = Some(format!("https://api.spotify.com/v1/{url}"));
    let mut items = Vec::new();
    while let Some(u) = next {
        let page: Value =
            HTTP.get(&u).header("Authorization", format!("Bearer {token}")).call()?.body_mut().read_json()?;
        let page = page.get("artists").cloned().unwrap_or(page);
        items.extend(page["items"].as_array().cloned().unwrap_or_default());
        next = page["next"].as_str().map(String::from);
    }
    Ok(items)
}

fn vars(uri: &str, offset: usize, limit: usize) -> Value {
    json!({"uri": uri, "locale": "", "offset": offset, "limit": limit, "order": "DATE_DESC", "enableWatchFeedEntrypoint": false})
}

fn pages(op: &str, uri: &str, list: &str, limit: usize, cfg: &Settings) -> R<(Value, Vec<Value>)> {
    let first = query(op, vars(uri, 0, limit), cfg)?;
    let total = first
        .pointer(&format!("{list}/totalCount"))
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{uri} unavailable"))?;
    let rest: Vec<Value> = (limit..total as usize)
        .step_by(limit)
        .collect::<Vec<_>>()
        .par_iter()
        .map(|o| {
            query(op, vars(uri, *o, limit), cfg)
                .map(|v| v.pointer(&format!("{list}/items")).cloned().unwrap_or_default())
        })
        .collect::<R<_>>()?;
    let items = std::iter::once(first.pointer(&format!("{list}/items")).cloned().unwrap_or_default()).chain(rest);
    Ok((
        first["data"].as_object().and_then(|d| d.values().next()).cloned().unwrap_or_default(),
        items.flat_map(|v| v.as_array().cloned().unwrap_or_default()).collect(),
    ))
}

fn build(tracks: Vec<(Value, String)>, list: Option<(&str, &str)>, cfg: &Settings) -> R<Vec<Song>> {
    let mut uris: Vec<&String> = tracks.iter().map(|(_, a)| a).collect();
    uris.sort();
    uris.dedup();
    let albums: HashMap<&String, Value> = uris
        .par_iter()
        .filter_map(|u| {
            let known = ALBUMS.lock().ok()?.get(*u).cloned();
            Some((
                *u,
                known.or_else(|| Some(query("getAlbum", vars(u, 0, 1), cfg).ok()?["data"]["albumUnion"].clone()))?,
            ))
        })
        .collect();
    let len = tracks.len();
    Ok(tracks
        .iter()
        .enumerate()
        .filter(|(_, (t, _))| t["uri"].as_str().is_some_and(|u| u.starts_with("spotify:track:")))
        .map(|(i, (t, a))| {
            let al = albums.get(a).unwrap_or(&Value::Null);
            let names = |v: &Value| -> Vec<String> {
                v.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x["profile"]["name"].as_str().or(x["name"].as_str()).map(String::from))
                    .collect()
            };
            let artists: Vec<String> =
                [&t["artists"]["items"], &t["firstArtist"]["items"], &t["otherArtists"]["items"], &t["artists"]]
                    .into_iter()
                    .flat_map(names)
                    .collect();
            let date = al["date"]["isoString"].as_str().unwrap_or_default().get(..10).unwrap_or_default().to_string();
            let id = |v: &Value| v.as_str().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
            let num = |a: &Value, b: &Value| a.as_u64().or(b.as_u64()).unwrap_or(1) as u32;
            Song {
                name: t["name"].as_str().unwrap_or_default().into(),
                artist: artists.first().cloned().unwrap_or_default(),
                album_artist: names(&al["artists"]["items"]).first().cloned().unwrap_or_default(),
                artist_id: id(&al["artists"]["items"][0]["uri"]),
                artists,
                disc_number: num(&t["discNumber"], &t["disc_number"]),
                disc_count: num(&al["discs"]["totalCount"], &Value::Null),
                album_name: al["name"].as_str().unwrap_or_default().into(),
                duration: (t["duration"]["totalMilliseconds"].as_f64())
                    .or(t["trackDuration"]["totalMilliseconds"].as_f64())
                    .or(t["duration_ms"].as_f64())
                    .unwrap_or_default()
                    / 1000.0,
                year: date.get(..4).and_then(|y| y.parse().ok()).unwrap_or_default(),
                date,
                track_number: num(&t["trackNumber"], &t["track_number"]),
                tracks_count: num(&al["tracksV2"]["totalCount"], &Value::Null),
                song_id: id(&t["uri"]),
                url: format!("https://open.spotify.com/track/{}", id(&t["uri"])),
                explicit: t["contentRating"]["label"] == "EXPLICIT" || t["explicit"] == true,
                publisher: al["label"].as_str().unwrap_or_default().into(),
                isrc: t["external_ids"]["isrc"].as_str().map(String::from),
                cover_url: al["coverArt"]["sources"]
                    .as_array()
                    .and_then(|s| s.iter().max_by_key(|x| x["width"].as_u64()))
                    .and_then(|x| x["url"].as_str().map(String::from)),
                copyright_text: al["copyright"]["items"][0]["text"].as_str().map(String::from),
                popularity: t["playcount"].as_str().and_then(|p| p.parse().ok()),
                album_id: id(&Value::String(a.clone())),
                album_type: al["type"].as_str().unwrap_or_default().to_lowercase(),
                list_name: list.map(|l| l.0.into()),
                list_url: list.map(|l| l.1.into()),
                list_position: list.map(|_| i + 1),
                list_length: list.map(|_| len),
                ..Song::default()
            }
        })
        .collect())
}

fn album(uri: &str, cfg: &Settings) -> R<Vec<(Value, String)>> {
    let (mut meta, items) = pages("getAlbum", uri, "/data/albumUnion/tracksV2", PAGE, cfg)?;
    meta["tracksV2"]["items"] = Value::Null;
    ALBUMS.lock().map_err(|e| e.to_string())?.insert(uri.into(), meta);
    Ok(items.into_iter().map(|i| (i["track"].clone(), uri.to_string())).collect())
}

fn artist(uri: &str, cfg: &Settings) -> R<Vec<Song>> {
    let (_, releases) = pages("queryArtistDiscographyAll", uri, "/data/artistUnion/discography/all", 50, cfg)?;
    let albums: Vec<&str> = releases.iter().filter_map(|x| x["releases"]["items"][0]["uri"].as_str()).collect();
    let tracks: Vec<(Value, String)> = albums.par_iter().flat_map(|a| album(a, cfg).unwrap_or_default()).collect();
    let mut seen = std::collections::HashSet::new();
    let songs = build(tracks, None, cfg)?.into_iter();
    Ok(songs.filter(|s| seen.insert((s.name.to_lowercase(), s.artists.clone()))).collect())
}

fn uri(query: &str) -> Option<String> {
    let q = query.split('?').next()?.trim_end_matches('/');
    ["track", "album", "playlist", "artist"].iter().find_map(|k| {
        let id = q.split_once(&format!("/{k}/")).or(q.split_once(&format!(":{k}:")))?.1;
        Some(format!("spotify:{k}:{}", id.split('/').next()?))
    })
}

pub fn resolve(q: &str, cfg: &Settings) -> R<Vec<Song>> {
    if q.ends_with(".spotdl") {
        let v: Value = serde_json::from_slice(&fs::read(q)?)?;
        return Ok(serde_json::from_value(v.get("songs").cloned().unwrap_or(v))?);
    }
    if let Some((yt, sp)) = q.split_once('|') {
        return Ok(resolve(sp, cfg)?.into_iter().map(|s| Song { download_url: Some(yt.into()), ..s }).collect());
    }
    if q.contains("youtube.com/") || q.contains("youtu.be/") {
        return crate::search::youtube(q, cfg);
    }
    let q = if q.contains("spotify.link") {
        ureq::ResponseExt::get_uri(&HTTP.get(q).call()?).to_string()
    } else {
        q.to_string()
    };
    let lib = |path: &str, key: &str| -> R<Vec<String>> {
        Ok(official(path, cfg)?
            .iter()
            .filter_map(|x| x[key]["uri"].as_str().or(x["uri"].as_str()).map(String::from))
            .collect())
    };
    let uris = match q.as_str() {
        "saved" => {
            let tracks = official("me/tracks?limit=50", cfg)?;
            return build(
                tracks
                    .into_iter()
                    .map(|t| (t["track"].clone(), t["track"]["album"]["uri"].as_str().unwrap_or_default().into()))
                    .collect(),
                None,
                cfg,
            );
        }
        "all-user-playlists" | "all-saved-playlists" => lib("me/playlists?limit=50", "")?,
        "all-user-saved-albums" => lib("me/albums?limit=50", "album")?,
        "all-user-followed-artists" => lib("me/following?type=artist&limit=50", "")?,
        _ => vec![],
    };
    if !uris.is_empty() {
        return Ok(uris.par_iter().flat_map(|u| resolve(u, cfg).unwrap_or_default()).collect());
    }
    let (kind, term) = ["album:", "playlist:", "artist:"]
        .iter()
        .find_map(|k| q.strip_prefix(k).map(|t| (k.trim_end_matches(':'), t.trim())))
        .unwrap_or(("track", q.as_str()));
    let uri = match uri(&q) {
        Some(u) => u,
        None => {
            let vars = json!({"searchTerm": term, "offset": 0, "limit": 10, "numberOfTopResults": 5, "includeAudiobooks": false, "includeArtistHasConcertsField": false, "includePreReleases": false, "includeLocalConcertsField": false});
            let s = query("searchDesktop", vars, cfg)?;
            let items = &s["data"]["searchV2"]
                [format!("{kind}s{}", if kind == "track" || kind == "album" { "V2" } else { "" })]["items"];
            let first = &items[0];
            [&first["item"]["data"]["uri"], &first["data"]["uri"]]
                .iter()
                .find_map(|u| u.as_str())
                .ok_or("no Spotify results")?
                .to_string()
        }
    };
    let k = uri.split(':').nth(1).unwrap_or_default();
    match k {
        "track" => {
            let t = query("getTrack", json!({"uri": uri}), cfg)?["data"]["trackUnion"].clone();
            let a = t["albumOfTrack"]["uri"].as_str().unwrap_or_default().to_string();
            build(vec![(t, a)], None, cfg)
        }
        "album" => build(album(&uri, cfg)?, None, cfg),
        "artist" => artist(&uri, cfg),
        _ => {
            let (p, items) = pages("fetchPlaylist", &uri, "/data/playlistV2/content", PAGE, cfg)?;
            let tracks = items.into_iter().map(|i| i["itemV2"]["data"].clone());
            let tracks = tracks
                .map(|t| (t.clone(), t["albumOfTrack"]["uri"].as_str().unwrap_or_default().to_string()))
                .collect();
            let url = format!("https://open.spotify.com/playlist/{}", uri.rsplit(':').next().unwrap_or_default());
            build(tracks, Some((p["name"].as_str().unwrap_or_default(), &url)), cfg)
        }
    }
}
