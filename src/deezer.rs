//! Deezer: the genres, label, barcode, dates, ISRCs, BPM and gain that Spotify's web player does not give.

use crate::settings::settings;
use crate::spotify::{self, base_title};
use crate::{
    cached,
    http::{self, Pace},
    lock, search, url_encode, Song, R,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

/// Deezer's copy of an album: only the genres when its tracks are not the Spotify album's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Album {
    genres: Vec<String>,
    upc: Option<String>,
    label: Option<String>,
    release_date: Option<String>,
    /// The tracks by `disc:position`.
    tracks: BTreeMap<String, Track>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Track {
    title: Option<String>,
    isrc: Option<String>,
    duration: Option<f64>,
    explicit: Option<bool>,
}

/// Deezer's album of each Spotify album (none when Deezer does not have it), or how many times it failed and why.
type Fetched<T> = Mutex<Result<T, (u8, String)>>;
static ALBUMS: LazyLock<Mutex<HashMap<String, Arc<Fetched<Arc<Option<Album>>>>>>> = LazyLock::new(Default::default);
/// Deezer allows 50 requests in 5 seconds.
static DEEZER: Pace = Pace::new(110);

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
        let reply: Value =
            serde_json::from_slice(&http::request(url, &[], None, settings().max_retries, Some(&DEEZER))?)?;
        match deezer_error(&reply) {
            None => return Ok(reply),
            Some((false, error)) => return Err(format!("Deezer: {error} for {url}").into()),
            Some((true, _)) => std::thread::sleep(Duration::from_secs(1 << attempt)),
        }
    }
    Err(format!("Deezer: still busy for {url}").into())
}

/// The value in `cell`, fetched by the first song of the album that asks. A failure is not kept, so that a later song
/// asks again, but at most 3 times: a Deezer that is down is not asked once for every song.
fn fetch_once<T: Clone>(cell: &Fetched<T>, fetch: impl FnOnce() -> R<T>) -> Result<T, String> {
    let mut cell = lock(cell);
    if let Err((tries @ 0..3, _)) = *cell {
        *cell = fetch().map_err(|error| (tries + 1, error.to_string()));
    }
    cell.clone().map_err(|(_, error)| error)
}

fn genres(album: &Value) -> Vec<String> {
    let genres = album["genres"]["data"].as_array().into_iter().flatten();
    genres.filter_map(|genre| genre["name"].as_str().map(String::from)).collect()
}

/// Whether the title has a note such as " (Remix)", " [SPED UP]" or " (Piano Version)": the release is another
/// recording than the one titled without the note.
fn version_note(title: &str) -> bool {
    let words = search::slug(&title[base_title(title).len()..], "-");
    let other = ["piano", "orchestral", "stripped", "unplugged", "demo"];
    words.split('-').any(|word| search::altered(word) || other.contains(&word))
}

/// The ids of the Deezer albums that can be the Spotify album, best first, each with whether it fits in every way. A
/// single, its album, the clean and explicit editions and a deluxe edition share a title, so hits rank by the same
/// title, then a title close to it, then the same record type, track count (unknown in a discography) and explicit
/// flag, then Deezer's order. A hit with only the base title ("Fever Ray" for "Fever Ray (Deluxe)") must be the same
/// kind of release, and is never taken for a remix or sped-up release of the base title's song.
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

/// Whether Deezer's tracks are the Spotify album's: at least half the positions of the longer list have a song as long
/// (within 3 s). A title can still be another release's: a 2-song single's album, another recording of an opera.
fn agrees(lengths: &HashMap<String, f64>, tracks: &BTreeMap<String, Track>) -> bool {
    let same = lengths.iter().filter(|(position, length)| {
        tracks.get(*position).and_then(|track| track.duration).is_some_and(|d| (d - **length).abs() <= 3.0)
    });
    same.count() * 2 >= lengths.len().max(tracks.len())
}

/// Deezer's copy of the song's album: genres, UPC, label, release date and its tracks' titles and ISRCs. Deezer's
/// search misses some albums and often names an edition differently ("Fever Ray" for "Fever Ray (Deluxe)") or returns
/// only the clean edition (GNX), so unless a search hit fits in every way the album is also looked for in the artist's
/// discography. An album whose tracks are not the Spotify album's gives only its genres. When no album matches, the
/// genres come from the album that has the song, or else from the artist's most common genre.
fn album(song: &Song) -> R<Option<Album>> {
    cached(&format!("deezer6:{}", song.album_id), 7, || {
        let same = |a: &str, b: &Value| {
            search::ratio(&search::slug(a, "-"), &search::slug(b.as_str().unwrap_or_default(), "-")) > 80.0
        };
        let list = |url: &str| -> R<Vec<Value>> {
            Ok(serde_json::from_value(deezer(url)?["data"].take()).unwrap_or_default())
        };
        let find =
            |kind: &str, query: &str| list(&format!("https://api.deezer.com/search/{kind}?q={}", url_encode(query)));
        let only_genres = |id: &dyn std::fmt::Display| -> R<Option<Album>> {
            let genres = genres(&deezer(&format!("https://api.deezer.com/album/{id}"))?);
            Ok(Some(Album { genres, ..Album::default() }))
        };
        let spotify = spotify::album(&format!("spotify:album:{}", song.album_id))?;
        let lengths: HashMap<String, f64> = spotify
            .iter()
            .map(|track| {
                let position = format!("{}:{}", track.disc.unwrap_or(1), track.number.unwrap_or(0));
                (position, track.length.unwrap_or_default() / 1000.0)
            })
            .collect();
        let explicit = spotify.iter().any(|track| track.explicit);
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
            let text = |value: &Value| value.as_str().map(String::from);
            let tracks = tracks["data"].as_array().into_iter().flatten().enumerate().map(|(i, track)| {
                let (disc, position) = (track["disk_number"].as_u64().unwrap_or(1), track["track_position"].as_u64());
                let (duration, explicit) = (track["duration"].as_f64(), track["explicit_lyrics"].as_bool());
                let track = Track { title: text(&track["title"]), isrc: text(&track["isrc"]), duration, explicit };
                (format!("{disc}:{}", position.unwrap_or(i as u64 + 1)), track)
            });
            let tracks: BTreeMap<String, Track> = tracks.collect();
            if agrees(&lengths, &tracks) {
                let details = deezer(&format!("https://api.deezer.com/album/{id}"))?;
                let (upc, label, release_date) =
                    (text(&details["upc"]), text(&details["label"]), text(&details["release_date"]));
                return Ok(Some(Album { genres: genres(&details), upc, label, release_date, tracks }));
            }
        }
        // Another release of the same title by the artist still has the album's genres, but not its UPC, label or date.
        if let Some((id, _)) = ids.first() {
            return only_genres(id);
        }
        let query = format!("artist:\"{}\" track:\"{}\"", song.artist, search::versionless(&song.name));
        let tracks = find("track", &query)?;
        let same_song = |track: &&Value| {
            let length = (track["duration"].as_f64().unwrap_or_default() - song.duration).abs();
            same(&song.artist, &track["artist"]["name"]) && length <= 3.0
        };
        if let Some(id) = tracks.iter().find(same_song).and_then(|track| track["album"]["id"].as_u64()) {
            return only_genres(&id);
        }
        let mut counts = HashMap::new();
        for genre in discography.iter().flatten().filter_map(|album| album["genre_id"].as_i64()).filter(|id| *id > 0) {
            *counts.entry(genre).or_insert(0) += 1;
        }
        let Some((id, _)) = counts.into_iter().max_by_key(|(id, count)| (*count, -id)) else {
            return Ok(None);
        };
        let genre = deezer(&format!("https://api.deezer.com/genre/{id}"))?["name"].as_str().map(String::from);
        Ok(Some(Album { genres: genre.into_iter().collect(), ..Album::default() }))
    })
}

/// The release date and original date from Spotify's date and Deezer's date of the album. Deezer pads dates it knows
/// only by year with 1 January too (29 of 894 cached albums, over ten times the real share), so that is read as the
/// year. Deezer's album is found by title, so it can be a reissue: its day is taken for a Spotify date of only a year
/// or month when it is in that year or month. Dates are compared only as far as both go, so a year is not earlier than
/// a day in it. Deezer dates albums it has no date for 1900-01-01 (or 0000-00-00): no date.
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

/// Adds Deezer's genres, label (when Spotify has none), UPC, original date, ISRC, BPM and gain. An error is Deezer's
/// failure (not a song that Deezer does not have): the song then has only what Spotify gives.
pub(crate) fn enrich(song: &mut Song) -> Result<(), String> {
    let release = song.extra.release.as_deref().cloned().unwrap_or_else(|| song.clone());
    // A save file written with a playlist option has only the playlist's album fields: a search by them finds no album
    // (and would be cached as the real album's), and the track at the playlist position is another song.
    let listed = release.list_name.as_ref() == Some(&release.album_name)
        && release.list_position == Some(release.track_number as usize)
        && release.disc_number == 1;
    let found = if release.album_id.is_empty() || listed {
        Ok(Arc::new(None))
    } else {
        let unasked = || Arc::new(Mutex::new(Err((0, String::new()))));
        let cell = lock(&ALBUMS).entry(song.album_id.clone()).or_insert_with(unasked).clone();
        fetch_once(&cell, || album(&release).map(Arc::new))
    };
    let mut result = found.as_ref().map(drop).map_err(String::clone);
    if let Some(album) = found.unwrap_or_default().as_ref() {
        song.genres.clone_from(&album.genres);
        song.extra.upc = album.upc.clone().unwrap_or_default();
        // Spotify's label is the downloaded release's; Deezer names labels its own way ("Young" for "Young Turks") and
        // its album can be another edition, so its label only fills a gap.
        if song.publisher.is_empty() {
            song.publisher = album.label.clone().unwrap_or_default();
        }
        (song.date, song.extra.original_date) = dates(&song.date, album.release_date.as_deref().unwrap_or_default());
        // The album is found by title, so it can be another edition (clean, remaster, other order): only trust the
        // track at this position when it has the same length and explicit flag. Deezer may number the album otherwise
        // (M83's two discs as one, a deluxe edition's bonus songs in between): then only the one track that also has
        // the song's full title, version note included, since an instrumental, remix or live take is as long as the
        // song and has its own ISRC.
        if song.isrc.as_deref().is_none_or(str::is_empty) {
            let same = |track: &&Track| {
                track.duration.is_some_and(|length| (length - song.duration).abs() <= 3.0)
                    && track.explicit.is_none_or(|explicit| explicit == song.explicit)
            };
            let placed = album.tracks.get(&format!("{}:{}", release.disc_number, release.track_number)).filter(same);
            let track = placed.or_else(|| {
                let title = search::slug(&release.name, "-");
                let titled =
                    |track: &&Track| track.title.as_deref().is_some_and(|name| search::slug(name, "-") == title);
                let mut titled = album.tracks.values().filter(|track| same(track) && titled(track));
                titled.next().filter(|_| titled.next().is_none())
            });
            song.isrc = track.and_then(|track| track.isrc.clone()).filter(|isrc| !isrc.is_empty());
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
#[path = "fuzz/deezer.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Deezer's album of a Spotify album, as if it had been fetched.
    fn fetched(album_id: &str, album: Value) {
        let album: Album = serde_json::from_value(album).unwrap();
        lock(&ALBUMS).insert(album_id.into(), Arc::new(Mutex::new(Ok(Arc::new(Some(album))))));
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
        let tracks = |pairs: &[(&str, f64)]| -> BTreeMap<String, Track> {
            let track = |length: &f64| Track { duration: Some(*length), ..Track::default() };
            pairs.iter().map(|(key, length)| (key.to_string(), track(length))).collect()
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
        let cell = Mutex::new(Err((0, String::new())));
        let calls = std::cell::Cell::new(0);
        let fail = || {
            calls.set(calls.get() + 1);
            Err("timed out".into())
        };
        assert_eq!(fetch_once(&cell, fail), Err("timed out".into()));
        assert_eq!(fetch_once(&cell, || Ok(json!({"genres": ["Pop"]}))), Ok(json!({"genres": ["Pop"]})));
        assert_eq!(fetch_once(&cell, fail), Ok(json!({"genres": ["Pop"]})));
        let cell = Mutex::new(Err((0, String::new())));
        for _ in 0..5 {
            assert!(fetch_once(&cell, fail).is_err());
        }
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn deezer_label_only_fills_in_a_missing_spotify_label() {
        let tag = |album_id: &str, label: &str| {
            fetched(album_id, json!({"genres": ["Opera"], "label": "Chandos"}));
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
        fetched("test-playlist-numbering", json!({"genres": ["Rock"], "tracks": tracks}));
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
        fetched("test-isrc-by-title", json!({"genres": ["Rock"], "tracks": tracks}));
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

    /// Albums cached by earlier versions, whose values were Deezer's own JSON, are still read.
    #[test]
    fn albums_cached_by_earlier_versions_are_read() {
        let old = json!({"genres": ["Pop"], "upc": "602", "label": "XL", "release_date": "2011-01-24",
            "tracks": {"1:1": {"title": "One", "isrc": "GB1", "duration": 215, "explicit": true}}});
        let album: Option<Album> = serde_json::from_value(old).unwrap();
        let album = album.unwrap();
        assert_eq!((album.upc.as_deref(), album.tracks["1:1"].duration), (Some("602"), Some(215.0)));
        assert_eq!(
            serde_json::from_value::<Option<Album>>(json!({"genres": ["Pop"]})).unwrap().unwrap().tracks.len(),
            0
        );
        assert_eq!(serde_json::from_value::<Option<Album>>(Value::Null).unwrap(), None);
    }

    #[test]
    fn deezer_dates_give_the_day_but_not_their_padding() {
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
}
