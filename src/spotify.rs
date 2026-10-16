use crate::{home, Settings, Song, R};
use rayon::prelude::*;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::fs;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";

pub static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| ureq::Agent::config_builder().user_agent(UA).build().into());

static STATE: Mutex<Option<Value>> = Mutex::new(None);

static ALBUMS: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

static DEEZER: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

static DEEZER_RATE: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(Default::default);

const PAGE: usize = 343;

fn field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| value.get(*key).filter(|value| !value.is_null()))
}

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
        if v.get("data").is_some() {
            Ok(v)
        } else {
            Err(format!("{op}: {}", v["errors"]).into())
        }
    };
    let mut rejected = None;
    let auth_error = |result: &R<Value>| {
        result.as_ref().err().is_some_and(|e| {
            let message = e.to_string();
            ["401", "hash", "PersistedQuery"].iter().any(|key| message.contains(key))
        })
    };
    let refresh = std::cell::Cell::new(false);
    let v = crate::retry(
        cfg.max_retries.saturating_add(1),
        |_| Ok(None),
        |_| attempt(refresh.get(), &mut rejected),
        |attempt, result| {
            if result.is_err() {
                let auth = auth_error(result);
                refresh.set(auth);
                Some(if auth { Duration::ZERO } else { Duration::from_millis(250 << (attempt + 1)) })
            } else {
                None
            }
        },
    )?;
    let _ =
        fs::create_dir_all(home().join("cache"))
            .and_then(|_| if cache { fs::write(&path, v.to_string()) } else { Ok(()) });
    Ok(v)
}

fn deezer(url: &str, query: Option<(&str, &str)>) -> R<Value> {
    let mut reserved = None;
    let value = crate::retry(
        3,
        |attempt| {
            if reserved == Some(attempt) {
                return Ok(None);
            }
            reserved = Some(attempt);
            let mut next = DEEZER_RATE.lock().map_err(|e| e.to_string())?;
            let now = Instant::now();
            let wait = next.map_or(Duration::ZERO, |at| at.saturating_duration_since(now));
            *next = Some(now + wait + Duration::from_millis(100));
            Ok(Some(wait))
        },
        |_| -> R<Value> {
            let mut request = HTTP.get(url);
            if let Some((key, value)) = query {
                request = request.query(key, value);
            }
            Ok(request.call()?.body_mut().read_json()?)
        },
        |_, result| result.as_ref().is_ok_and(|value| value["error"]["code"] == 4).then_some(Duration::from_secs(5)),
    )?;
    if value["error"]["code"] == 4 {
        Err("Deezer rate limit".into())
    } else {
        Ok(value)
    }
}

fn high_match(left: &str, right: &str) -> bool {
    let left = crate::search::slug(left);
    let right = crate::search::slug(right);
    crate::search::ratio(&left, &right) > 80.0
}

fn deezer_album(uri: &str, album: &Value) -> Option<Value> {
    if let Some(v) = DEEZER.lock().ok()?.get(uri).cloned() {
        return Some(v);
    }
    let artist =
        album["artists"]["items"][0]["profile"]["name"].as_str().or(album["artists"]["items"][0]["name"].as_str())?;
    let title = field(album, &["name", "title"])?.as_str()?;
    let term = format!("{artist} {title}");
    let hits = deezer("https://api.deezer.com/search/album", Some(("q", &term))).ok()?;
    let hit = hits["data"].as_array()?.iter().find(|h| {
        high_match(title, field(h, &["title", "name"]).and_then(Value::as_str).unwrap_or_default())
            && high_match(
                artist,
                h["artist"]["profile"]["name"].as_str().or(h["artist"]["name"].as_str()).unwrap_or_default(),
            )
    })?;
    let id = hit["id"].as_u64()?;
    let v = deezer(&format!("https://api.deezer.com/album/{id}"), None).ok()?;
    DEEZER.lock().ok()?.insert(uri.into(), v.clone());
    Some(v)
}

fn deezer_track<'a>(album: &'a Value, track: &Value) -> Option<&'a Value> {
    let tracks = album["tracks"]["data"].as_array()?;
    let disc = field(track, &["discNumber", "disc_number"]).and_then(Value::as_u64).unwrap_or(1);
    let number = field(track, &["trackNumber", "track_number"]).and_then(Value::as_u64).unwrap_or(1);
    tracks
        .iter()
        .find(|t| {
            field(t, &["disk_number"]).and_then(Value::as_u64) == Some(disc)
                && field(t, &["track_position"]).and_then(Value::as_u64) == Some(number)
        })
        .or_else(|| {
            let title = field(track, &["name", "title"]).and_then(Value::as_str).unwrap_or_default();
            tracks
                .iter()
                .filter(|t| high_match(title, field(t, &["title", "name"]).and_then(Value::as_str).unwrap_or_default()))
                .max_by_key(|t| {
                    crate::search::ratio(
                        title,
                        field(t, &["title", "name"]).and_then(Value::as_str).unwrap_or_default(),
                    ) as u32
                })
        })
}

pub fn enrich(song: &mut Song) -> R<()> {
    if song.deezer_track_id.is_empty() {
        return Ok(());
    }
    let v = deezer(&format!("https://api.deezer.com/track/{}", song.deezer_track_id), None)?;
    let missing_isrc = song.isrc.as_ref().is_none_or(|isrc| isrc.is_empty());
    if missing_isrc {
        song.isrc = v["isrc"].as_str().filter(|isrc| !isrc.is_empty()).map(String::from);
    }
    song.bpm = v["bpm"].as_f64().unwrap_or_default();
    song.gain = v["gain"].as_f64().unwrap_or_default();
    Ok(())
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
        let result = page.get("artists").unwrap_or(&page);
        items.extend(result["items"].as_array().cloned().unwrap_or_default());
        next = result["next"].as_str().map(String::from);
    }
    Ok(items)
}

fn pages(op: &str, uri: &str, list: &str, limit: usize, cfg: &Settings) -> R<(Value, Vec<Value>)> {
    let variables = |offset| {
        json!({
            "uri": uri,
            "locale": "",
            "offset": offset,
            "limit": limit,
            "order": "DATE_DESC",
            "enableWatchFeedEntrypoint": false
        })
    };
    let items = |v: &Value| v.pointer(&format!("{list}/items")).and_then(Value::as_array).cloned().unwrap_or_default();
    let first = query(op, variables(0), cfg)?;
    let total =
        first.pointer(&format!("{list}/totalCount")).and_then(Value::as_u64).ok_or(format!("{uri} unavailable"))?;
    let offsets: Vec<usize> = (limit..total as usize).step_by(limit).collect();
    let rest: Vec<Vec<Value>> =
        offsets.par_iter().map(|o| query(op, variables(*o), cfg).map(|v| items(&v))).collect::<R<_>>()?;
    let meta = first["data"].as_object().and_then(|d| d.values().next()).cloned().unwrap_or_default();
    Ok((meta, std::iter::once(items(&first)).chain(rest).flatten().collect()))
}

fn build(tracks: Vec<(Value, String)>, list: Option<(&str, &str)>, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    let mut uris: Vec<&String> = tracks.iter().map(|(_, a)| a).collect();
    uris.sort();
    uris.dedup();
    let albums: HashMap<&String, (Value, Value)> = uris
        .par_iter()
        .filter_map(|u| {
            let keep = ["name", "artists", "date", "discs", "tracksV2", "label", "coverArt", "copyright", "type"];
            let cached = ALBUMS.lock().ok()?.get(*u).cloned();
            let album = cached.or_else(|| {
                let mut album = query(
                    "getAlbum",
                    json!({"uri": u, "locale": "", "offset": 0, "limit": 1, "order": "DATE_DESC", "enableWatchFeedEntrypoint": false}),
                    cfg,
                )
                .ok()?["data"]["albumUnion"]
                    .take();
                album.as_object_mut()?.retain(|key, _| keep.contains(&key.as_str()));
                ALBUMS.lock().ok()?.insert((*u).clone(), album.clone());
                Some(album)
            })?;
            let deezer = if enrich { deezer_album((*u).as_str(), &album).unwrap_or_default() } else { Value::Null };
            Some((*u, (album, deezer)))
        })
        .collect();
    let len = tracks.len();
    Ok(tracks
        .iter()
        .enumerate()
        .filter(|(_, (t, _))| t["uri"].as_str().is_some_and(|u| u.starts_with("spotify:track:")))
        .map(|(i, (t, a))| {
            let pair = albums.get(a);
            let al = pair.map_or(&Value::Null, |v| &v.0);
            let dz = pair.map_or(&Value::Null, |v| &v.1);
            let artists: Vec<String> =
                [&t["artists"]["items"], &t["firstArtist"]["items"], &t["otherArtists"]["items"], &t["artists"]]
                    .into_iter()
                    .flat_map(|value| value.as_array().into_iter().flatten())
                    .filter_map(|artist| {
                        artist["profile"]["name"].as_str().or(artist["name"].as_str()).map(String::from)
                    })
                    .collect();
            let date = al["date"]["isoString"].as_str().unwrap_or_default().get(..10).unwrap_or_default().to_string();
            let dur = |k: &str| t[k]["totalMilliseconds"].as_f64();
            let covers = al["coverArt"]["sources"].as_array().into_iter().flatten();
            let dz_track = deezer_track(dz, t);
            let song_id = t["uri"].as_str().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
            let url = format!("https://open.spotify.com/track/{song_id}");
            let artist_uri = al["artists"]["items"][0]["uri"].as_str().unwrap_or_default();
            Song {
                name: field(t, &["name", "title"]).and_then(Value::as_str).unwrap_or_default().into(),
                artist: artists.first().cloned().unwrap_or_default(),
                album_artist: al["artists"]["items"][0]["profile"]["name"]
                    .as_str()
                    .or(al["artists"]["items"][0]["name"].as_str())
                    .unwrap_or_default()
                    .into(),
                artist_id: artist_uri.rsplit(':').next().unwrap_or_default().into(),
                artists,
                disc_number: field(t, &["discNumber", "disc_number"]).and_then(Value::as_u64).unwrap_or(1) as u32,
                disc_count: field(&al["discs"], &["totalCount"]).and_then(Value::as_u64).unwrap_or(1) as u32,
                album_name: field(al, &["name", "title"]).and_then(Value::as_str).unwrap_or_default().into(),
                duration: dur("duration").or(dur("trackDuration")).or(t["duration_ms"].as_f64()).unwrap_or_default()
                    / 1000.0,
                year: date.get(..4).and_then(|y| y.parse().ok()).unwrap_or_default(),
                date,
                track_number: field(t, &["trackNumber", "track_number"]).and_then(Value::as_u64).unwrap_or(1) as u32,
                tracks_count: field(&al["tracksV2"], &["totalCount"])
                    .or_else(|| field(&al["tracks"], &["totalCount"]))
                    .and_then(Value::as_u64)
                    .unwrap_or(1) as u32,
                song_id,
                url,
                explicit: t["contentRating"]["label"] == "EXPLICIT" || t["explicit"] == true,
                publisher: dz["label"].as_str().or(al["label"].as_str()).unwrap_or_default().into(),
                isrc: t["external_ids"]["isrc"].as_str().map(String::from),
                cover_url: covers.max_by_key(|x| x["width"].as_u64()).and_then(|x| x["url"].as_str().map(String::from)),
                copyright_text: al["copyright"]["items"][0]["text"].as_str().map(String::from),
                popularity: t["playcount"].as_str().and_then(|p| p.parse().ok()),
                album_id: a.rsplit(':').next().unwrap_or_default().into(),
                album_type: field(al, &["type"]).and_then(Value::as_str).unwrap_or_default().to_lowercase(),
                genres: dz["genres"]["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|g| field(g, &["name"]).and_then(Value::as_str).map(String::from))
                    .collect(),
                original_date: dz["release_date"].as_str().unwrap_or_default().into(),
                upc: dz["upc"].as_str().unwrap_or_default().into(),
                deezer_track_id: dz_track
                    .and_then(|d| field(d, &["id"]).and_then(Value::as_u64))
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
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
    Ok(items.into_iter().map(|mut i| (i["track"].take(), uri.to_string())).collect())
}

fn artist(uri: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    let (_, releases) = pages("queryArtistDiscographyAll", uri, "/data/artistUnion/discography/all", 50, cfg)?;
    let albums: Vec<&str> = releases.iter().filter_map(|x| x["releases"]["items"][0]["uri"].as_str()).collect();
    let tracks: Vec<(Value, String)> = albums.par_iter().flat_map(|a| album(a, cfg).unwrap_or_default()).collect();
    let mut seen = std::collections::HashSet::new();
    let songs = build(tracks, None, cfg, enrich)?.into_iter();
    Ok(songs.filter(|s| seen.insert((s.name.to_lowercase(), s.artists.clone()))).collect())
}

fn uri(query: &str) -> Option<String> {
    let q = query.split('?').next()?.trim_end_matches('/');
    ["track", "album", "playlist", "artist"].iter().find_map(|k| {
        let id = q.split_once(&format!("/{k}/")).or(q.split_once(&format!(":{k}:")))?.1;
        Some(format!("spotify:{k}:{}", id.split('/').next()?))
    })
}

pub fn resolve(q: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    if q.ends_with(".spotdl") {
        let v: Value = serde_json::from_slice(&fs::read(q)?)?;
        return Ok(serde_json::from_value(v.get("songs").cloned().unwrap_or(v))?);
    }
    if let Some((yt, sp)) = q.split_once('|') {
        return Ok(resolve(sp, cfg, enrich)?
            .into_iter()
            .map(|s| Song { download_url: Some(yt.into()), ..s })
            .collect());
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
                tracks
                    .map(|track| {
                        let album = track["album"]["uri"].as_str().unwrap_or_default().into();
                        (track, album)
                    })
                    .collect(),
                None,
                cfg,
                enrich,
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
            .flat_map(|u| resolve(u, cfg, enrich).unwrap_or_default())
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
            let mut response = query("searchDesktop", vars, cfg)?;
            let s = response["data"]["searchV2"].take();
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
            let mut response = query("getTrack", json!({"uri": uri}), cfg)?;
            let t = response["data"]["trackUnion"].take();
            let a = t["albumOfTrack"]["uri"].as_str().unwrap_or_default().to_string();
            build(vec![(t, a)], None, cfg, enrich)
        }
        "album" => build(album(&uri, cfg)?, None, cfg, enrich),
        "artist" => artist(&uri, cfg, enrich),
        _ => {
            let (p, items) = pages("fetchPlaylist", &uri, "/data/playlistV2/content", PAGE, cfg)?;
            let tracks = items.into_iter().map(|mut i| i["itemV2"]["data"].take());
            let tracks = tracks
                .map(|track| {
                    let album = track["albumOfTrack"]["uri"].as_str().unwrap_or_default().to_string();
                    (track, album)
                })
                .collect();
            let url = format!("https://open.spotify.com/playlist/{}", uri.rsplit(':').next().unwrap_or_default());
            build(tracks, Some((p["name"].as_str().unwrap_or_default(), &url)), cfg, enrich)
        }
    }
}
