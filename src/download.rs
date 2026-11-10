//! Downloading songs and tagging them, and `meta`, which tags the files that are already there.

use crate::files::{audio_files, folder_images, output_root, path, skip_path, write_folder_art};
use crate::genius::{genius_same, genius_titled, released_on, Asked, Page};
use crate::lyrics::{
    clean_lrc, find_lyrics, found_lyrics, genius_answered, instrumental_title, is_synced, plain_lyrics,
    report_unchecked, same_lyrics, timed_for, unlisted, words, write_lrc, Lyrics,
};
use crate::replaygain::{album_gains, write_track_gain};
use crate::tags::{advisory_key, audio, no_lyrics_key, source_url, write_lyrics, write_tags};
use crate::{list, log, number, on, search, spotify, text, Song, R};
use lofty::{prelude::*, tag::ItemKey};
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

/// Template fields that only exist after Deezer enrichment.
const ENRICHED: [&str; 7] = ["{genre}", "{isrc}", "{publisher}", "{upc}", "{bpm}", "{gain}", "{original-date}"];

static ARCHIVED: OnceLock<HashSet<String>> = OnceLock::new();

static SCANNED: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();

/// The files `sync` moved to a new path, whose tags are written again.
static MOVED: OnceLock<HashSet<PathBuf>> = OnceLock::new();

/// The files `meta --redownload` downloads again, by song URL.
static REDOWNLOADS: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();

fn archive(url: &str) -> R<()> {
    if let Some(path) = text("archive") {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        std::io::Write::write_all(&mut file, format!("{url}\n").as_bytes())?;
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
    // yt-dlp makes the template's folder when it writes the file, so a song with no match or no working download
    // leaves no empty artist and album folders.
    let template = out.with_extension(format!("{id}.%(ext)s"));
    let (got, abr) = search::youtube(|| {
        let mut command = search::yt_dlp();
        command.args(["-f", selector, "--no-playlist", "--no-warnings", "--force-overwrites", "-q"]);
        // The source's bitrate (`NA` when unknown) is what `--bitrate auto` encodes at.
        command.args(["--print", "after_move:%(abr)s %(filepath)s", "-o"]).arg(&template);
        if on("sponsor_block") {
            command.args(["--sponsorblock-remove", "all"]);
        }
        let output = search::run(command.arg(url))?;
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
        let output = search::run(ffmpeg.args(text("ffmpeg_args").unwrap_or_default().split_whitespace()).arg(&temp))?;
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

/// The id in `fetch`'s temporary names (`<stem>.<id>.converted.<format>`). The stem may fill 200 bytes, so a
/// Spotify id (22 bytes) or YouTube id (11) is kept, while a missing or longer id (SoundCloud slugs have no fixed
/// length) is hashed to 16 hex digits: the names then stay under the 255-byte file name limit.
fn temp_id(song: &Song) -> String {
    match song.song_id.as_str() {
        id if !id.is_empty() && id.len() <= 22 => id.into(),
        _ => format!("{:x}", Sha1::digest(&song.url))[..16].into(),
    }
}

/// Downloads the pinned URL, else the search candidates in order; returns the URL that worked and the
/// downloaded (untagged) temporary file.
fn download(song: &Song, out: &Path) -> R<(String, PathBuf)> {
    let id = temp_id(song);
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
        let tagged = files.into_iter().filter_map(|file| Some((source_url(audio(&file).ok()?.primary_tag()?)?, file)));
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
        let tag = audio(&target).ok();
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
    let audio = audio(file)?;
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
    let wanted = Song { album_artist, artist_id: "-".into(), cover_url: Some(String::new()), ..Song::default() };
    let missing_art = on("folder_art") && !folder_images(file, &wanted).is_empty();
    let stored = tag.get_string(&ItemKey::Lyrics).filter(|lyrics| !lyrics.is_empty()).map(String::from);
    // Synced lyrics in the tags with lines after the song's end are timed to a longer recording: their words stay,
    // without the times.
    let length = audio.properties().duration().as_secs_f64();
    let untimed = |lyrics: String| match timed_for(&lyrics, length) {
        true => lyrics,
        false => plain_lyrics(&lyrics).trim().to_string(),
    };
    let lyrics = stored.as_deref().map(unlisted).map(untimed).filter(|lyrics| !lyrics.is_empty());
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
        duration: length,
        lyrics: lyrics.clone(),
        ..Song::default()
    };
    // `--overwrite metadata` or `force` checks the files' lyrics again.
    let replace = matches!(text("overwrite"), Some("force" | "metadata"));
    clean_lrc(file)?;
    if complete && !force && !redownload && !replace && !missing_art {
        if measure {
            write_track_gain(file)?;
        }
        // Lyrics with Genius's song list or ticket ad in them are cleaned, also when nothing else is written.
        if stored.is_some() && lyrics != stored {
            write_lyrics(file, lyrics.clone())?;
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
            Some(Lyrics::Instrumental) => {
                cleared = lyrics.is_some();
                None
            }
            found => found.and_then(Lyrics::into_words).or(lyrics.clone()),
        }
    } else {
        lyrics.clone().or_else(|| look.then(|| find_lyrics(&song)).flatten())
    };
    // Lyrics in the tags that are another song's (LRCLIB's "KING" of VULTURES 1 under BULLY's) are replaced by the
    // song's Genius lyrics.
    let genius = list("lyrics_providers").any(|name| name == "genius" || name == "synced");
    if replace && !force && genius {
        // Genius's page is asked for its albums only when the lyrics differ.
        let artists = song.artists.iter().chain([&song.artist]).filter(|artist| !artist.is_empty());
        let page = song
            .lyrics
            .as_ref()
            .and_then(|_| artists.clone().find_map(|artist| genius_same(&song, artist).ok().flatten()));
        let differ = |page: &Page| song.lyrics.as_ref().is_some_and(|old| !same_lyrics(old, &page.lyrics));
        if let Some(page) = page.filter(|page| differ(page) && released_on(&song, page) == Ok(true)) {
            cleared = words(&page.lyrics).is_empty();
            if !cleared {
                log("WARNING", &format!("{}: lyrics replaced, they were another song's", file.display()));
            }
            song.lyrics = (!cleared).then_some(page.lyrics);
        } else if let Some(old) = song.lyrics.clone().filter(|_| instrumental_title(&song.name)) {
            // An instrumental's lyrics are those of its own page ("Big Poppa (Instrumental)", the studio talk of a
            // Beatles take), or none; the sung song's words are another recording's.
            let pages: Asked<Vec<Vec<Page>>> = artists.clone().map(|artist| genius_titled(&song, artist, 1)).collect();
            let pages = pages.ok().map(|pages| pages.concat());
            if let Some(pages) = pages.filter(|pages| !pages.iter().any(|page| same_lyrics(&old, &page.lyrics))) {
                let page = pages.into_iter().find(|page| !words(&page.lyrics).is_empty());
                cleared = page.is_none();
                if !cleared {
                    log("WARNING", &format!("{}: lyrics replaced, they were another song's", file.display()));
                }
                song.lyrics = page.map(|page| page.lyrics);
            }
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
        // `--overwrite metadata` writes the tags again from Spotify and Deezer (fixed track totals, dates and
        // flags reach files tagged before), keeping the file's lyrics, source URL and measured gain.
        if !tagged || force || replace {
            write_tags(file, &song)?;
        }
        if cleared || (tagged && !force && (song.lyrics != stored || (!complete && genius_answered(&song)))) {
            write_lyrics(file, song.lyrics.clone())?;
        }
        write_lrc(file, &song)?;
    } else {
        // A song that is not on Spotify keeps its own tags; only its lyrics change.
        let new = song.lyrics.clone().filter(|_| song.lyrics != stored);
        let none_found = song.lyrics.is_none() && (cleared || !no_lyrics && genius_answered(&song));
        write_lrc(file, &song)?;
        if let Some(new) = new {
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
    // `threads` files at a time, as downloads: most of the time goes to waiting for lyrics sites.
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
    use super::*;

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

    /// A song without a Spotify id got its URL's 40-digit SHA1 as the temporary names' id, and a SoundCloud song its
    /// slug: with a 200-byte stem the names went over 255 bytes and FFmpeg failed with "File name too long".
    #[test]
    fn temp_names_fit_the_file_name_limit() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let song = |id: &str, url: &str| Song {
            song_id: id.into(),
            url: url.into(),
            name: "Live At The BBC ".repeat(20),
            artists: vec!["Art".into()],
            ..Song::default()
        };
        let slug = "a-very-long-soundcloud-track-slug-that-goes-on-and-on-and-on-and-on-and-on-forever";
        for song in [
            song("", "https://open.spotify.com/track/none"),
            song(slug, &format!("https://soundcloud.com/art/{slug}")),
            song("4uLU6hMCjMI75M1A2tKUQC", "https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"),
        ] {
            let out = path(&song);
            let id = temp_id(&song);
            for name in [format!("{id}.converted.flac"), format!("{id}.webm.part")] {
                assert!(out.with_extension(name).file_name().unwrap().len() <= 255);
            }
        }
        // Short ids stay as they were, so earlier runs' temporary files are still found and removed.
        assert_eq!(temp_id(&song("4uLU6hMCjMI75M1A2tKUQC", "")), "4uLU6hMCjMI75M1A2tKUQC");
    }
}

#[cfg(test)]
#[path = "fuzz/download.rs"]
mod fuzz;
