use crate::{cached, http, http_json, number, on, search, url_encode, Pace, Song, HOME, R};
use hmac::{Hmac, Mac};
use rayon::prelude::*;
use serde_json::{json, Value};
use sha1::Sha1;
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{Arc, LazyLock, Mutex, OnceLock},
    time::Duration,
};

const UA: (&str, &str) = (
    "User-Agent",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36",
);
const SECRETS: &str = "https://code.thetadev.de/ThetaDev/spotify-secrets/raw/branch/main/secrets/secretDict.json";
const QUERIES: [&str; 6] =
    ["getTrack", "getAlbum", "fetchPlaylist", "queryArtistDiscographyAll", "searchDesktop", "queryArtistOverview"];

static SESSION: Mutex<Option<Arc<Value>>> = Mutex::new(None);
static ALBUMS: LazyLock<Mutex<HashMap<String, Arc<Value>>>> = LazyLock::new(Default::default);
static DEEZER_ALBUMS: LazyLock<Mutex<HashMap<String, Arc<OnceLock<Value>>>>> = LazyLock::new(Default::default);
/// Deezer allows 50 requests in 5 seconds.
static DEEZER: Pace = Pace::new(110);

/// The web player's TOTP: HOTP-SHA1 of the 30-second step, keyed by the digits of the XOR-ed secret.
fn totp(secret: &[u8]) -> String {
    let digits = secret.iter().enumerate().map(|(i, byte)| (byte ^ (i % 33 + 9) as u8).to_string());
    let mut mac = Hmac::<Sha1>::new_from_slice(digits.collect::<String>().as_bytes()).expect("any key size works");
    mac.update(&(crate::now() / 30).to_be_bytes());
    let d = mac.finalize().into_bytes();
    let o = (d[19] & 15) as usize;
    format!("{:06}", (u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) & 0x7fff_ffff) % 1_000_000)
}

/// Persisted-query hashes from the web player's main bundle and its search and artist route chunks.
fn hashes() -> R<Value> {
    let get = |url: &str| -> R<String> { Ok(String::from_utf8(http(url, &[UA], None)?)?) };
    let html = get("https://open.spotify.com")?;
    let main = html.split('"').find(|s| s.contains("/web-player/web-player.") && s.ends_with(".js"));
    let js = get(main.ok_or("no web-player bundle")?)?;
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
    let chunks = chunks.filter_map(|c| get(&format!("https://open.spotifycdn.com/cdn/build/web-player/{c}")).ok());
    let all = chunks.collect::<String>() + &js;
    let found = QUERIES.iter().filter_map(|op| {
        Some((op.to_string(), json!(all.split_once(&format!("\"{op}\",\"query\",\""))?.1.get(..64)?)))
    });
    Ok(Value::Object(found.collect()))
}

/// The anonymous web-player token and query hashes, kept in `~/.spotdl/anon.json` until they expire.
fn session(rejected: Option<&str>) -> R<Arc<Value>> {
    let mut slot = SESSION.lock().map_err(|error| error.to_string())?;
    let path = HOME.join("anon.json");
    let disk = || serde_json::from_slice(&fs::read(&path).ok()?).ok().filter(|_| !on("no_cache")).map(Arc::new);
    let fresh = |state: &Arc<Value>| {
        state["exp"].as_u64() > Some(crate::now() * 1000 + 60_000)
            && rejected.is_none_or(|token| state["token"] != token)
    };
    if let Some(state) = slot.clone().or_else(disk).filter(fresh) {
        *slot = Some(state.clone());
        return Ok(state);
    }
    let secrets: HashMap<String, Vec<u8>> = http_json(SECRETS, &[UA], None)?;
    let (version, secret) =
        secrets.iter().max_by_key(|(k, _)| k.parse::<u32>().unwrap_or(0)).ok_or("no TOTP secret")?;
    let code = totp(secret);
    let url = format!("https://open.spotify.com/api/token?reason=init&productType=web-player&totp={code}&totpVer={version}&totpServer={code}");
    let token: Value = http_json(&url, &[UA], None)?;
    let token = token["accessToken"].as_str().map(|t| (t, &token["accessTokenExpirationTimestampMs"]));
    let (token, expires) = token.ok_or("Spotify token missing")?;
    let state = Arc::new(json!({"token": token, "exp": expires, "hashes": hashes()?}));
    if !on("no_cache") {
        fs::create_dir_all(&*HOME)?;
        fs::write(path, state.to_string())?;
    }
    *slot = Some(state.clone());
    Ok(state)
}

fn query(op: &str, variables: Value) -> R<Value> {
    let days = match op {
        "getTrack" if on("use_cache_file") => u64::MAX,
        "getTrack" | "getAlbum" | "queryArtistOverview" => 7,
        _ => 0,
    };
    cached(&format!("{op}{variables}"), days, || {
        let (mut rejected, mut last) = (None::<String>, String::new());
        for _ in 0..=number("max_retries") {
            let state = session(rejected.as_deref())?;
            let token = state["token"].as_str().unwrap_or_default();
            let hash = state["hashes"][op].as_str().ok_or("missing query hash")?;
            let extensions = json!({"persistedQuery": {"version": 1, "sha256Hash": hash}}).to_string();
            let (variables, extensions) = (url_encode(&variables.to_string()), url_encode(&extensions));
            let url = format!("https://api-partner.spotify.com/pathfinder/v1/query?operationName={op}&variables={variables}&extensions={extensions}");
            match http_json::<Value>(&url, &[UA, ("Authorization", &format!("Bearer {token}"))], None) {
                Ok(reply) if reply.get("data").is_some() => return Ok(reply),
                Ok(reply) => last = format!("{op}: {}", reply["errors"]),
                Err(error) => last = error.to_string(),
            }
            if ["401", "hash", "PersistedQuery"].iter().any(|word| last.contains(word)) {
                rejected = Some(token.into());
            }
        }
        Err(last.into())
    })
}

/// All items of a paged list (album tracks, playlist items, discography), with the list's own data.
fn pages(op: &str, uri: &str, list: &str, limit: usize) -> R<(Value, Vec<Value>)> {
    let page = |offset: usize| {
        let variables = json!({"uri": uri, "locale": "", "offset": offset, "limit": limit, "order": "DATE_DESC", "enableWatchFeedEntrypoint": false});
        let mut reply = query(op, variables)?;
        let items = reply.pointer_mut(&format!("{list}/items")).map(Value::take);
        R::Ok((reply, serde_json::from_value::<Vec<Value>>(items.unwrap_or_default()).unwrap_or_default()))
    };
    let (mut first, items) = page(0)?;
    let total = first.pointer(list).and_then(|v| v["totalCount"].as_u64()).ok_or(format!("{uri} unavailable"))?;
    let offsets: Vec<usize> = (limit..total as usize).step_by(limit).collect();
    let rest = offsets.into_par_iter().map(|offset| Ok(page(offset)?.1)).collect::<R<Vec<_>>>()?;
    let meta = first["data"][list.split('/').nth(2).unwrap_or_default()].take();
    Ok((meta, items.into_iter().chain(rest.into_iter().flatten()).collect()))
}

fn cover(value: &Value) -> Option<String> {
    let size =
        |image: &&Value| image["width"].as_u64().unwrap_or_default() * image["height"].as_u64().unwrap_or_default();
    let sources = [&value["images"], &value["images"]["items"], &value["coverArt"]["sources"], &value["sources"]];
    let images = sources.into_iter().filter_map(Value::as_array);
    images.into_iter().find_map(|images| images.iter().max_by_key(size)?["url"].as_str().map(String::from))
}

/// The URL of the artist's largest picture.
pub fn artist_image(artist_id: &str) -> Option<String> {
    let uri = format!("spotify:artist:{artist_id}");
    let reply = query("queryArtistOverview", json!({"uri": uri, "locale": "", "includePrerelease": true})).ok()?;
    cover(&reply["data"]["artistUnion"]["visuals"]["avatarImage"])
}

fn album_meta(uri: &str) -> R<Arc<Value>> {
    if let Some(album) = ALBUMS.lock().map_err(|error| error.to_string())?.get(uri) {
        return Ok(album.clone());
    }
    let mut album =
        query("getAlbum", json!({"uri": uri, "locale": "", "offset": 0, "limit": 1}))?["data"]["albumUnion"].take();
    let keep = ["name", "artists", "date", "discs", "tracksV2", "label", "coverArt", "copyright", "type"];
    album.as_object_mut().ok_or("Spotify album metadata is missing")?.retain(|key, _| keep.contains(&key.as_str()));
    Ok(ALBUMS.lock().map_err(|error| error.to_string())?.entry(uri.into()).or_insert(Arc::new(album)).clone())
}

/// Songs from web-player tracks; `list` carries the playlist fields that every song shares.
fn build(tracks: Vec<Value>, list: Song) -> R<Vec<Song>> {
    let album_uri = |track: &Value| track["albumOfTrack"]["uri"].as_str().unwrap_or_default().to_string();
    let uris: HashSet<String> = tracks.iter().map(album_uri).filter(|uri| !uri.is_empty()).collect();
    let albums: HashMap<String, Arc<Value>> =
        uris.into_par_iter().map(|uri| Ok((uri.clone(), album_meta(&uri)?))).collect::<R<_>>()?;
    let (length, listed) = (tracks.len(), list.list_name.is_some());
    let playable =
        tracks.iter().enumerate().filter(|(_, t)| t["uri"].as_str().is_some_and(|u| u.starts_with("spotify:track:")));
    let song = |(index, track): (usize, &Value)| {
        let uri = album_uri(track);
        let album = albums.get(&uri).map_or(&Value::Null, |album| album.as_ref());
        let credits = [&track["firstArtist"]["items"], &track["otherArtists"]["items"], &track["artists"]["items"]];
        let artists = credits.into_iter().flat_map(|items| items.as_array().into_iter().flatten());
        let artists: Vec<String> = artists.filter_map(|a| a["profile"]["name"].as_str().map(String::from)).collect();
        let date = album["date"]["isoString"].as_str().and_then(|date| date.get(..10)).unwrap_or_default().to_string();
        let id = |value: &Value| value.as_str().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
        let duration = ["duration", "trackDuration"].iter().find_map(|key| track[key]["totalMilliseconds"].as_f64());
        let song_id = id(&track["uri"]);
        Song {
            name: track["name"].as_str().unwrap_or_default().into(),
            artist: artists.first().cloned().unwrap_or_default(),
            artists,
            album_artist: album["artists"]["items"][0]["profile"]["name"].as_str().unwrap_or_default().into(),
            artist_id: id(&album["artists"]["items"][0]["uri"]),
            disc_number: track["discNumber"].as_u64().unwrap_or(1) as u32,
            disc_count: album["discs"]["totalCount"].as_u64().unwrap_or(1) as u32,
            album_name: album["name"].as_str().unwrap_or_default().into(),
            duration: duration.unwrap_or_default() / 1000.0,
            year: date.get(..4).and_then(|year| year.parse().ok()).unwrap_or_default(),
            date,
            track_number: track["trackNumber"].as_u64().unwrap_or(1) as u32,
            tracks_count: album["tracksV2"]["totalCount"].as_u64().unwrap_or(1) as u32,
            url: format!("https://open.spotify.com/track/{song_id}"),
            song_id,
            explicit: track["contentRating"]["label"] == "EXPLICIT",
            publisher: album["label"].as_str().unwrap_or_default().into(),
            cover_url: cover(album),
            copyright_text: album["copyright"]["items"][0]["text"].as_str().map(String::from),
            popularity: track["playcount"].as_str().and_then(|count| count.parse().ok()),
            album_id: id(&json!(uri)),
            album_type: album["type"].as_str().unwrap_or_default().to_lowercase(),
            list_position: listed.then_some(index + 1),
            list_length: listed.then_some(length),
            ..list.clone()
        }
    };
    Ok(playable.map(song).collect())
}

fn album(uri: &str) -> R<Vec<Value>> {
    let (meta, items) = pages("getAlbum", uri, "/data/albumUnion/tracksV2", 343)?;
    ALBUMS.lock().map_err(|error| error.to_string())?.insert(uri.into(), Arc::new(meta));
    let track = |mut item: Value| {
        item["track"]["albumOfTrack"]["uri"] = json!(uri);
        item["track"].take()
    };
    Ok(items.into_iter().map(track).collect())
}

/// `getTrack` has no disc number; on multi-disc albums the album's track list has it.
fn track(uri: &str) -> R<Value> {
    let mut track = query("getTrack", json!({"uri": uri}))?["data"]["trackUnion"].take();
    let album_uri = track["albumOfTrack"]["uri"].as_str().unwrap_or_default().to_string();
    if !album_uri.is_empty() && album_meta(&album_uri)?["discs"]["totalCount"].as_u64().unwrap_or(1) > 1 {
        let tracks = album(&album_uri)?;
        if let Some(disc) = tracks.iter().find(|item| item["uri"] == uri).map(|item| &item["discNumber"]) {
            track["discNumber"] = disc.clone();
        }
    }
    Ok(track)
}

fn artist(uri: &str) -> R<Vec<Song>> {
    let releases = pages("queryArtistDiscographyAll", uri, "/data/artistUnion/discography/all", 50)?.1;
    let mut releases: Vec<&Value> = releases.iter().map(|release| &release["releases"]["items"][0]).collect();
    // Releases in this order: albums, EPs, singles, then compilations; originals before reissues. Albums and EPs
    // are kept whole; singles and compilations add only the songs that no earlier release has.
    let kind =
        |release: &Value| ["ALBUM", "EP", "SINGLE", "COMPILATION"].iter().position(|kind| release["type"] == *kind);
    let date = |release: &Value| release["date"]["isoString"].as_str().unwrap_or_default().to_string();
    releases
        .sort_by_key(|release| (kind(release).unwrap_or(4), date(release), release["tracks"]["totalCount"].as_u64()));
    let mut seen = HashSet::new();
    releases.retain(|release| release["uri"].as_str().is_some_and(|uri| seen.insert(uri.to_string())));
    let uris: Vec<&str> = releases.iter().filter_map(|release| release["uri"].as_str()).collect();
    let tracks = uris.into_par_iter().map(album).collect::<R<Vec<_>>>()?;
    let albums = tracks.into_iter().map(|tracks| build(tracks, Song::default())).collect::<R<Vec<_>>>()?;
    let key = |song: &Song| (search::versionless(&song.name).to_lowercase(), song.artists.clone());
    // A bigger edition of the same album (same base title, or one that starts with it) that has at least 90% of a smaller edition's songs
    // is kept whole, and the smaller edition is skipped; of two editions with the same songs, the first is
    // kept. "BULLY - DELUXE" replaces "BULLY", even though one of its songs gained a featured artist. Titles
    // are compared without featured artists and version notes, so "Rubber Soul (Super Deluxe)" with its
    // "(2026 Mix)" songs replaces the 2009 remaster.
    let titles: Vec<HashSet<String>> =
        albums.iter().map(|songs| songs.iter().map(|song| search::edition_title(&song.name)).collect()).collect();
    let title = |release: &Value| base_title(release["name"].as_str().unwrap_or_default()).to_lowercase();
    // "OK Computer OKNOTOK 1997 2017", "KID A MNESIA" and "Wish You Were Here 50" are editions of the albums whose
    // titles they start with.
    let same_title = |a: &str, b: &str| {
        let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        long.strip_prefix(short).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    let replaces = |j: usize, i: usize| {
        let shared = titles[i].intersection(&titles[j]).count();
        j != i
            && same_title(&title(releases[j]), &title(releases[i]))
            && (titles[j].len() > titles[i].len() || (titles[j].len() == titles[i].len() && j < i))
            && shared * 10 >= titles[i].len() * 9
    };
    let n = releases.len();
    let mut kept: Vec<usize> = (0..n).filter(|&i| !(0..n).any(|j| replaces(j, i))).collect();
    // The bigger edition takes the place of the first edition it replaces, so that no release in between takes
    // its songs.
    kept.sort_by_key(|&j| (0..n).filter(|&i| i == j || replaces(j, i)).min());
    // Albums and EPs keep every track, also songs that another album or EP has: the "She's A Riot" EP keeps its
    // title track, which is also on "Learn To Exist". Singles and compilations add only new titles, not their
    // edits of album songs.
    let mut seen: HashSet<(String, Vec<String>)> = HashSet::new();
    let mut songs = Vec::new();
    for i in kept {
        let whole = kind(releases[i]).is_some_and(|kind| kind < 2);
        let new: Vec<&Song> = albums[i].iter().filter(|song| whole || !seen.contains(&key(song))).collect();
        seen.extend(new.iter().map(|song| key(song)));
        songs.extend(new.into_iter().cloned());
    }
    Ok(songs)
}

fn playlist(uri: &str) -> R<Vec<Song>> {
    let (meta, items) = pages("fetchPlaylist", uri, "/data/playlistV2/content", 343)?;
    let list = Song {
        list_name: Some(meta["name"].as_str().unwrap_or_default().into()),
        list_url: Some(format!("https://open.spotify.com/playlist/{}", uri.rsplit(':').next().unwrap_or_default())),
        extra: crate::Extra {
            list_author: meta["ownerV2"]["data"]["name"].as_str().map(String::from),
            list_cover_url: cover(&meta),
            ..Default::default()
        },
        ..Song::default()
    };
    build(items.into_iter().map(|mut item| item["itemV2"]["data"].take()).collect(), list)
}

/// The first Spotify result for free text; `album:`, `playlist:` and `artist:` pick the result type.
fn search(text: &str) -> R<String> {
    let prefix = ["album:", "playlist:", "artist:"]
        .iter()
        .find_map(|kind| Some((&kind[..kind.len() - 1], text.strip_prefix(kind)?)));
    let (kind, term) = prefix.map_or(("track", text.replace(" - ", " ")), |(kind, term)| (kind, term.trim().into()));
    let variables = json!({"searchTerm": term, "offset": 0, "limit": 10, "numberOfTopResults": 5, "includeAudiobooks": false, "includeArtistHasConcertsField": false, "includePreReleases": false, "includeLocalConcertsField": false});
    let results = query("searchDesktop", variables)?;
    let list = if kind == "track" || kind == "album" { format!("{kind}sV2") } else { format!("{kind}s") };
    let first = &results["data"]["searchV2"][list]["items"][0];
    let name = first["item"]["data"]["name"].as_str().unwrap_or_default();
    if kind == "track" && search::ratio(&term, name) < 25.0 {
        return Err("no Spotify results".into());
    }
    let uri = first["item"]["data"]["uri"].as_str().or(first["data"]["uri"].as_str());
    Ok(uri.ok_or("no Spotify results")?.into())
}

/// `youtube-url|spotify-url`: the Spotify metadata with that video, or a YouTube playlist matched by position.
fn paired(youtube: &str, spotify: &str) -> R<Vec<Song>> {
    let songs = resolve(spotify)?;
    if !youtube.contains("playlist?list=") && !youtube.contains("browse/VLPL") {
        return Ok(songs.into_iter().map(|song| Song { download_url: Some(youtube.into()), ..song }).collect());
    }
    let url = youtube.replacen("://www.youtube.com/", "://music.youtube.com/", 1).replacen(
        "://youtube.com/",
        "://music.youtube.com/",
        1,
    );
    let found = search::source(&url, false)?;
    if songs.len() != found.len() {
        return Err(
            format!("YouTube ({}) and Spotify ({}) lists have different lengths", found.len(), songs.len()).into()
        );
    }
    Ok(songs.into_iter().zip(found).map(|(song, video)| search::with_video(song, video)).collect())
}

/// Songs for one query: a Spotify URL or URI, a save file, a YouTube or SoundCloud URL, or search text.
pub fn resolve(text: &str) -> R<Vec<Song>> {
    if text.ends_with(".spotdl") {
        let mut file: Value = serde_json::from_slice(&fs::read(text)?)?;
        return Ok(serde_json::from_value(file.get_mut("songs").map(Value::take).unwrap_or(file))?);
    }
    if let Some((youtube, spotify)) = text.split_once('|') {
        return paired(youtube, spotify);
    }
    if ["youtube.com/", "youtu.be/", "soundcloud.com/"].iter().any(|host| text.contains(host)) {
        return search::source(text, !text.contains("soundcloud.com/"));
    }
    let text = match text.contains("spotify.link") {
        true => ureq::ResponseExt::get_uri(&crate::AGENT.get(text).call()?).to_string(),
        false => text.into(),
    };
    let link = text.split('?').next().unwrap_or_default().trim_end_matches('/');
    let uri = ["track", "album", "playlist", "artist"].iter().find_map(|kind| {
        let id = link.split_once(&format!("/{kind}/")).or(link.split_once(&format!(":{kind}:")))?.1;
        Some(format!("spotify:{kind}:{}", id.split('/').next()?))
    });
    let uri = match uri {
        Some(uri) => uri,
        None => search(&text)?,
    };
    match uri.split(':').nth(1).unwrap_or_default() {
        "track" => build(vec![track(&uri)?], Song::default()),
        "album" => build(album(&uri)?, Song::default()),
        "artist" => artist(&uri),
        _ => playlist(&uri),
    }
}

fn deezer(url: &str) -> R<Value> {
    for attempt in 0..6 {
        DEEZER.wait();
        let reply: Value = http_json(url, &[], None)?;
        if reply["error"]["code"] != 4 {
            return Ok(reply);
        }
        std::thread::sleep(Duration::from_secs(1 << attempt));
    }
    Err("Deezer rate limit".into())
}

/// The title before a " (Deluxe Edition)", " [Remastered]" or " - Single" note.
fn base_title(title: &str) -> &str {
    [" (", " [", " - "].iter().filter_map(|note| title.find(note)).min().map_or(title, |end| &title[..end])
}

fn genres(album: &Value) -> Vec<Value> {
    album["genres"]["data"].as_array().into_iter().flatten().map(|genre| genre["name"].clone()).collect()
}

/// Deezer's copy of the song's album: genres, UPC, label, release date and track ISRCs. Deezer's search misses
/// some albums and often names an edition differently ("Fever Ray" for "Fever Ray (Deluxe)"), so the album is
/// also looked for in the artist's discography. When no album matches, the genres come from the album that has
/// the song, or else from the artist's most common genre.
fn deezer_album(song: &Song) -> R<Value> {
    cached(&format!("deezer4:{}", song.album_id), 7, || {
        let same = |a: &str, b: &Value| {
            search::ratio(&search::slug(a, "-"), &search::slug(b.as_str().unwrap_or_default(), "-")) > 80.0
        };
        let list = |url: &str| -> R<Vec<Value>> {
            Ok(serde_json::from_value(deezer(url)?["data"].take()).unwrap_or_default())
        };
        let find =
            |kind: &str, query: &str| list(&format!("https://api.deezer.com/search/{kind}?q={}", url_encode(query)));
        let title = |hit: &Value| base_title(hit["title"].as_str().unwrap_or_default()).to_string();
        let pick = |albums: &[Value]| {
            let full = albums.iter().find(|hit| same(&song.album_name, &hit["title"]));
            full.or_else(|| albums.iter().find(|hit| same(base_title(&song.album_name), &json!(title(hit)))))
                .and_then(|album| album["id"].as_u64())
        };
        let hits = find("album", &format!("{} {}", song.album_artist, base_title(&song.album_name)))?;
        let hits: Vec<Value> =
            hits.into_iter().filter(|hit| same(&song.album_artist, &hit["artist"]["name"])).collect();
        let mut discography = None;
        let mut id = pick(&hits);
        if id.is_none() {
            let artists = find("artist", &song.album_artist)?;
            if let Some(artist) = artists.iter().find(|hit| same(&song.album_artist, &hit["name"])) {
                let albums = list(&format!("https://api.deezer.com/artist/{}/albums?limit=500", artist["id"]))?;
                id = pick(&albums);
                discography = Some(albums);
            }
        }
        if let Some(id) = id {
            let details = deezer(&format!("https://api.deezer.com/album/{id}"))?;
            let tracks = deezer(&format!("https://api.deezer.com/album/{id}/tracks?limit=500"))?;
            let tracks = tracks["data"].as_array().into_iter().flatten().enumerate().map(|(i, track)| {
                let (disc, position) = (track["disk_number"].as_u64().unwrap_or(1), track["track_position"].as_u64());
                let data =
                    json!({"isrc": track["isrc"], "duration": track["duration"], "explicit": track["explicit_lyrics"]});
                (format!("{disc}:{}", position.unwrap_or(i as u64 + 1)), data)
            });
            let tracks: serde_json::Map<String, Value> = tracks.collect();
            let (upc, label, date) = (&details["upc"], &details["label"], &details["release_date"]);
            return Ok(
                json!({"genres": genres(&details), "upc": upc, "label": label, "release_date": date, "tracks": tracks}),
            );
        }
        let query = format!("artist:\"{}\" track:\"{}\"", song.artist, search::versionless(&song.name));
        let tracks = find("track", &query)?;
        let same_song = |track: &&Value| {
            let length = (track["duration"].as_f64().unwrap_or_default() - song.duration).abs();
            same(&song.artist, &track["artist"]["name"]) && length <= 3.0
        };
        if let Some(id) = tracks.iter().find(same_song).and_then(|track| track["album"]["id"].as_u64()) {
            return Ok(json!({"genres": genres(&deezer(&format!("https://api.deezer.com/album/{id}"))?)}));
        }
        let mut counts = HashMap::new();
        for genre in discography.iter().flatten().filter_map(|album| album["genre_id"].as_i64()).filter(|id| *id > 0) {
            *counts.entry(genre).or_insert(0) += 1;
        }
        Ok(match counts.into_iter().max_by_key(|(id, count)| (*count, -id)) {
            Some((id, _)) => json!({"genres": [deezer(&format!("https://api.deezer.com/genre/{id}"))?["name"]]}),
            None => Value::Null,
        })
    })
}

/// Adds Deezer's genres, label, UPC, original date, ISRC, BPM and gain.
pub fn enrich(song: &mut Song) {
    let cell = DEEZER_ALBUMS
        .lock()
        .ok()
        .filter(|_| !song.album_id.is_empty())
        .map(|mut map| map.entry(song.album_id.clone()).or_default().clone());
    let album = cell.as_ref().map_or(&Value::Null, |cell| cell.get_or_init(|| deezer_album(song).unwrap_or_default()));
    if !album.is_null() {
        song.genres =
            album["genres"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        song.extra.upc = album["upc"].as_str().unwrap_or_default().into();
        song.publisher = album["label"].as_str().map_or(song.publisher.clone(), String::from);
        let released = album["release_date"].as_str().unwrap_or_default();
        song.extra.original_date = match released {
            "" => String::new(),
            _ if song.date.is_empty() || released < song.date.as_str() => released.into(),
            _ => song.date.clone(),
        };
        // The album is found by title, so it can be another edition (clean, remaster, other order): only trust
        // the track at this position when it has the same length and explicit flag.
        if song.isrc.as_deref().is_none_or(str::is_empty) {
            let track = &album["tracks"][format!("{}:{}", song.disc_number, song.track_number)];
            let same = track["duration"].as_f64().is_some_and(|length| (length - song.duration).abs() <= 3.0)
                && track["explicit"].as_bool().is_none_or(|explicit| explicit == song.explicit);
            song.isrc = track["isrc"].as_str().filter(|isrc| same && !isrc.is_empty()).map(String::from);
        }
    }
    if let Some(isrc) =
        song.isrc.clone().filter(|isrc| !isrc.is_empty() && song.extra.bpm == 0.0 && song.extra.gain == 0.0)
    {
        if let Ok(track) = deezer(&format!("https://api.deezer.com/track/isrc:{}", url_encode(&isrc))) {
            song.extra.bpm = track["bpm"].as_f64().unwrap_or_default();
            song.extra.gain = track["gain"].as_f64().unwrap_or_default();
        }
    }
}
