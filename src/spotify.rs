//! Spotify's web player: its anonymous session and queries, and the songs of tracks, albums, playlists and artists.

use crate::settings::settings;
use crate::{
    cached,
    http::{self, HttpError, Pace},
    lock, search, url_encode, Song, HOME, R,
};
use hmac::{Hmac, Mac};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::Sha1;
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{Arc, LazyLock, Mutex},
};

const UA: (&str, &str) = (
    "User-Agent",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
    Chrome/131.0 Safari/537.36",
);
const SECRETS: &str = "https://code.thetadev.de/ThetaDev/spotify-secrets/raw/branch/main/secrets/secretDict.json";
const QUERIES: [&str; 6] =
    ["getTrack", "getAlbum", "fetchPlaylist", "queryArtistDiscographyAll", "searchDesktop", "queryArtistOverview"];

static SESSION: Mutex<Option<Arc<Value>>> = Mutex::new(None);
/// The albums of this run's songs.
static ALBUMS: LazyLock<Mutex<HashMap<String, Arc<Album>>>> = LazyLock::new(Default::default);
/// Unspaced; held when Spotify's API answers 429, so that all threads wait together.
static SPOTIFY: Pace = Pace::new(0);

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
    let get = |url: &str| -> R<String> { Ok(String::from_utf8(http::get(url, &[UA])?)?) };
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

/// Whether a session has every query's hash: a route chunk that failed to load leaves some out.
fn complete(state: &Value) -> bool {
    QUERIES.iter().all(|op| state["hashes"][op].is_string())
}

/// The anonymous web-player token and query hashes, kept in `~/.spotdl/anon.json` until they expire.
fn session(rejected: Option<&str>) -> R<Arc<Value>> {
    let mut slot = lock(&SESSION);
    let path = HOME.join("anon.json");
    let disk = || {
        let state = serde_json::from_slice(&fs::read(&path).ok()?).ok().filter(|_| !settings().no_cache);
        state.filter(complete).map(Arc::new)
    };
    let fresh = |state: &Arc<Value>| {
        state["exp"].as_u64() > Some(crate::now() * 1000 + 60_000)
            && rejected.is_none_or(|token| state["token"] != token)
    };
    if let Some(state) = slot.clone().or_else(disk).filter(fresh) {
        *slot = Some(state.clone());
        return Ok(state);
    }
    let secrets: HashMap<String, Vec<u8>> = http::get_json(SECRETS, &[UA])?;
    let (version, secret) =
        secrets.iter().max_by_key(|(k, _)| k.parse::<u32>().unwrap_or(0)).ok_or("no TOTP secret")?;
    let code = totp(secret);
    let url = format!(
        "https://open.spotify.com/api/token?reason=init&productType=web-player\
        &totp={code}&totpVer={version}&totpServer={code}"
    );
    let token: Value = http::get_json(&url, &[UA])?;
    let token = token["accessToken"].as_str().map(|t| (t, &token["accessTokenExpirationTimestampMs"]));
    let (token, expires) = token.ok_or("Spotify token missing")?;
    let state = Arc::new(json!({"token": token, "exp": expires, "hashes": hashes()?}));
    // A partial set is kept for this run only, so the next run looks for the missing hashes again.
    if !settings().no_cache && complete(&state) {
        fs::create_dir_all(&*HOME)?;
        fs::write(path, state.to_string())?;
    }
    *slot = Some(state.clone());
    Ok(state)
}

/// One web-player query. Its callers keep what they read of the reply, not the reply: a track's is 80 KB.
fn query(op: &str, variables: Value) -> R<Value> {
    let (mut rejected, mut last) = (None::<String>, String::new());
    for _ in 0..=settings().max_retries {
        let state = session(rejected.as_deref())?;
        let token = state["token"].as_str().unwrap_or_default();
        // A hash the bundles did not yield is asked of a new session, as for a rejected token.
        let Some(hash) = state["hashes"][op].as_str() else {
            (rejected, last) = (Some(token.into()), format!("{op}: missing query hash"));
            continue;
        };
        let extensions = json!({"persistedQuery": {"version": 1, "sha256Hash": hash}}).to_string();
        let (variables, extensions) = (url_encode(&variables.to_string()), url_encode(&extensions));
        let url = format!(
            "https://api-partner.spotify.com/pathfinder/v1/query\
            ?operationName={op}&variables={variables}&extensions={extensions}"
        );
        let headers = [UA, ("Authorization", &format!("Bearer {token}"))];
        let reply = http::request(&url, &headers, None, settings().max_retries, Some(&SPOTIFY));
        match reply.and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?)) {
            // A GraphQL failure comes as `"data": null` (or with every field null), with "errors": it is no answer,
            // and must not be kept as one ("Stay - 2016 Remix" then resolved to no song for a week).
            Ok(reply) if reply["data"].as_object().is_some_and(|data| data.values().any(|v| !v.is_null())) => {
                return Ok(reply)
            }
            Ok(reply) => last = format!("{op}: {}", reply["errors"]),
            Err(error) if HttpError::status(&*error) == Some(401) => last = error.to_string(),
            // Lost connections, 429 and 5xx were retried above: asking again here would square the requests.
            Err(error) => return Err(error),
        }
        if ["401", "hash", "PersistedQuery"].iter().any(|word| last.contains(word)) {
            rejected = Some(token.into());
        }
    }
    Err(last.into())
}

/// All items of a paged list (album tracks, playlist items, discography), with the list's own data. No items: the reply
/// has no such list (an album or playlist that is gone).
fn pages(op: &str, uri: &str, list: &str, limit: usize) -> R<(Value, Option<Vec<Value>>)> {
    let page = |offset: usize| {
        let variables = json!({
            "uri": uri, "locale": "", "offset": offset, "limit": limit, "order": "DATE_DESC",
            "enableWatchFeedEntrypoint": false,
        });
        let mut reply = query(op, variables)?;
        let items = reply.pointer_mut(&format!("{list}/items")).map(Value::take);
        R::Ok((reply, serde_json::from_value::<Vec<Value>>(items.unwrap_or_default()).unwrap_or_default()))
    };
    let (mut first, items) = page(0)?;
    let total = first.pointer(list).and_then(|list| list["totalCount"].as_u64());
    let offsets: Vec<usize> = (limit..total.unwrap_or(0) as usize).step_by(limit).collect();
    let rest = offsets.into_par_iter().map(|offset| Ok(page(offset)?.1)).collect::<R<Vec<_>>>()?;
    let meta = first["data"][list.split('/').nth(2).unwrap_or_default()].take();
    Ok((meta, total.map(|_| items.into_iter().chain(rest.into_iter().flatten()).collect())))
}

fn listed<T>(items: Option<T>, uri: &str) -> R<T> {
    items.ok_or_else(|| format!("{uri} unavailable").into())
}

fn cover(value: &Value) -> Option<String> {
    let size =
        |image: &&Value| image["width"].as_u64().unwrap_or_default() * image["height"].as_u64().unwrap_or_default();
    let sources = [&value["images"], &value["images"]["items"], &value["coverArt"]["sources"], &value["sources"]];
    let images = sources.into_iter().filter_map(Value::as_array);
    images.into_iter().find_map(|images| images.iter().max_by_key(size)?["url"].as_str().map(String::from))
}

/// The URL of the artist's largest picture.
pub(crate) fn artist_image(artist_id: &str) -> Option<String> {
    let fetch = || {
        let variables = json!({"uri": format!("spotify:artist:{artist_id}"), "locale": "", "includePrerelease": true});
        Ok(cover(&query("queryArtistOverview", variables)?["data"]["artistUnion"]["visuals"]["avatarImage"]))
    };
    cached(&format!("artist-image:{artist_id}"), 7, fetch).ok().flatten()
}

/// The last part of a Spotify URI: its id.
fn id(uri: &str) -> String {
    uri.rsplit(':').next().unwrap_or_default().into()
}

/// A Spotify album, as far as the tags of its songs need it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Album {
    name: String,
    artist: String,
    artist_id: String,
    date: String,
    label: String,
    kind: String,
    cover: Option<String>,
    copyright: Option<String>,
    discs: Option<u64>,
    /// The number of tracks on each disc, by disc number.
    disc_tracks: Vec<(u64, u64)>,
    /// The number of tracks; none for an album that is gone.
    tracks: Option<u64>,
}

impl Album {
    fn parse(album: &Value) -> Self {
        let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
        let artist = &album["artists"]["items"][0];
        let discs = album["discs"]["items"].as_array().into_iter().flatten();
        let on_disc = |disc: &Value| Some((disc["number"].as_u64()?, disc["tracks"]["totalCount"].as_u64()?));
        Self {
            name: text(&album["name"]),
            artist: text(&artist["profile"]["name"]),
            artist_id: id(artist["uri"].as_str().unwrap_or_default()),
            date: release_date(&album["date"]),
            label: text(&album["label"]),
            kind: text(&album["type"]).to_lowercase(),
            cover: cover(album),
            copyright: album["copyright"]["items"][0]["text"].as_str().map(String::from),
            discs: album["discs"]["totalCount"].as_u64(),
            disc_tracks: discs.filter_map(on_disc).collect(),
            tracks: album["tracksV2"]["totalCount"].as_u64(),
        }
    }

    /// The number of tracks on a disc, as TRACKTOTAL and ID3's TRCK "n/total" count them: track numbers start again
    /// on each disc, so disc 2 of a 12+5 album is "1 of 5", not "1 of 17". The album's total is the fallback when
    /// Spotify lists no discs.
    fn disc_tracks(&self, disc: u64) -> u32 {
        let on_disc = self.disc_tracks.iter().find(|(number, _)| *number == disc).map(|(_, count)| *count);
        on_disc.or(self.tracks).unwrap_or(1) as u32
    }
}

/// A web-player track, as far as a song is built from it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Track {
    uri: String,
    name: String,
    artists: Vec<String>,
    album_uri: String,
    pub disc: Option<u64>,
    pub number: Option<u64>,
    /// In milliseconds.
    pub length: Option<f64>,
    pub explicit: bool,
    playcount: Option<u64>,
}

impl Track {
    fn parse(track: &Value) -> Self {
        let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
        let credits = [&track["firstArtist"]["items"], &track["otherArtists"]["items"], &track["artists"]["items"]];
        let artists = credits.into_iter().flat_map(|items| items.as_array().into_iter().flatten());
        Self {
            uri: text(&track["uri"]),
            name: text(&track["name"]),
            artists: artists.filter_map(|artist| artist["profile"]["name"].as_str().map(String::from)).collect(),
            album_uri: text(&track["albumOfTrack"]["uri"]),
            disc: track["discNumber"].as_u64(),
            number: track["trackNumber"].as_u64(),
            length: ["duration", "trackDuration"].iter().find_map(|key| track[key]["totalMilliseconds"].as_f64()),
            explicit: track["contentRating"]["label"] == "EXPLICIT",
            playcount: track["playcount"].as_str().and_then(|count| count.parse().ok()),
        }
    }
}

/// An album and its tracks, asked once a week: every song's tags need its album, and Deezer's album is matched by the
/// tracks.
fn fetch_album(uri: &str) -> R<(Album, Vec<Track>)> {
    cached(&format!("album:{uri}"), 7, || {
        let (meta, items) = pages("getAlbum", uri, "/data/albumUnion/tracksV2", 343)?;
        if !meta.is_object() {
            return Err("Spotify album metadata is missing".into());
        }
        let track = |item: &Value| Track { album_uri: uri.into(), ..Track::parse(&item["track"]) };
        Ok((Album::parse(&meta), items.unwrap_or_default().iter().map(track).collect()))
    })
}

fn remember(uri: &str, album: Album) -> Arc<Album> {
    lock(&ALBUMS).entry(uri.into()).or_insert_with(|| Arc::new(album)).clone()
}

fn album_meta(uri: &str) -> R<Arc<Album>> {
    let known = lock(&ALBUMS).get(uri).cloned();
    known.map_or_else(|| Ok(remember(uri, fetch_album(uri)?.0)), Ok)
}

/// Spotify dates some albums by year or month only and pads its isoString with 1 January ("1994-01-01T00:00:00Z" for
/// The Division Bell, out on 28 March 1994), so only the part its precision gives is the date.
fn release_date(date: &Value) -> String {
    let known = match date["precision"].as_str() {
        Some("YEAR") => 4,
        Some("MONTH") => 7,
        _ => 10,
    };
    date["isoString"].as_str().and_then(|iso| iso.get(..known)).unwrap_or_default().into()
}

/// Songs from web-player tracks; `list` carries the playlist fields that every song shares.
fn build(tracks: Vec<Track>, list: Song) -> R<Vec<Song>> {
    let uris: HashSet<&str> =
        tracks.iter().map(|track| track.album_uri.as_str()).filter(|uri| !uri.is_empty()).collect();
    let albums: HashMap<&str, Arc<Album>> =
        uris.into_par_iter().map(|uri| Ok((uri, album_meta(uri)?))).collect::<R<_>>()?;
    let (length, listed, unknown) = (tracks.len(), list.list_name.is_some(), Album::default());
    let playable = tracks.iter().enumerate().filter(|(_, track)| track.uri.starts_with("spotify:track:"));
    let song = |(index, track): (usize, &Track)| {
        let album = albums.get(track.album_uri.as_str()).map_or(&unknown, |album| album.as_ref());
        let song_id = id(&track.uri);
        Song {
            name: track.name.clone(),
            artist: track.artists.first().cloned().unwrap_or_default(),
            artists: track.artists.clone(),
            album_artist: album.artist.clone(),
            artist_id: album.artist_id.clone(),
            disc_number: track.disc.unwrap_or(1) as u32,
            disc_count: album.discs.unwrap_or(1) as u32,
            album_name: album.name.clone(),
            duration: track.length.unwrap_or_default() / 1000.0,
            year: album.date.get(..4).and_then(|year| year.parse().ok()).unwrap_or_default(),
            date: album.date.clone(),
            track_number: track.number.unwrap_or(1) as u32,
            tracks_count: album.disc_tracks(track.disc.unwrap_or(1)),
            url: format!("https://open.spotify.com/track/{song_id}"),
            song_id,
            explicit: track.explicit,
            publisher: album.label.clone(),
            cover_url: album.cover.clone(),
            copyright_text: album.copyright.clone(),
            popularity: track.playcount,
            album_id: id(&track.album_uri),
            album_type: album.kind.clone(),
            list_position: listed.then_some(index + 1),
            list_length: listed.then_some(length),
            ..list.clone()
        }
    };
    Ok(playable.map(song).collect())
}

/// The tracks of an album.
pub(crate) fn album(uri: &str) -> R<Vec<Track>> {
    let (album, tracks) = fetch_album(uri)?;
    listed(remember(uri, album).tracks.map(|_| tracks), uri)
}

/// `getTrack` has no disc number; on multi-disc albums the album's track list has it.
fn track(uri: &str) -> R<Track> {
    let days = if settings().use_cache_file { u64::MAX } else { 7 };
    let fetch = || Ok(Track::parse(&query("getTrack", json!({"uri": uri}))?["data"]["trackUnion"]));
    let mut track: Track = cached(&format!("track:{uri}"), days, fetch)?;
    if !track.album_uri.is_empty() && album_meta(&track.album_uri)?.discs.unwrap_or(1) > 1 {
        if let Some(listed) = album(&track.album_uri)?.into_iter().find(|listed| listed.uri == uri) {
            track.disc = listed.disc;
        }
    }
    Ok(track)
}

fn artist(uri: &str) -> R<Vec<Song>> {
    let releases = listed(pages("queryArtistDiscographyAll", uri, "/data/artistUnion/discography/all", 50)?.1, uri)?;
    let mut releases: Vec<&Value> = releases.iter().map(|release| &release["releases"]["items"][0]).collect();
    // Releases in this order: albums, EPs, singles, then compilations; originals before reissues. Albums and EPs are
    // kept whole; singles and compilations add only the songs that no earlier release has.
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
    // Albums and EPs keep every track, also songs that another album or EP has: the "She's A Riot" EP keeps its title
    // track, which is also on "Learn To Exist". Singles and compilations add only new titles, not their edits of album
    // songs.
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

/// The releases to keep, in the order their songs are taken. A bigger edition of the same album (same base title, or
/// one that starts with it) that has at least 90% of a smaller edition's songs is kept whole, and the smaller edition
/// is skipped. "BULLY - DELUXE" replaces "BULLY", even though one of its songs gained a featured artist. Titles are
/// compared without featured artists and version notes, so "Rubber Soul (Super Deluxe)" with its "(2026 Mix)" songs
/// replaces the 2009 remaster.
fn editions(names: &[&str], albums: &[Vec<Song>]) -> Vec<usize> {
    let titles: Vec<HashSet<String>> =
        albums.iter().map(|songs| songs.iter().map(|song| search::edition_title(&song.name)).collect()).collect();
    let explicit: Vec<usize> = albums.iter().map(|songs| songs.iter().filter(|song| song.explicit).count()).collect();
    let title = |i: usize| base_title(names[i]).to_lowercase();
    // "OK Computer OKNOTOK 1997 2017", "KID A MNESIA" and "Wish You Were Here 50" are editions of the albums whose
    // titles they start with.
    let same_title = |a: &str, b: &str| {
        let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        // "Revenge Of The Dreamers III: Director's Cut" is an edition of "Revenge Of The Dreamers III".
        long.strip_prefix(short).is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', ':']))
    };
    // A clean edition never replaces one with explicit songs. Of two editions with the same songs, the explicit one,
    // then the one titled only with the album's title, then the first, is kept: "Animals (2018 Remix)", which Spotify
    // dates two days before "Animals", is not the album.
    let rank = |k: usize| (explicit[k] > 0, titles[k].len(), explicit[k], names[k] == base_title(names[k]));
    let replaces = |j: usize, i: usize| {
        let shared = titles[i].intersection(&titles[j]).count();
        // The clean edit's censored titles ("****** Wit The Wrong Nigga") and clean-only remixes leave it sharing only
        // most of its songs with the explicit edition, which still replaces it.
        let edit = explicit[j] > 0 && explicit[i] == 0;
        j != i
            && same_title(&title(j), &title(i))
            && (rank(j), i) > (rank(i), j)
            && if edit { shared * 2 > titles[i].len() } else { shared * 10 >= titles[i].len() * 9 }
    };
    let n = names.len();
    let mut kept: Vec<usize> = (0..n).filter(|&i| !(0..n).any(|j| replaces(j, i))).collect();
    // The bigger edition takes the place of the first edition it replaces, so that no release in between takes its
    // songs.
    kept.sort_by_key(|&j| (0..n).filter(|&i| i == j || replaces(j, i)).min());
    kept
}

fn playlist(uri: &str) -> R<Vec<Song>> {
    let (meta, items) = pages("fetchPlaylist", uri, "/data/playlistV2/content", 343)?;
    let items = listed(items, uri)?;
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
    build(items.iter().map(|item| Track::parse(&item["itemV2"]["data"])).collect(), list)
}

/// The first Spotify result for free text; `album:`, `playlist:` and `artist:` pick the result type.
fn search(text: &str) -> R<String> {
    let prefix = ["album:", "playlist:", "artist:"]
        .iter()
        .find_map(|kind| Some((&kind[..kind.len() - 1], text.strip_prefix(kind)?)));
    let (kind, term) = prefix.map_or(("track", text.replace(" - ", " ")), |(kind, term)| (kind, term.trim().into()));
    let variables = json!({
        "searchTerm": term, "offset": 0, "limit": 10, "numberOfTopResults": 5,
        "includeAudiobooks": false, "includeArtistHasConcertsField": false,
        "includePreReleases": false, "includeLocalConcertsField": false,
    });
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
    let found = search::videos(&url)?;
    if songs.len() != found.len() {
        return Err(
            format!("YouTube ({}) and Spotify ({}) lists have different lengths", found.len(), songs.len()).into()
        );
    }
    Ok(songs.into_iter().zip(found).map(|(song, video)| search::with_video(song, video)).collect())
}

/// Songs for one query: a Spotify URL or URI, a save file, a YouTube or SoundCloud URL, or search text.
pub(crate) fn resolve(text: &str) -> R<Vec<Song>> {
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
        true => http::final_url(text)?,
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

/// The title that a song has on every release: without a " - Remastered 2009" suffix, and slugged, because box sets and
/// compilations write "Nowhere Man (First Version - Take 2)" as "Nowhere Man - First Version - Take 2". The take, demo
/// and mix words stay: "Take 2" and "Take 5" are other recordings.
fn song_key(name: &str) -> String {
    search::slug(search::versionless(name), "-")
}

/// The title before a " (Deluxe Edition)", " [Remastered]" or " - Single" note.
pub(crate) fn base_title(title: &str) -> &str {
    search::note_at(title, true).map_or(title, |end| &title[..end])
}

#[cfg(test)]
#[path = "fuzz/spotify.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::{complete, editions, query, release_date, song_key, Album, SESSION};
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
    fn the_track_total_is_the_number_of_tracks_on_the_songs_disc() {
        // getAlbum's discs for good kid, m.A.A.d city (Deluxe): 12 songs, then 5 bonus songs.
        let discs = json!({"items": [{"number": 1, "tracks": {"totalCount": 12}}, {"number": 2, "tracks": {"totalCount": 5}}], "totalCount": 2});
        let album = json!({"discs": discs, "tracksV2": {"totalCount": 17}});
        let album = Album::parse(&album);
        assert_eq!((album.disc_tracks(1), album.disc_tracks(2)), (12, 5));
        // Without per-disc counts, the album's.
        assert_eq!(Album::parse(&json!({"tracksV2": {"totalCount": 17}})).disc_tracks(2), 17);
        assert_eq!(Album::parse(&json!({})).disc_tracks(1), 1);
    }

    #[test]
    fn a_session_without_a_querys_hash_is_built_again() {
        crate::settings::use_defaults();
        // As if the search route chunk had failed to load: a live token, but no searchDesktop hash.
        let partial = json!({"token": "partial", "exp": u64::MAX, "hashes": {"getTrack": "0".repeat(64)}});
        assert!(!complete(&partial));
        *SESSION.lock().unwrap() = Some(std::sync::Arc::new(partial));
        let variables = json!({"searchTerm": "Bohemian Rhapsody", "offset": 0, "limit": 1, "numberOfTopResults": 1, "includeAudiobooks": false, "includeArtistHasConcertsField": false, "includePreReleases": false, "includeLocalConcertsField": false});
        let reply = query("searchDesktop", variables).expect("a new session has the hash");
        assert!(reply["data"]["searchV2"].is_object());
        assert!(complete(SESSION.lock().unwrap().as_ref().unwrap()));
    }

    #[test]
    fn dates_keep_only_the_known_part() {
        let date = |iso: &str, precision: &str| release_date(&json!({"isoString": iso, "precision": precision}));
        assert_eq!(date("1994-01-01T00:00:00Z", "YEAR"), "1994");
        assert_eq!(date("1998-04-01T00:00:00Z", "MONTH"), "1998-04");
        assert_eq!(date("2000-01-01T00:00:00Z", "DAY"), "2000-01-01");
        assert_eq!(date("2020-05-01T12:00:00Z", "MINUTE"), "2020-05-01");
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
        // An edition after a colon: the Director's Cut has the 18 songs of the album and 12 more.
        let titles: Vec<String> = (1..=30).map(|n| format!("Song {n}")).collect();
        let names_of = |n: usize| titles[..n].iter().map(String::as_str).collect::<Vec<_>>();
        let cut = ["Revenge Of The Dreamers III", "Revenge Of The Dreamers III: Director's Cut"];
        assert_eq!(editions(&cut, &[songs(&names_of(18), true), songs(&names_of(30), true)]), [1]);
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
