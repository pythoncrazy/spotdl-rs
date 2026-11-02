use crate::{http, list, log, number, on, search, spotify, text, url_encode, Pace, Song, R};
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::*,
    tag::{ItemKey, ItemValue, Tag, TagItem, TagType},
};
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering::Relaxed},
        Condvar, Mutex, OnceLock, PoisonError,
    },
};

/// Template fields that only exist after Deezer enrichment.
const ENRICHED: [&str; 7] = ["{genre}", "{isrc}", "{publisher}", "{upc}", "{bpm}", "{gain}", "{original-date}"];
const AUDIO: [&str; 6] = ["mp3", "flac", "ogg", "opus", "m4a", "wav"];

static ARCHIVED: OnceLock<HashSet<String>> = OnceLock::new();
static SCANNED: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
static LRCLIB: Pace = Pace::new(750);
static GENIUS: Pace = Pace::new(1000);
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

/// The output file: each template part filled, made path-safe and cut to `max_filename_length` (and 200 bytes).
pub fn path(s: &Song) -> PathBuf {
    let template = match text("output").unwrap_or_default() {
        "" => "{artists} - {title}".into(),
        output if output.contains('{') => output.to_string(),
        dir => format!("{dir}/{{artists}} - {{title}}"),
    };
    let max = crate::number("max_filename_length");
    let part = |part: &str| {
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
        } else if AUDIO.contains(&extension.as_str()) {
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

/// LRCLIB's lyrics for versions as long as the song: those of the song's album first, then synced ones.
/// LRCLIB's search results for a URL. LRCLIB refuses requests (429, 503) when it is busy. All threads wait, then
/// try again, so that a busy moment does not replace synced lyrics with plain ones.
fn lrclib_search(url: &str) -> Option<Vec<Value>> {
    let mut body = None;
    for attempt in 0..6 {
        LRCLIB.wait();
        match http(url, &[], None) {
            Ok(bytes) => {
                body = String::from_utf8(bytes).ok();
                break;
            }
            Err(error) if ["HTTP 429", "HTTP 503"].iter().any(|code| error.to_string().contains(code)) => {
                LRCLIB.hold(std::time::Duration::from_secs(15 << attempt.min(3)));
            }
            Err(error) => {
                log("WARNING", &format!("LRCLIB: {error}"));
                break;
            }
        }
    }
    body.and_then(|body| serde_json::from_str::<Vec<Value>>(&body).ok())
}

fn lrclib(song: &Song) -> Vec<String> {
    let url = format!(
        "https://lrclib.net/api/search?artist_name={}&track_name={}",
        url_encode(&song.artist),
        url_encode(lyrics_title(song))
    );
    // The lyrics provider and the `.lrc` writer search for the same song; LRCLIB is often busy, so it is asked once
    // and the second search takes the results (they are then not kept: a library has many songs).
    static FOUND: std::sync::LazyLock<Mutex<HashMap<String, Vec<Value>>>> = std::sync::LazyLock::new(Default::default);
    let found = FOUND.lock().unwrap_or_else(PoisonError::into_inner).remove(&url);
    let tracks = found.or_else(|| {
        let tracks = lrclib_search(&url)?;
        FOUND.lock().unwrap_or_else(PoisonError::into_inner).insert(url.clone(), tracks.clone());
        Some(tracks)
    });
    let Some(tracks) = tracks else { return vec![] };
    let album = plain_title(&song.album_name);
    let text =
        |track: &Value, key: &str| track[key].as_str().filter(|lyrics| !lyrics.trim().is_empty()).map(String::from);
    let mut found: Vec<(bool, bool, String)> = tracks
        .iter()
        .filter(|track| (track["duration"].as_f64().unwrap_or_default() - song.duration).abs() < 4.0)
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
/// (a song can have several pages: "Everybody", "Everybody (Original)", "Everybody (Demo 3)").
fn genius(song: &Song, artist: &str, max: usize) -> Vec<String> {
    let get = |url: &str, headers: &[(&str, &str)]| String::from_utf8(http(url, headers, None).ok()?).ok();
    let auth = text("genius_token").filter(|token| !token.is_empty()).map(|token| format!("Bearer {token}"));
    let headers: Vec<(&str, &str)> = auth.iter().map(|auth| ("Authorization", auth.as_str())).collect();
    let title = lyrics_title(song);
    GENIUS.wait();
    let url = format!("https://genius.com/api/search/song?q={}", url_encode(&format!("{artist} {title}")));
    let Some(reply) = get(&url, &headers).and_then(|reply| serde_json::from_str::<Value>(&reply).ok()) else {
        return vec![];
    };
    let (artist, song_title) = (search::slug(artist, ""), plain_title(title));
    // The same artist and the same title: the artist's first hit can be another of the artist's songs. The
    // artist may be one of several on the page ("¥$, Kanye West & Ty Dolla $ign").
    let same = |hit: &&Value| {
        let artists = search::slug(hit["result"]["artist_names"].as_str().unwrap_or_default(), "");
        let primary = search::slug(hit["result"]["primary_artist"]["name"].as_str().unwrap_or_default(), "");
        let hit_title = plain_title(hit["result"]["title"].as_str().unwrap_or_default());
        !artist.is_empty()
            && (artists.contains(&artist) || (!primary.is_empty() && artist.contains(&primary)))
            && !hit_title.is_empty()
            && (hit_title.contains(&song_title) || song_title.contains(&hit_title))
    };
    let hits = reply["response"]["sections"][0]["hits"].as_array().cloned().unwrap_or_default();
    let pages: Vec<String> =
        hits.iter().filter(same).filter_map(|hit| hit["result"]["url"].as_str().map(String::from)).take(max).collect();
    pages
        .iter()
        .filter_map(|page| {
            GENIUS.wait();
            genius_lyrics(&get(page, &[])?)
        })
        .collect()
}

/// Lyrics from one provider: `synced` (LRCLIB), `genius`, `azlyrics` or `musixmatch`.
fn provider(name: &str, song: &Song) -> Option<String> {
    let get = |url: &str, headers: &[(&str, &str)]| String::from_utf8(http(url, headers, None).ok()?).ok();
    let (artist, title) = (&song.artist, lyrics_title(song));
    let lyrics = match name {
        "synced" => lrclib(song).into_iter().next()?,
        "genius" => genius(song, artist, 1).into_iter().next()?,
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

fn find_lyrics(song: &Song) -> Option<String> {
    list("lyrics_providers").find_map(|name| provider(name, song))
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
    // Without an aligner, or when it cannot hear the words, synced lyrics as long as the file are trusted.
    let trusted = |own: Option<String>| -> R<bool> {
        let synced = own.filter(|lyrics| fits && is_synced(lyrics));
        if let Some(lrc) = synced.or_else(|| lrclib(&recording).into_iter().find(|lyrics| is_synced(lyrics))) {
            fs::write(&sidecar, lrc)?;
        }
        Ok(true)
    };
    if !text("lyrics_aligner").is_some_and(|command| !command.trim().is_empty()) {
        return trusted(own);
    }
    // The aligner checks each candidate against the recording, in this order, and times the first that fits:
    // the song's own lyrics, LRCLIB's versions as long as the file, and for a song that is not on Spotify, the
    // Genius pages of each of its artists. Same titles and lengths do not make the same song ("KING" on BULLY
    // and on VULTURES 1).
    let mut candidates: Vec<String> = own.clone().into_iter().collect();
    candidates.extend(lrclib(&recording));
    if song.song_id.is_empty() {
        for artist in song.artists.iter().chain([&song.artist]) {
            candidates.extend(genius(song, artist, 4));
        }
    }
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
            Aligned::Unsure => return trusted(own),
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

fn write_tags(file: &Path, s: &Song) -> R<()> {
    let mut audio = lofty::read_from_path(file)?;
    let format = audio.primary_tag_type();
    let id3 = format == TagType::Id3v2;
    let mut tag = Tag::new(format);
    tag.set_title(s.name.clone());
    tag.set_artist(s.artists.join(text("id3_separator").unwrap_or("/")));
    tag.set_album(s.album_name.clone());
    tag.set_track(s.track_number);
    tag.set_track_total(s.tracks_count);
    tag.set_disk(s.disc_number);
    tag.set_disk_total(s.disc_count);
    tag.set_comment(s.download_url.clone().unwrap_or_default());
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
        (if id3 { ItemKey::IntegerBpm } else { ItemKey::Bpm }, single(bpm)),
        (ItemKey::Barcode, single(s.extra.upc.clone())),
        // Deezer's gain is the track's loudness against 0 dB; ReplayGain targets -18 dB.
        (
            ItemKey::ReplayGainTrackGain,
            single(if s.extra.gain != 0.0 { format!("{:.2} dB", -18.0 - s.extra.gain) } else { String::new() }),
        ),
        (ItemKey::FlagCompilation, single(if s.album_type == "compilation" { "1".into() } else { String::new() })),
        (ItemKey::ParentalAdvisory, single(if s.explicit { "1" } else { "0" }.into())),
        (ItemKey::TrackArtists, if id3 { single(s.artists.join("\0")) } else { s.artists.clone() }),
        (ItemKey::Genre, s.genres.clone()),
        (ItemKey::Unknown("RELEASETYPE".into()), single(s.album_type.clone())),
        (url_key(format), single(s.url.clone())),
    ];
    for (key, value) in
        items.into_iter().flat_map(|(key, values)| values.into_iter().map(move |value| (key.clone(), value)))
    {
        if value.is_empty() {
            continue;
        }
        let url = matches!(key, ItemKey::AudioSourceUrl);
        let item = TagItem::new(key.clone(), if url { ItemValue::Locator(value) } else { ItemValue::Text(value) });
        if matches!(key, ItemKey::Unknown(_)) {
            tag.insert_unchecked(item);
        } else {
            tag.push(item);
        }
    }
    let cover = s.cover_url.as_deref().filter(|_| !on("skip_album_art")).and_then(|url| http(url, &[], None).ok());
    if let Some(cover) = cover {
        tag.push_picture(Picture::new_unchecked(PictureType::CoverFront, Some(MimeType::Jpeg), None, cover));
    }
    audio.clear();
    if file.extension().is_some_and(|extension| extension == "wav") {
        audio.insert_tag(lofty::iff::wav::RiffInfoList::from(tag.clone()).into());
    }
    audio.insert_tag(tag);
    Ok(audio.save_to_path(file, WriteOptions::default())?)
}

/// Downloads one video's audio to `out`, converting with FFmpeg unless it is already in the right format.
fn fetch(url: &str, out: &Path, id: &str) -> R<()> {
    let (format, bitrate) = (text("format").unwrap_or("opus"), text("bitrate").unwrap_or("auto"));
    let selector = match format {
        "m4a" => "bestaudio[ext=m4a]/bestaudio",
        "opus" => "bestaudio[ext=webm]/bestaudio",
        _ => "bestaudio",
    };
    let template = out.with_extension(format!("{id}.%(ext)s"));
    let got = search::youtube(|| {
        let mut command = search::yt_dlp();
        command.args(["-f", selector, "--no-playlist", "--no-warnings", "--force-overwrites", "-q"]);
        command.args(["--print", "after_move:filepath", "-o"]).arg(&template);
        if on("sponsor_block") {
            command.args(["--sponsorblock-remove", "all"]);
        }
        let output = command.arg(url).output().map_err(|error| error.to_string())?;
        let got = String::from_utf8_lossy(&output.stdout).lines().rev().map(PathBuf::from).find(|path| path.is_file());
        got.ok_or_else(|| format!("yt-dlp: {}", String::from_utf8_lossy(&output.stderr).trim()))
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
        if !keep {
            ffmpeg.args(["-b:a", bitrate]);
        }
        let output = ffmpeg.args(text("ffmpeg_args").unwrap_or_default().split_whitespace()).arg(&temp).output()?;
        match output.status.success() {
            true => Ok(()),
            false => Err(format!("ffmpeg: {}", String::from_utf8_lossy(&output.stderr).trim()).into()),
        }
    };
    let result = convert().and_then(|()| Ok(fs::rename(&temp, out)?));
    let _ = fs::remove_file(&got);
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Downloads the pinned URL, else the search candidates in order; returns the URL that worked.
fn download(song: &Song, out: &Path) -> R<String> {
    fs::create_dir_all(out.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new(".")))?;
    let id = match song.song_id.as_str() {
        "" => format!("{:x}", Sha1::digest(&song.url)),
        id => id.into(),
    };
    let mut failure = None;
    if let Some(url) = &song.download_url {
        match fetch(url, out, &id) {
            Ok(()) => return Ok(url.clone()),
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
            Ok(()) => return Ok(url),
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| "no match".into()))
}

/// Saves, prints or downloads one song; `redownload` is a download that replaces the file.
fn finish(s: &mut Song, op: &str) -> R<PathBuf> {
    if op == "save" {
        if on("preload") && s.download_url.is_none() {
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
            s.download_url = search::find(s)?.into_iter().next();
        }
        return Ok(path(s));
    }
    let early = text("output").is_some_and(|output| ENRICHED.iter().any(|field| output.contains(field)));
    if early {
        spotify::enrich(s);
    }
    let out = path(s);
    let scan = || {
        let output = Path::new(text("output").unwrap_or_default());
        let root: PathBuf =
            output.components().take_while(|part| !part.as_os_str().to_string_lossy().contains('{')).collect();
        let mut files = Vec::new();
        audio_files(if root.as_os_str().is_empty() { Path::new(".") } else { &root }, &mut files);
        let tagged = files
            .into_iter()
            .filter_map(|file| Some((source_url(lofty::read_from_path(&file).ok()?.primary_tag()?)?, file)));
        tagged.collect()
    };
    let scanned = on("scan_for_songs").then(|| SCANNED.get_or_init(scan).get(&s.url).cloned()).flatten();
    let detected = list("detect_formats").map(|format| out.with_extension(format)).find(|path| path.is_file());
    let mut target = scanned.or_else(|| out.is_file().then(|| out.clone())).or(detected).unwrap_or_else(|| out.clone());
    let archived = text("archive").is_some_and(|file| {
        let read = || fs::read_to_string(file).unwrap_or_default().lines().map(String::from).collect();
        ARCHIVED.get_or_init(read).contains(&s.url)
    });
    let skip = on("respect_skip_file") && skip_path(&target).exists();
    let (exists, overwrite) = (target.is_file(), text("overwrite").unwrap_or("skip"));
    if op != "redownload" && (archived || skip || exists && overwrite == "skip" && !on("force_update_metadata")) {
        if exists && !archived {
            archive(&s.url)?;
        }
        return Ok(target);
    }
    if !early {
        spotify::enrich(s);
    }
    // Lyrics sites are paced, so fetch them while the audio downloads.
    let song = s.clone();
    let (lyrics, downloaded) = std::thread::scope(|scope| {
        let lyrics = scope.spawn(|| song.lyrics.clone().or_else(|| find_lyrics(&song)));
        let downloaded = (!exists || overwrite == "force" || op == "redownload").then(|| download(&song, &out));
        (lyrics.join().ok().flatten(), downloaded)
    });
    s.lyrics = lyrics;
    if let Some(url) = downloaded.transpose()? {
        s.download_url = Some(url);
        if target != out && target.is_file() {
            fs::remove_file(&target)?;
        }
        target = out;
    }
    write_tags(&target, s)?;
    write_lrc(&target, s)?;
    if on("create_skip_file") {
        fs::write(skip_path(&target), "")?;
    }
    archive(&s.url)?;
    Ok(target)
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
    let files = |file: &Path| [file.to_path_buf(), skip_path(file), file.with_extension("lrc")];
    for song in old {
        let from = path(song);
        let to = next.get(song.url.as_str()).map(|song| path(song));
        if to.as_ref() == Some(&from) {
            continue;
        }
        let targets = to.filter(|to| !to.exists()).map(|to| files(&to));
        for (i, source) in files(&from).iter().enumerate().take(if on("sync_remove_lrc") { 3 } else { 2 }) {
            let result = match &targets {
                Some(targets) => fs::rename(source, &targets[i]),
                None => fs::remove_file(source),
            };
            if let Some(error) = result.err().filter(|error| error.kind() != std::io::ErrorKind::NotFound) {
                log("ERROR", &format!("could not move or remove {}: {error}", source.display()));
            }
        }
    }
}

/// Fills in missing tags, lyrics and `.lrc` files of local songs, and with `redownload` downloads them again.
fn meta_file(file: &Path) -> R<()> {
    let audio = lofty::read_from_path(file)?;
    let tag = audio.primary_tag().ok_or("no tags")?;
    let has = |key: ItemKey| tag.get_string(&key).is_some_and(|value| !value.is_empty());
    let complete = has(ItemKey::TrackTitle)
        && has(ItemKey::TrackArtist)
        && has(ItemKey::TrackArtists)
        && (list("lyrics_providers").next().is_none() || has(ItemKey::Lyrics))
        && (on("skip_album_art") || !tag.pictures().is_empty());
    let lyrics = tag.get_string(&ItemKey::Lyrics).map(String::from);
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
        duration: audio.properties().duration().as_secs_f64(),
        lyrics: lyrics.clone(),
        ..Song::default()
    };
    if complete && !force && !redownload {
        // `--overwrite metadata` or `force` checks the `.lrc` files that exist again.
        let replace = matches!(text("overwrite"), Some("force" | "metadata"));
        if !on("generate_lrc") || (file.with_extension("lrc").exists() && !replace) {
            return Ok(());
        }
        if lyrics.as_deref().is_some_and(is_synced) {
            return write_lrc(file, &local).map(drop);
        }
    }
    let url = source_url(tag).filter(|url| url.contains("spotify"));
    let found = match &url {
        Some(url) => spotify::resolve(url)?.into_iter().next(),
        // Without a Spotify URL the search result must be the same song, so that a song that is not on Spotify
        // (an unreleased one) does not take the tags of a released song with the same title.
        None => spotify::resolve(&format!("{} - {}", local.artist, local.name))
            .ok()
            .and_then(|songs| songs.into_iter().next())
            .filter(|song| {
                let same = |a: &str, b: &str| search::versionless(a).eq_ignore_ascii_case(search::versionless(b));
                same(&song.name, &local.name)
                    && song.artists.contains(&local.artist)
                    && (song.duration - local.duration).abs() < 4.0
            }),
    };
    let Some(mut song) = found.or_else(|| url.is_none().then(|| local.clone())) else { return Err("no match".into()) };
    let on_spotify = !song.song_id.is_empty();
    if on_spotify {
        spotify::enrich(&mut song);
    }
    // `force` fetches the lyrics again, for example to get synced lyrics that were not available before.
    song.lyrics =
        if force { find_lyrics(&song).or(lyrics.clone()) } else { lyrics.clone().or_else(|| find_lyrics(&song)) };
    if on_spotify {
        if !complete || force {
            write_tags(file, &song)?;
        }
        write_lrc(file, &song)?;
    } else {
        // A song that is not on Spotify keeps its own tags. Without other lyrics, the words of its old `.lrc` file
        // are timed again. New lyrics go into the tags unless the aligner finds that they are another song's.
        let new = song.lyrics.clone().filter(|_| song.lyrics != lyrics);
        if song.lyrics.is_none() {
            song.lyrics = fs::read_to_string(file.with_extension("lrc")).ok().map(|lrc| plain_lyrics(&lrc));
        }
        let fits = write_lrc(file, &song)?;
        if let Some(new) = new.filter(|_| fits) {
            let mut audio = lofty::read_from_path(file)?;
            audio.primary_tag_mut().ok_or("no tags")?.insert_text(ItemKey::Lyrics, new);
            audio.save_to_path(file, WriteOptions::default())?;
        }
    }
    if redownload && on_spotify {
        let album = format!("https://open.spotify.com/album/{}", song.album_id);
        let songs = if on("fetch_albums") { spotify::resolve(&album)? } else { vec![song] };
        for song in songs {
            process(song, "redownload").1?;
        }
    }
    Ok(())
}

pub fn meta(paths: &[String]) -> R<()> {
    let mut files = Vec::new();
    for path in paths.iter().map(Path::new) {
        match path.is_file() {
            true => files.push(path.to_path_buf()),
            false => audio_files(path, &mut files),
        }
    }
    let failed = files.iter().filter(|file| {
        let result = meta_file(file);
        println!(
            "{}: {}",
            file.display(),
            result.as_ref().map_or_else(|error| error.to_string(), |()| "updated".into())
        );
        result.is_err()
    });
    match failed.count() {
        0 => Ok(()),
        count => Err(format!("{count} of {} files failed", files.len()).into()),
    }
}
