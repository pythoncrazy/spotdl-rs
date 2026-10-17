use crate::{http, Song, HOME, R};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rayon::prelude::*;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{Arc, LazyLock, Mutex, OnceLock},
    time::UNIX_EPOCH,
};

const UA: (&str, &str) = (
    "User-Agent",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36",
);

const SECRETS_URL: &str = "https://code.thetadev.de/ThetaDev/spotify-secrets/raw/branch/main/secrets/secretDict.json";

static STATE: Mutex<Option<Value>> = Mutex::new(None);

static ALBUMS: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

fn hotp(key: &[u8], counter: u64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC accepts keys of any size");
    mac.update(&counter.to_be_bytes());
    let d = mac.finalize().into_bytes();
    let o = (d[19] & 15) as usize;
    format!("{:06}", (u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) & 0x7fff_ffff) % 1_000_000)
}

fn totp(secret: &[u8]) -> R<String> {
    let key: Vec<u8> = secret
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ ((i % 33 + 9) as u8))
        .flat_map(|b| b.to_string().into_bytes())
        .collect();
    Ok(hotp(&key, UNIX_EPOCH.elapsed().unwrap_or_default().as_secs() / 30))
}

fn hashes(ops: &[&str], cfg: &Value) -> R<Value> {
    let text = |url: &str| -> R<String> { Ok(String::from_utf8(http("GET", url, &[UA], None, cfg)?)?) };
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

fn state(cfg: &Value, rejected: Option<&str>) -> R<Value> {
    let path = HOME.join("anon.json");
    let memory = STATE.lock().map_err(|e| e.to_string())?.clone();
    let cached = memory.or_else(|| {
        (cfg["no_cache"] != true).then(|| serde_json::from_slice::<Value>(&fs::read(&path).ok()?).ok()).flatten()
    });
    let now = UNIX_EPOCH.elapsed().unwrap_or_default().as_millis() as u64;
    let fresh = |v: &Value| {
        v["exp"].as_u64() > Some(now + 60_000)
            && rejected.is_none_or(|t| v["at"].as_u64() > Some(now - 60_000) && v["token"].as_str() != Some(t))
    };
    if let Some(v) = cached.filter(fresh) {
        *STATE.lock().map_err(|e| e.to_string())? = Some(v.clone());
        return Ok(v);
    }
    let secrets: HashMap<String, Vec<u8>> = crate::http_json("GET", SECRETS_URL, &[UA], None, cfg)?;
    let (ver, secret) = secrets.iter().max_by_key(|(k, _)| k.parse::<u32>().unwrap_or(0)).ok_or("no TOTP secret")?;
    let code = totp(secret)?;
    let url = format!("https://open.spotify.com/api/token?reason=init&productType=web-player&totp={code}&totpVer={ver}&totpServer={code}");
    let tok: Value = crate::http_json("GET", &url, &[UA], None, cfg)?;
    let v = json!({
        "token": tok["accessToken"].as_str().ok_or("Spotify token missing")?,
        "exp": tok["accessTokenExpirationTimestampMs"],
        "at": now,
        "hashes": hashes(&["getTrack", "getAlbum", "fetchPlaylist", "queryArtistDiscographyAll", "searchDesktop"], cfg)?,
    });
    if cfg["no_cache"] != true {
        fs::create_dir_all(&*HOME)?;
        fs::write(path, v.to_string())?;
    }
    *STATE.lock().map_err(|e| e.to_string())? = Some(v.clone());
    Ok(v)
}

fn cached(key: &str, cache: bool, forever: bool, fetch: impl FnOnce() -> R<Value>) -> R<Value> {
    let key = format!("{:x}", Sha1::digest(key.as_bytes()));
    let path = HOME.join("cache").join(&key[..16]);
    if cache {
        let age = fs::metadata(&path).ok().and_then(|meta| meta.modified().ok()?.elapsed().ok());
        let fresh = age.is_some_and(|age| forever || age.as_secs() < 604_800);
        if let Some(value) =
            fs::read(&path).ok().filter(|_| fresh).and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            return Ok(value);
        }
    }
    let value = fetch()?;
    if cache {
        let _ = fs::create_dir_all(HOME.join("cache")).and_then(|_| fs::write(&path, value.to_string()));
    }
    Ok(value)
}

fn query(op: &str, vars: Value, cfg: &Value) -> R<Value> {
    let key = format!("{op}{vars}");
    let forever = op == "getTrack" && cfg["use_cache_file"] == true;
    let cache = cfg["no_cache"] != true && ["getAlbum", "getTrack"].contains(&op);
    cached(&key, cache, forever, || {
        let mut rejected: Option<String> = None;
        let mut last = String::new();
        for _ in 0..=cfg["max_retries"].as_u64().unwrap_or(3) {
            let state = state(cfg, rejected.as_deref())?;
            let token = state["token"].as_str().unwrap_or_default();
            let hash = state["hashes"][op].as_str().ok_or("missing query hash")?;
            let extensions = json!({"persistedQuery": {"version": 1, "sha256Hash": hash}}).to_string();
            let [variables, extensions] = [vars.to_string(), extensions].map(|value| crate::url_encode(&value));
            let url = format!(
            "https://api-partner.spotify.com/pathfinder/v1/query?operationName={op}&variables={variables}&extensions={extensions}"
        );
            let auth = format!("Bearer {token}");
            let value = crate::http_json::<Value>("GET", &url, &[UA, ("Authorization", &auth)], None, cfg);
            match value {
                Ok(value) if value.get("data").is_some() => {
                    return Ok(value);
                }
                Ok(value) => last = format!("{op}: {}", value["errors"]),
                Err(error) => last = error.to_string(),
            }
            if ["401", "hash", "PersistedQuery"].iter().any(|word| last.contains(word)) {
                rejected = Some(token.to_string());
            }
        }
        Err(last.into())
    })
}

type DeezerCache = HashMap<String, Arc<OnceLock<Option<Value>>>>;

static DEEZER: LazyLock<Mutex<DeezerCache>> = LazyLock::new(Default::default);

fn deezer_get(url: &str, cfg: &Value) -> R<Value> {
    for wait in [0, 2, 5] {
        std::thread::sleep(std::time::Duration::from_secs(wait));
        let reply: Value = crate::http_json("GET", url, &[], None, cfg)?;
        if reply["error"]["code"] != 4 {
            return Ok(reply);
        }
    }
    Err("Deezer rate limit".into())
}

fn fetch_deezer_album(album_id: &str, artist: &str, title: &str, cfg: &Value) -> R<Value> {
    cached(&format!("deezer:{album_id}"), cfg["no_cache"] != true, false, || {
        let search =
            format!("https://api.deezer.com/search/album?q={}", crate::url_encode(&format!("{artist} {title}")));
        let hits = deezer_get(&search, cfg)?;
        let close = |expected: &str, found: &Value| {
            let found = crate::search::slug(found.as_str().unwrap_or_default(), "-");
            crate::search::ratio(&crate::search::slug(expected, "-"), &found) > 80.0
        };
        let hit = hits["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|hit| close(title, &hit["title"]) && close(artist, &hit["artist"]["name"]));
        let Some(hit) = hit else {
            return Ok(Value::Null);
        };
        let id = hit["id"].as_u64().ok_or("Deezer album ID missing")?;
        let details = deezer_get(&format!("https://api.deezer.com/album/{id}"), cfg)?;
        let tracks = deezer_get(&format!("https://api.deezer.com/album/{id}/tracks?limit=500"), cfg)?;
        let tracks = tracks["data"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, track)| {
                let disc = track["disk_number"].as_u64().unwrap_or(1);
                let position = track["track_position"].as_u64().unwrap_or(index as u64 + 1);
                (format!("{disc}:{position}"), json!({"isrc": track["isrc"]}))
            })
            .collect::<serde_json::Map<String, Value>>();
        let genres =
            details["genres"]["data"].as_array().into_iter().flatten().filter_map(|genre| genre["name"].as_str());
        Ok(json!({
            "genres": genres.collect::<Vec<_>>(),
            "upc": details["upc"],
            "label": details["label"],
            "release_date": details["release_date"],
            "tracks": tracks
        }))
    })
}

fn deezer_album(album_id: &str, artist: &str, title: &str, cfg: &Value) -> Option<Value> {
    let cell = DEEZER.lock().ok()?.entry(album_id.into()).or_default().clone();
    cell.get_or_init(|| fetch_deezer_album(album_id, artist, title, cfg).ok()).clone()
}

pub fn enrich(song: &mut Song, cfg: &Value) {
    let deezer = (!song.album_id.is_empty())
        .then(|| deezer_album(&song.album_id, &song.album_artist, &song.album_name, cfg))
        .flatten();
    if let Some(deezer) = deezer.filter(|value| !value.is_null()) {
        song.genres =
            deezer["genres"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        song.upc = deezer["upc"].as_str().unwrap_or_default().into();
        if let Some(label) = deezer["label"].as_str() {
            song.publisher = label.into();
        }
        let release_date = deezer["release_date"].as_str().unwrap_or_default();
        song.original_date = if release_date.is_empty() {
            String::new()
        } else if song.date.is_empty() || release_date < song.date.as_str() {
            release_date.into()
        } else {
            song.date.clone()
        };
        if song.isrc.as_deref().is_none_or(str::is_empty) {
            let track = &deezer["tracks"][format!("{}:{}", song.disc_number, song.track_number)];
            song.isrc = track["isrc"].as_str().filter(|isrc| !isrc.is_empty()).map(String::from);
        }
    }
    if let Some(isrc) = song.isrc.as_deref().filter(|isrc| !isrc.is_empty()) {
        if song.bpm == 0.0 && song.gain == 0.0 {
            if let Ok(track) =
                deezer_get(&format!("https://api.deezer.com/track/isrc:{}", crate::url_encode(isrc)), cfg)
            {
                song.bpm = track["bpm"].as_f64().unwrap_or_default();
                song.gain = track["gain"].as_f64().unwrap_or_default();
            }
        }
    }
}

static TOKEN: Mutex<Option<(String, u64)>> = Mutex::new(None);

fn token_request(cfg: &Value, fields: &[(&str, &str)]) -> R<Value> {
    let id = format!(
        "{}:{}",
        cfg["client_id"].as_str().unwrap_or_default(),
        cfg["client_secret"].as_str().unwrap_or_default()
    );
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(id.as_bytes()));
    let form = fields.iter().map(|(key, value)| format!("{key}={}", crate::url_encode(value))).collect::<Vec<_>>();
    let headers = [("Authorization", auth.as_str()), ("Content-Type", "application/x-www-form-urlencoded")];
    crate::http_json("POST", "https://accounts.spotify.com/api/token", &headers, Some(form.join("&").as_bytes()), cfg)
}

fn authorization_code(cfg: &Value, state: &str) -> R<String> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:9900")?;
    let url = format!(
        "https://accounts.spotify.com/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&state={state}",
        crate::url_encode(cfg["client_id"].as_str().unwrap_or_default()),
        crate::url_encode("http://127.0.0.1:9900/"),
        crate::url_encode("user-library-read user-follow-read playlist-read-private"),
    );
    let opener = match std::env::consts::OS {
        "macos" => vec!["open"],
        "windows" => vec!["cmd", "/C", "start", ""],
        _ => vec!["xdg-open"],
    };
    let opened =
        cfg["headless"] != true && std::process::Command::new(opener[0]).args(&opener[1..]).arg(&url).spawn().is_ok();
    if !opened {
        println!("{url}");
    }
    crate::log(cfg, "INFO", "Waiting for Spotify authorization at 127.0.0.1:9900");
    let (mut stream, _) = listener.accept()?;
    let mut request = [0; 8192];
    let length = stream.read(&mut request)?;
    let request = String::from_utf8_lossy(&request[..length]);
    let route = request.split_whitespace().nth(1).unwrap_or_default();
    let query = route.split_once('?').map_or("", |(_, query)| query);
    let parameter = |name: &str| query.split('&').find_map(|part| part.strip_prefix(&format!("{name}=")));
    let code = parameter("code").filter(|_| parameter("state") == Some(state)).map(String::from);
    let body = if code.is_some() { "Spotify login complete. You can close this tab." } else { "Spotify login failed." };
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len())?;
    code.ok_or_else(|| "Spotify did not return a valid authorization code".into())
}

pub fn token(cfg: &Value, user: bool) -> R<String> {
    if let Some(token) = cfg["auth_token"].as_str() {
        return Ok(token.into());
    }
    let current = TOKEN.lock().map_err(|error| error.to_string())?.clone();
    if let Some((token, _)) =
        current.as_ref().filter(|(_, expiry)| *expiry > UNIX_EPOCH.elapsed().unwrap_or_default().as_secs() + 30)
    {
        return Ok(token.clone());
    }
    let path = cfg["cache_path"].as_str().map_or_else(|| HOME.join(".spotipy"), Into::into);
    let read = fs::read(&path).ok().filter(|_| cfg["no_cache"] != true);
    let cache = read.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()).unwrap_or_default();
    let expiry = cache["expires_at"].as_u64().unwrap_or(0);
    if let Some(token) =
        cache["access_token"].as_str().filter(|_| expiry > UNIX_EPOCH.elapsed().unwrap_or_default().as_secs() + 30)
    {
        *TOKEN.lock().map_err(|error| error.to_string())? = Some((token.into(), expiry));
        return Ok(token.into());
    }
    let refresh = cache["refresh_token"].as_str();
    let state = format!("{:x}", UNIX_EPOCH.elapsed().unwrap_or_default().as_secs());
    let reply = match (refresh, user) {
        (Some(refresh), _) => token_request(cfg, &[("grant_type", "refresh_token"), ("refresh_token", refresh)])?,
        (None, true) => {
            let code = authorization_code(cfg, &state)?;
            let fields =
                [("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", "http://127.0.0.1:9900/")];
            token_request(cfg, &fields)?
        }
        (None, false) => token_request(cfg, &[("grant_type", "client_credentials")])?,
    };
    let access = reply["access_token"].as_str().ok_or("Spotify did not return an access token")?.to_string();
    let expiry = UNIX_EPOCH.elapsed().unwrap_or_default().as_secs() + reply["expires_in"].as_u64().unwrap_or(3600);
    *TOKEN.lock().map_err(|error| error.to_string())? = Some((access.clone(), expiry));
    let refresh = reply["refresh_token"].as_str().or(refresh);
    if cfg["no_cache"] != true {
        fs::create_dir_all(path.parent().unwrap_or(&HOME))?;
        fs::write(&path, json!({"access_token": access, "expires_at": expiry, "refresh_token": refresh}).to_string())?;
    }
    Ok(access)
}

fn official(path: &str, cfg: &Value) -> R<Vec<Value>> {
    let auth = format!("Bearer {}", token(cfg, cfg["user_auth"] == true)?);
    let mut next = Some(format!("https://api.spotify.com/v1/{path}"));
    let mut items = Vec::new();
    while let Some(url) = next {
        let page: Value = crate::http_json("GET", &url, &[("Authorization", &auth)], None, cfg)?;
        let keys = ["tracks", "albums", "artists", "playlists", "shows"];
        let result = keys.iter().find_map(|key| page.get(key)).unwrap_or(&page);
        items.extend(result["items"].as_array().cloned().unwrap_or_default());
        if result["items"].is_null() && result["id"].is_string() {
            items.push(result.clone());
        }
        next = result["next"].as_str().map(String::from);
    }
    Ok(items)
}

fn pages(op: &str, uri: &str, list: &str, limit: usize, cfg: &Value) -> R<(Value, Vec<Value>)> {
    let variables = |offset: usize| json!({"uri": uri, "locale": "", "offset": offset, "limit": limit, "order": "DATE_DESC", "enableWatchFeedEntrypoint": false});
    let items =
        |value: &Value| value.pointer(&format!("{list}/items")).and_then(Value::as_array).cloned().unwrap_or_default();
    let first = query(op, variables(0), cfg)?;
    let total =
        first.pointer(&format!("{list}/totalCount")).and_then(Value::as_u64).ok_or(format!("{uri} unavailable"))?;
    let offsets: Vec<usize> = (limit..total as usize).step_by(limit).collect();
    let rest: Vec<Vec<Value>> = offsets
        .par_iter()
        .map(|offset| query(op, variables(*offset), cfg).map(|page| items(&page)))
        .collect::<R<_>>()?;
    let meta = first["data"].as_object().and_then(|data| data.values().next()).cloned().unwrap_or_default();
    Ok((meta, std::iter::once(items(&first)).chain(rest).flatten().collect()))
}

fn cover(value: &Value) -> Option<&str> {
    let size =
        |image: &&Value| image["width"].as_u64().unwrap_or_default() * image["height"].as_u64().unwrap_or_default();
    let sources = [&value["images"], &value["images"]["items"], &value["coverArt"]["sources"]];
    sources
        .into_iter()
        .filter_map(Value::as_array)
        .find_map(|images| images.iter().max_by_key(size).and_then(|image| image["url"].as_str()))
}

type List<'a> = (&'a str, &'a str, Option<&'a str>, Option<&'a str>);

fn album_meta(uri: &str, cfg: &Value) -> Option<Value> {
    let keep = ["name", "artists", "date", "discs", "tracksV2", "label", "coverArt", "copyright", "type"];
    let cached = ALBUMS.lock().ok()?.get(uri).cloned();
    let album = match cached {
        Some(album) => album,
        None => {
            let variables = json!({"uri": uri, "locale": "", "offset": 0, "limit": 1});
            let mut album = query("getAlbum", variables, cfg).ok()?["data"]["albumUnion"].take();
            album.as_object_mut()?.retain(|key, _| keep.contains(&key.as_str()));
            ALBUMS.lock().ok()?.insert(uri.into(), album.clone());
            album
        }
    };
    Some(album)
}

fn build(tracks: Vec<Value>, list: Option<List>, cfg: &Value) -> Vec<Song> {
    let uris: HashSet<&str> = tracks
        .iter()
        .filter_map(|track| track["albumOfTrack"]["uri"].as_str().or(track["album"]["uri"].as_str()))
        .collect();
    let albums: HashMap<&str, Value> =
        uris.into_par_iter().filter_map(|uri| Some((uri, album_meta(uri, cfg)?))).collect();
    let length = tracks.len();
    let empty = Value::Null;
    let playable = tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track["uri"].as_str().is_some_and(|uri| uri.starts_with("spotify:track:")));
    let song = |(index, track): (usize, &Value)| {
        let uri = track["albumOfTrack"]["uri"].as_str().or(track["album"]["uri"].as_str()).unwrap_or_default();
        let album = albums.get(uri).unwrap_or(&empty);
        let names = [
            &track["firstArtist"]["items"],
            &track["otherArtists"]["items"],
            &track["artists"]["items"],
            &track["artists"],
        ];
        let artists = names.into_iter().flat_map(|value| value.as_array().into_iter().flatten());
        let artists: Vec<String> = artists
            .filter_map(|artist| artist["profile"]["name"].as_str().or(artist["name"].as_str()).map(String::from))
            .collect();
        let date = album["date"]["isoString"].as_str().unwrap_or_default().get(..10).unwrap_or_default().to_string();
        let disc = track["discNumber"].as_u64().or(track["disc_number"].as_u64()).unwrap_or(1);
        let number = track["trackNumber"].as_u64().or(track["track_number"].as_u64()).unwrap_or(1);
        let isrc = [&track["isrc"], &track["external_ids"]["isrc"]]
            .into_iter()
            .filter_map(Value::as_str)
            .find(|isrc| !isrc.is_empty());
        let id = |value: &Value| value.as_str().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
        let song_id = id(&track["uri"]);
        Song {
            name: track["name"].as_str().unwrap_or_default().into(),
            artist: artists.first().cloned().unwrap_or_default(),
            album_artist: album["artists"]["items"][0]["profile"]["name"].as_str().unwrap_or_default().into(),
            artist_id: id(&album["artists"]["items"][0]["uri"]),
            artists,
            disc_number: disc as u32,
            disc_count: album["discs"]["totalCount"].as_u64().unwrap_or(1) as u32,
            album_name: album["name"].as_str().unwrap_or_default().into(),
            duration: track["duration"]["totalMilliseconds"]
                .as_f64()
                .or(track["duration_ms"].as_f64())
                .unwrap_or_default()
                / 1000.0,
            year: date.get(..4).and_then(|year| year.parse().ok()).unwrap_or_default(),
            date,
            track_number: number as u32,
            tracks_count: album["tracksV2"]["totalCount"].as_u64().unwrap_or(1) as u32,
            url: format!("https://open.spotify.com/track/{song_id}"),
            song_id,
            explicit: track["contentRating"]["label"] == "EXPLICIT" || track["explicit"] == true,
            publisher: album["label"].as_str().unwrap_or_default().into(),
            isrc: isrc.map(String::from),
            cover_url: cover(album).map(String::from),
            copyright_text: album["copyright"]["items"][0]["text"].as_str().map(String::from),
            popularity: track["playcount"]
                .as_str()
                .and_then(|count| count.parse().ok())
                .or_else(|| track["popularity"].as_u64()),
            album_id: uri.rsplit(':').next().unwrap_or_default().into(),
            album_type: album["type"].as_str().unwrap_or_default().to_lowercase(),
            list_name: list.map(|list| list.0.into()),
            list_url: list.map(|list| list.1.into()),
            list_position: list.map(|_| index + 1),
            list_length: list.map(|_| length),
            list_author: list.and_then(|list| list.2.map(String::from)),
            list_cover_url: list.and_then(|list| list.3.map(String::from)),
            ..Song::default()
        }
    };
    playable.map(song).collect()
}

fn album(uri: &str, cfg: &Value) -> R<Vec<Value>> {
    let (mut meta, items) = pages("getAlbum", uri, "/data/albumUnion/tracksV2", 343, cfg)?;
    meta["tracksV2"]["items"] = Value::Null;
    ALBUMS.lock().map_err(|error| error.to_string())?.insert(uri.into(), meta);
    Ok(items
        .into_iter()
        .map(|mut item| {
            item["track"]["albumOfTrack"]["uri"] = json!(uri);
            item["track"].take()
        })
        .collect())
}

fn artist(uri: &str, cfg: &Value) -> R<Vec<Song>> {
    let id = uri.rsplit(':').next().unwrap_or_default();
    let api = cfg["use_official_api"] == true;
    let releases = match api {
        true => official(&format!("artists/{id}/albums?include_groups=album,single,compilation&limit=50"), cfg)?,
        false => pages("queryArtistDiscographyAll", uri, "/data/artistUnion/discography/all", 50, cfg)?.1,
    };
    let release =
        |value: &Value| value["uri"].as_str().or(value["releases"]["items"][0]["uri"].as_str()).map(String::from);
    let mut seen = HashSet::new();
    let albums: Vec<String> = releases.iter().filter_map(release).filter(|uri| seen.insert(uri.clone())).collect();
    let tracks = |uri: &String| match api {
        true => {
            let id = uri.rsplit(':').next().unwrap_or_default();
            let mut tracks = official(&format!("albums/{id}/tracks?limit=50"), cfg).unwrap_or_default();
            tracks.iter_mut().for_each(|track| track["albumOfTrack"]["uri"] = json!(uri));
            tracks
        }
        false => album(uri, cfg).unwrap_or_default(),
    };
    let tracks: Vec<Value> = albums.par_iter().flat_map_iter(tracks).collect();
    let mut seen = HashSet::new();
    let songs = build(tracks, None, cfg).into_iter();
    Ok(songs.filter(|song| seen.insert((song.name.to_lowercase(), song.artists.clone()))).collect())
}

fn uri(query: &str) -> Option<String> {
    let query = query.split('?').next()?.trim_end_matches('/');
    ["track", "album", "playlist", "artist"].iter().find_map(|kind| {
        let id = query.split_once(&format!("/{kind}/")).or(query.split_once(&format!(":{kind}:")))?.1;
        Some(format!("spotify:{kind}:{}", id.split('/').next()?))
    })
}

fn paired_youtube(yt: &str, spotify: &str, cfg: &Value) -> R<Vec<Song>> {
    let songs = resolve(spotify, cfg)?;
    if !yt.contains("playlist?list=") && !yt.contains("browse/VLPL") {
        return Ok(songs.into_iter().map(|song| Song { download_url: Some(yt.into()), ..song }).collect());
    }
    let url = yt.replacen("https://www.youtube.com/", "https://music.youtube.com/", 1);
    let found =
        crate::search::source(&url.replacen("https://youtube.com/", "https://music.youtube.com/", 1), cfg, false)?;
    if songs.len() != found.len() {
        let message =
            format!("YouTube Music ({}) and Spotify ({}) lists have different lengths", found.len(), songs.len());
        return Err(message.into());
    }
    let merge = |(mut song, found): (Song, Song)| {
        crate::search::apply_ytm_data(&mut song, found, cfg["ytm_data"] == true);
        song
    };
    Ok(songs.into_iter().zip(found).map(merge).collect())
}

fn search(text: &str, cfg: &Value) -> R<String> {
    let prefix = ["album:", "playlist:", "artist:"]
        .iter()
        .find_map(|kind| Some((kind.trim_end_matches(':'), text.strip_prefix(kind)?.trim())));
    let (kind, term) = prefix.unwrap_or(("track", text));
    if cfg["use_official_api"] == true && kind == "track" {
        let hits = official(&format!("search?q={}&type=track&limit=1", crate::url_encode(term)), cfg)?;
        return Ok(hits.first().and_then(|hit| hit["uri"].as_str()).ok_or("no Spotify results")?.to_string());
    }
    let search_term = if kind == "track" { term.replace(" - ", " ") } else { term.to_string() };
    let vars = json!({"searchTerm": &search_term, "offset": 0, "limit": 10, "numberOfTopResults": 5, "includeAudiobooks": false, "includeArtistHasConcertsField": false, "includePreReleases": false, "includeLocalConcertsField": false});
    let mut response = query("searchDesktop", vars, cfg)?;
    let results = response["data"]["searchV2"].take();
    let list = if kind == "track" || kind == "album" { format!("{kind}sV2") } else { format!("{kind}s") };
    let first = &results[list]["items"][0];
    let name = first["item"]["data"]["name"].as_str().unwrap_or_default();
    if kind == "track" && crate::search::ratio(&search_term, name) < 25.0 {
        return Err("no Spotify results".into());
    }
    let uri = first["item"]["data"]["uri"].as_str().or(first["data"]["uri"].as_str());
    Ok(uri.ok_or("no Spotify results")?.to_string())
}

fn playlist(uri: &str, id: &str, api: bool, cfg: &Value) -> R<Vec<Song>> {
    let (meta, tracks) = match api {
        true => {
            let meta = official(&format!("playlists/{id}"), cfg)?.into_iter().next().unwrap_or_default();
            let items = official(&format!("playlists/{id}/tracks?limit=100"), cfg)?;
            (meta, items.into_iter().map(|mut item| item["track"].take()).collect::<Vec<_>>())
        }
        false => {
            let (meta, items) = pages("fetchPlaylist", uri, "/data/playlistV2/content", 343, cfg)?;
            (meta, items.into_iter().map(|mut item| item["itemV2"]["data"].take()).collect())
        }
    };
    let url = format!("https://open.spotify.com/playlist/{id}");
    let author = meta["ownerV2"]["data"]["name"].as_str().or(meta["owner"]["display_name"].as_str());
    let list = (meta["name"].as_str().unwrap_or_default(), url.as_str(), author, cover(&meta));
    Ok(build(tracks, Some(list), cfg))
}

fn library(text: &str, cfg: &Value) -> R<Option<Vec<Song>>> {
    let take = |path: &str, key: &str| -> R<Vec<Value>> {
        Ok(official(path, cfg)?.into_iter().map(|mut value| value[key].take()).collect())
    };
    let lists = |values: &[Value]| -> Vec<Song> {
        values
            .par_iter()
            .filter_map(|value| value["uri"].as_str())
            .flat_map_iter(|uri| resolve(uri, cfg).unwrap_or_default())
            .collect()
    };
    if let Some((_, path)) = text.split_once("open.spotify.com/user/") {
        let user = path.split(['/', '?']).next().unwrap_or_default();
        return Ok(Some(lists(&official(&format!("users/{user}/playlists?limit=50"), cfg)?)));
    }
    let songs = match text {
        "saved" => build(take("me/tracks?limit=50", "track")?, None, cfg),
        "all-user-playlists" | "all-saved-playlists" => {
            let user = official("me", cfg)?.into_iter().next().and_then(|user| user["id"].as_str().map(String::from));
            let user = user.ok_or("Spotify did not return the current user")?;
            let owned = |playlist: &Value| playlist["owner"]["id"].as_str().is_some_and(|owner| owner == user);
            let playlists = official("me/playlists?limit=50", cfg)?.into_iter();
            let playlists: Vec<Value> =
                playlists.filter(|playlist| owned(playlist) == (text == "all-user-playlists")).collect();
            lists(&playlists)
        }
        "all-user-saved-albums" => lists(&take("me/albums?limit=50", "album")?),
        "all-user-followed-artists" => lists(&official("me/following?type=artist&limit=50", cfg)?),
        _ => return Ok(None),
    };
    Ok(Some(songs))
}

pub fn resolve(text: &str, cfg: &Value) -> R<Vec<Song>> {
    if text.ends_with(".spotdl") {
        let mut value: Value = serde_json::from_slice(&fs::read(text)?)?;
        let songs = value.get_mut("songs").map(Value::take).unwrap_or(value);
        return Ok(serde_json::from_value(songs)?);
    }
    if let Some((yt, spotify)) = text.split_once('|') {
        return paired_youtube(yt, spotify, cfg);
    }
    if ["youtube.com/", "youtu.be/"].iter().any(|host| text.contains(host)) {
        return crate::search::source(text, cfg, true);
    }
    if text.contains("soundcloud.com/") {
        return crate::search::source(text, cfg, false);
    }
    let text = if text.contains("spotify.link") {
        ureq::ResponseExt::get_uri(&ureq::get(text).call()?).to_string()
    } else {
        text.to_string()
    };
    if let Some(songs) = library(&text, cfg)? {
        return Ok(songs);
    }
    let uri = match uri(&text) {
        Some(uri) => uri,
        None => search(&text, cfg)?,
    };
    let id = uri.rsplit(':').next().unwrap_or_default();
    let api = cfg["use_official_api"] == true;
    let kind = uri.split(':').nth(1).unwrap_or_default();
    let tracks = match (kind, api) {
        ("track", true) => official(&format!("tracks/{id}"), cfg)?,
        ("track", false) => vec![query("getTrack", json!({"uri": uri}), cfg)?["data"]["trackUnion"].take()],
        ("album", true) => {
            let mut tracks = official(&format!("albums/{id}/tracks?limit=50"), cfg)?;
            tracks.iter_mut().for_each(|track| track["albumOfTrack"]["uri"] = json!(uri));
            tracks
        }
        ("album", false) => album(&uri, cfg)?,
        ("artist", _) => return artist(&uri, cfg),
        _ => return playlist(&uri, id, api, cfg),
    };
    Ok(build(tracks, None, cfg))
}

#[cfg(test)]
mod tests {
    use super::hotp;

    #[test]
    fn hotp_matches_rfc_4226_vector() {
        assert_eq!(hotp(b"12345678901234567890", 1), "287082");
    }
}
