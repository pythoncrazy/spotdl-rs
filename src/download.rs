use crate::{http, http_retrying, list, log, number, on, search, spotify, text, url_encode, Pace, Song, R};
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureInformation, PictureType},
    prelude::*,
    tag::{ItemKey, ItemValue, Tag, TagItem, TagType},
};
use rayon::prelude::*;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
        Condvar, Mutex, OnceLock, PoisonError,
    },
};

/// Template fields that only exist after Deezer enrichment.
const ENRICHED: [&str; 7] = ["{genre}", "{isrc}", "{publisher}", "{upc}", "{bpm}", "{gain}", "{original-date}"];
const AUDIO: [&str; 6] = ["mp3", "flac", "ogg", "opus", "m4a", "wav"];

static ARCHIVED: OnceLock<HashSet<String>> = OnceLock::new();
static SCANNED: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
/// The files `sync` moved to a new path, whose tags are written again.
static MOVED: OnceLock<HashSet<PathBuf>> = OnceLock::new();
/// The files `meta --redownload` downloads again, by song URL.
static REDOWNLOADS: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
static LRCLIB: Pace = Pace::new(750);
static GENIUS: Pace = Pace::new(500);
static ALIGNING: Mutex<usize> = Mutex::new(0);
static ALIGNED: Condvar = Condvar::new();

/// One of the `lyrics_aligner_jobs` places for an aligner run; the place is free again when this is dropped.
struct Turn;

impl Turn {
    fn take() -> Self {
        let jobs = number("lyrics_aligner_jobs").max(1) as usize;
        let mut running = ALIGNING.lock().unwrap_or_else(PoisonError::into_inner);
        while *running >= jobs {
            running = ALIGNED.wait(running).unwrap_or_else(PoisonError::into_inner);
        }
        *running += 1;
        Turn
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        *ALIGNING.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        ALIGNED.notify_one();
    }
}

fn path_safe(value: &str) -> String {
    value.chars().filter(|c| !"/\\?%*:|\"<>".contains(*c) && !c.is_control()).collect()
}

/// Fills `{field}`s from the song: save-file fields (`{album-name}`) plus the shorthands below.
pub fn fill(template: &str, s: &Song) -> String {
    let original_date = if s.extra.original_date.is_empty() { &s.date } else { &s.extra.original_date };
    let fields = [
        ("title", s.name.clone()),
        ("artists", s.artists.join(", ")),
        ("album", s.album_name.clone()),
        ("list", s.list_name.clone().unwrap_or_else(|| s.album_name.clone())),
        ("genre", s.genres.first().cloned().unwrap_or_default()),
        ("upc", s.extra.upc.clone()),
        ("bpm", s.extra.bpm.to_string()),
        ("gain", s.extra.gain.to_string()),
        ("original-date", original_date.clone()),
        ("track-id", s.song_id.clone()),
        ("track-number", format!("{:02}", s.track_number)),
        // "07", or "2-07" when the album has more than one disc, so the discs' track numbers do not collide.
        (
            "disc-track-number",
            match s.disc_count {
                0 | 1 => format!("{:02}", s.track_number),
                _ => format!("{}-{:02}", s.disc_number, s.track_number),
            },
        ),
        ("duration", (s.duration as u64).to_string()),
        ("output-ext", text("format").unwrap_or("opus").into()),
    ];
    let json = serde_json::to_value(s).unwrap_or_default();
    let saved = json.as_object().into_iter().flatten().map(|(key, value)| {
        let value = match value {
            Value::Null => String::new(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        (key.replace('_', "-"), value)
    });
    let fields = fields.into_iter().map(|(key, value)| (key.to_string(), value)).chain(saved);
    fields.fold(template.into(), |out, (key, value)| {
        out.replace(&format!("{{{key}}}"), path_safe(&value).trim().trim_end_matches('.'))
    })
}

/// The output file for the `--output` template.
pub fn path(s: &Song) -> PathBuf {
    template_path(text("output").unwrap_or_default(), s)
}

/// The output file: each template part with a `{field}` filled, made path-safe and cut to `max_filename_length` (and
/// 200 bytes). Parts without a field are the user's own folders (`..`, `My Music: 50% Off`) and stay as written, so
/// files land where `--output` says and `--scan-for-songs`, which looks there, finds them.
fn template_path(output: &str, s: &Song) -> PathBuf {
    let template = match output {
        "" => "{artists} - {title}".into(),
        output if output.contains('{') => output.to_string(),
        dir => format!("{dir}/{{artists}} - {{title}}"),
    };
    let max = crate::number("max_filename_length");
    let part = |part: &str| {
        if !part.contains('{') {
            return part.to_string();
        }
        let value = fill(part, s);
        let value = match text("restrict") {
            Some("strict") => search::slug(&value, "_"),
            Some("ascii") => deunicode::deunicode(&value),
            _ => value,
        };
        let value = path_safe(&value);
        if value == "." || value == ".." {
            return "_".into();
        }
        let mut part = String::new();
        for c in value.chars().take(if max == 0 { usize::MAX } else { max as usize }) {
            if part.len() + c.len_utf8() > 200 {
                break;
            }
            part.push(c);
        }
        part
    };
    let parts = template.trim_start_matches('/').trim_end_matches(".{output-ext}").split('/').map(part);
    let mut out = Path::new(if template.starts_with('/') { "/" } else { "" }).join(parts.collect::<PathBuf>());
    out.as_mut_os_string().push(format!(".{}", text("format").unwrap_or("opus")));
    out
}

/// The fixed folder of the output template, the part before its first `{field}`.
fn output_root() -> PathBuf {
    let output = Path::new(text("output").unwrap_or_default());
    output.components().take_while(|part| !part.as_os_str().to_string_lossy().contains('{')).collect()
}

fn skip_path(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_os_string();
    path.push(".skip");
    path.into()
}

fn archive(url: &str) -> R<()> {
    if let Some(path) = text("archive") {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        std::io::Write::write_all(&mut file, format!("{url}\n").as_bytes())?;
    }
    Ok(())
}

fn audio_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for path in fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path()) {
        let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if path.is_dir() {
            audio_files(&path, files);
        } else if AUDIO.contains(&extension.as_str()) && !temp_file(&path) {
            files.push(path);
        }
    }
}

/// The text between `start` and `end`, without tags, `<br>` as newlines, and excluded `<div>`s dropped.
fn html(page: &str, start: &str, end: &str) -> Option<String> {
    let region = page.split_once(start)?.1.split(end).next()?;
    let (mut skip, mut out) = (0, String::new());
    for (i, piece) in region.split('<').enumerate() {
        let (tag, text) = match i {
            0 => ("", piece.split_once('>').map_or(piece, |x| x.1)),
            _ => piece.split_once('>').unwrap_or((piece, "")),
        };
        skip = match tag {
            t if t.contains("data-exclude-from-selection") => skip + 1,
            t if skip > 0 && t.starts_with("div") => skip + 1,
            "/div" if skip > 0 => skip - 1,
            _ => skip,
        };
        if skip == 0 {
            out.push_str(if tag.starts_with("br") { "\n" } else { "" });
            out.extend(text.chars().filter(|c| !matches!(c, '\n' | '\r')));
        }
    }
    Some(out.replace("&#x27;", "'").replace("&quot;", "\"").replace("&amp;", "&"))
}

/// Genius lyrics: the text of each `data-lyrics-container` div. The page puts song lists ("You might also
/// like") and ads between these divs, so only the divs are read.
fn genius_lyrics(page: &str) -> Option<String> {
    let blocks = page.split("data-lyrics-container=\"true\"").skip(1).filter_map(|part| {
        let mut depth = 1;
        let end = part.match_indices('<').find_map(|(i, _)| {
            let tag = &part[i + 1..];
            depth += usize::from(tag.starts_with("div"));
            depth -= usize::from(tag.starts_with("/div"));
            (depth == 0).then_some(i)
        })?;
        html(&part[..end], ">", "\0")
    });
    let text: Vec<String> = blocks.map(|block| block.trim().to_string()).filter(|block| !block.is_empty()).collect();
    Some(text.join("\n")).filter(|text| !text.is_empty())
}

/// The title that lyrics sites list: without " - Remastered", "(feat. ...)", "(Uncensored)" or other notes.
fn lyrics_title(song: &Song) -> &str {
    let title = search::versionless(&song.name);
    let end = [" (", " ["].iter().filter_map(|note| title.find(note)).min().unwrap_or(title.len());
    title[..end].trim()
}

/// A lowercase title of letters and digits, without notes in brackets.
fn plain_title(title: &str) -> String {
    let title = title.to_lowercase();
    let end = [" (", " [", " - "].iter().filter_map(|note| title.find(note)).min().unwrap_or(title.len());
    search::slug(&title[..end], "")
}

/// LRCLIB's search results for a URL. LRCLIB refuses requests when it is busy, so that a busy moment does not
/// replace synced lyrics with plain ones, all threads wait and then try again: long after 429 (too many requests
/// from us), and briefly after 503 (LRCLIB is overloaded and refuses some of everyone's requests). Only this loop
/// retries: [`http`]'s own retries would ask again within seconds, unpaced and during a hold, and keep the limit on.
fn lrclib_search(url: &str) -> Option<Vec<Value>> {
    for attempt in 0..12 {
        LRCLIB.wait();
        match http_retrying(url, &[], None, 0) {
            Ok(bytes) => return serde_json::from_slice(&bytes).ok(),
            Err(error) if error.to_string().contains("HTTP 429") => {
                LRCLIB.hold(std::time::Duration::from_secs(15 << attempt.min(3)));
            }
            Err(error) if error.to_string().contains("HTTP 503") => {
                LRCLIB.hold(std::time::Duration::from_secs(2 + 2 * attempt.min(10)));
            }
            // A timeout, a lost connection or another server error passes; a refused request does not.
            Err(error) if attempt < 4 && !error.to_string().contains("HTTP 4") => {
                std::thread::sleep(std::time::Duration::from_secs(2 << attempt));
            }
            Err(error) => {
                log("WARNING", &format!("LRCLIB: {error}"));
                return None;
            }
        }
    }
    log("WARNING", &format!("LRCLIB: still too many requests for {url}"));
    None
}

/// LRCLIB's lyrics for versions as long as the song: those of the song's album first, then synced ones; `""` when
/// LRCLIB says that the song is an instrumental. `keep`: the `.lrc` writer will search again (see [`lrclib_tracks`]).
fn lrclib(song: &Song, keep: bool) -> Vec<String> {
    // LRCLIB files a song under one of its artists ("KING" of ¥$ is under Kanye West).
    let mut artists = vec![&song.artist];
    artists.extend(song.artists.iter().filter(|artist| **artist != song.artist));
    artists
        .into_iter()
        .filter(|artist| !artist.is_empty())
        .map(|artist| lrclib_by(song, artist, keep))
        .find(|found| !found.is_empty())
        .unwrap_or_default()
}

fn lrclib_by(song: &Song, artist: &str, keep: bool) -> Vec<String> {
    let url = format!(
        "https://lrclib.net/api/search?artist_name={}&track_name={}",
        url_encode(artist),
        url_encode(lyrics_title(song))
    );
    let Some(tracks) = lrclib_tracks(&url, keep, lrclib_search) else { return vec![] };
    lrclib_lyrics(song, &tracks)
}

/// LRCLIB's search results kept for the `.lrc` writer, by URL, oldest first.
type Kept = std::collections::VecDeque<(String, Vec<Value>)>;
static LRCLIB_FOUND: std::sync::LazyLock<Mutex<Kept>> = std::sync::LazyLock::new(Default::default);
/// A search reply can be over 100 KB; this is room for the songs in progress, not for a library.
const LRCLIB_KEPT: usize = 256;

/// `search`'s results for a URL. The lyrics provider and the `.lrc` writer search for the same song; LRCLIB is often
/// busy, so it is asked once: the provider keeps its results (`keep`) and the writer takes them. The writer keeps
/// nothing, since nothing searches after it, and results the writer does not take (no `.lrc` to write) are dropped
/// when newer ones come: once kept by everyone, they filled gigabytes over a library.
fn lrclib_tracks(url: &str, keep: bool, search: fn(&str) -> Option<Vec<Value>>) -> Option<Vec<Value>> {
    let taken = {
        let mut found = LRCLIB_FOUND.lock().unwrap_or_else(PoisonError::into_inner);
        found.iter().position(|(key, _)| key == url).and_then(|at| found.remove(at)).map(|(_, tracks)| tracks)
    };
    if taken.is_some() {
        return taken;
    }
    let tracks = search(url)?;
    if keep {
        let mut found = LRCLIB_FOUND.lock().unwrap_or_else(PoisonError::into_inner);
        if found.len() >= LRCLIB_KEPT {
            found.pop_front();
        }
        found.push_back((url.to_string(), tracks.clone()));
    }
    Some(tracks)
}

/// [`lrclib_by`]'s lyrics of LRCLIB's search results. When every version as long as the song is marked
/// instrumental, LRCLIB answers that the song has no words: that answer is `""`, so that the lyrics providers stop
/// and do not take another song's words from a site that goes by the title alone.
fn lrclib_lyrics(song: &Song, tracks: &[Value]) -> Vec<String> {
    let near: Vec<&Value> = tracks
        .iter()
        .filter(|track| (track["duration"].as_f64().unwrap_or_default() - song.duration).abs() < 4.0)
        .collect();
    if !near.is_empty() && near.iter().all(|track| track["instrumental"].as_bool() == Some(true)) {
        return vec![String::new()];
    }
    let album = plain_title(&song.album_name);
    let text =
        |track: &Value, key: &str| track[key].as_str().filter(|lyrics| !lyrics.trim().is_empty()).map(String::from);
    let mut found: Vec<(bool, bool, String)> = near
        .into_iter()
        .filter_map(|track| {
            let other = plain_title(track["albumName"].as_str().unwrap_or_default());
            let same_album =
                !album.is_empty() && !other.is_empty() && (other.starts_with(&album) || album.starts_with(&other));
            let synced = text(track, "syncedLyrics");
            Some((!same_album, synced.is_none(), synced.or_else(|| text(track, "plainLyrics"))?))
        })
        .collect();
    found.sort_by_key(|(other_album, plain, _)| (*other_album, *plain));
    let mut seen = HashSet::new();
    found.into_iter().map(|(_, _, lyrics)| lyrics).filter(|lyrics| seen.insert(lyrics.clone())).collect()
}

/// Genius's lyrics for the song's title by `artist`: the pages of up to `max` hits with that artist and title
/// (a song can have several pages: "Everybody", "Everybody (Original)", "Everybody (Demo 3)"), or nothing when
/// Genius could not be asked.
fn genius(song: &Song, artist: &str, max: usize) -> Option<Vec<String>> {
    Some(genius_titled(song, artist, max)?.into_iter().map(|page| page.lyrics).collect())
}

/// A Genius page of lyrics.
#[derive(Clone)]
struct Page {
    lyrics: String,
    /// Genius's song id.
    id: u64,
    /// The page has the song's title.
    titled: bool,
    /// Genius dates the song to the year of the song's release.
    same_year: bool,
}

/// Genius's page of the song, only one with the song's title: the check that replaces lyrics must not take
/// another song with a like title ("In the Flesh?" for The Wall's other "In the Flesh"). The outer `None`: Genius
/// could not be asked.
fn genius_same(song: &Song, artist: &str) -> Option<Option<Page>> {
    Some(genius_titled(song, artist, 1)?.into_iter().next().filter(|page| page.titled))
}

/// Whether the page is the song's recording and not another of the artist's songs with that title: Genius dates it
/// to the release's year or has it on the release's album. Spotify calls Until the End of Time's "When I Get Free
/// II (Remix)" "When I Get Free", the title of a song from R U Still Down; Genius has that song on R U Still Down
/// and Euthanasia. `None` when Genius could not be asked.
fn released_on(song: &Song, page: &Page) -> Option<bool> {
    if page.same_year {
        return Some(true);
    }
    let url = format!("https://genius.com/api/songs/{}", page.id);
    let album = plain_title(&song.album_name);
    let reply = serde_json::from_str::<Value>(&genius_get(&url, &[])?).ok();
    let albums = reply.as_ref().and_then(|reply| reply["response"]["song"]["albums"].as_array());
    Some(albums.into_iter().flatten().filter_map(|other| other["name"].as_str()).map(plain_title).any(|other| {
        !album.is_empty() && !other.is_empty() && (other.starts_with(&album) || album.starts_with(&other))
    }))
}

/// The pages Genius gave for each song and artist. The lyrics provider and the `.lrc` writer both ask; Genius is
/// paced, so a song's pages are fetched once. A failed request is not kept: the next ask tries again.
static GENIUS_FOUND: std::sync::LazyLock<Mutex<HashMap<String, Vec<Page>>>> =
    std::sync::LazyLock::new(Default::default);

fn genius_key(song: &Song, artist: &str, max: usize) -> String {
    format!("{artist}\n{}\n{}\n{max}", song.name, song.year)
}

/// [`genius`]'s pages. `None` (Genius could not be asked) is not "no page": no page lets LRCLIB's lyrics in
/// unchecked, while a Genius that cannot be asked leaves them for a later run that can check them.
fn genius_titled(song: &Song, artist: &str, max: usize) -> Option<Vec<Page>> {
    let key = genius_key(song, artist, max);
    if let Some(pages) = GENIUS_FOUND.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return Some(pages.clone());
    }
    let pages = genius_pages(song, artist, max)?;
    GENIUS_FOUND.lock().unwrap_or_else(PoisonError::into_inner).insert(key, pages.clone());
    Some(pages)
}

/// Genius's website answered 403: it blocks this client, so it is not asked again in this run (asking again would
/// not be waiting out a limit but working around a block). The lyrics pages are on the website, so without it
/// Genius cannot be asked at all.
static GENIUS_BLOCKED: AtomicBool = AtomicBool::new(false);
/// The API's token is out of requests (the shared docs client has 100 an hour): the website's search is asked.
static GENIUS_API_SPENT: AtomicBool = AtomicBool::new(false);
/// The songs whose lyrics were left unchecked because Genius could not be asked, for [`report_unchecked`].
static UNCHECKED: std::sync::LazyLock<Mutex<HashSet<String>>> = std::sync::LazyLock::new(Default::default);

fn unchecked(song: &Song) {
    let mut songs = UNCHECKED.lock().unwrap_or_else(PoisonError::into_inner);
    if songs.is_empty() {
        log("WARNING", "Genius cannot be asked: LRCLIB lyrics, which it must check, are left for a later run");
    }
    songs.insert(format!("{} - {}", song.artist, song.name));
}

/// Says at the end of a run how many songs got no checked synced lyrics, so that a `meta` run adds them later.
pub fn report_unchecked() {
    let count = UNCHECKED.lock().unwrap_or_else(PoisonError::into_inner).len();
    if count > 0 {
        log("WARNING", &format!("{count} songs without LRCLIB lyrics or .lrc files: Genius could not be asked"));
    }
}

/// Whether the lyrics providers asked Genius, the lyrics authority, for the song and it answered: only then is
/// "no lyrics" an answer (an instrumental) and not a failed request (a 403 or a timeout), which must not stop
/// later runs from looking again.
fn genius_answered(song: &Song) -> bool {
    list("lyrics_providers").any(|name| name == "genius" || name == "synced")
        && GENIUS_FOUND.lock().unwrap_or_else(PoisonError::into_inner).contains_key(&genius_key(song, &song.artist, 1))
}

/// A Genius page or search. Genius answers 429 when it is asked too often; so that the check of a library does not
/// skip the songs of that moment (no page would leave their lyrics unchecked), all threads wait, longer each time,
/// and then try again. A timeout or a server error is tried again a few times.
fn genius_get(url: &str, headers: &[(&str, &str)]) -> Option<String> {
    for attempt in 0..12 {
        if GENIUS_BLOCKED.load(Relaxed) {
            return None;
        }
        GENIUS.wait();
        match http_retrying(url, headers, None, 0) {
            Ok(bytes) => return String::from_utf8(bytes).ok(),
            Err(error) if error.to_string().contains("HTTP 429") => {
                GENIUS.hold(std::time::Duration::from_secs(30 << attempt.min(3)));
            }
            Err(error) if error.to_string().contains("HTTP 403") => {
                if !GENIUS_BLOCKED.swap(true, Relaxed) {
                    log("WARNING", &format!("Genius: {error}: it is not asked again in this run"));
                }
                return None;
            }
            // A timeout, a lost connection or a server error passes; a page that is not there does not.
            Err(error) if attempt < 4 && !error.to_string().contains("HTTP 4") => {
                std::thread::sleep(std::time::Duration::from_secs(2 << attempt));
            }
            Err(error) => {
                log("WARNING", &format!("Genius: {error} ({url})"));
                return None;
            }
        }
    }
    log("WARNING", &format!("Genius: still too many requests for {url}"));
    None
}

/// Whether the artist `name` is one of `names` ("¥$" of "¥$, Kanye West & Ty Dolla $ign"): its slug is a run of
/// whole words there, however they are spaced ("Lil Wayne", "LilWayne"). As a substring, "¥$" (slug "y") was in
/// every name with a y, Genius Ukrainian Translations' too, and "Nas" in Jonas Brothers. A slug of a letter or two
/// could still be another name's word ("Y"), so such a name must also be spelled the same there.
fn among(name: &str, names: &str) -> bool {
    let (slug, words) = (search::slug(name, ""), search::slug(names, " "));
    let words: Vec<&str> = words.split(' ').collect();
    let run = (0..words.len()).any(|start| {
        let mut joined = String::new();
        words[start..].iter().any(|word| {
            joined.push_str(word);
            joined == slug
        })
    });
    !slug.is_empty() && run && (slug.len() >= 3 || names.to_lowercase().contains(&name.to_lowercase()))
}

/// Genius's own accounts ("Genius Ukrainian Translations (Український переклад)", "Genius Romanizations",
/// "Genius Traducciones al Español") post translations of other artists' songs, never the songs' own lyrics.
fn genius_account(primary: &str, artist: &str) -> bool {
    primary.starts_with("Genius ") && !among(primary, artist)
}

/// The pages, or nothing when Genius could not be asked.
fn genius_pages(song: &Song, artist: &str, max: usize) -> Option<Vec<Page>> {
    if GENIUS_BLOCKED.load(Relaxed) {
        return None;
    }
    let title = lyrics_title(song);
    let query = url_encode(&format!("{artist} {title}"));
    // With a `genius_token`, Genius's API (as spotDL asks it); it counts requests apart from the website's search,
    // which is asked when the API cannot answer (a token that many people use is often out of requests).
    // A token out of requests stays so for the hour: it is not asked again in this run.
    let token = text("genius_token").filter(|token| !token.is_empty() && !GENIUS_API_SPENT.load(Relaxed));
    let from_api = token.map(|token| format!("Bearer {token}")).and_then(|auth| {
        GENIUS.wait();
        let url = format!("https://api.genius.com/search?q={query}");
        let reply = http_retrying(&url, &[("Authorization", &auth)], None, 0);
        if reply.as_ref().is_err_and(|error| error.to_string().contains("HTTP 429")) {
            GENIUS_API_SPENT.store(true, Relaxed);
        }
        serde_json::from_slice::<Value>(&reply.ok()?).ok()?["response"]["hits"].as_array().cloned()
    });
    let mut hits = match from_api {
        Some(hits) => hits,
        None => {
            let url = format!("https://genius.com/api/search/song?q={query}");
            let reply = serde_json::from_str::<Value>(&genius_get(&url, &[])?).ok()?;
            reply["response"]["sections"][0]["hits"].as_array().cloned().unwrap_or_default()
        }
    };
    let song_title = plain_title(title);
    fn primary(hit: &Value) -> &str {
        hit["result"]["primary_artist"]["name"].as_str().unwrap_or_default()
    }
    // The same artist and the same title: the artist's first hit can be another of the artist's songs. The
    // artist may be one of several on the page ("¥$, Kanye West & Ty Dolla $ign").
    let same = |hit: &&Value| {
        let artists = hit["result"]["artist_names"].as_str().unwrap_or_default();
        let hit_title = plain_title(hit["result"]["title"].as_str().unwrap_or_default());
        (among(artist, artists) || among(primary(hit), artist))
            && !genius_account(primary(hit), artist)
            && !hit_title.is_empty()
            && (hit_title.contains(&song_title) || song_title.contains(&hit_title))
    };
    hits.retain(|hit| same(&hit));
    // The page of the song's whole title goes first: "WHISKEY (RELEASE ME)" before Maroon 5's "Whiskey" (feat.
    // A$AP Rocky), and "For No One" before "For No One (Take 10 / Backing Track)", a session take from the 2022 box
    // set whose "For No One - 2022 Mix" is the song. Then the artist's own page, then the same title without notes
    // ("Intro" before "Intro (Skit)"), and as an artist can have two songs with one title ("KING" on VULTURES 1 in
    // 2024 and on BULLY in 2026), the song of the release's year.
    let year = |hit: &Value| hit["result"]["release_date_components"]["year"].as_u64();
    fn hit_title(hit: &Value) -> &str {
        hit["result"]["title"].as_str().unwrap_or_default()
    }
    // Artists' names are taken out of titles, but not names of a letter or two: "¥$" would take out every "y".
    let artists: Vec<String> = song.artists.iter().filter(|a| search::slug(a, "").len() >= 3).cloned().collect();
    let whole_title = search::clean_name(search::versionless(&song.name), &artists);
    let whole = |hit: &Value| search::clean_name(hit_title(hit), &artists) == whole_title;
    // A question or exclamation mark tells two titles apart where the words are the same ("In the Flesh?").
    let marks = |title: &str| -> String {
        let end = [" (", " ["].iter().filter_map(|note| title.find(note)).min().unwrap_or(title.len());
        title[..end].chars().filter(|c| matches!(c, '?' | '!')).collect()
    };
    let song_marks = marks(search::versionless(&song.name));
    let titled = |hit: &Value| whole(hit) && marks(hit_title(hit)) == song_marks;
    let own = |hit: &Value| among(primary(hit), artist);
    let exact = |hit: &Value| plain_title(hit_title(hit)) == song_title;
    hits.sort_by_key(|hit| {
        (!titled(hit), !whole(hit), !own(hit), !exact(hit), song.year == 0 || year(hit) != Some(u64::from(song.year)))
    });
    let same_year = |hit: &Value| song.year != 0 && year(hit) == Some(u64::from(song.year));
    let mut pages = vec![];
    for hit in hits.iter().take(max) {
        let (Some(url), Some(id)) = (hit["result"]["url"].as_str(), hit["result"]["id"].as_u64()) else { continue };
        // An instrumental's page has no lyrics blocks, only "This song is an instrumental", and the API says so too.
        let page = genius_get(url, &[])?;
        let said =
            hit["result"]["instrumental"].as_bool() == Some(true) || page.contains("This song is an instrumental");
        let Some(lyrics) = genius_lyrics(&page).or_else(|| said.then(|| "[Instrumental]".into())) else { continue };
        pages.push(Page { lyrics, id, titled: titled(hit), same_year: same_year(hit) });
    }
    Some(pages)
}

/// Lyrics from one provider: `synced` (LRCLIB), `genius`, `azlyrics` or `musixmatch`.
fn provider(name: &str, song: &Song) -> Option<String> {
    let get = |url: &str, headers: &[(&str, &str)]| String::from_utf8(http(url, headers, None).ok()?).ok();
    let (artist, title) = (&song.artist, lyrics_title(song));
    let lyrics = match name {
        // LRCLIB's lyrics are checked against Genius's; without an answer from Genius, they are left for a later run.
        // LRCLIB's word that the song is an instrumental (`""`) counts only when Genius has no words to check it by.
        "synced" => {
            let Some(pages) = genius_titled(song, artist, 1) else {
                unchecked(song);
                return None;
            };
            let page = pages.into_iter().next();
            if page.as_ref().is_some_and(|page| instrumental(song, page)) {
                return Some(String::new());
            }
            let checks = page.map(|page| page.lyrics).filter(|lyrics| !words(lyrics).is_empty());
            let agrees =
                |lyrics: &String| checks.as_ref().is_none_or(|by| !lyrics.is_empty() && same_lyrics(lyrics, by));
            return lrclib(song, true).into_iter().find(agrees).map(|lyrics| lyrics.trim().to_string());
        }
        // A page without words is the song's answer that it has none, or else another song's.
        "genius" => {
            let page = genius_titled(song, artist, 1)?.into_iter().next()?;
            if instrumental(song, &page) {
                return Some(String::new());
            }
            Some(page.lyrics).filter(|lyrics| !words(lyrics).is_empty())?
        }
        "azlyrics" => {
            let url = format!(
                "https://www.azlyrics.com/lyrics/{}/{}.html",
                search::slug(artist, ""),
                search::slug(title, "")
            );
            html(&get(&url, &[])?, "<!-- Usage of azlyrics.com", "</div>")?
        }
        _ => {
            let url =
                format!("https://www.musixmatch.com/lyrics/{}/{}", search::slug(artist, "-"), search::slug(title, "-"));
            let body = get(&url, &[])?.split_once("\"body\":")?.1.to_string();
            serde_json::Deserializer::from_str(&body).into_iter::<String>().next()?.ok()?
        }
    };
    Some(lyrics.trim().to_string()).filter(|lyrics| !lyrics.is_empty())
}

/// The lyrics of the first provider that has them. `Some(None)`: a provider found that the song has no words (an
/// instrumental), so the providers after it, which go by the title alone, are not asked: azlyrics would give
/// "Interstellar Overdrive" the words of another Pink Floyd song.
fn found_lyrics(song: &Song) -> Option<Option<String>> {
    let found = list("lyrics_providers").find_map(|name| provider(name, song))?;
    Some(Some(found).filter(|lyrics| !lyrics.is_empty()))
}

fn find_lyrics(song: &Song) -> Option<String> {
    found_lyrics(song).flatten()
}

/// Whether the lyrics have `[mm:ss` timestamps.
fn is_synced(lyrics: &str) -> bool {
    lyrics.lines().any(|line| {
        let minutes = line.bytes().skip(1).take_while(u8::is_ascii_digit).count();
        line.starts_with('[') && minutes > 0 && line.as_bytes().get(minutes + 1) == Some(&b':')
    })
}

/// The lyrics without `[mm:ss.xx]` timestamps.
fn plain_lyrics(lyrics: &str) -> String {
    let untimed = |line: &str| {
        let mut line = line.trim_start();
        while let Some(rest) = line.strip_prefix('[').filter(|_| is_synced(line)) {
            line = rest.split_once(']').map_or("", |(_, rest)| rest).trim_start();
        }
        line.to_string()
    };
    lyrics.lines().map(untimed).collect::<Vec<_>>().join("\n")
}

/// Whether two lyrics are the words of one song: at least 30% of the word pairs of the shorter are in the longer.
/// Lyrics sites write a song a little differently (ad-libs, spellings), but another song shares only common pairs
/// ("i dont", "you know"): on 92 songs of a library, LRCLIB and Genius shared 80% of the pairs of a song (median)
/// and at most 8% with another song.
fn same_lyrics(a: &str, b: &str) -> bool {
    let pairs = |lyrics: &str| -> HashSet<(String, String)> {
        words(lyrics).windows(2).map(|pair| (pair[0].clone(), pair[1].clone())).collect()
    };
    let (a, b) = (pairs(a), pairs(b));
    let (shorter, longer) = (a.len().min(b.len()), a.len().max(b.len()));
    // A few words cannot tell: a backing track's count-in that one site writes "1,2,3,4 (instrumental)" and another,
    // with the studio talk, "One, two, three, four", or two instrumentals' "[Instrumental]". No words against a
    // song's words can: an instrumental's "[Instrumental]" is not "See-Saw"'s lyrics under "Interstellar Overdrive".
    match shorter {
        0 => longer < 20,
        1..20 => true,
        _ => a.intersection(&b).count() * 10 >= shorter * 3,
    }
}

/// The lowercase words of the lyrics, without timestamps and section names ("[Chorus]").
fn words(lyrics: &str) -> Vec<String> {
    plain_lyrics(lyrics)
        .lines()
        .filter(|line| !(line.trim_start().starts_with('[') && line.trim_end().ends_with(']')))
        .flat_map(|line| {
            line.to_lowercase()
                .replace(['\'', '’'], "")
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| !word.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether Genius's page is the song's own (see [`genius_same`] and [`released_on`]) and has no words, only
/// section names ("[Instrumental]"): the song is an instrumental. A page of another song with a like title
/// ("Bohemian Rhapsody (Instrumental)") says nothing about the song.
fn instrumental(song: &Song, page: &Page) -> bool {
    page.titled && words(&page.lyrics).is_empty() && released_on(song, page) == Some(true)
}

/// Lyrics without the "You might also like" song list that Genius puts between its lyrics blocks, which earlier
/// scrapes of its pages glued into the text ("I love youYou might also like[Chorus]"). One followed by a space is
/// a line of lyrics ("You might also like me").
fn unlisted(lyrics: &str) -> String {
    const LIST: &str = "You might also like";
    let (mut out, mut rest) = (String::new(), lyrics);
    while let Some((before, after)) = rest.split_once(LIST) {
        out.push_str(before);
        if after.starts_with(' ') {
            out.push_str(LIST);
        } else if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        rest = after;
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// The words that the song's lyrics must have: its own plain lyrics (from the first lyrics provider that had
/// them; Genius by default), or else its Genius page. Genius has a page for each song, with the artists and the
/// release date; LRCLIB is a crowd's uploads and has, for example, "KING" from VULTURES 1 under BULLY's "KING".
/// `Some(None)`: Genius has no such page. `None`: Genius could not be asked, so nothing can be checked.
fn reference(song: &Song, own: Option<&String>) -> Option<Option<String>> {
    if let Some(own) = own.filter(|lyrics| !is_synced(lyrics)) {
        return Some(Some(own.clone()));
    }
    let mut asked = true;
    for artist in song.artists.iter().chain([&song.artist]).filter(|artist| !artist.is_empty()) {
        let Some(page) = genius_same(song, artist) else {
            asked = false;
            continue;
        };
        match page.map(|page| (released_on(song, &page), page)) {
            Some((Some(true), page)) => return Some(Some(page.lyrics)),
            Some((None, _)) => asked = false,
            _ => {}
        }
    }
    asked.then_some(None)
}

/// What the `lyrics_aligner` command made of the lyrics.
enum Aligned {
    Lrc(String),
    /// The lyrics are not the words of this recording (exit status 3).
    NoFit,
    /// The recording cannot tell: its words are not heard (exit status 4).
    Unsure,
    Failed,
}

/// Plain lyrics timed to the recording by the `lyrics_aligner` command. The command gets the audio file, a text
/// file with the lyrics and the `.lrc` file to write. It exits with status 3 when the lyrics do not fit the
/// recording, and with status 4 when it cannot tell (the words of the recording are not heard).
fn align(file: &Path, lyrics: &str) -> Aligned {
    let mut command = text("lyrics_aligner").unwrap_or_default().split_whitespace();
    let Some(program) = command.next() else { return Aligned::Failed };
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let temp =
        std::env::temp_dir().join(format!("spotdl-align-{}-{}", std::process::id(), COUNT.fetch_add(1, Relaxed)));
    let (input, output) = (temp.with_extension("txt"), temp.with_extension("lrc"));
    if fs::write(&input, lyrics).is_err() {
        return Aligned::Failed;
    }
    // `lyrics_aligner_jobs` songs at a time (default 1): the aligner uses every CPU core, or the GPU.
    let result = {
        let _turn = Turn::take();
        Command::new(program).args(command).arg(file).arg(&input).arg(&output).output()
    };
    let lrc = match result {
        Ok(result) if result.status.success() => {
            fs::read_to_string(&output).ok().filter(|lrc| is_synced(lrc)).map_or(Aligned::Failed, Aligned::Lrc)
        }
        Ok(result) => {
            let error = String::from_utf8_lossy(&result.stderr);
            let error = error.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or_default();
            log("WARNING", &format!("lyrics aligner: {}: {}", file.display(), error.trim()));
            match result.status.code() {
                Some(3) => Aligned::NoFit,
                Some(4) => Aligned::Unsure,
                _ => Aligned::Failed,
            }
        }
        Err(error) => {
            log("WARNING", &format!("lyrics aligner: {program}: {error}"));
            Aligned::Failed
        }
    };
    let _ = (fs::remove_file(&input), fs::remove_file(&output));
    lrc
}

/// The `.lrc` next to the file; false when the aligner finds that no lyrics found are the song's. Synced lyrics
/// are timed to one recording: without an aligner, they are used when they were timed to a recording as long as
/// the file (the song's own lyrics, or else LRCLIB's for the file's length).
fn write_lrc(file: &Path, song: &Song) -> R<bool> {
    let sidecar = file.with_extension("lrc");
    let replace = matches!(text("overwrite"), Some("force" | "metadata"));
    if !on("generate_lrc") || (sidecar.exists() && !replace) {
        return Ok(true);
    }
    let length = lofty::read_from_path(file)?.properties().duration().as_secs_f64();
    let recording = Song { duration: length, ..song.clone() };
    let fits = (length - song.duration).abs() < 4.0;
    let own = song.lyrics.clone().filter(|lyrics| !lyrics.trim().is_empty());
    // Without an aligner, or when it cannot hear the words, synced lyrics are used when they were timed to a
    // recording as long as the file and their words are the song's (see `reference`): the song's own, the old
    // `.lrc`, or LRCLIB's.
    // Without such lyrics, an old `.lrc` with the words of another song is removed.
    // Without an answer from Genius, nothing can be checked: no `.lrc` is written and none is removed.
    let checked = |own: Option<String>| -> R<bool> {
        let Some(reference) = reference(song, own.as_ref()) else {
            unchecked(song);
            return Ok(true);
        };
        let agrees = |lyrics: &String| reference.as_ref().is_none_or(|words| same_lyrics(lyrics, words));
        // The old `.lrc` goes before LRCLIB, which is often busy.
        let own_synced = own.filter(|lyrics| fits && is_synced(lyrics));
        let old = fs::read_to_string(&sidecar).ok().filter(|lyrics| is_synced(lyrics));
        let found = std::iter::once_with(|| lrclib(&recording, false)).flatten().filter(|lyrics| is_synced(lyrics));
        if let Some(lrc) = own_synced.into_iter().chain(old).chain(found).find(agrees) {
            fs::write(&sidecar, lrc)?;
        } else if replace && fs::read_to_string(&sidecar).is_ok_and(|old| !agrees(&old)) {
            log("WARNING", &format!("{}: removed, its lyrics are another song's", sidecar.display()));
            fs::remove_file(&sidecar)?;
        }
        Ok(true)
    };
    if !text("lyrics_aligner").is_some_and(|command| !command.trim().is_empty()) {
        return checked(own);
    }
    // The aligner checks each candidate against the recording, in this order, and times the first that fits: the
    // old `.lrc`, the song's own synced lyrics, LRCLIB's versions as long as the file, the song's own plain lyrics,
    // and for a song that is not on Spotify, the Genius pages of each of its artists. Same titles and lengths do not
    // make the same song ("KING" on BULLY and on VULTURES 1). LRCLIB is often busy, so it is asked only when the
    // lyrics before it do not fit.
    let old = fs::read_to_string(&sidecar).ok().filter(|lrc| is_synced(lrc));
    let (own_synced, own_plain): (Vec<String>, Vec<String>) = own.clone().into_iter().partition(|l| is_synced(l));
    let sources: [Box<dyn FnOnce() -> Vec<String> + '_>; 5] = [
        Box::new(|| old.into_iter().collect()),
        Box::new(|| own_synced),
        Box::new(|| lrclib(&recording, false).into_iter().filter(|lyrics| !lyrics.is_empty()).collect()),
        Box::new(|| own_plain),
        Box::new(|| {
            let artists = song.artists.iter().chain([&song.artist]);
            if song.song_id.is_empty() {
                artists.filter_map(|artist| genius(song, artist, 4)).flatten().collect()
            } else {
                vec![]
            }
        }),
    ];
    let candidates = sources.into_iter().flat_map(|source| source());
    let mut tried = HashSet::new();
    let mut no_fit = false;
    for lyrics in candidates.into_iter().filter(|lyrics| tried.insert(plain_lyrics(lyrics))) {
        match align(file, &lyrics) {
            Aligned::Lrc(lrc) => {
                fs::write(sidecar, lrc)?;
                return Ok(true);
            }
            Aligned::NoFit => no_fit = true,
            // One transcript serves every candidate, so the others cannot tell either. An old `.lrc` stays.
            Aligned::Unsure if sidecar.exists() => return Ok(true),
            Aligned::Unsure => return checked(own),
            Aligned::Failed => return Ok(true),
        }
    }
    // An old `.lrc` with the words of another recording is removed.
    if no_fit && sidecar.exists() {
        log("WARNING", &format!("{}: removed, its lyrics do not fit the song", sidecar.display()));
        fs::remove_file(sidecar)?;
    }
    Ok(!no_fit)
}

/// Where the Spotify URL goes: the keys Python spotDL uses, so either tool can rescan the other's files.
fn url_key(format: TagType) -> ItemKey {
    match format {
        TagType::Id3v2 => ItemKey::AudioSourceUrl,
        TagType::Mp4Ilst => ItemKey::Unknown("----:spotdl:WOAS".into()),
        _ => ItemKey::Unknown("WOAS".into()),
    }
}

/// The key that says the lyrics providers found no lyrics for the song, so that `meta` does not look for them on
/// every run. It is not the lyrics key, which music servers would show as lyrics.
fn no_lyrics_key(format: TagType) -> ItemKey {
    match format {
        TagType::Mp4Ilst => ItemKey::Unknown("----:spotdl:NOLYRICS".into()),
        _ => ItemKey::Unknown("SPOTDL_NOLYRICS".into()),
    }
}

/// Writes only the lyrics into the tags, or without them (none found, or an instrumental's removed), the key that
/// says that none were found.
fn write_lyrics(file: &Path, lyrics: Option<String>) -> R<()> {
    let mut audio = lofty::read_from_path(file)?;
    let tag = audio.primary_tag_mut().ok_or("no tags")?;
    let marker = no_lyrics_key(tag.tag_type());
    tag.retain(|item| item.key() != &marker);
    match lyrics {
        Some(lyrics) => drop(tag.insert_text(ItemKey::Lyrics, lyrics)),
        None => {
            tag.remove_key(&ItemKey::Lyrics);
            tag.push_unchecked(TagItem::new(marker, ItemValue::Text("1".into())));
        }
    }
    Ok(audio.save_to_path(file, WriteOptions::default())?)
}

/// The Spotify URL of a tagged file, also from the `WWWAUDIOFILE` keys that earlier spotdl-rs versions wrote.
fn source_url(tag: &Tag) -> Option<String> {
    let names = ["WOAS", "----:spotdl:WOAS", "WWWAUDIOFILE", "----:com.apple.iTunes:WWWAUDIOFILE"];
    let known = |key: &ItemKey| match key {
        ItemKey::AudioSourceUrl | ItemKey::AudioFileUrl => true,
        ItemKey::Unknown(key) => names.iter().any(|name| name.eq_ignore_ascii_case(key)),
        _ => false,
    };
    let urls = tag.items().filter(|item| known(item.key()));
    urls.filter_map(|item| item.value().text().or(item.value().locator())).find(|url| !url.is_empty()).map(String::from)
}

/// The ReplayGain 2.0 reference loudness, in LUFS.
const RG_REFERENCE: f64 = -18.0;

/// The EBU R128 integrated loudness (LUFS) and true peak (linear) of a file, measured by FFmpeg.
fn loudness(file: &Path) -> Option<(f64, f64)> {
    let output = Command::new(text("ffmpeg").unwrap_or("ffmpeg"))
        .args(["-nostats", "-hide_banner", "-i"])
        .arg(file)
        .args(["-map", "0:a", "-filter:a", "ebur128=peak=true", "-f", "null", "-"])
        .output()
        .ok()?;
    let log = String::from_utf8_lossy(&output.stderr);
    let summary = &log[log.rfind("Summary:")?..];
    let value = |label: &str, unit: &str| summary.split_once(label)?.1.split_once(unit)?.0.trim().parse::<f64>().ok();
    // A silent track's peak is "-inf".
    let peak = value("Peak:", "dBFS").unwrap_or(-70.0);
    Some((value("I:", "LUFS")?, 10f64.powf(peak / 20.0)))
}

/// The track gain and peak tags: measured with `--replaygain`, else from Deezer's gain (no peak).
fn track_gain(file: &Path, s: &Song) -> (String, String) {
    match on("replaygain").then(|| loudness(file)).flatten() {
        Some((lufs, peak)) => (format!("{:.2} dB", RG_REFERENCE - lufs), format!("{peak:.6}")),
        // Deezer's gain is the track's loudness against 0 dB.
        None if s.extra.gain != 0.0 => (format!("{:.2} dB", RG_REFERENCE - s.extra.gain), String::new()),
        None => (String::new(), String::new()),
    }
}

/// The key of the explicit flag: lofty writes `ITUNESADVISORY` for ID3 and `rtng` for MP4, but drops it from
/// Vorbis comments (Opus, FLAC, Ogg), where music servers read `ITUNESADVISORY` too.
fn advisory_key(format: TagType) -> ItemKey {
    match format {
        TagType::VorbisComments => ItemKey::Unknown("ITUNESADVISORY".into()),
        _ => ItemKey::ParentalAdvisory,
    }
}

/// The compilation flag marks a Various Artists release, as Picard and iTunes set it, and is the same for every song
/// of the album. Spotify calls an artist's own anthologies and best-ofs (the 2018 White Album, 2Pac's Greatest Hits)
/// compilations too: flagged, they are filed under Various Artists by players. `RELEASETYPE` keeps Spotify's word.
fn compilation(s: &Song) -> bool {
    s.album_type == "compilation" && s.album_artist.eq_ignore_ascii_case("Various Artists")
}

fn write_tags(file: &Path, s: &Song) -> R<()> {
    let mut audio = lofty::read_from_path(file)?;
    let format = audio.primary_tag_type();
    let id3 = format == TagType::Id3v2;
    // spotdl's keys go into the file's own tag, so that the keys of other tools (MusicBrainz IDs, composers,
    // sort names, album gains) stay.
    let mut tag = audio.primary_tag().cloned().unwrap_or_else(|| Tag::new(format));
    let (gain, peak) = track_gain(file, s);
    // A measured gain (one with a peak) stays unless it is measured again: Deezer's gain is only an estimate.
    let measured = tag.get_string(&ItemKey::ReplayGainTrackPeak).is_some_and(|peak| !peak.is_empty());
    let gain = if peak.is_empty() && measured { String::new() } else { gain };
    let has_lyrics = s.lyrics.as_deref().is_some_and(|lyrics| !lyrics.is_empty());
    tag.set_title(s.name.clone());
    tag.set_artist(s.artists.join(text("id3_separator").unwrap_or("/")));
    tag.set_album(s.album_name.clone());
    tag.set_track(s.track_number);
    tag.set_track_total(s.tracks_count);
    tag.set_disk(s.disc_number);
    tag.set_disk_total(s.disc_count);
    if let Some(url) = &s.download_url {
        tag.set_comment(url.clone());
    }
    let bpm = match s.extra.bpm {
        0.0 => String::new(),
        bpm if id3 => bpm.round().to_string(),
        bpm => bpm.to_string(),
    };
    let single = |value: String| vec![value];
    let items = [
        (ItemKey::AlbumArtist, single(s.album_artist.clone())),
        (ItemKey::RecordingDate, single(s.date.clone())),
        (ItemKey::ReleaseDate, single(s.date.clone())),
        (ItemKey::OriginalReleaseDate, single(s.extra.original_date.clone())),
        (ItemKey::Year, single(if s.year > 0 { s.year.to_string() } else { String::new() })),
        (ItemKey::Isrc, single(s.isrc.clone().unwrap_or_default())),
        (ItemKey::Label, single(s.publisher.clone())),
        (ItemKey::CopyrightMessage, single(s.copyright_text.clone().unwrap_or_default())),
        (ItemKey::Lyrics, single(s.lyrics.clone().unwrap_or_default())),
        (no_lyrics_key(format), single(if !has_lyrics && genius_answered(s) { "1" } else { "" }.into())),
        (if id3 { ItemKey::IntegerBpm } else { ItemKey::Bpm }, single(bpm)),
        (ItemKey::Barcode, single(s.extra.upc.clone())),
        (ItemKey::ReplayGainTrackGain, single(gain)),
        (ItemKey::ReplayGainTrackPeak, single(peak)),
        (ItemKey::FlagCompilation, single(if compilation(s) { "1" } else { "" }.into())),
        (advisory_key(format), single(if s.explicit { "1" } else { "0" }.into())),
        (ItemKey::TrackArtists, if id3 { single(s.artists.join("\0")) } else { s.artists.clone() }),
        (ItemKey::Genre, s.genres.clone()),
        (ItemKey::Unknown("RELEASETYPE".into()), single(s.album_type.clone())),
        (url_key(format), single(s.url.clone())),
    ];
    // Other tools may write unknown Vorbis keys in lower case.
    let same_key = |a: &ItemKey, b: &ItemKey| match (a, b) {
        (ItemKey::Unknown(a), ItemKey::Unknown(b)) => a.eq_ignore_ascii_case(b),
        _ => a == b,
    };
    for (key, values) in items {
        let values: Vec<String> = values.into_iter().filter(|value| !value.is_empty()).collect();
        // A value spotdl does not have leaves the file's own, but the compilation flag is spotdl's to clear, and
        // lyrics clear the key that says none were found.
        let cleared = key == ItemKey::FlagCompilation || (has_lyrics && key == no_lyrics_key(format));
        if values.is_empty() && !cleared {
            continue;
        }
        tag.retain(|item| !same_key(item.key(), &key));
        for value in values {
            let url = matches!(key, ItemKey::AudioSourceUrl);
            let item = TagItem::new(key.clone(), if url { ItemValue::Locator(value) } else { ItemValue::Text(value) });
            if matches!(key, ItemKey::Unknown(_)) {
                tag.push_unchecked(item);
            } else {
                tag.push(item);
            }
        }
    }
    let cover = s.cover_url.as_deref().filter(|_| !on("skip_album_art")).and_then(art);
    if let Some(cover) = cover {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::Jpeg), None, cover));
    }
    // Tags of other types (a stray ID3v1 tag) go.
    audio.clear();
    if file.extension().is_some_and(|extension| extension == "wav") {
        audio.insert_tag(lofty::iff::wav::RiffInfoList::from(tag.clone()).into());
    }
    audio.insert_tag(tag);
    Ok(audio.save_to_path(file, WriteOptions::default())?)
}

/// With `--replaygain`, the album gain and peak of each album in the folders of `files`, from the track gains and
/// peaks of all of the folder's songs of that album: the album's loudness is the songs' loudness averaged as
/// energy over their lengths, and its peak the highest song peak. An album with a song that has no measured gain
/// is left as it is.
pub fn album_gains(files: &[PathBuf]) {
    if !on("replaygain") {
        return;
    }
    let dirs: HashSet<&Path> = files.iter().filter_map(|file| file.parent()).collect();
    dirs.into_par_iter().for_each(|dir| {
        if let Err(error) = album_gain(dir) {
            log("ERROR", &format!("{}: album gain: {error}", dir.display()));
        }
    });
}

fn album_gain(dir: &Path) -> R<()> {
    type Track = (PathBuf, Option<(f64, f64)>, f64);
    let mut albums: HashMap<String, Vec<Track>> = HashMap::new();
    for entry in fs::read_dir(dir)? {
        let file = entry?.path();
        let extension = file.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !AUDIO.contains(&extension.as_str()) || temp_file(&file) {
            continue;
        }
        let audio = lofty::read_from_path(&file)?;
        let Some(tag) = audio.primary_tag() else { continue };
        let number = |key: ItemKey| tag.get_string(&key)?.split_whitespace().next()?.parse::<f64>().ok();
        let loud = number(ItemKey::ReplayGainTrackGain).zip(number(ItemKey::ReplayGainTrackPeak));
        let length = audio.properties().duration().as_secs_f64();
        albums.entry(tag.album().unwrap_or_default().into()).or_default().push((file, loud, length));
    }
    for tracks in albums.values() {
        let Some(measured) =
            tracks.iter().map(|(_, loud, length)| loud.map(|loud| (loud, *length))).collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let total: f64 = measured.iter().map(|(_, length)| length).sum();
        if total <= 0.0 {
            continue;
        }
        let energy: f64 =
            measured.iter().map(|((gain, _), length)| length * 10f64.powf((RG_REFERENCE - gain) / 10.0)).sum();
        let gain = format!("{:.2} dB", RG_REFERENCE - 10.0 * (energy / total).log10());
        let peak = format!("{:.6}", measured.iter().map(|((_, peak), _)| *peak).fold(0.0, f64::max));
        for (file, _, _) in tracks {
            let mut audio = lofty::read_from_path(file)?;
            let tag = audio.primary_tag_mut().ok_or("no tags")?;
            if tag.get_string(&ItemKey::ReplayGainAlbumGain) == Some(gain.as_str())
                && tag.get_string(&ItemKey::ReplayGainAlbumPeak) == Some(peak.as_str())
            {
                continue;
            }
            tag.insert_text(ItemKey::ReplayGainAlbumGain, gain.clone());
            tag.insert_text(ItemKey::ReplayGainAlbumPeak, peak.clone());
            audio.save_to_path(file, WriteOptions::default())?;
        }
    }
    Ok(())
}

/// The track gain and peak written into a file that has its other tags: `--replaygain` on a song from before it.
fn write_track_gain(file: &Path) -> R<()> {
    let (gain, peak) = track_gain(file, &Song::default());
    if gain.is_empty() {
        return Err("could not measure the loudness".into());
    }
    let mut audio = lofty::read_from_path(file)?;
    let tag = audio.primary_tag_mut().ok_or("no tags")?;
    tag.insert_text(ItemKey::ReplayGainTrackGain, gain);
    tag.insert_text(ItemKey::ReplayGainTrackPeak, peak);
    Ok(audio.save_to_path(file, WriteOptions::default())?)
}

/// The folder of an album artist: the folder above the song's when it is named after the album artist, as with
/// `{album-artist}/{album}/...` output paths.
fn artist_folder(file: &Path, s: &Song) -> Option<PathBuf> {
    let folder = file.parent()?.parent()?;
    let name = folder.file_name()?.to_string_lossy();
    (!s.album_artist.is_empty() && search::slug(&name, "") == search::slug(&s.album_artist, ""))
        .then(|| folder.to_path_buf())
}

/// The image at `url`. Spotify lists album covers (`ab67616d<size><hash>`) at most 640 px wide, but serves the
/// same cover in its uploaded size (up to 2000 px) with the size `000082c1`; that one is taken when it is a JPEG
/// wider than 640 px, as old uploads may lack it or be smaller. Artist pictures are taken as listed.
fn art(url: &str) -> Option<Vec<u8>> {
    let hash = url.strip_prefix("https://i.scdn.co/image/ab67616d").and_then(|rest| rest.get(8..));
    let original = hash.map(|hash| format!("https://i.scdn.co/image/ab67616d000082c1{hash}"));
    let larger = |bytes: &Vec<u8>| PictureInformation::from_jpeg(bytes).is_ok_and(|info| info.width > 640);
    original.and_then(|original| http(&original, &[], None).ok().filter(larger)).or_else(|| http(url, &[], None).ok())
}

/// The images that `--folder-art` puts next to the songs, which music servers (Navidrome, Jellyfin) show:
/// `cover.jpg` in the song's folder, and the album artist's Spotify picture as `artist.jpg` in the artist's
/// folder.
fn folder_images(file: &Path, s: &Song) -> Vec<(PathBuf, Option<String>)> {
    if !on("folder_art") {
        return vec![];
    }
    let cover = file.parent().map(|dir| (dir.join("cover.jpg"), s.cover_url.clone()));
    let artist =
        artist_folder(file, s).filter(|_| !s.artist_id.is_empty()).map(|folder| (folder.join("artist.jpg"), None));
    cover.into_iter().chain(artist).filter(|(image, _)| !image.exists()).collect()
}

fn write_folder_art(file: &Path, s: &Song) -> R<()> {
    for (image, url) in folder_images(file, s) {
        let url = url.or_else(|| spotify::artist_image(&s.artist_id));
        let Some(bytes) = url.and_then(|url| art(&url)) else { continue };
        // Songs of one album are written at the same time: each writes its own part file and renames it.
        let part = image.with_extension(format!("{}.part", file.file_stem().unwrap_or_default().to_string_lossy()));
        fs::write(&part, bytes)?;
        fs::rename(&part, &image)?;
    }
    Ok(())
}

/// Downloads one video's audio next to `out`, converting with FFmpeg unless it is already in the right format;
/// returns the temporary file, which becomes `out` only once it is tagged.
fn fetch(url: &str, out: &Path, id: &str) -> R<PathBuf> {
    let (format, bitrate) = (text("format").unwrap_or("opus"), text("bitrate").unwrap_or("auto"));
    let selector = match format {
        "m4a" => "bestaudio[ext=m4a]/bestaudio",
        "opus" => "bestaudio[ext=webm]/bestaudio",
        _ => "bestaudio",
    };
    let template = out.with_extension(format!("{id}.%(ext)s"));
    let (got, abr) = search::youtube(|| {
        let mut command = search::yt_dlp();
        command.args(["-f", selector, "--no-playlist", "--no-warnings", "--force-overwrites", "-q"]);
        // The source's bitrate (`NA` when unknown) is what `--bitrate auto` encodes at.
        command.args(["--print", "after_move:%(abr)s %(filepath)s", "-o"]).arg(&template);
        if on("sponsor_block") {
            command.args(["--sponsorblock-remove", "all"]);
        }
        let output = command.arg(url).output().map_err(|error| error.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let got =
            stdout.lines().rev().filter_map(|line| line.split_once(' ')).find(|(_, path)| Path::new(path).is_file());
        got.map(|(abr, path)| (PathBuf::from(path), abr.parse::<f64>().ok()))
            .ok_or_else(|| format!("yt-dlp: {}", String::from_utf8_lossy(&output.stderr).trim()))
    })?;
    let extension = got.extension().unwrap_or_default().to_string_lossy().to_string();
    let keep = matches!(bitrate, "auto" | "disable");
    let temp = out.with_extension(format!("{id}.converted.{format}"));
    let convert = || -> R<()> {
        if extension == format && keep {
            return Ok(fs::rename(&got, &temp)?);
        }
        let codec = match format {
            _ if keep && extension == "webm" && format == "opus" => "copy",
            "mp3" => "libmp3lame",
            "flac" => "flac",
            "ogg" => "libvorbis",
            "opus" => "libopus",
            "m4a" => "aac",
            _ => "pcm_s16le",
        };
        let mut ffmpeg = Command::new(text("ffmpeg").unwrap_or("ffmpeg"));
        ffmpeg.args(["-nostdin", "-y", "-loglevel", "error", "-i"]).arg(&got);
        ffmpeg.args(["-vn", "-map_metadata", "-1", "-c:a", codec]);
        if codec != "copy" {
            ffmpeg.args(rate_args(bitrate, format, abr));
        }
        let output = ffmpeg.args(text("ffmpeg_args").unwrap_or_default().split_whitespace()).arg(&temp).output()?;
        match output.status.success() {
            true => Ok(()),
            false => Err(format!("ffmpeg: {}", String::from_utf8_lossy(&output.stderr).trim()).into()),
        }
    };
    let result = convert();
    let _ = fs::remove_file(&got);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map(|()| temp)
}

/// FFmpeg's arguments for `--bitrate`, as in Python spotDL: `auto` encodes at the source's bitrate (128k when
/// yt-dlp does not know it), digits are a VBR quality (`-q:a`, LAME's V0 to V9), anything else a bitrate, and
/// `disable` leaves the encoder's default. FLAC and WAV are lossless, so a bitrate means nothing to them.
fn rate_args(bitrate: &str, format: &str, abr: Option<f64>) -> Vec<String> {
    match bitrate {
        "disable" => vec![],
        "auto" if matches!(format, "flac" | "wav") => vec![],
        "auto" => vec!["-b:a".into(), format!("{}k", abr.filter(|abr| *abr >= 1.0).map_or(128, |abr| abr as u32))],
        _ if !bitrate.is_empty() && bitrate.bytes().all(|b| b.is_ascii_digit()) => vec!["-q:a".into(), bitrate.into()],
        _ => vec!["-b:a".into(), bitrate.into()],
    }
}

/// Whether `file` is one of `fetch`'s temporary files (`<stem>.<id>.converted.<format>`), which an interrupted
/// run can leave behind: it is no song of the library.
fn temp_file(file: &Path) -> bool {
    file.file_stem().map(Path::new).and_then(Path::extension).is_some_and(|extension| extension == "converted")
}

/// Removes the temporary files an interrupted download of `out` left: yt-dlp's `<stem>.<id>.<ext>` (and
/// `.part`) and `fetch`'s `<stem>.<id>.converted.<format>`. Only names that start with `<stem>.<id>.` go.
fn remove_temp_files(out: &Path, id: &str) {
    let prefix = out.with_extension(format!("{id}."));
    let (Some(dir), Some(prefix)) = (prefix.parent(), prefix.file_name()) else { return };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().as_encoded_bytes().starts_with(prefix.as_encoded_bytes()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Downloads the pinned URL, else the search candidates in order; returns the URL that worked and the
/// downloaded (untagged) temporary file.
fn download(song: &Song, out: &Path) -> R<(String, PathBuf)> {
    fs::create_dir_all(out.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
    let id = match song.song_id.as_str() {
        "" => format!("{:x}", Sha1::digest(&song.url)),
        id => id.into(),
    };
    remove_temp_files(out, &id);
    let mut failure = None;
    if let Some(url) = &song.download_url {
        match fetch(url, out, &id) {
            Ok(temp) => return Ok((url.clone(), temp)),
            Err(error) => failure = Some(error),
        }
    }
    let candidates = match search::find(song) {
        Ok(candidates) => candidates,
        Err(error) if error.to_string().starts_with("no match for ") && failure.is_some() => {
            return Err(failure.unwrap())
        }
        Err(error) => return Err(error),
    };
    for url in candidates.into_iter().filter(|url| song.download_url.as_ref() != Some(url)) {
        match fetch(&url, out, &id) {
            Ok(temp) => return Ok((url, temp)),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| "no match".into()))
}

/// The file `op` writes a song to: `meta --redownload` replaces a local song's own file (its name with the
/// selected format's extension), not a file at the output template's path, which would leave the old file as a
/// duplicate.
fn out_path(s: &Song, op: &str) -> (PathBuf, Option<&'static PathBuf>) {
    match REDOWNLOADS.get().filter(|_| op == "redownload").and_then(|files| files.get(&s.url)) {
        Some(file) => (file.with_extension(text("format").unwrap_or("opus")), Some(file)),
        None => (path(s), None),
    }
}

/// Saves, prints or downloads one song; `redownload` is a download that replaces the file.
fn finish(s: &mut Song, op: &str) -> R<PathBuf> {
    if op == "save" {
        if on("preload") && s.download_url.is_none() {
            // Spotify gives no ISRC; Deezer's is what lets the search find the release's own recording, as a
            // download does. The save file keeps it, so a later download searches by it too.
            enrich(s);
            s.download_url = search::find(s).ok().and_then(|urls| urls.into_iter().next());
        }
        s.lyrics = find_lyrics(s);
        return Ok(path(s));
    }
    if s.name.trim().is_empty() {
        return Err(format!("Spotify has no metadata for {}", s.url).into());
    }
    if op == "url" {
        if s.download_url.is_none() {
            // The same search as a download's: with Deezer's ISRC.
            enrich(s);
            s.download_url = search::find(s)?.into_iter().next();
        }
        return Ok(path(s));
    }
    let early = text("output").is_some_and(|output| ENRICHED.iter().any(|field| output.contains(field)));
    if early {
        enrich(s);
    }
    let (out, local) = out_path(s, op);
    let scan = || {
        let root = output_root();
        let mut files = Vec::new();
        audio_files(if root.as_os_str().is_empty() { Path::new(".") } else { &root }, &mut files);
        let tagged = files
            .into_iter()
            .filter_map(|file| Some((source_url(lofty::read_from_path(&file).ok()?.primary_tag()?)?, file)));
        tagged.collect()
    };
    let scanned = on("scan_for_songs").then(|| SCANNED.get_or_init(scan).get(&s.url).cloned()).flatten();
    let detected = list("detect_formats").map(|format| out.with_extension(format)).find(|path| path.is_file());
    let mut target = local
        .cloned()
        .or(scanned)
        .or_else(|| out.is_file().then(|| out.clone()))
        .or(detected)
        .unwrap_or_else(|| out.clone());
    let archived = text("archive").is_some_and(|file| {
        let read = || fs::read_to_string(file).unwrap_or_default().lines().map(String::from).collect();
        ARCHIVED.get_or_init(read).contains(&s.url)
    });
    let skip = on("respect_skip_file") && skip_path(&target).exists();
    let (exists, overwrite) = (target.is_file(), text("overwrite").unwrap_or("skip"));
    // A moved file is not downloaded again, but its tags are written again for its new title, album or year.
    let moved = exists && MOVED.get().is_some_and(|moved| moved.contains(&target));
    let keep = archived || skip || exists && overwrite == "skip" && !on("force_update_metadata");
    if op != "redownload" && !moved && keep {
        if exists && !archived {
            archive(&s.url)?;
        }
        return Ok(target);
    }
    if !early {
        enrich(s);
    }
    // A moved file's lyrics are the song's: they are kept rather than fetched again from paced sites.
    if moved && s.lyrics.is_none() {
        let tag = lofty::read_from_path(&target).ok();
        let lyrics = tag.as_ref().and_then(|audio| audio.primary_tag()?.get_string(&ItemKey::Lyrics).map(String::from));
        s.lyrics = lyrics.filter(|lyrics| !lyrics.is_empty());
    }
    // Lyrics sites are paced, so fetch them while the audio downloads.
    let song = s.clone();
    let (lyrics, downloaded) = std::thread::scope(|scope| {
        let lyrics = scope.spawn(|| song.lyrics.clone().or_else(|| find_lyrics(&song)));
        let downloaded = (!exists || overwrite == "force" || op == "redownload").then(|| download(&song, &out));
        (lyrics.join().ok().flatten(), downloaded)
    });
    s.lyrics = lyrics;
    match downloaded.transpose()? {
        // The audio is tagged before it gets its name: a file at `out` is a finished song, which later runs skip
        // and archive, so a failed or interrupted tagging must not leave an untagged one there.
        Some((url, temp)) => {
            s.download_url = Some(url);
            if let Err(error) = write_tags(&temp, s).and_then(|()| Ok(fs::rename(&temp, &out)?)) {
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
            if target != out && target.is_file() {
                fs::remove_file(&target)?;
                let _ = fs::remove_file(skip_path(&target));
            }
            target = out;
        }
        None => write_tags(&target, s)?,
    }
    write_folder_art(&target, s)?;
    write_lrc(&target, s)?;
    if on("create_skip_file") {
        fs::write(skip_path(&target), "")?;
    }
    archive(&s.url)?;
    Ok(target)
}

/// Adds Deezer's metadata, warning when Deezer failed: the song is then tagged without genres, ISRC or barcode.
fn enrich(s: &mut Song) -> bool {
    let result = spotify::enrich(s);
    if let Err(error) = &result {
        log("WARNING", &format!("{} - {}: no Deezer genres, ISRC or barcode: {error}", s.artists.join(", "), s.name));
    }
    result.is_ok()
}

pub fn process(mut song: Song, op: &str) -> (Song, R<PathBuf>) {
    let label = format!("{} - {}", song.artists.join(", "), song.name);
    let result = finish(&mut song, op);
    match &result {
        Ok(_) if op == "url" => println!("{}", song.download_url.as_deref().unwrap_or_default()),
        Ok(path) if on("simple_tui") => println!("{label}: {}", path.display()),
        Ok(path) => log("INFO", &format!("{label}: {}", path.display())),
        Err(error) => {
            log("ERROR", &format!("{label}: {error}"));
            if on("add_unavailable") && error.to_string().starts_with("no match for ") {
                let _ = archive(&song.url);
            }
        }
    }
    (song, result)
}

/// Moves (or, for songs no longer listed, deletes) the files of songs whose output path changed.
pub fn prune(old: &[Song], new: &[Song]) {
    let next: HashMap<&str, &Song> = new.iter().rev().map(|song| (song.url.as_str(), song)).collect();
    // A file a listed song writes is that song's, also when its URL changed (Spotify gives re-released
    // songs new IDs): it is kept for the song's job, not deleted before a download that can fail.
    let kept: HashSet<PathBuf> = new.iter().map(path).collect();
    let files = |file: &Path| [file.to_path_buf(), skip_path(file), file.with_extension("lrc")];
    let mut moved = HashSet::new();
    for song in old {
        let from = path(song);
        if kept.contains(&from) {
            continue;
        }
        let to = next.get(song.url.as_str()).map(|song| path(song));
        let targets = to.as_ref().filter(|to| !to.exists()).map(|to| files(to));
        // A moved song's `.lrc` goes with it; `--sync-remove-lrc` decides only whether a removed song's goes.
        let lrc = to.is_some() || on("sync_remove_lrc");
        let had = from.is_file();
        for (i, source) in files(&from).iter().enumerate().take(if lrc { 3 } else { 2 }) {
            // Only a missing source is no error: a renamed album's new folder does not exist yet.
            if !source.exists() {
                continue;
            }
            let result = match &targets {
                Some(targets) => fs::create_dir_all(targets[i].parent().unwrap_or(Path::new("")))
                    .and_then(|()| fs::rename(source, &targets[i])),
                None => fs::remove_file(source),
            };
            if let Err(error) = result {
                log("ERROR", &format!("could not move or remove {}: {error}", source.display()));
            }
        }
        // The file at the new path has the old title, album or year in its tags until they are written again.
        if let Some(to) = to.filter(|to| had && to.is_file()) {
            moved.insert(to);
        }
        remove_empty(&from);
    }
    let _ = MOVED.set(moved);
}

/// Removes the folders of `file` (up to the output's fixed folder) that hold nothing more than the folder
/// images of songs that are gone, so that a renamed or removed album leaves no empty album in the library.
fn remove_empty(file: &Path) {
    let root = output_root();
    let image = |file: &PathBuf| {
        file.is_file() && file.file_name().is_some_and(|name| name == "cover.jpg" || name == "artist.jpg")
    };
    for dir in
        file.ancestors().skip(1).take_while(|dir| !dir.as_os_str().is_empty() && dir.starts_with(&root) && *dir != root)
    {
        let Ok(entries) =
            fs::read_dir(dir).map(|entries| entries.flatten().map(|entry| entry.path()).collect::<Vec<_>>())
        else {
            return;
        };
        if !entries.iter().all(image)
            || entries.iter().try_for_each(fs::remove_file).and_then(|()| fs::remove_dir(dir)).is_err()
        {
            return;
        }
    }
}

/// Fills in missing tags, lyrics and `.lrc` files of a local song, and returns it when `redownload` is to download
/// it again.
fn meta_file(file: &Path) -> R<Option<Song>> {
    let audio = lofty::read_from_path(file)?;
    let tag = audio.primary_tag().ok_or("no tags")?;
    let has = |key: ItemKey| tag.get_string(&key).is_some_and(|value| !value.is_empty());
    // A Spotify song written before the explicit flag reached Vorbis comments has its tags written again.
    let from_spotify = source_url(tag).is_some_and(|url| url.contains("spotify"));
    let tagged = has(ItemKey::TrackTitle)
        && has(ItemKey::TrackArtist)
        && has(ItemKey::TrackArtists)
        && (on("skip_album_art") || !tag.pictures().is_empty())
        && (!from_spotify || has(advisory_key(tag.tag_type())));
    // A song whose lyrics were looked for and not found (an instrumental) is not looked for again on each run.
    let no_lyrics = has(no_lyrics_key(tag.tag_type()));
    let complete = tagged && (list("lyrics_providers").next().is_none() || has(ItemKey::Lyrics) || no_lyrics);
    // Deezer's gain has no peak: `--replaygain` measures songs without one.
    let measure = on("replaygain") && !has(ItemKey::ReplayGainTrackPeak);
    let album_artist = tag.get_string(&ItemKey::AlbumArtist).unwrap_or_default().to_string();
    let wanted = Song { album_artist, artist_id: "-".into(), ..Song::default() };
    let missing_art = !folder_images(file, &wanted).is_empty();
    let stored = tag.get_string(&ItemKey::Lyrics).filter(|lyrics| !lyrics.is_empty()).map(String::from);
    let lyrics = stored.as_deref().map(unlisted).filter(|lyrics| !lyrics.is_empty());
    // The comment is the URL the audio was downloaded from; tags written again keep it.
    let source = tag.comment().filter(|comment| comment.starts_with("http")).map(String::from);
    let (force, redownload) = (on("force_update_metadata"), on("redownload"));
    // The song as the file's own tags describe it.
    let mut artists: Vec<String> = tag.get_strings(&ItemKey::TrackArtists).map(String::from).collect();
    if artists.len() < 2 {
        artists = tag.artist().map(|a| a.split(['/', ';']).map(|a| a.trim().into()).collect()).unwrap_or_default();
    }
    let local = Song {
        name: tag.title().unwrap_or_default().into(),
        artist: artists.first().cloned().unwrap_or_default(),
        artists,
        album_name: tag.album().unwrap_or_default().into(),
        year: tag.year().unwrap_or_default(),
        track_number: tag.track().unwrap_or_default(),
        duration: audio.properties().duration().as_secs_f64(),
        lyrics: lyrics.clone(),
        ..Song::default()
    };
    // `--overwrite metadata` or `force` checks the files' lyrics again.
    let replace = matches!(text("overwrite"), Some("force" | "metadata"));
    if complete && !force && !redownload && !replace && !missing_art {
        if measure {
            write_track_gain(file)?;
        }
        if !on("generate_lrc") || file.with_extension("lrc").exists() || (lyrics.is_none() && no_lyrics) {
            return Ok(None);
        }
        if lyrics.as_deref().is_some_and(is_synced) {
            return write_lrc(file, &local).map(|_| None);
        }
    }
    let url = source_url(tag).filter(|url| url.contains("spotify"));
    let found = match &url {
        Some(url) => spotify::resolve(url)?.into_iter().next(),
        // Without a Spotify URL the match must be the same song on the same release, so that a song that is not
        // on Spotify (an unreleased one) does not take the tags of a released song with the same title, and an
        // album's song does not take those of a compilation or another edition. The track search finds a song's
        // most popular release (Three Little Birds of Exodus is on Legend), so the album is searched too.
        None => {
            let same = |a: &str, b: &str| search::versionless(a).eq_ignore_ascii_case(search::versionless(b));
            let fits = |song: &Song| {
                same(&song.name, &local.name)
                    && song.artists.contains(&local.artist)
                    && (song.duration - local.duration).abs() < 4.0
                    && (local.album_name.is_empty() || same(&song.album_name, &local.album_name))
                    && (local.track_number == 0 || song.track_number == local.track_number)
            };
            let track = spotify::resolve(&format!("{} - {}", local.artist, local.name)).ok();
            track.and_then(|songs| songs.into_iter().next()).filter(fits).or_else(|| {
                let album = format!("album:{} {}", local.artist, local.album_name);
                let songs = (!local.album_name.is_empty()).then(|| spotify::resolve(&album).ok()).flatten()?;
                songs.into_iter().find(fits)
            })
        }
    };
    let Some(mut song) = found.or_else(|| url.is_none().then(|| local.clone())) else { return Err("no match".into()) };
    let on_spotify = !song.song_id.is_empty();
    song.download_url = song.download_url.or(source);
    if on_spotify && !enrich(&mut song) {
        // Without Deezer the date is Spotify's, which can be only a year or month: the file's Deezer day stays.
        let date = tag.get_string(&ItemKey::RecordingDate);
        if let Some(date) = date.filter(|date| date.len() > song.date.len() && date.starts_with(&song.date)) {
            song.date = date.into();
        }
    }
    // `force` fetches the lyrics again, for example to get synced lyrics that were not available before; so does
    // `--overwrite metadata` for a song whose lyrics were not found before.
    let look = !no_lyrics || replace;
    // An instrumental's lyrics are removed: they are another song's ("See-Saw"'s under "Interstellar Overdrive").
    let mut cleared = false;
    song.lyrics = if force {
        match found_lyrics(&song) {
            Some(None) => {
                cleared = lyrics.is_some();
                None
            }
            found => found.flatten().or(lyrics.clone()),
        }
    } else {
        lyrics.clone().or_else(|| look.then(|| find_lyrics(&song)).flatten())
    };
    // Lyrics in the tags that are another song's (LRCLIB's "KING" of VULTURES 1 under BULLY's) are replaced by the
    // song's Genius lyrics.
    if replace && !force {
        // Genius's page is asked for its albums only when the lyrics differ.
        let artists = song.artists.iter().chain([&song.artist]).filter(|artist| !artist.is_empty());
        let page =
            song.lyrics.as_ref().and_then(|_| artists.clone().find_map(|artist| genius_same(&song, artist).flatten()));
        let differ = |page: &Page| song.lyrics.as_ref().is_some_and(|old| !same_lyrics(old, &page.lyrics));
        if let Some(page) = page.filter(|page| differ(page) && released_on(&song, page) == Some(true)) {
            cleared = words(&page.lyrics).is_empty();
            if !cleared {
                log("WARNING", &format!("{}: lyrics replaced, they were another song's", file.display()));
            }
            song.lyrics = (!cleared).then_some(page.lyrics);
        }
    }
    if cleared {
        log("WARNING", &format!("{}: lyrics removed, the song is an instrumental", file.display()));
    }
    if on_spotify {
        write_folder_art(file, &song)?;
    }
    // A song whose tags are not written again (tagged, or not on Spotify) is measured on its own.
    if measure && (!on_spotify || (tagged && !force)) {
        write_track_gain(file)?;
    }
    if on_spotify {
        // Missing lyrics alone do not write the tags again: only the lyrics, or the key that none were found. Tags
        // written again keep the file's lyrics when the song has none, so an instrumental's are removed after them.
        if !tagged || force {
            write_tags(file, &song)?;
        }
        if cleared || (tagged && !force && (song.lyrics != stored || (!complete && genius_answered(&song)))) {
            write_lyrics(file, song.lyrics.clone())?;
        }
        write_lrc(file, &song)?;
    } else {
        // A song that is not on Spotify keeps its own tags. Without other lyrics, the words of its old `.lrc` file
        // are timed again. New lyrics go into the tags unless the aligner finds that they are another song's.
        let new = song.lyrics.clone().filter(|_| song.lyrics != stored);
        let none_found = song.lyrics.is_none() && (cleared || !no_lyrics && genius_answered(&song));
        if song.lyrics.is_none() && !cleared {
            song.lyrics = fs::read_to_string(file.with_extension("lrc")).ok().map(|lrc| plain_lyrics(&lrc));
        }
        let fits = write_lrc(file, &song)?;
        if let Some(new) = new.filter(|_| fits) {
            write_lyrics(file, Some(new))?;
        } else if none_found {
            write_lyrics(file, None)?;
        }
    }
    Ok((redownload && on_spotify).then_some(song))
}

/// One download per output file, local songs first: a song is downloaded once, also when it is on the album of
/// several files (`--fetch-albums`), and the threads do not write the same file. A pinned video is kept.
fn redownload_jobs(songs: Vec<Song>) -> Vec<Song> {
    let (mut jobs, mut slots) = (Vec::<Song>::new(), HashMap::new());
    for song in songs {
        let slot = *slots.entry(out_path(&song, "redownload").0).or_insert_with(|| {
            jobs.push(song.clone());
            jobs.len() - 1
        });
        if jobs[slot].download_url.is_none() {
            jobs[slot].download_url = song.download_url;
        }
    }
    jobs
}

pub fn meta(paths: &[String]) -> R<()> {
    let mut files = Vec::new();
    for path in paths.iter().map(Path::new) {
        match path.is_file() {
            true => files.push(path.to_path_buf()),
            false => audio_files(path, &mut files),
        }
    }
    // `threads` files at a time, as downloads: most of the time goes to waiting for lyrics sites and the aligner.
    let pool = rayon::ThreadPoolBuilder::new().num_threads(number("threads").max(1) as usize).build()?;
    let results: Vec<R<Option<Song>>> = pool.install(|| {
        files
            .par_iter()
            .with_max_len(1)
            .map(|file| {
                let result = meta_file(file);
                println!(
                    "{}: {}",
                    file.display(),
                    result.as_ref().map_or_else(|error| error.to_string(), |_| "updated".into())
                );
                result
            })
            .collect()
    });
    let (count, failed) = (files.len(), results.iter().filter(|result| result.is_err()).count());
    // The songs are downloaded again after all files are done, as a download, so that each is downloaded once.
    let local: Vec<(PathBuf, Song)> =
        files.iter().zip(results).filter_map(|(file, result)| Some((file.clone(), result.ok()??))).collect();
    let _ = REDOWNLOADS.set(local.iter().rev().map(|(file, song)| (song.url.clone(), file.clone())).collect());
    let mut songs: Vec<Song> = local.into_iter().map(|(_, song)| song).collect();
    let mut lost = 0;
    if on("fetch_albums") {
        let mut albums: Vec<String> = songs
            .iter()
            .filter(|song| !song.album_id.is_empty())
            .map(|song| format!("https://open.spotify.com/album/{}", song.album_id))
            .collect();
        albums.sort();
        albums.dedup();
        let resolved: Vec<_> =
            pool.install(|| albums.par_iter().map(|album| (album, spotify::resolve(album))).collect());
        for (album, result) in resolved {
            match result {
                Ok(more) => songs.extend(more),
                Err(error) => {
                    log("ERROR", &format!("{album}: {error}"));
                    lost += 1;
                }
            }
        }
    }
    let jobs = redownload_jobs(songs);
    let done: Vec<R<PathBuf>> =
        pool.install(|| jobs.into_par_iter().with_max_len(1).map(|song| process(song, "redownload").1).collect());
    lost += done.iter().filter(|result| result.is_err()).count();
    album_gains(&files.into_iter().chain(done.into_iter().flatten()).collect::<Vec<_>>());
    report_unchecked();
    match (failed, lost) {
        (0, 0) => Ok(()),
        (failed, 0) => Err(format!("{failed} of {count} files failed").into()),
        (failed, lost) => Err(format!("{failed} of {count} files and {lost} downloads failed").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        among, art, compilation, genius_account, lrclib_lyrics, lrclib_search, lrclib_tracks, out_path, path, provider,
        rate_args, redownload_jobs, reference, same_lyrics, template_path, unlisted, Relaxed, Song, GENIUS_BLOCKED,
        LRCLIB_FOUND, LRCLIB_KEPT, REDOWNLOADS,
    };
    use std::path::PathBuf;

    const BULLY_KING: &str =
        "[Intro]\nThe time is now, right now\nThis is the hour, this is the new dawn, this is the new day\n\
        Now is the time, for nature and all her glory have named you her king\nShe has named you the king\n\
        This that feelin' we need more of\nThe hatin' just brought me more love";
    const VULTURES_KING: &str =
        "[00:05.86]After everything said, huh, man, man\n[00:12.00]After everything said, huh\n\
        [00:18.27]\"Crazy, bipolar, antisemite\"\n[00:23.04]And I'm still the king\n\
        [00:31.56]They thought headlines was my kryptonite\n[00:36.14]Still the king";

    /// Covers were the 640 px image that Spotify lists, though it serves the same cover at 1500 px.
    #[test]
    fn album_covers_are_taken_in_their_original_size() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let cover = art("https://i.scdn.co/image/ab67616d0000b273b694e89ba937dd2631ff584c").unwrap();
        assert_eq!(lofty::picture::PictureInformation::from_jpeg(&cover).unwrap().width, 1500);
    }

    /// Spotify's compilations of one artist's songs were flagged as compilations, which players file under Various
    /// Artists, out of the artist's discography.
    #[test]
    fn only_various_artists_compilations_are_flagged() {
        let album =
            |kind: &str, artist: &str| Song { album_type: kind.into(), album_artist: artist.into(), ..Song::default() };
        assert!(compilation(&album("compilation", "Various Artists")));
        assert!(compilation(&album("compilation", "various artists")));
        assert!(!compilation(&album("compilation", "The Beatles")));
        assert!(!compilation(&album("album", "Various Artists")));
    }

    #[test]
    fn lyrics_of_one_song_agree_and_of_two_songs_do_not() {
        let synced = "[00:00.30]The time is now, right now\n[00:04.18]This is the hour, this is the new dawn, \
            this is the new day\n[00:10.38]Now is the time, for nature and all her glory have named you her king";
        assert!(same_lyrics(synced, BULLY_KING));
        assert!(!same_lyrics(VULTURES_KING, BULLY_KING));
        // An instrumental's page has no words, which another song's words are not.
        assert!(!same_lyrics("[Instrumental]", BULLY_KING));
        assert!(same_lyrics("[Instrumental]", "[Instrumental]"));
        // Too few words to tell.
        let count_in = "Drive my car take 1\n\n1,2,3,4\n\n(instrumental)\n\n2\n\n1,2,3,4\n\n(instrumental)";
        let talk = "[00:01.30]\"Drive My Car,\" take one\n[00:12.63]Can you take it a little bit slower this time?\n\
            [00:17.37]One, two, three, four\n[01:02.16]Two\n[01:13.77]That's the coming in part\n[01:15.61]One, two";
        assert!(same_lyrics(count_in, talk));
    }

    /// `--bitrate auto` was taken as `disable` (FFmpeg's 128k for MP3), and `--bitrate 0` went to `-b:a 0`, which
    /// FFmpeg also takes as 128k CBR instead of LAME's V0.
    #[test]
    fn bitrates_are_passed_as_python_spotdl_does() {
        assert_eq!(rate_args("auto", "mp3", Some(160.155)), ["-b:a", "160k"]);
        assert_eq!(rate_args("auto", "ogg", None), ["-b:a", "128k"]);
        assert!(rate_args("auto", "flac", Some(160.0)).is_empty());
        assert!(rate_args("disable", "mp3", Some(160.0)).is_empty());
        assert_eq!(rate_args("0", "mp3", Some(160.0)), ["-q:a", "0"]);
        assert_eq!(rate_args("256k", "mp3", Some(160.0)), ["-b:a", "256k"]);
    }

    /// LRCLIB's word that a song is an instrumental is an answer (`""`), but only when every version as long as the
    /// song says so: all 20 results for Pink Floyd's "Interstellar Overdrive" are instrumental.
    #[test]
    fn lrclib_instrumentals_are_an_answer() {
        let song = Song { name: "Interstellar Overdrive".into(), duration: 581.0, ..Song::default() };
        let track = |d: f64, i: bool, p: &str| serde_json::json!({"duration": d, "instrumental": i, "plainLyrics": p});
        let instrumental = [track(582.0, true, ""), track(580.0, true, ""), track(300.0, false, "Marigolds")];
        assert_eq!(lrclib_lyrics(&song, &instrumental), vec![String::new()]);
        let mixed = [track(582.0, true, ""), track(580.0, false, "Some words")];
        assert_eq!(lrclib_lyrics(&song, &mixed), vec!["Some words".to_string()]);
        assert!(lrclib_lyrics(&song, &[track(300.0, true, "")]).is_empty());
    }

    /// LRCLIB's results are kept only for the `.lrc` writer's search after the provider's, and only for a while:
    /// every search's results, kept until the run ended, filled gigabytes over a library.
    #[test]
    fn lrclib_results_are_not_kept_for_the_whole_run() {
        let reply = |_: &str| Some(vec![serde_json::json!({"duration": 200.0, "plainLyrics": "Words"})]);
        let none = |_: &str| None;
        let kept = |url: &str| LRCLIB_FOUND.lock().unwrap().iter().any(|(key, _)| key == url);
        // The writer's search, the only one when Genius answered, keeps nothing.
        assert!(lrclib_tracks("test:writer", false, reply).is_some());
        assert!(!kept("test:writer"));
        // The provider's search is kept, and the writer takes it.
        assert!(lrclib_tracks("test:provider", true, reply).is_some());
        assert!(lrclib_tracks("test:provider", false, none).is_some());
        assert!(!kept("test:provider"));
        // Results nobody takes make room for newer ones.
        for n in 0..=LRCLIB_KEPT {
            lrclib_tracks(&format!("test:untaken {n}"), true, reply);
        }
        assert!(LRCLIB_FOUND.lock().unwrap().len() <= LRCLIB_KEPT);
        assert!(!kept("test:untaken 0") && kept(&format!("test:untaken {LRCLIB_KEPT}")));
    }

    /// Genius's "You might also like", glued into lyrics by earlier scrapes, is not lyrics.
    #[test]
    fn genius_song_lists_are_dropped_from_lyrics() {
        assert_eq!(unlisted("[Instrumental and Screams]You might also like"), "[Instrumental and Screams]");
        assert_eq!(unlisted("I love youYou might also like[Chorus]\nOh"), "I love you\n[Chorus]\nOh");
        assert_eq!(unlisted("Hey\nYou might also like[Bridge]"), "Hey\n[Bridge]");
        assert_eq!(unlisted("You might also like me"), "You might also like me");
    }

    /// A Genius that cannot be asked (its website answers 403) is not a Genius without the song's page, which
    /// would let LRCLIB's lyrics in unchecked: BULLY's "KING" would get VULTURES 1's.
    #[test]
    fn lyrics_are_not_checked_against_a_genius_that_cannot_be_asked() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        GENIUS_BLOCKED.store(true, Relaxed);
        let artists = vec!["¥$".to_string(), "Kanye West".to_string()];
        let song = Song { name: "KING".into(), artist: "¥$".into(), artists, year: 2026, ..Song::default() };
        assert_eq!(reference(&song, None), None);
        assert_eq!(provider("synced", &song), None);
        // The song's own plain lyrics still check synced lyrics.
        assert_eq!(reference(&song, Some(&BULLY_KING.into())), Some(Some(BULLY_KING.into())));
    }

    /// LRCLIB is asked again only through the paced loop: `http`'s own retries came 2, 4 and 8 seconds after a
    /// 503 or 429, from each thread, ignoring the hold that told every thread to wait.
    #[test]
    fn lrclib_retries_wait_for_the_shared_hold() {
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/search", server.local_addr().unwrap());
        let (sent, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (n, stream) in server.incoming().take(2).enumerate() {
                let mut stream = stream.unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    request.push(byte[0]);
                }
                sent.send(Instant::now()).unwrap();
                let (status, body) = if n == 0 { ("503 Service Unavailable", "") } else { ("200 OK", "[]") };
                let reply =
                    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let search = std::thread::spawn(move || lrclib_search(&url));
        let first = asked.recv().unwrap();
        // Another thread was told to slow down.
        super::LRCLIB.hold(Duration::from_secs(5));
        assert_eq!(search.join().unwrap(), Some(Vec::new()));
        assert!(asked.recv().unwrap() - first >= Duration::from_secs(4));
    }

    /// An artist is matched by whole words: "¥$" (slug "y") took Genius Ukrainian Translations' "CARNIVAL" as its
    /// own, as that name has a y in it.
    #[test]
    fn genius_artists_are_matched_by_whole_names() {
        assert!(among("¥$", "¥$, Kanye West & Ty Dolla $ign"));
        assert!(!among("¥$", "Genius Ukrainian Translations (Український переклад)"));
        assert!(!among("¥$", "Kanye West & Ty Dolla $ign"));
        assert!(!among("¥$", "Y & Kanye West"));
        assert!(!among("Nas", "Jonas Brothers"));
        assert!(among("Lil Wayne", "Drake & LilWayne"));
        assert!(among("Kanye West", "¥$, Kanye West & Ty Dolla $ign"));
        assert!(!among("", "Kanye West"));
        assert!(genius_account("Genius Ukrainian Translations (Український переклад)", "¥$"));
        assert!(genius_account("Genius Traducciones al Español", "Bad Bunny"));
        assert!(!genius_account("¥$", "¥$"));
    }

    /// `meta --redownload --fetch-albums` downloaded an album once for each of its files, from several threads into
    /// the same file, and saved songs at the output template's path, leaving the local `.mp3` beside the new `.opus`.
    #[test]
    fn redownloads_replace_local_files_and_download_each_song_once() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let file = PathBuf::from("/music/Artist/Album (2020)/Artist - Album (2020) - 01 - One.mp3");
        let _ = REDOWNLOADS.set([("https://open.spotify.com/track/1".into(), file.clone())].into());
        let song = |id: &str, name: &str| Song {
            url: format!("https://open.spotify.com/track/{id}"),
            name: name.into(),
            artists: vec!["Artist".into()],
            ..Song::default()
        };
        let local = Song { download_url: Some("https://music.youtube.com/watch?v=pinned".into()), ..song("1", "One") };
        // The local song, then the album's songs of each of two files of the album.
        let album = [song("1", "One"), song("2", "Two")];
        let songs = [vec![local], album.to_vec(), album.to_vec()].concat();
        let jobs = redownload_jobs(songs);
        assert_eq!(jobs.iter().map(|song| song.name.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
        assert_eq!(jobs[0].download_url.as_deref(), Some("https://music.youtube.com/watch?v=pinned"));
        assert_eq!(out_path(&jobs[0], "redownload"), (file.with_extension("opus"), Some(&file)));
        assert_eq!(out_path(&jobs[1], "redownload"), (path(&jobs[1]), None));
        // Only `meta --redownload` writes to the local file; a download goes by the output template.
        assert_eq!(out_path(&jobs[0], "download").0, PathBuf::from("Artist - One.opus"));
    }

    /// The folders written in `--output` were filled like song values: `../lib` became `_/lib` and `My Music: 50% Off`
    /// became `My Music 50 Off`, so songs went elsewhere than `--scan-for-songs` looked.
    #[test]
    fn output_folders_stay_as_written() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let song = Song { name: "Song: A".into(), artists: vec!["Art".into()], ..Song::default() };
        let out = |template: &str| template_path(template, &song);
        assert_eq!(out("../lib/{artists} - {title}.{output-ext}"), PathBuf::from("../lib/Art - Song A.opus"));
        assert_eq!(out("./x/{title}"), PathBuf::from("./x/Song A.opus"));
        assert_eq!(out("../Music"), PathBuf::from("../Music/Art - Song A.opus"));
        assert_eq!(
            out("/srv/My Music: 50% Off/{artists}/{title}"),
            PathBuf::from("/srv/My Music: 50% Off/Art/Song A.opus")
        );
        // A filled part is still made safe: a value cannot climb out of the folder.
        let climb = Song { name: "../etc".into(), artists: vec!["Art".into()], ..Song::default() };
        assert_eq!(template_path("lib/{title}", &climb), PathBuf::from("lib/..etc.opus"));
    }
}
