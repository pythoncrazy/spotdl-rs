use crate::{http, list, log, on, search, spotify, text, url_encode, Song, R};
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
    sync::{LazyLock, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

/// Template fields that only exist after Deezer enrichment.
const ENRICHED: [&str; 7] = ["{genre}", "{isrc}", "{publisher}", "{upc}", "{bpm}", "{gain}", "{original-date}"];
const AUDIO: [&str; 6] = ["mp3", "flac", "ogg", "opus", "m4a", "wav"];

static ARCHIVED: OnceLock<HashSet<String>> = OnceLock::new();
static SCANNED: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();
static LRCLIB: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));
static GENIUS: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

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

/// Waits for this site's next free slot, so all threads together send one request per `ms`.
fn pace(next: &Mutex<Instant>, ms: u64) {
    let slot = {
        let mut next = next.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = (*next).max(Instant::now());
        *next = slot + Duration::from_millis(ms);
        slot
    };
    std::thread::sleep(slot.saturating_duration_since(Instant::now()));
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

/// Lyrics from one provider: `synced` (LRCLIB), `genius`, `azlyrics` or `musixmatch`.
fn provider(name: &str, song: &Song) -> Option<String> {
    let get = |url: &str, headers: &[(&str, &str)]| String::from_utf8(http(url, headers, None).ok()?).ok();
    let (artist, title) = (&song.artist, search::versionless(&song.name));
    let lyrics = match name {
        "synced" => {
            pace(&LRCLIB, 750);
            let url = format!(
                "https://lrclib.net/api/search?artist_name={}&track_name={}",
                url_encode(artist),
                url_encode(title)
            );
            let tracks: Vec<Value> = serde_json::from_str(&get(&url, &[])?).ok()?;
            let same = |track: &&Value| (track["duration"].as_f64().unwrap_or_default() - song.duration).abs() < 4.0;
            let track = tracks.iter().find(same)?;
            track["syncedLyrics"].as_str().or(track["plainLyrics"].as_str())?.into()
        }
        "genius" => {
            let auth = text("genius_token").filter(|token| !token.is_empty()).map(|token| format!("Bearer {token}"));
            let headers: Vec<(&str, &str)> = auth.iter().map(|auth| ("Authorization", auth.as_str())).collect();
            pace(&GENIUS, 1000);
            let url = format!("https://genius.com/api/search/song?q={}", url_encode(&format!("{artist} {title}")));
            let reply: Value = serde_json::from_str(&get(&url, &headers)?).ok()?;
            let artist = search::slug(artist, "");
            let same = |hit: &&Value| {
                let primary = search::slug(hit["result"]["primary_artist"]["name"].as_str().unwrap_or_default(), "");
                !artist.is_empty() && !primary.is_empty() && (primary.contains(&artist) || artist.contains(&primary))
            };
            let page =
                reply["response"]["sections"][0]["hits"].as_array()?.iter().find(same)?["result"]["url"].as_str()?;
            pace(&GENIUS, 1000);
            html(&get(page, &[])?, "data-lyrics-container=\"true\"", "LyricsFooter")?
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

/// The `.lrc` next to the file, from the song's lyrics or else from LRCLIB, when they are synced.
fn write_lrc(file: &Path, song: &Song) -> R<()> {
    let sidecar = file.with_extension("lrc");
    let replace = matches!(text("overwrite"), Some("force" | "metadata"));
    if !on("generate_lrc") || (sidecar.exists() && !replace) {
        return Ok(());
    }
    let synced = song.lyrics.clone().filter(|lyrics| is_synced(lyrics));
    if let Some(lyrics) = synced.or_else(|| provider("synced", song).filter(|lyrics| is_synced(lyrics))) {
        fs::write(sidecar, lyrics)?;
    }
    Ok(())
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
    if complete && !force && !redownload {
        if !on("generate_lrc") || file.with_extension("lrc").exists() {
            return Ok(());
        }
        if lyrics.as_deref().is_some_and(is_synced) {
            return write_lrc(file, &Song { lyrics, ..Song::default() });
        }
    }
    let url = source_url(tag).filter(|url| url.contains("spotify"));
    let query =
        url.unwrap_or_else(|| format!("{} - {}", tag.artist().unwrap_or_default(), tag.title().unwrap_or_default()));
    let mut song = spotify::resolve(&query)?.into_iter().next().ok_or("no match")?;
    spotify::enrich(&mut song);
    song.lyrics = lyrics.or_else(|| find_lyrics(&song));
    if !complete || force {
        write_tags(file, &song)?;
    }
    write_lrc(file, &song)?;
    if redownload {
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
