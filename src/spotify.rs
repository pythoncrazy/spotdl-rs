use crate::{cached, http, http_json, number, on, search, url_encode, Pace, Song, HOME, R};
use hmac::{Hmac, Mac};
use rayon::prelude::*;
use serde_json::{json, Value};
use sha1::Sha1;
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{Arc, LazyLock, Mutex, PoisonError},
    time::Duration,
};

const UA: (&str, &str) = (
    "User-Agent",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36",
);
const SECRETS: &str = "https://code.thetadev.de/ThetaDev/spotify-secrets/raw/branch/main/secrets/secretDict.json";
/// Seconds by which two copies of one song can differ in length (a reissue, a remaster).
const SAME_LENGTH: f64 = 5.0;
const QUERIES: [&str; 6] =
    ["getTrack", "getAlbum", "fetchPlaylist", "queryArtistDiscographyAll", "searchDesktop", "queryArtistOverview"];

static SESSION: Mutex<Option<Arc<Value>>> = Mutex::new(None);
static ALBUMS: LazyLock<Mutex<HashMap<String, Arc<Value>>>> = LazyLock::new(Default::default);
/// Deezer's album of each Spotify album, or how many times it failed and why.
type DeezerAlbum = Arc<Mutex<Result<Value, (u8, String)>>>;
static DEEZER_ALBUMS: LazyLock<Mutex<HashMap<String, DeezerAlbum>>> = LazyLock::new(Default::default);
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

/// Spotify dates some albums by year or month only and pads its isoString with 1 January ("1994-01-01T00:00:00Z"
/// for The Division Bell, out on 28 March 1994), so only the part its precision gives is the date.
fn release_date(date: &Value) -> String {
    let known = match date["precision"].as_str() {
        Some("YEAR") => 4,
        Some("MONTH") => 7,
        _ => 10,
    };
    date["isoString"].as_str().and_then(|iso| iso.get(..known)).unwrap_or_default().into()
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
        let date = release_date(&album["date"]);
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
    // Releases in this order: albums, EPs, singles, then compilations; originals before reissues. A song is kept
    // from its first release only, so remasters, reissues and singles add only the songs that are new.
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
    let key = |song: &Song| (song_key(&song.name), song.artists.clone());
    let names: Vec<&str> = releases.iter().map(|release| release["name"].as_str().unwrap_or_default()).collect();
    let kept = editions(&names, &albums);
    // Inside one release every track stays (takes, mixes); across releases a song is kept once. On albums and
    // EPs, songs with the same title and artists are the same song only when they are about as long: a reissue
    // may be a few seconds longer, but an artist's "Intro" on one album is not the "Intro" on another. Singles
    // and compilations add only new titles, not their edits of album songs.
    let mut seen: HashMap<(String, Vec<String>), Vec<f64>> = HashMap::new();
    let mut songs = Vec::new();
    let same = |a: f64, b: f64| (a - b).abs() <= SAME_LENGTH;
    for i in kept {
        let album = kind(releases[i]).is_some_and(|kind| kind < 2);
        let known = |song: &&Song| {
            seen.get(&key(song)).is_some_and(|lengths| !album || lengths.iter().any(|l| same(*l, song.duration)))
        };
        let new: Vec<&Song> = albums[i].iter().filter(|song| !known(song)).collect();
        for song in &new {
            seen.entry(key(song)).or_default().push(song.duration);
        }
        songs.extend(new.into_iter().cloned());
    }
    Ok(songs)
}

/// The releases to keep, in the order their songs are taken. A bigger edition of the same album (same base title,
/// or one that starts with it) that has at least 90% of a smaller edition's songs is kept whole, and the smaller
/// edition is skipped. "BULLY - DELUXE" replaces "BULLY", even though one of its songs gained a featured artist.
/// Titles are compared without featured artists and version notes, so "Rubber Soul (Super Deluxe)" with its
/// "(2026 Mix)" songs replaces the 2009 remaster.
fn editions(names: &[&str], albums: &[Vec<Song>]) -> Vec<usize> {
    let titles: Vec<HashSet<String>> =
        albums.iter().map(|songs| songs.iter().map(|song| search::edition_title(&song.name)).collect()).collect();
    let explicit: Vec<usize> = albums.iter().map(|songs| songs.iter().filter(|song| song.explicit).count()).collect();
    let title = |i: usize| base_title(names[i]).to_lowercase();
    // "OK Computer OKNOTOK 1997 2017", "KID A MNESIA" and "Wish You Were Here 50" are editions of the albums whose
    // titles they start with.
    let same_title = |a: &str, b: &str| {
        let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        long.strip_prefix(short).is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    // A clean edition never replaces one with explicit songs. Of two editions with the same songs, the explicit
    // one, then the one titled only with the album's title, then the first, is kept: "Animals (2018 Remix)",
    // which Spotify dates two days before "Animals", is not the album.
    let rank = |k: usize| (explicit[k] > 0, titles[k].len(), explicit[k], names[k] == base_title(names[k]));
    let replaces = |j: usize, i: usize| {
        let shared = titles[i].intersection(&titles[j]).count();
        // The clean edit's censored titles ("****** Wit The Wrong Nigga") and clean-only remixes leave it sharing
        // only most of its songs with the explicit edition, which still replaces it.
        let edit = explicit[j] > 0 && explicit[i] == 0;
        j != i
            && same_title(&title(j), &title(i))
            && (rank(j), i) > (rank(i), j)
            && if edit { shared * 2 > titles[i].len() } else { shared * 10 >= titles[i].len() * 9 }
    };
    let n = names.len();
    let mut kept: Vec<usize> = (0..n).filter(|&i| !(0..n).any(|j| replaces(j, i))).collect();
    // The bigger edition takes the place of the first edition it replaces, so that no release in between takes
    // its songs.
    kept.sort_by_key(|&j| (0..n).filter(|&i| i == j || replaces(j, i)).min());
    kept
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

/// The error of a Deezer reply (sent with HTTP 200), and whether asking again can help: 4 is over the quota and 700
/// "service busy". 800 "no data" is no error: the item is not on Deezer. Read as data, an error reply has no genres,
/// tracks or search hits, so the album would be taken for one Deezer does not have.
fn deezer_error(reply: &Value) -> Option<(bool, String)> {
    let error = &reply["error"];
    let code = error["code"].as_u64();
    (!error.is_null() && code != Some(800)).then(|| (matches!(code, Some(4 | 700)), error.to_string()))
}

fn deezer(url: &str) -> R<Value> {
    for attempt in 0..6 {
        DEEZER.wait();
        let reply: Value = http_json(url, &[], None)?;
        match deezer_error(&reply) {
            None => return Ok(reply),
            Some((false, error)) => return Err(format!("Deezer: {error} for {url}").into()),
            Some((true, _)) => std::thread::sleep(Duration::from_secs(1 << attempt)),
        }
    }
    Err(format!("Deezer: still busy for {url}").into())
}

/// The value in `cell`, fetched by the first song of the album that asks. A failure is not kept, so that a later
/// song asks again, but at most 3 times: a Deezer that is down is not asked once for every song.
fn fetch_once(cell: &Mutex<Result<Value, (u8, String)>>, fetch: impl FnOnce() -> R<Value>) -> Result<Value, String> {
    let mut cell = cell.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err((tries @ 0..3, _)) = *cell {
        *cell = fetch().map_err(|error| (tries + 1, error.to_string()));
    }
    cell.clone().map_err(|(_, error)| error)
}

/// The title that a song has on every release: without a " - Remastered 2009" suffix, and slugged, because box sets
/// and compilations write "Nowhere Man (First Version - Take 2)" as "Nowhere Man - First Version - Take 2". The
/// take, demo and mix words stay: "Take 2" and "Take 5" are other recordings.
fn song_key(name: &str) -> String {
    search::slug(search::versionless(name), "-")
}

/// The title before a " (Deluxe Edition)", " [Remastered]" or " - Single" note.
fn base_title(title: &str) -> &str {
    [" (", " [", " - "].iter().filter_map(|note| title.find(note)).min().map_or(title, |end| &title[..end])
}

fn genres(album: &Value) -> Vec<Value> {
    album["genres"]["data"].as_array().into_iter().flatten().map(|genre| genre["name"].clone()).collect()
}

/// Whether the title has a note such as " (Remix)", " [SPED UP]" or " (Piano Version)": the release is another
/// recording than the one titled without the note.
fn version_note(title: &str) -> bool {
    let words = search::slug(&title[base_title(title).len()..], "-");
    let other = ["piano", "orchestral", "stripped", "unplugged", "demo"];
    words.split('-').any(|word| search::altered(word) || other.contains(&word))
}

/// The ids of the Deezer albums that can be the Spotify album, best first, each with whether it fits in every
/// way. A single, its album, the clean and explicit editions and a deluxe edition share a title, so hits rank by
/// the same title, then a title close to it, then the same record type, track count (unknown in a discography)
/// and explicit flag, then Deezer's order. A hit with only the base title ("Fever Ray" for "Fever Ray (Deluxe)")
/// must be the same kind of release, and is never taken for a remix or sped-up release of the base title's song.
fn ranked(hits: &[Value], title: &str, kind: &str, count: u64, explicit: bool) -> Vec<(u64, bool)> {
    let slug = |text: &str| search::slug(text, "-");
    let close = |a: &str, b: &str| search::ratio(&slug(a), &slug(b)) > 80.0;
    let mut scored: Vec<_> = hits
        .iter()
        .enumerate()
        .filter_map(|(i, hit)| {
            let name = hit["title"].as_str().unwrap_or_default();
            let (same_kind, same_count) =
                (hit["record_type"] == kind, hit["nb_tracks"].as_u64().is_none_or(|n| n == count));
            let full = close(title, name);
            let base = same_kind && same_count && !version_note(title) && close(base_title(title), base_title(name));
            let fits = (slug(title) == slug(name), full, same_kind, same_count, hit["explicit_lyrics"] == explicit);
            Some((fits, std::cmp::Reverse(i), hit["id"].as_u64().filter(|_| full || base)?))
        })
        .collect();
    scored.sort_by(|a, b| b.cmp(a));
    let mut seen = HashSet::new();
    let best = scored.into_iter().filter(|(.., id)| seen.insert(*id));
    best.map(|(fits, _, id)| (id, fits == (true, true, true, true, true))).collect()
}

/// Whether Deezer's tracks are the Spotify album's: at least half the positions of the longer list have a song as
/// long (within 3 s). A title can still be another release's: a 2-song single's album, another recording of
/// an opera.
fn agrees(lengths: &HashMap<String, f64>, tracks: &serde_json::Map<String, Value>) -> bool {
    let same = lengths.iter().filter(|(position, length)| {
        tracks.get(*position).and_then(|track| track["duration"].as_f64()).is_some_and(|d| (d - **length).abs() <= 3.0)
    });
    same.count() * 2 >= lengths.len().max(tracks.len())
}

/// Deezer's copy of the song's album: genres, UPC, label, release date and its tracks' titles and ISRCs. Deezer's
/// search misses some albums and often names an edition differently ("Fever Ray" for "Fever Ray (Deluxe)") or
/// returns only the clean edition (GNX), so unless a search hit fits in every way the album is also looked for in
/// the artist's discography. An album whose tracks are not the Spotify album's gives only its genres. When no album
/// matches, the genres come from the album that has the song, or else from the artist's most common genre.
fn deezer_album(song: &Song) -> R<Value> {
    cached(&format!("deezer6:{}", song.album_id), 7, || {
        let same = |a: &str, b: &Value| {
            search::ratio(&search::slug(a, "-"), &search::slug(b.as_str().unwrap_or_default(), "-")) > 80.0
        };
        let list = |url: &str| -> R<Vec<Value>> {
            Ok(serde_json::from_value(deezer(url)?["data"].take()).unwrap_or_default())
        };
        let find =
            |kind: &str, query: &str| list(&format!("https://api.deezer.com/search/{kind}?q={}", url_encode(query)));
        let spotify = album(&format!("spotify:album:{}", song.album_id))?;
        let length = |track: &Value| {
            ["duration", "trackDuration"].iter().find_map(|key| track[key]["totalMilliseconds"].as_f64())
        };
        let lengths: HashMap<String, f64> = spotify
            .iter()
            .map(|track| {
                let position = format!("{}:{}", track["discNumber"].as_u64().unwrap_or(1), track["trackNumber"]);
                (position, length(track).unwrap_or_default() / 1000.0)
            })
            .collect();
        let explicit = spotify.iter().any(|track| track["contentRating"]["label"] == "EXPLICIT");
        let kind = if song.album_type == "compilation" { "compile" } else { &song.album_type };
        let rank = |albums: &[Value]| ranked(albums, &song.album_name, kind, lengths.len() as u64, explicit);
        let hits = find("album", &format!("{} {}", song.album_artist, base_title(&song.album_name)))?;
        let hits: Vec<Value> =
            hits.into_iter().filter(|hit| same(&song.album_artist, &hit["artist"]["name"])).collect();
        let mut discography = None;
        let mut ids = rank(&hits);
        if !ids.first().is_some_and(|(_, fits)| *fits) {
            let artists = find("artist", &song.album_artist)?;
            if let Some(artist) = artists.iter().find(|hit| same(&song.album_artist, &hit["name"])) {
                let albums = list(&format!("https://api.deezer.com/artist/{}/albums?limit=500", artist["id"]))?;
                ids = rank(&[hits, albums.clone()].concat());
                discography = Some(albums);
            }
        }
        for &(id, _) in ids.iter().take(3) {
            let tracks = deezer(&format!("https://api.deezer.com/album/{id}/tracks?limit=500"))?;
            let tracks = tracks["data"].as_array().into_iter().flatten().enumerate().map(|(i, track)| {
                let (disc, position) = (track["disk_number"].as_u64().unwrap_or(1), track["track_position"].as_u64());
                let (isrc, duration, explicit) = (&track["isrc"], &track["duration"], &track["explicit_lyrics"]);
                let data = json!({"title": track["title"], "isrc": isrc, "duration": duration, "explicit": explicit});
                (format!("{disc}:{}", position.unwrap_or(i as u64 + 1)), data)
            });
            let tracks: serde_json::Map<String, Value> = tracks.collect();
            if agrees(&lengths, &tracks) {
                let details = deezer(&format!("https://api.deezer.com/album/{id}"))?;
                let (upc, label, date) = (&details["upc"], &details["label"], &details["release_date"]);
                return Ok(
                    json!({"genres": genres(&details), "upc": upc, "label": label, "release_date": date, "tracks": tracks}),
                );
            }
        }
        // Another release of the same title by the artist still has the album's genres, but not its UPC, label or
        // date.
        if let Some((id, _)) = ids.first() {
            return Ok(json!({"genres": genres(&deezer(&format!("https://api.deezer.com/album/{id}"))?)}));
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

/// The release date and original date from Spotify's date and Deezer's date of the album. Deezer pads dates it
/// knows only by year with 1 January too (29 of 894 cached albums, over ten times the real share), so that is read as
/// the year. Deezer's album is found by title, so it can be a reissue: its day is taken for a Spotify date of
/// only a year or month when it is in that year or month. Dates are compared only as far as both go, so a year
/// is not earlier than a day in it. Deezer dates albums it has no date for 1900-01-01 (or 0000-00-00): no date.
fn dates(spotify: &str, deezer: &str) -> (String, String) {
    let deezer = if deezer.get(..4).is_some_and(|year| year > "1900") { deezer } else { "" };
    let deezer = deezer.strip_suffix("-01-01").unwrap_or(deezer);
    let date = if deezer.len() > spotify.len() && !spotify.is_empty() && deezer.starts_with(spotify) {
        deezer
    } else {
        spotify
    };
    let shared = date.len().min(deezer.len());
    let original = match deezer {
        "" => "",
        _ if date.is_empty() || deezer.get(..shared) < date.get(..shared) => deezer,
        _ => date,
    };
    (date.into(), original.into())
}

/// Adds Deezer's genres, label (when Spotify has none), UPC, original date, ISRC, BPM and gain. An error is Deezer's failure (not a song
/// that Deezer does not have): the song then has only what Spotify gives.
pub fn enrich(song: &mut Song) -> Result<(), String> {
    let release = song.extra.release.as_deref().cloned().unwrap_or_else(|| song.clone());
    // A save file written with a playlist option has only the playlist's album fields: a search by them finds
    // no album (and would be cached as the real album's), and the track at the playlist position is another song.
    let listed = release.list_name.as_ref() == Some(&release.album_name)
        && release.list_position == Some(release.track_number as usize)
        && release.disc_number == 1;
    let album = if release.album_id.is_empty() || listed {
        Ok(Value::Null)
    } else {
        let mut albums = DEEZER_ALBUMS.lock().unwrap_or_else(PoisonError::into_inner);
        let cell = albums.entry(song.album_id.clone()).or_insert_with(|| Arc::new(Mutex::new(Err((0, "".into())))));
        let cell = cell.clone();
        drop(albums);
        fetch_once(&cell, || deezer_album(&release))
    };
    let mut result = album.as_ref().map(drop).map_err(String::clone);
    let album = album.unwrap_or_default();
    if !album.is_null() {
        song.genres =
            album["genres"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        song.extra.upc = album["upc"].as_str().unwrap_or_default().into();
        // Spotify's label is the downloaded release's; Deezer names labels its own way ("Young" for "Young
        // Turks") and its album can be another edition, so its label only fills a gap.
        if song.publisher.is_empty() {
            song.publisher = album["label"].as_str().unwrap_or_default().into();
        }
        (song.date, song.extra.original_date) = dates(&song.date, album["release_date"].as_str().unwrap_or_default());
        // The album is found by title, so it can be another edition (clean, remaster, other order): only trust
        // the track at this position when it has the same length and explicit flag. Deezer may number the album
        // otherwise (M83's two discs as one, a deluxe edition's bonus songs in between): then only the one track
        // that also has the song's full title, version note included, since an instrumental, remix or live take
        // is as long as the song and has its own ISRC.
        if song.isrc.as_deref().is_none_or(str::is_empty) {
            let same = |track: &Value| {
                track["duration"].as_f64().is_some_and(|length| (length - song.duration).abs() <= 3.0)
                    && track["explicit"].as_bool().is_none_or(|explicit| explicit == song.explicit)
            };
            let track = &album["tracks"][format!("{}:{}", release.disc_number, release.track_number)];
            let track = if same(track) {
                Some(track)
            } else {
                let title = search::slug(&release.name, "-");
                let tracks = album["tracks"].as_object().into_iter().flat_map(|tracks| tracks.values());
                let mut titled = tracks.filter(|track| {
                    same(track) && track["title"].as_str().is_some_and(|name| search::slug(name, "-") == title)
                });
                titled.next().filter(|_| titled.next().is_none())
            };
            song.isrc =
                track.and_then(|track| track["isrc"].as_str()).filter(|isrc| !isrc.is_empty()).map(String::from);
        }
    }
    if let Some(isrc) =
        song.isrc.clone().filter(|isrc| !isrc.is_empty() && song.extra.bpm == 0.0 && song.extra.gain == 0.0)
    {
        match deezer(&format!("https://api.deezer.com/track/isrc:{}", url_encode(&isrc))) {
            Ok(track) => {
                song.extra.bpm = track["bpm"].as_f64().unwrap_or_default();
                song.extra.gain = track["gain"].as_f64().unwrap_or_default();
            }
            Err(error) => result = result.and(Err(error.to_string())),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{agrees, dates, deezer_error, editions, enrich, fetch_once, ranked, release_date, song_key};
    use crate::Song;
    use serde_json::json;

    #[test]
    fn a_song_has_one_key_however_a_release_punctuates_its_title() {
        let same = |a, b| assert_eq!(song_key(a), song_key(b), "{a} / {b}");
        // Anthology 3 and the Anthology Collection: the same recordings.
        same(
            "Happiness Is A Warm Gun (Esher Demo With False Start - Remastered)",
            "Happiness Is A Warm Gun - Esher Demo With False Start - Remastered",
        );
        same("Nowhere Man (First Version - Take 2)", "Nowhere Man - First Version - Take 2");
        same("I Will (Take 1)", "I Will - Take 1");
        same("Day Tripper (Songwriting Work Tape)", "Day Tripper [Songwriting Work Tape]");
        same("Something - Remastered 2009", "Something");
        // Other takes are other songs.
        assert_ne!(song_key("I Will (Take 1)"), song_key("I Will (Take 2)"));
        assert_ne!(song_key("Nowhere Man (First Version - Take 2)"), song_key("Nowhere Man - Take 5"));
    }

    #[test]
    fn deezer_albums_rank_by_title_kind_count_and_explicit_flag_and_must_have_the_same_tracks() {
        let hit = |id: u64, title: &str, kind: &str, count: Option<u64>, explicit: bool| json!({"id": id, "title": title, "record_type": kind, "nb_tracks": count, "explicit_lyrics": explicit});
        let tbhc = [
            hit(63203772, "Tranquility Base Hotel & Casino", "album", Some(11), true),
            hit(78773022, "Tranquility Base Hotel & Casino", "single", Some(2), true),
        ];
        let ids = |hits: &[_], title, kind, count, explicit| -> Vec<u64> {
            ranked(hits, title, kind, count, explicit).into_iter().map(|(id, _)| id).collect()
        };
        // The 2-song single is the single, not the album with its title.
        assert_eq!(ids(&tbhc, "Tranquility Base Hotel & Casino", "single", 2, true), [78773022, 63203772]);
        assert_eq!(ids(&tbhc, "Tranquility Base Hotel & Casino", "album", 11, true), [63203772, 78773022]);
        // The exact title before the deluxe edition that Deezer lists first.
        let gkmc = [
            hit(7203293, "good kid, m.A.A.d city (Deluxe)", "album", Some(16), true),
            hit(10222930, "good kid, m.A.A.d city", "album", Some(13), false),
        ];
        assert_eq!(ids(&gkmc, "good kid, m.A.A.d city", "album", 13, true)[0], 10222930);
        // GNX's search finds only the clean edition, which does not fit; the discography's explicit one does.
        let clean = [hit(674017741, "GNX", "album", Some(12), false)];
        assert_eq!(ranked(&clean, "GNX", "album", 12, true), [(674017741, false)]);
        let discography = [clean[0].clone(), hit(673968231, "GNX", "album", None, true)];
        assert_eq!(ranked(&discography, "GNX", "album", 12, true), [(673968231, true), (674017741, false)]);
        // Only the base title: a deluxe edition's standard album, but not a remix single's original.
        let original = [hit(1, "Big Dawgs", "single", Some(1), true)];
        assert_eq!(ids(&original, "Big Dawgs (Remix) [with A$AP Rocky]", "single", 1, true), [] as [u64; 0]);
        assert_eq!(ids(&original, "Praise You (Piano Version)", "single", 1, true), [] as [u64; 0]);
        assert_eq!(ids(&original, "Big Dawgs (Deluxe)", "single", 1, true), [1]);
        assert_eq!(ids(&original, "Big Dawgs (Deluxe)", "album", 1, true), [] as [u64; 0]);
        assert_eq!(ids(&original, "Big Dawgs (Deluxe)", "single", 3, true), [] as [u64; 0]);

        let lengths = |pairs: &[(&str, f64)]| pairs.iter().map(|(key, length)| (key.to_string(), *length)).collect();
        let tracks = |pairs: &[(&str, f64)]| {
            pairs.iter().map(|(key, length)| (key.to_string(), json!({"duration": length}))).collect()
        };
        let twelve: Vec<(String, f64)> = (1..=12).map(|i| (format!("1:{i}"), 200.0 + i as f64)).collect();
        let twelve: Vec<(&str, f64)> = twelve.iter().map(|(key, length)| (key.as_str(), *length)).collect();
        // A 2-song single is not its 12-song album, though both start with the same songs.
        assert!(!agrees(&lengths(&twelve[..2]), &tracks(&twelve)));
        assert!(agrees(&lengths(&twelve), &tracks(&twelve)));
        // A deluxe edition with two more songs, and lengths up to 3 s apart, are the same album.
        assert!(agrees(&lengths(&twelve[..10]), &tracks(&twelve)));
        assert!(agrees(&lengths(&[("1:1", 203.5), ("1:2", 202.0)]), &tracks(&twelve[..2])));
        // A remix single, or another recording, has songs of other lengths.
        assert!(!agrees(&lengths(&[("1:1", 230.0), ("1:2", 202.0), ("1:3", 260.0)]), &tracks(&twelve[..3])));
    }

    #[test]
    fn deezer_error_replies_are_failures_except_no_data() {
        let reply = |code: u64| json!({"error": {"type": "Exception", "message": "x", "code": code}});
        assert_eq!(deezer_error(&json!({"data": []})), None);
        // An item or ISRC that Deezer does not have.
        assert_eq!(deezer_error(&reply(800)), None);
        // Over the quota and "service busy" are asked again.
        assert!(deezer_error(&reply(4)).is_some_and(|(retry, _)| retry));
        assert!(deezer_error(&reply(700)).is_some_and(|(retry, _)| retry));
        assert!(deezer_error(&reply(600)).is_some_and(|(retry, _)| !retry));
    }

    #[test]
    fn a_failed_deezer_album_is_fetched_again_by_a_later_song_but_not_for_every_song() {
        let cell = std::sync::Mutex::new(Err((0, String::new())));
        let calls = std::cell::Cell::new(0);
        let fail = || {
            calls.set(calls.get() + 1);
            Err("timed out".into())
        };
        assert_eq!(fetch_once(&cell, fail), Err("timed out".into()));
        assert_eq!(fetch_once(&cell, || Ok(json!({"genres": ["Pop"]}))), Ok(json!({"genres": ["Pop"]})));
        assert_eq!(fetch_once(&cell, fail), Ok(json!({"genres": ["Pop"]})));
        let cell = std::sync::Mutex::new(Err((0, String::new())));
        for _ in 0..5 {
            assert!(fetch_once(&cell, fail).is_err());
        }
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn deezer_label_only_fills_in_a_missing_spotify_label() {
        let tag = |album_id: &str, label: &str| {
            let album = json!({"genres": ["Opera"], "label": "Chandos"});
            let cell = std::sync::Arc::new(std::sync::Mutex::new(Ok(album)));
            super::DEEZER_ALBUMS.lock().unwrap().insert(album_id.into(), cell);
            let mut song = Song { album_id: album_id.into(), publisher: label.into(), ..Song::default() };
            assert_eq!(enrich(&mut song), Ok(()));
            song.publisher
        };
        assert_eq!(tag("test-label-spotify", "Universal Music Group"), "Universal Music Group");
        assert_eq!(tag("test-label-missing", ""), "Chandos");
    }

    #[test]
    fn a_playlist_numbered_song_reads_deezer_by_its_own_album_position() {
        let tracks = json!({
            "1:2": {"isrc": "PLAYLIST2", "duration": 200.0, "explicit": false},
            "2:7": {"isrc": "ALBUM27", "duration": 201.0, "explicit": false},
        });
        let album = json!({"genres": ["Rock"], "tracks": tracks});
        let cell = std::sync::Arc::new(std::sync::Mutex::new(Ok(album)));
        super::DEEZER_ALBUMS.lock().unwrap().insert("test-playlist-numbering".into(), cell);
        let real = Song {
            album_id: "test-playlist-numbering".into(),
            album_name: "Album".into(),
            disc_number: 2,
            track_number: 7,
            duration: 200.0,
            list_name: Some("Mix".into()),
            list_position: Some(2),
            ..Song::default()
        };
        // As `--playlist-numbering` leaves it; a gain keeps the ISRC from being looked up online.
        let listed = Song { album_name: "Mix".into(), disc_number: 1, track_number: 2, ..real.clone() };
        let mut song = listed.clone();
        song.extra.release = Some(Box::new(real));
        song.extra.gain = -1.0;
        assert_eq!(enrich(&mut song), Ok(()));
        assert_eq!((song.isrc.as_deref(), song.genres), (Some("ALBUM27"), vec!["Rock".to_string()]));
        // Read from a save file, the real album fields are gone: nothing is read rather than another song's ISRC.
        let mut song = listed;
        song.extra.gain = -1.0;
        assert_eq!(enrich(&mut song), Ok(()));
        assert_eq!((song.isrc, song.genres.len()), (None, 0));
    }

    #[test]
    fn an_isrc_off_its_position_comes_only_from_the_one_track_of_the_same_title_length_and_flag() {
        // M83's two discs are one on Deezer, and a deluxe edition orders its songs otherwise.
        let tracks = json!({
            "1:2": {"title": "Overdue", "isrc": "VOCAL", "duration": 200.0, "explicit": false},
            "1:3": {"title": "Overdue (Instrumental)", "isrc": "INSTRUMENTAL", "duration": 201.0, "explicit": false},
            "1:4": {"title": "Twice", "isrc": "TWICE1", "duration": 180.0, "explicit": false},
            "1:5": {"title": "Twice", "isrc": "TWICE2", "duration": 181.0, "explicit": false},
            "1:22": {"title": "Outro", "isrc": "OUTRO", "duration": 247.0, "explicit": false},
        });
        let album = json!({"genres": ["Rock"], "tracks": tracks});
        let cell = std::sync::Arc::new(std::sync::Mutex::new(Ok(album)));
        super::DEEZER_ALBUMS.lock().unwrap().insert("test-isrc-by-title".into(), cell);
        let isrc = |name: &str, disc: u32, track: u32, duration: f64, explicit: bool| {
            let mut song = Song {
                album_id: "test-isrc-by-title".into(),
                name: name.into(),
                disc_number: disc,
                track_number: track,
                duration,
                explicit,
                ..Song::default()
            };
            song.extra.gain = -1.0;
            assert_eq!(enrich(&mut song), Ok(()));
            song.isrc
        };
        assert_eq!(isrc("Outro", 2, 11, 247.03, false).as_deref(), Some("OUTRO"));
        // Not the vocal take, as long, for the instrumental.
        assert_eq!(isrc("Overdue - Instrumental", 2, 2, 200.5, false).as_deref(), Some("INSTRUMENTAL"));
        // Nothing for a song of another length or flag, or for one of two tracks of its title.
        assert_eq!(isrc("Outro", 2, 11, 251.0, false), None);
        assert_eq!(isrc("Overdue", 2, 1, 200.0, true), None);
        assert_eq!(isrc("Twice", 2, 4, 180.5, false), None);
    }

    #[test]
    fn dates_keep_only_the_known_part_and_take_the_day_from_deezer() {
        let date = |iso: &str, precision: &str| release_date(&json!({"isoString": iso, "precision": precision}));
        assert_eq!(date("1994-01-01T00:00:00Z", "YEAR"), "1994");
        assert_eq!(date("1998-04-01T00:00:00Z", "MONTH"), "1998-04");
        assert_eq!(date("2000-01-01T00:00:00Z", "DAY"), "2000-01-01");
        assert_eq!(date("2020-05-01T12:00:00Z", "MINUTE"), "2020-05-01");
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        // Deezer's day of the year, but not its 1 January padding or another year's reissue.
        assert_eq!(dates("1998", "1998-04-20"), pair("1998-04-20", "1998-04-20"));
        assert_eq!(dates("1998", "1998-01-01"), pair("1998", "1998"));
        assert_eq!(dates("1994", "2011-09-26"), pair("1994", "1994"));
        assert_eq!(dates("1998-04", "1998-03-30"), pair("1998-04", "1998-03-30"));
        // A padded or year-only date is not earlier than a day in its year.
        assert_eq!(dates("1994-03-28", "1994-01-01"), pair("1994-03-28", "1994-03-28"));
        assert_eq!(dates("2000-01-01", "2000-01-01"), pair("2000-01-01", "2000-01-01"));
        assert_eq!(dates("2011-09-26", "1993-01-01"), pair("2011-09-26", "1993"));
        assert_eq!(dates("2011-09-26", "1994-03-28"), pair("2011-09-26", "1994-03-28"));
        assert_eq!(dates("", "1994-03-28"), pair("", "1994-03-28"));
        assert_eq!(dates("1994", ""), pair("1994", ""));
        // Deezer's placeholder for an album without a date (She Wants Revenge's Valleyheart) is no date.
        assert_eq!(dates("2011-05-23", "1900-01-01"), pair("2011-05-23", ""));
        assert_eq!(dates("2011", "0000-00-00"), pair("2011", ""));
    }

    #[test]
    fn remix_and_clean_editions_do_not_add_the_album_again() {
        let songs = |names: &[&str], explicit: bool| -> Vec<Song> {
            names.iter().map(|name| Song { name: name.to_string(), explicit, ..Song::default() }).collect()
        };
        // Spotify dates the remix edition first; the album titled only "Animals" is kept.
        let remix =
            songs(&["Pigs on the Wing 1 - 2018 Remix", "Dogs - 2018 Remix", "Pigs on the Wing 2 - 2018 Remix"], false);
        let original = songs(&["Pigs on the Wing 1", "Dogs", "Pigs on the Wing 2"], false);
        assert_eq!(editions(&["Animals (2018 Remix)", "Animals"], &[remix, original]), [1]);
        let remix = songs(&["Signs of Life - Remix 2019", "A New Machine Part 1 - Remix 2019"], false);
        let original = songs(&["Signs of Life", "A New Machine, Pt. 1"], false);
        let names = ["A Momentary Lapse of Reason (2019 Remix)", "A Momentary Lapse of Reason"];
        assert_eq!(editions(&names, &[remix, original]), [1]);
        // The clean edition, bigger here and sharing only 3 of its 5 titles, never replaces the explicit one; the
        // explicit one replaces it.
        let clean = [
            "Ballad Of A Dead Soulja - Album Version (Edited)",
            "****** Wit The Wrong Nigga - Album Version (Edited)",
            "Let Em Have It - Remix",
            "Until The End Of Time - Album Version (Edit)",
            "M.O.B. - Album Version (Edited)",
        ];
        let explicit = ["Ballad Of A Dead Soulja", "Fuckin Wit The Wrong Nigga", "Until The End Of Time", "M.O.B."];
        let both = [songs(&clean, false), songs(&explicit, true)];
        assert_eq!(editions(&["Until The End Of Time"; 2], &both), [1]);
        // A remix with a name stays another song.
        let remixes = songs(&["Dogs - Skrillex Remix", "Sheep - Skrillex Remix"], false);
        let original = songs(&["Dogs", "Sheep"], false);
        assert_eq!(editions(&["Animals", "Animals (Remixes)"], &[original, remixes]), [0, 1]);
    }
}
