use crate::{home, Settings, Song, R};
use rayon::prelude::*;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::fs;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";
pub static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| ureq::Agent::config_builder().user_agent(UA).build().into());
static STATE: Mutex<Option<Value>> = Mutex::new(None);
static ALBUMS: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);
const PAGE: usize = 343;
fn totp(secret: &[u8]) -> R<String> {
    let key: Vec<u8> = secret
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ ((i % 33 + 9) as u8))
        .flat_map(|b| b.to_string().into_bytes())
        .collect();
    let pad = |x: u8| (0..64).map(|i| key.get(i).unwrap_or(&0) ^ x).collect::<Vec<u8>>();
    let inner = Sha1::new()
        .chain_update(pad(0x36))
        .chain_update((SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 30).to_be_bytes())
        .finalize();
    let d = Sha1::new().chain_update(pad(0x5c)).chain_update(inner).finalize();
    let o = (d[19] & 15) as usize;
    Ok(format!("{:06}", (u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) & 0x7fff_ffff) % 1_000_000))
}
fn hashes(ops: &[&str]) -> R<Value> {
    let text = |url: &str| -> R<String> {
        Ok(HTTP.get(url).call()?.body_mut().with_config().limit(1 << 26).read_to_string()?)
    };
    let html = text("https://open.spotify.com")?;
    let main =
        html.split('"').find(|s| s.contains("/web-player/web-player.") && s.ends_with(".js")).ok_or("no web-player")?;
    let js = text(main)?;
    let maps: Vec<HashMap<&str, &str>> = js
        .split('{')
        .filter_map(|m| {
            m.split_once('}').map(|x| x.0).filter(|m| m.len() > 200 && m.starts_with(|c: char| c.is_ascii_digit()))
        })
        .map(|m| m.split(',').filter_map(|kv| kv.split_once(':')).map(|(k, v)| (k, v.trim_matches('"'))).collect())
        .collect();
    let routes = maps.iter().flatten().filter(|(_, n)| n.contains("routes-search") || n.contains("routes-artist"));
    let chunks = routes.flat_map(|(k, n)| {
        maps.iter().filter_map(move |h| h.get(k).filter(|h| *h != n).map(|h| format!("{n}.{h}.js")))
    });
    let chunks: Vec<String> =
        chunks.filter_map(|c| text(&format!("https://open.spotifycdn.com/cdn/build/web-player/{c}")).ok()).collect();
    let all = chunks.concat() + &js;
    let found = ops.iter().filter_map(|op| {
        Some((op.to_string(), json!(all.split_once(&format!("\"{op}\",\"query\",\""))?.1.get(..64)?)))
    });
    Ok(Value::Object(found.collect()))
}
fn state(rejected: Option<&str>) -> R<Value> {
    let mut guard = STATE.lock().map_err(|e| e.to_string())?;
    let path = home().join("anon.json");
    let cached = guard.clone().or_else(|| serde_json::from_slice::<Value>(&fs::read(&path).ok()?).ok());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let fresh = |v: &Value| {
        v["exp"].as_u64() > Some(now + 60_000)
            && rejected.is_none_or(|t| v["at"].as_u64() > Some(now - 60_000) && v["token"].as_str() != Some(t))
    };
    if let Some(v) = cached.filter(fresh) {
        *guard = Some(v.clone());
        return Ok(v);
    }
    let secrets =
        HTTP.get("https://code.thetadev.de/ThetaDev/spotify-secrets/raw/branch/main/secrets/secretDict.json").call()?;
    let secrets: HashMap<String, Vec<u8>> = secrets.into_body().read_json()?;
    let (ver, secret) = secrets.iter().max_by_key(|(k, _)| k.parse::<u32>().unwrap_or(0)).ok_or("no TOTP secret")?;
    let code = totp(secret)?;
    let url = format!("https://open.spotify.com/api/token?reason=init&productType=web-player&totp={code}&totpVer={ver}&totpServer={code}");
    let tok: Value = HTTP.get(&url).call()?.body_mut().read_json()?;
    let v = json!({
        "token": tok["accessToken"].as_str().ok_or("Spotify token missing")?,
        "exp": tok["accessTokenExpirationTimestampMs"],
        "at": now,
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
    let cache = !cfg.no_cache && ["getAlbum", "getTrack"].contains(&op);
    let fresh = cache
        && fs::metadata(&path)
            .is_ok_and(|m| m.modified().is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() < 604_800)));
    if let Some(v) = fs::read(&path).ok().filter(|_| fresh).and_then(|b| serde_json::from_slice(&b).ok()) {
        return Ok(v);
    }
    let attempt = |refresh: bool, rejected: &mut Option<String>| -> R<Value> {
        let s = state(if refresh { rejected.as_deref() } else { None })?;
        *rejected = Some(s["token"].as_str().unwrap_or_default().into());
        let ext = json!({"persistedQuery": {"version": 1, "sha256Hash": s["hashes"][op].as_str().ok_or("missing query hash")?}});
        let v: Value = serde_json::from_slice(
            &HTTP
                .get("https://api-partner.spotify.com/pathfinder/v1/query")
                .query("operationName", op)
                .query("variables", vars.to_string())
                .query("extensions", ext.to_string())
                .header("Authorization", format!("Bearer {}", s["token"].as_str().unwrap_or_default()))
                .call()?
                .body_mut()
                .with_config()
                .limit(1 << 26)
                .read_to_vec()?,
        )?;
        v.get("data").map(|_| v.clone()).ok_or_else(|| format!("{op}: {}", v["errors"]).into())
    };
    let mut rejected = None;
    let v = (1..=cfg.max_retries).fold(attempt(false, &mut rejected), |r, n| {
        r.or_else(|e| {
            let auth = ["401", "hash", "PersistedQuery"].iter().any(|k| e.to_string().contains(k));
            std::thread::sleep(Duration::from_millis(if auth { 0 } else { 250 << n }));
            attempt(auth, &mut rejected)
        })
    })?;
    let _ =
        fs::create_dir_all(home().join("cache"))
            .and_then(|_| if cache { fs::write(&path, v.to_string()) } else { Ok(()) });
    Ok(v)
}
fn official(url: &str, cfg: &Settings) -> R<Vec<Value>> {
    let creds =
        [("grant_type", "client_credentials"), ("client_id", &cfg.client_id), ("client_secret", &cfg.client_secret)];
    let login = || -> R<String> {
        let v: Value = HTTP.post("https://accounts.spotify.com/api/token").send_form(creds)?.body_mut().read_json()?;
        Ok(v["access_token"].as_str().ok_or("official Spotify login failed")?.into())
    };
    let token = cfg.auth_token.clone().map_or_else(login, Ok)?;
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
    let items = |v: &Value| v.pointer(&format!("{list}/items")).and_then(Value::as_array).cloned().unwrap_or_default();
    let first = query(op, vars(uri, 0, limit), cfg)?;
    let total =
        first.pointer(&format!("{list}/totalCount")).and_then(Value::as_u64).ok_or(format!("{uri} unavailable"))?;
    let offsets: Vec<usize> = (limit..total as usize).step_by(limit).collect();
    let rest: Vec<Vec<Value>> =
        offsets.par_iter().map(|o| query(op, vars(uri, *o, limit), cfg).map(|v| items(&v))).collect::<R<_>>()?;
    let meta = first["data"].as_object().and_then(|d| d.values().next()).cloned().unwrap_or_default();
    Ok((meta, std::iter::once(items(&first)).chain(rest).flatten().collect()))
}
fn build(tracks: Vec<(Value, String)>, list: Option<(&str, &str)>, cfg: &Settings) -> R<Vec<Song>> {
    let mut uris: Vec<&String> = tracks.iter().map(|(_, a)| a).collect();
    uris.sort();
    uris.dedup();
    let albums: HashMap<&String, Value> = uris
        .par_iter()
        .filter_map(|u| {
            let keep = ["name", "artists", "date", "discs", "tracksV2", "label", "coverArt", "copyright", "type"];
            let fetch = || {
                let mut a = query("getAlbum", vars(u, 0, 1), cfg).ok()?["data"]["albumUnion"].take();
                a.as_object_mut()?.retain(|k, _| keep.contains(&k.as_str()));
                Some(a)
            };
            let known = ALBUMS.lock().ok()?.get(*u).cloned();
            Some((*u, known.or_else(fetch)?))
        })
        .collect();
    let len = tracks.len();
    Ok(tracks
        .iter()
        .enumerate()
        .filter(|(_, (t, _))| t["uri"].as_str().is_some_and(|u| u.starts_with("spotify:track:")))
        .map(|(i, (t, a))| {
            let al = albums.get(a).unwrap_or(&Value::Null);
            let name = |x: &Value| x["profile"]["name"].as_str().or(x["name"].as_str()).map(String::from);
            let names = |v: &Value| -> Vec<String> { v.as_array().into_iter().flatten().filter_map(name).collect() };
            let artists: Vec<String> =
                [&t["artists"]["items"], &t["firstArtist"]["items"], &t["otherArtists"]["items"], &t["artists"]]
                    .into_iter()
                    .flat_map(names)
                    .collect();
            let date = al["date"]["isoString"].as_str().unwrap_or_default().get(..10).unwrap_or_default().to_string();
            let id = |v: &Value| v.as_str().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
            let dur = |k: &str| t[k]["totalMilliseconds"].as_f64();
            let covers = al["coverArt"]["sources"].as_array().into_iter().flatten();
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
                duration: dur("duration").or(dur("trackDuration")).or(t["duration_ms"].as_f64()).unwrap_or_default()
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
                cover_url: covers.max_by_key(|x| x["width"].as_u64()).and_then(|x| x["url"].as_str().map(String::from)),
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
    let short = q.contains("spotify.link").then(|| HTTP.get(q).call()).transpose()?;
    let q = short.map_or(q.to_string(), |r| ureq::ResponseExt::get_uri(&r).to_string());
    let lib =
        |path: &str, key: &str| -> R<Vec<Value>> { Ok(official(path, cfg)?.iter().map(|x| x[key].clone()).collect()) };
    let uris = match q.as_str() {
        "saved" => {
            let tracks = lib("me/tracks?limit=50", "track")?.into_iter();
            return build(
                tracks.map(|t| (t.clone(), t["album"]["uri"].as_str().unwrap_or_default().into())).collect(),
                None,
                cfg,
            );
        }
        "all-user-playlists" | "all-saved-playlists" => official("me/playlists?limit=50", cfg)?,
        "all-user-saved-albums" => lib("me/albums?limit=50", "album")?,
        "all-user-followed-artists" => official("me/following?type=artist&limit=50", cfg)?,
        _ => vec![],
    };
    if !uris.is_empty() {
        return Ok(uris
            .par_iter()
            .filter_map(|u| u["uri"].as_str())
            .flat_map(|u| resolve(u, cfg).unwrap_or_default())
            .collect());
    }
    let (kind, term) = ["album:", "playlist:", "artist:"]
        .iter()
        .find_map(|k| q.strip_prefix(k).map(|t| (k.trim_end_matches(':'), t.trim())))
        .unwrap_or(("track", q.as_str()));
    let uri = match uri(&q) {
        Some(u) => u,
        None => {
            let vars = json!({"searchTerm": term, "offset": 0, "limit": 10, "numberOfTopResults": 5, "includeAudiobooks": false, "includeArtistHasConcertsField": false, "includePreReleases": false, "includeLocalConcertsField": false});
            let s = query("searchDesktop", vars, cfg)?["data"]["searchV2"].clone();
            let first =
                &s[format!("{kind}s{}", if kind == "track" || kind == "album" { "V2" } else { "" })]["items"][0];
            let name = first["item"]["data"]["name"].as_str().unwrap_or_default();
            if kind == "track" && crate::search::ratio(term, name) < 25.0 {
                return Err("no Spotify results".into());
            }
            first["item"]["data"]["uri"]
                .as_str()
                .or(first["data"]["uri"].as_str())
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
