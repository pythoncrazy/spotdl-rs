use crate::{home, Settings, Song, R};
use rayon::prelude::*;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";

pub static HTTP: LazyLock<ureq::Agent> = LazyLock::new(|| ureq::Agent::config_builder().user_agent(UA).build().into());

static STATE: Mutex<Option<Value>> = Mutex::new(None);

static ALBUMS: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

static DEEZER: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

static DEEZER_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);

static DEEZER_RATE: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(Default::default);

static TOKEN: LazyLock<Mutex<Option<(String, Instant)>>> = LazyLock::new(Default::default);

static CACHE_FILE_LOCK: Mutex<()> = Mutex::new(());

const PAGE: usize = 343;

struct PlaylistInfo<'a> {
    name: &'a str,
    url: &'a str,
    author: Option<&'a str>,
    cover: Option<&'a str>,
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

fn state(cfg: &Settings, rejected: Option<&str>) -> R<Value> {
    let mut guard = STATE.lock().map_err(|e| e.to_string())?;
    let path = home().join("anon.json");
    let cached = guard
        .clone()
        .or_else(|| (!cfg.no_cache).then(|| serde_json::from_slice::<Value>(&fs::read(&path).ok()?).ok()).flatten());
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
    if !cfg.no_cache {
        fs::create_dir_all(home())?;
        fs::write(path, v.to_string())?;
    }
    *guard = Some(v.clone());
    Ok(v)
}

fn read_persistent(key: &str, cfg: &Settings) -> Option<Value> {
    if cfg.no_cache || !cfg.use_cache_file {
        return None;
    }
    let _lock = CACHE_FILE_LOCK.lock().ok()?;
    fs::read(home().join(".spotify_cache"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|values| values.get(key).cloned())
}

fn write_persistent(key: &str, value: &Value, cfg: &Settings) -> R<()> {
    if cfg.no_cache || !cfg.use_cache_file {
        return Ok(());
    }
    let _lock = CACHE_FILE_LOCK.lock().map_err(|error| error.to_string())?;
    fs::create_dir_all(home())?;
    let path = home().join(".spotify_cache");
    let mut values = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    values[key] = value.clone();
    fs::write(path, serde_json::to_vec(&values)?)?;
    Ok(())
}

fn query(op: &str, vars: Value, cfg: &Settings) -> R<Value> {
    let key = format!("{:x}", Sha1::digest(format!("{op}{vars}")));
    let path = home().join("cache").join(&key[..16]);
    let cache = !cfg.no_cache && ["getAlbum", "getTrack"].contains(&op);
    let persistent_key = format!("pathfinder:{key}");
    if op == "getTrack" {
        if let Some(value) = read_persistent(&persistent_key, cfg) {
            return Ok(value);
        }
    }
    let fresh = cache
        && fs::metadata(&path)
            .is_ok_and(|m| m.modified().is_ok_and(|t| t.elapsed().is_ok_and(|age| age.as_secs() < 604_800)));
    if let Some(v) = fs::read(&path).ok().filter(|_| fresh).and_then(|b| serde_json::from_slice(&b).ok()) {
        return Ok(v);
    }
    let mut rejected: Option<String> = None;
    let refresh = std::cell::Cell::new(false);
    let v = crate::retry(
        cfg.max_retries.saturating_add(1),
        |_| {
            let s = state(cfg, if refresh.get() { rejected.as_deref() } else { None })?;
            rejected = Some(s["token"].as_str().unwrap_or_default().into());
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
        },
        |error| {
            let message = error.to_string();
            let auth = ["401", "hash", "PersistedQuery"].iter().any(|key| message.contains(key));
            refresh.set(auth);
            Some(if auth { Duration::ZERO } else { Duration::from_millis(250) })
        },
    )?;
    if op == "getTrack" {
        write_persistent(&persistent_key, &v, cfg)?;
    }
    let _ =
        fs::create_dir_all(home().join("cache"))
            .and_then(|_| if cache { fs::write(&path, v.to_string()) } else { Ok(()) });
    Ok(v)
}

fn deezer_album(uri: &str, album: &Value, cfg: &Settings) -> Option<Value> {
    let lock = DEEZER_LOCKS.lock().ok()?.entry(uri.to_string()).or_insert_with(|| Arc::new(Mutex::new(()))).clone();
    let _guard = lock.lock().ok()?;
    if let Some(cached) = DEEZER.lock().ok()?.get(uri).cloned() {
        return Some(cached);
    }
    let fetch = |url: &str, query: Option<(&str, &str)>| -> R<Value> {
        crate::retry(
            cfg.max_retries.saturating_add(1),
            |_| {
                let mut next = DEEZER_RATE.lock().map_err(|error| error.to_string())?;
                let now = Instant::now();
                let wait = next.map_or(Duration::ZERO, |at| at.saturating_duration_since(now));
                *next = Some(now + wait + Duration::from_millis(100));
                drop(next);
                std::thread::sleep(wait);
                let request = query.map_or_else(|| HTTP.get(url), |(key, value)| HTTP.get(url).query(key, value));
                let mut response = match request.call() {
                    Ok(response) => response,
                    Err(error) if error.to_string().contains("429") => return Err("Deezer rate limit".into()),
                    Err(error) => return Err(error.into()),
                };
                let response: Value = response.body_mut().read_json()?;
                if response["error"]["code"] == 4 {
                    Err("Deezer rate limit".into())
                } else {
                    Ok(response)
                }
            },
            |error| (error.to_string() == "Deezer rate limit").then_some(Duration::from_secs(5)),
        )
    };
    let artist = album["artists"]["items"][0]["profile"]["name"].as_str()?;
    let title = album["name"].as_str()?;
    let hits = fetch("https://api.deezer.com/search/album", Some(("q", &format!("{artist} {title}")))).ok()?;
    let matches = |expected: &str, found: &str| {
        crate::search::ratio(&crate::search::slug(expected), &crate::search::slug(found)) > 80.0
    };
    let hit = hits["data"].as_array()?.iter().find(|hit| {
        matches(title, hit["title"].as_str().unwrap_or_default())
            && matches(artist, hit["artist"]["name"].as_str().unwrap_or_default())
    })?;
    let id = hit["id"].as_u64()?;
    let details = fetch(&format!("https://api.deezer.com/album/{id}"), Some(("limit", "100"))).ok()?;
    let tracks: HashMap<String, Value> = details["tracks"]["data"]
        .as_array()?
        .par_iter()
        .enumerate()
        .filter_map(|(index, track)| {
            let detail = if track["isrc"].as_str().is_some() && track["bpm"].as_f64().is_some() {
                track.clone()
            } else {
                track["id"]
                    .as_u64()
                    .and_then(|id| fetch(&format!("https://api.deezer.com/track/{id}"), None).ok())
                    .unwrap_or_else(|| track.clone())
            };
            let position = format!(
                "{}:{}",
                detail["disk_number"].as_u64().unwrap_or(1),
                detail["track_position"].as_u64().unwrap_or(index as u64 + 1)
            );
            Some((position, json!({"isrc": detail["isrc"], "bpm": detail["bpm"], "gain": detail["gain"]})))
        })
        .collect::<HashMap<_, _>>();
    let genres = details["genres"]["data"].as_array().into_iter().flatten().filter_map(|g| g["name"].as_str());
    let result = json!({"genres": genres.collect::<Vec<_>>(), "upc": details["upc"], "label": details["label"],
        "release_date": details["release_date"], "tracks": tracks});
    DEEZER.lock().ok()?.insert(uri.into(), result.clone());
    Some(result)
}

fn base64(value: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in value.chunks(3) {
        let a = chunk[0] as usize;
        let b = chunk.get(1).copied().unwrap_or_default() as usize;
        let c = chunk.get(2).copied().unwrap_or_default() as usize;
        encoded.push(TABLE[a >> 2] as char);
        encoded.push(TABLE[((a & 3) << 4) | (b >> 4)] as char);
        encoded.push(if chunk.len() > 1 { TABLE[((b & 15) << 2) | (c >> 6)] as char } else { '=' });
        encoded.push(if chunk.len() > 2 { TABLE[c & 63] as char } else { '=' });
    }
    encoded
}

fn token_request(cfg: &Settings, fields: &[(&str, &str)]) -> R<Value> {
    let credentials = base64(format!("{}:{}", cfg.client_id, cfg.client_secret).as_bytes());
    let mut response = HTTP
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", format!("Basic {credentials}"))
        .send_form(fields.iter().copied())?;
    Ok(response.body_mut().read_json()?)
}

fn authorization_code(cfg: &Settings, state: &str) -> R<String> {
    let listener = TcpListener::bind("127.0.0.1:9900")?;
    let callback = "http://127.0.0.1:9900/";
    let scope = "user-library-read user-follow-read playlist-read-private";
    let url = format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&state={state}",
        crate::url_encode(&cfg.client_id),
        crate::url_encode(callback),
        crate::url_encode(scope),
    );
    if cfg.headless {
        println!("{url}");
    } else {
        let mut browser = if cfg!(target_os = "macos") {
            Command::new("open").arg(&url).spawn()
        } else if cfg!(windows) {
            Command::new("cmd").args(["/C", "start", ""]).arg(&url).spawn()
        } else {
            Command::new("xdg-open").arg(&url).spawn()
        };
        if browser.as_mut().is_err() {
            println!("{url}");
        }
    }
    crate::log(cfg, "INFO", "Waiting for Spotify authorization at 127.0.0.1:9900");
    let (mut stream, _) = listener.accept()?;
    let mut request = [0; 8192];
    let length = stream.read(&mut request)?;
    let request = String::from_utf8_lossy(&request[..length]);
    let route =
        request.lines().next().and_then(|line| line.split_whitespace().nth(1)).ok_or("invalid OAuth callback")?;
    let query = route.split_once('?').map_or("", |(_, query)| query);
    let parameter = |name: &str| query.split('&').find_map(|part| part.strip_prefix(&format!("{name}=")));
    if parameter("state") != Some(state) {
        let body = b"Invalid Spotify OAuth state";
        write!(stream, "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n", body.len())?;
        stream.write_all(body)?;
        return Err("Spotify OAuth state did not match".into());
    }
    let code = parameter("code").ok_or("Spotify did not return an authorization code")?.to_string();
    let body = b"Spotify login complete. You can close this tab.";
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())?;
    stream.write_all(body)?;
    Ok(code)
}

pub(crate) fn token(cfg: &Settings, user_auth: bool) -> R<String> {
    if let Some(token) = &cfg.auth_token {
        return Ok(token.clone());
    }
    let mut current = TOKEN.lock().map_err(|error| error.to_string())?;
    if let Some((value, _)) = current.as_ref().filter(|(_, expires)| *expires > Instant::now()) {
        return Ok(value.clone());
    }
    let path = cfg.cache_path.as_ref().map(PathBuf::from).unwrap_or_else(|| home().join(".spotipy"));
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let cache = if cfg.no_cache {
        None
    } else {
        fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    };
    if let Some(value) = cache
        .as_ref()
        .filter(|cache| cache["expires_at"].as_u64().is_some_and(|expiry| expiry > now + 30))
        .and_then(|cache| cache["access_token"].as_str())
    {
        let expiry = cache.as_ref().and_then(|cache| cache["expires_at"].as_u64()).unwrap_or(now);
        *current = Some((value.into(), Instant::now() + Duration::from_secs(expiry - now)));
        return Ok(value.into());
    }
    let response = if let Some(refresh) = cache.as_ref().and_then(|cache| cache["refresh_token"].as_str()) {
        token_request(cfg, &[("grant_type", "refresh_token"), ("refresh_token", refresh)])?
    } else if user_auth {
        let state = format!("{now:x}");
        let code = authorization_code(cfg, &state)?;
        token_request(
            cfg,
            &[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", "http://127.0.0.1:9900/")],
        )?
    } else {
        token_request(cfg, &[("grant_type", "client_credentials")])?
    };
    let access = response["access_token"].as_str().ok_or("Spotify did not return an access token")?.to_string();
    let expires_in = response["expires_in"].as_u64().unwrap_or(3600);
    let expires_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + expires_in;
    *current = Some((access.clone(), Instant::now() + Duration::from_secs(expires_in)));
    if !cfg.no_cache {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut cached = json!({"access_token": access, "expires_at": expires_at});
        if let Some(refresh) = response["refresh_token"]
            .as_str()
            .or_else(|| cache.as_ref().and_then(|cache| cache["refresh_token"].as_str()))
        {
            cached["refresh_token"] = json!(refresh);
        }
        fs::write(path, serde_json::to_vec(&cached)?)?;
    }
    Ok(access)
}

fn official(url: &str, cfg: &Settings) -> R<Vec<Value>> {
    let cache_key = format!("official:{url}");
    if let Some(cached) = read_persistent(&cache_key, cfg).and_then(|value| value.as_array().cloned()) {
        return Ok(normalize_official(cached));
    }
    let token = token(cfg, false)?;
    let mut next = Some(format!("https://api.spotify.com/v1/{url}"));
    let mut items = Vec::new();
    while let Some(u) = next {
        let page: Value = crate::retry(
            cfg.max_retries.saturating_add(1),
            |_| {
                let mut response = HTTP.get(&u).header("Authorization", format!("Bearer {token}")).call()?;
                Ok(response.body_mut().read_json::<Value>()?)
            },
            |error| {
                let message = error.to_string();
                ["429", "500", "502", "503", "504"]
                    .iter()
                    .any(|status| message.contains(status))
                    .then_some(Duration::from_secs(1))
            },
        )?;
        let result =
            ["tracks", "albums", "artists", "playlists", "shows"].iter().find_map(|key| page.get(key)).unwrap_or(&page);
        items.extend(result["items"].as_array().cloned().unwrap_or_default());
        if result["items"].is_null() && result["id"].is_string() {
            items.push(result.clone());
        }
        next = result["next"].as_str().map(String::from);
    }
    let items = normalize_official(items);
    write_persistent(&cache_key, &json!(items), cfg)?;
    Ok(items)
}

fn pathfinder_track(mut track: Value) -> Value {
    let artists: Vec<Value> = track["artists"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|artist| json!({"profile": {"name": artist["name"]}, "uri": artist["uri"]}))
        .collect();
    track["firstArtist"] = json!({"items": artists.first().cloned().into_iter().collect::<Vec<_>>()});
    track["otherArtists"] = json!({"items": artists.into_iter().skip(1).collect::<Vec<_>>()});
    track["duration"] = json!({"totalMilliseconds": track["duration_ms"]});
    track["discNumber"] = track["disc_number"].clone();
    track["trackNumber"] = track["track_number"].clone();
    track["contentRating"] = json!({"label": if track["explicit"] == true { "EXPLICIT" } else { "NONE" }});
    track["isrc"] = track["external_ids"]["isrc"].clone();
    track["playcount"] = json!(track["popularity"].as_u64().map(|count| count.to_string()));
    track
}

fn normalize_official(items: Vec<Value>) -> Vec<Value> {
    items
        .into_iter()
        .map(|mut item| {
            if item["track"]["duration"].is_null() && item["track"]["duration_ms"].is_number() {
                item["track"] = pathfinder_track(item["track"].take());
                item
            } else if item["duration"].is_null() && item["duration_ms"].is_number() {
                pathfinder_track(item)
            } else {
                item
            }
        })
        .collect()
}

fn official_search(term: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    let results = official(&format!("search?q={}&type=track&limit=10", crate::url_encode(term)), cfg)?;
    let track = results.into_iter().next().ok_or("no Spotify results")?;
    let album = track["album"]["uri"].as_str().unwrap_or_default().to_string();
    build(vec![(track, album)], None, cfg, enrich)
}

fn variables(uri: &str, offset: usize, limit: usize) -> Value {
    json!({
        "uri": uri,
        "locale": "",
        "offset": offset,
        "limit": limit,
        "order": "DATE_DESC",
        "enableWatchFeedEntrypoint": false
    })
}

fn pages(op: &str, uri: &str, list: &str, limit: usize, cfg: &Settings) -> R<(Value, Vec<Value>)> {
    let items = |v: &Value| v.pointer(&format!("{list}/items")).and_then(Value::as_array).cloned().unwrap_or_default();
    let first = query(op, variables(uri, 0, limit), cfg)?;
    let total =
        first.pointer(&format!("{list}/totalCount")).and_then(Value::as_u64).ok_or(format!("{uri} unavailable"))?;
    let offsets: Vec<usize> = (limit..total as usize).step_by(limit).collect();
    let rest: Vec<Vec<Value>> =
        offsets.par_iter().map(|o| query(op, variables(uri, *o, limit), cfg).map(|v| items(&v))).collect::<R<_>>()?;
    let meta = first["data"].as_object().and_then(|d| d.values().next()).cloned().unwrap_or_default();
    Ok((meta, std::iter::once(items(&first)).chain(rest).flatten().collect()))
}

fn image_url(images: &Value) -> Option<&str> {
    images
        .as_array()?
        .iter()
        .max_by_key(|image| image["width"].as_u64().unwrap_or_default() * image["height"].as_u64().unwrap_or_default())
        .and_then(|image| image["url"].as_str())
}

fn playlist_cover(playlist: &Value) -> Option<&str> {
    image_url(&playlist["images"])
        .or_else(|| image_url(&playlist["images"]["items"]))
        .or_else(|| image_url(&playlist["coverArt"]["sources"]))
}

fn build(tracks: Vec<(Value, String)>, list: Option<&PlaylistInfo<'_>>, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    let mut uris: Vec<&String> = tracks.iter().map(|(_, a)| a).collect();
    uris.sort();
    uris.dedup();
    let albums: HashMap<&String, (Value, Value)> = uris
        .par_iter()
        .filter_map(|u| {
            let keep = ["name", "artists", "date", "discs", "tracksV2", "label", "coverArt", "copyright", "type"];
            let cached = ALBUMS.lock().ok()?.get(*u).cloned();
            let album = cached.or_else(|| {
                let mut album = query("getAlbum", variables(u, 0, 1), cfg).ok()?["data"]["albumUnion"].take();
                album.as_object_mut()?.retain(|key, _| keep.contains(&key.as_str()));
                ALBUMS.lock().ok()?.insert((*u).clone(), album.clone());
                Some(album)
            })?;
            let deezer =
                if enrich { deezer_album((*u).as_str(), &album, cfg).unwrap_or_default() } else { Value::Null };
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
            let artists: Vec<String> = [&t["firstArtist"]["items"], &t["otherArtists"]["items"]]
                .into_iter()
                .flat_map(|value| value.as_array().into_iter().flatten())
                .filter_map(|artist| artist["profile"]["name"].as_str().map(String::from))
                .collect();
            let date = al["date"]["isoString"].as_str().unwrap_or_default().get(..10).unwrap_or_default().to_string();
            let covers = al["coverArt"]["sources"].as_array().into_iter().flatten();
            let track_key =
                format!("{}:{}", t["discNumber"].as_u64().unwrap_or(1), t["trackNumber"].as_u64().unwrap_or(1));
            let dz_track = &dz["tracks"][&track_key];
            let isrc = t["isrc"]
                .as_str()
                .filter(|isrc| !isrc.is_empty())
                .or(dz_track["isrc"].as_str())
                .filter(|isrc| !isrc.is_empty())
                .map(String::from);
            let song_id = t["uri"].as_str().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
            let url = format!("https://open.spotify.com/track/{song_id}");
            let artist_uri = al["artists"]["items"][0]["uri"].as_str().unwrap_or_default();
            Song {
                name: t["name"].as_str().unwrap_or_default().into(),
                artist: artists.first().cloned().unwrap_or_default(),
                album_artist: al["artists"]["items"][0]["profile"]["name"].as_str().unwrap_or_default().into(),
                artist_id: artist_uri.rsplit(':').next().unwrap_or_default().into(),
                artists,
                disc_number: t["discNumber"].as_u64().unwrap_or(1) as u32,
                disc_count: al["discs"]["totalCount"].as_u64().unwrap_or(1) as u32,
                album_name: al["name"].as_str().unwrap_or_default().into(),
                duration: t["duration"]["totalMilliseconds"].as_f64().unwrap_or_default() / 1000.0,
                year: date.get(..4).and_then(|y| y.parse().ok()).unwrap_or_default(),
                date,
                track_number: t["trackNumber"].as_u64().unwrap_or(1) as u32,
                tracks_count: al["tracksV2"]["totalCount"].as_u64().unwrap_or(1) as u32,
                song_id,
                url,
                explicit: t["contentRating"]["label"] == "EXPLICIT",
                publisher: dz["label"].as_str().or(al["label"].as_str()).unwrap_or_default().into(),
                isrc,
                cover_url: covers.max_by_key(|x| x["width"].as_u64()).and_then(|x| x["url"].as_str().map(String::from)),
                copyright_text: al["copyright"]["items"][0]["text"].as_str().map(String::from),
                popularity: t["playcount"].as_str().and_then(|p| p.parse().ok()),
                album_id: a.rsplit(':').next().unwrap_or_default().into(),
                album_type: al["type"].as_str().unwrap_or_default().to_lowercase(),
                genres: dz["genres"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect(),
                original_date: dz["release_date"].as_str().unwrap_or_default().into(),
                upc: dz["upc"].as_str().unwrap_or_default().into(),
                bpm: dz_track["bpm"].as_f64().unwrap_or_default(),
                gain: dz_track["gain"].as_f64().unwrap_or_default(),
                list_name: list.map(|l| l.name.into()),
                list_url: list.map(|l| l.url.into()),
                list_position: list.map(|_| i + 1),
                list_length: list.map(|_| len),
                list_author: list.and_then(|l| l.author.map(String::from)),
                list_cover_url: list.and_then(|l| l.cover.map(String::from)),
                ..Song::default()
            }
        })
        .collect())
}

fn unique_songs(songs: Vec<Song>) -> Vec<Song> {
    let mut seen = HashSet::new();
    songs.into_iter().filter(|song| seen.insert((song.name.to_lowercase(), song.artists.clone()))).collect()
}

fn album(uri: &str, cfg: &Settings) -> R<Vec<(Value, String)>> {
    let (mut meta, items) = pages("getAlbum", uri, "/data/albumUnion/tracksV2", PAGE, cfg)?;
    meta["tracksV2"]["items"] = Value::Null;
    ALBUMS.lock().map_err(|e| e.to_string())?.insert(uri.into(), meta);
    Ok(items.into_iter().map(|mut i| (i["track"].take(), uri.to_string())).collect())
}

fn artist(uri: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    let (_, releases) = pages("queryArtistDiscographyAll", uri, "/data/artistUnion/discography/all", 50, cfg)?;
    let mut seen = HashSet::new();
    let albums: Vec<&str> = releases
        .iter()
        .filter_map(|x| x["releases"]["items"][0]["uri"].as_str())
        .filter(|uri| seen.insert(*uri))
        .collect();
    let tracks: Vec<(Value, String)> = albums.par_iter().flat_map(|a| album(a, cfg).unwrap_or_default()).collect();
    Ok(unique_songs(build(tracks, None, cfg, enrich)?))
}

fn official_artist(uri: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    let id = uri.rsplit(':').next().unwrap_or_default();
    let albums = official(&format!("artists/{id}/albums?include_groups=album,single,compilation&limit=50"), cfg)?;
    let mut seen = HashSet::new();
    let albums: Vec<Value> = albums
        .into_iter()
        .filter(|album| album["uri"].as_str().is_some_and(|value| seen.insert(value.to_string())))
        .collect();
    let tracks: Vec<(Value, String)> = albums
        .par_iter()
        .flat_map_iter(|album| {
            let Some(album_uri) = album["uri"].as_str() else {
                return Vec::new().into_iter();
            };
            let id = album_uri.rsplit(':').next().unwrap_or_default();
            official(&format!("albums/{id}/tracks?limit=50"), cfg)
                .unwrap_or_default()
                .into_iter()
                .map(|mut track| {
                    track["album"] = json!({"uri": album_uri});
                    (track, album_uri.to_string())
                })
                .collect::<Vec<_>>()
                .into_iter()
        })
        .collect();
    Ok(unique_songs(build(tracks, None, cfg, enrich)?))
}

fn uri(query: &str) -> Option<String> {
    let q = query.split('?').next()?.trim_end_matches('/');
    ["track", "album", "playlist", "artist"].iter().find_map(|k| {
        let id = q.split_once(&format!("/{k}/")).or(q.split_once(&format!(":{k}:")))?.1;
        Some(format!("spotify:{k}:{}", id.split('/').next()?))
    })
}

fn playlist_songs(playlists: &[Value], cfg: &Settings, enrich: bool) -> Vec<Song> {
    playlists
        .par_iter()
        .filter_map(|playlist| playlist["uri"].as_str())
        .flat_map_iter(|playlist| resolve(playlist, cfg, enrich).unwrap_or_default())
        .collect()
}

#[derive(Clone, Copy)]
enum UrlKind {
    Youtube,
    Source,
}

const URL_DISPATCH: &[(&str, UrlKind)] =
    &[("youtube.com/", UrlKind::Youtube), ("youtu.be/", UrlKind::Youtube), ("soundcloud.com/", UrlKind::Source)];

fn resolve_url(query: &str, cfg: &Settings, enrich: bool) -> Option<R<Vec<Song>>> {
    let kind = URL_DISPATCH.iter().find(|(prefix, _)| query.contains(prefix))?.1;
    Some(match kind {
        UrlKind::Youtube => crate::search::youtube(query, cfg, enrich),
        UrlKind::Source => crate::search::source(query, cfg),
    })
}

fn expand_shortlink(query: &str) -> R<String> {
    if !query.contains("spotify.link") {
        return Ok(query.to_string());
    }
    let response = HTTP.get(query).call()?;
    Ok(ureq::ResponseExt::get_uri(&response).to_string())
}

fn paired_youtube(yt: &str, spotify: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    if !yt.contains("playlist?list=") && !yt.contains("browse/VLPL") {
        return Ok(resolve(spotify, cfg, enrich)?
            .into_iter()
            .map(|song| Song { download_url: Some(yt.into()), ..song })
            .collect());
    }
    let ytm_url = yt.replacen("https://www.youtube.com/", "https://music.youtube.com/", 1).replacen(
        "https://youtube.com/",
        "https://music.youtube.com/",
        1,
    );
    let ytm_songs = crate::search::source(&ytm_url, cfg)?;
    let songs = resolve(spotify, cfg, enrich)?;
    if songs.len() != ytm_songs.len() {
        return Err(format!(
            "YouTube Music ({}) and Spotify ({}) lists have different lengths",
            ytm_songs.len(),
            songs.len()
        )
        .into());
    }
    Ok(songs
        .into_iter()
        .zip(ytm_songs)
        .map(|(mut spotify_song, ytm_song)| {
            if cfg.ytm_data {
                spotify_song.name = ytm_song.name;
                spotify_song.artist = ytm_song.artist;
                spotify_song.artists = ytm_song.artists;
                spotify_song.duration = ytm_song.duration;
                spotify_song.album_name = ytm_song.album_name;
            }
            spotify_song.download_url = ytm_song.download_url;
            spotify_song
        })
        .collect())
}

pub fn resolve(q: &str, cfg: &Settings, enrich: bool) -> R<Vec<Song>> {
    if q.ends_with(".spotdl") {
        let mut value: Value = serde_json::from_slice(&fs::read(q)?)?;
        let songs = value.get_mut("songs").map(Value::take).unwrap_or(value);
        return Ok(serde_json::from_value(songs)?);
    }
    if let Some((yt, sp)) = q.split_once('|') {
        return paired_youtube(yt, sp, cfg, enrich);
    }
    if let Some(result) = resolve_url(q, cfg, enrich) {
        return result;
    }
    let q = expand_shortlink(q)?;
    if let Some(user) = q
        .split_once("open.spotify.com/user/")
        .map(|(_, path)| path.split('/').next().and_then(|value| value.split('?').next()).unwrap_or_default())
    {
        let playlists = official(&format!("users/{user}/playlists?limit=50"), cfg)?;
        return Ok(playlist_songs(&playlists, cfg, enrich));
    }
    let lib = |path: &str, key: &str| -> R<Vec<Value>> {
        Ok(official(path, cfg)?.into_iter().map(|mut value| value[key].take()).collect())
    };
    match q.as_str() {
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
        "all-user-playlists" | "all-saved-playlists" => {
            let user_id =
                official("me", cfg)?.into_iter().next().and_then(|user| user["id"].as_str().map(String::from));
            let user_id = user_id.ok_or("Spotify did not return the current user")?;
            let owned = q == "all-user-playlists";
            let playlists: Vec<Value> = official("me/playlists?limit=50", cfg)?
                .into_iter()
                .filter(|playlist| {
                    playlist["owner"]["id"].as_str().is_some_and(|owner| (owner == user_id.as_str()) == owned)
                })
                .collect();
            return Ok(playlist_songs(&playlists, cfg, enrich));
        }
        "all-user-saved-albums" => {
            let albums = lib("me/albums?limit=50", "album")?;
            return Ok(playlist_songs(&albums, cfg, enrich));
        }
        "all-user-followed-artists" => {
            let artists = official("me/following?type=artist&limit=50", cfg)?;
            return Ok(playlist_songs(&artists, cfg, enrich));
        }
        _ => {}
    }
    let (kind, term) = ["album:", "playlist:", "artist:"]
        .iter()
        .find_map(|k| q.strip_prefix(k).map(|t| (k.trim_end_matches(':'), t.trim())))
        .unwrap_or(("track", q.as_str()));
    let uri = match uri(&q) {
        Some(u) => u,
        None if cfg.use_official_api && kind == "track" => return official_search(term, cfg, enrich),
        None => {
            let search_term = if kind == "track" { term.replace(" - ", " ") } else { term.to_string() };
            let vars = json!({"searchTerm": &search_term, "offset": 0, "limit": 10, "numberOfTopResults": 5, "includeAudiobooks": false, "includeArtistHasConcertsField": false, "includePreReleases": false, "includeLocalConcertsField": false});
            let mut response = query("searchDesktop", vars, cfg)?;
            let s = response["data"]["searchV2"].take();
            let first =
                &s[format!("{kind}s{}", if kind == "track" || kind == "album" { "V2" } else { "" })]["items"][0];
            let name = first["item"]["data"]["name"].as_str().unwrap_or_default();
            if kind == "track" && crate::search::ratio(&search_term, name) < 25.0 {
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
            if cfg.use_official_api {
                let id = uri.rsplit(':').next().unwrap_or_default();
                let track = official(&format!("tracks/{id}"), cfg)?.into_iter().next().ok_or("no Spotify track")?;
                let album = track["album"]["uri"].as_str().unwrap_or_default().to_string();
                return build(vec![(track, album)], None, cfg, enrich);
            }
            let mut response = query("getTrack", json!({"uri": uri}), cfg)?;
            let t = response["data"]["trackUnion"].take();
            let a = t["albumOfTrack"]["uri"].as_str().unwrap_or_default().to_string();
            build(vec![(t, a)], None, cfg, enrich)
        }
        "album" if cfg.use_official_api => {
            let id = uri.rsplit(':').next().unwrap_or_default();
            let tracks = official(&format!("albums/{id}/tracks?limit=50"), cfg)?
                .into_iter()
                .map(|mut track| {
                    track["album"]["uri"] = json!(uri);
                    (track, uri.clone())
                })
                .collect();
            build(tracks, None, cfg, enrich)
        }
        "album" => build(album(&uri, cfg)?, None, cfg, enrich),
        "artist" if cfg.use_official_api => official_artist(&uri, cfg, enrich),
        "artist" => artist(&uri, cfg, enrich),
        "playlist" if cfg.use_official_api => {
            let id = uri.rsplit(':').next().unwrap_or_default();
            let meta = official(&format!("playlists/{id}"), cfg)?.into_iter().next().unwrap_or_default();
            let name = meta["name"].as_str().unwrap_or_default().to_string();
            let url = format!("https://open.spotify.com/playlist/{id}");
            let list = PlaylistInfo {
                name: &name,
                url: &url,
                author: meta["owner"]["display_name"].as_str(),
                cover: playlist_cover(&meta),
            };
            let tracks = official(&format!("playlists/{id}/tracks?limit=100"), cfg)?
                .into_iter()
                .filter_map(|mut item| {
                    let track = item["track"].take();
                    let album = track["album"]["uri"].as_str()?.to_string();
                    Some((track, album))
                })
                .collect();
            build(tracks, Some(&list), cfg, enrich)
        }
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
            let list = PlaylistInfo {
                name: p["name"].as_str().unwrap_or_default(),
                url: &url,
                author: p["ownerV2"]["data"]["name"].as_str().or(p["owner"]["display_name"].as_str()),
                cover: playlist_cover(&p),
            };
            build(tracks, Some(&list), cfg, enrich)
        }
    }
}
