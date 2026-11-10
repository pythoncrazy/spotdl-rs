//! Downloading songs: fetching the audio, tagging it, and putting it where the output template says.

use crate::files::{audio_files, output_root, path, skip_path, write_folder_art};
use crate::lyrics::{find_lyrics, write_lrc};
use crate::meta::REDOWNLOADS;
use crate::settings::settings;
use crate::tags::{audio, source_url, write_tags};
use crate::{log, search, spotify, Song, R};
use lofty::{prelude::*, tag::ItemKey};
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

fn archive(url: &str) -> R<()> {
    if let Some(path) = settings().archive.as_deref() {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
        std::io::Write::write_all(&mut file, format!("{url}\n").as_bytes())?;
    }
    Ok(())
}

/// Downloads one video's audio next to `out`, converting with FFmpeg unless it is already in the right format; returns
/// the temporary file, which becomes `out` only once it is tagged.
fn fetch(url: &str, out: &Path, id: &str) -> R<PathBuf> {
    let (format, bitrate) = (settings().format.as_str(), settings().bitrate.as_str());
    let selector = match format {
        "m4a" => "bestaudio[ext=m4a]/bestaudio",
        "opus" => "bestaudio[ext=webm]/bestaudio",
        _ => "bestaudio",
    };
    // yt-dlp makes the template's folder when it writes the file, so a song with no match or no working download leaves
    // no empty artist and album folders.
    let template = out.with_extension(format!("{id}.%(ext)s"));
    let (got, abr) = search::youtube(|| {
        let mut command = search::yt_dlp();
        command.args(["-f", selector, "--no-playlist", "--no-warnings", "--force-overwrites", "-q"]);
        // The source's bitrate (`NA` when unknown) is what `--bitrate auto` encodes at.
        command.args(["--print", "after_move:%(abr)s %(filepath)s", "-o"]).arg(&template);
        if settings().sponsor_block {
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
        let mut ffmpeg = Command::new(settings().ffmpeg.as_str());
        ffmpeg.args(["-nostdin", "-y", "-loglevel", "error", "-i"]).arg(&got);
        ffmpeg.args(["-vn", "-map_metadata", "-1", "-c:a", codec]);
        if codec != "copy" {
            ffmpeg.args(rate_args(bitrate, format, abr));
        }
        let output = search::run(
            ffmpeg.args(settings().ffmpeg_args.as_deref().unwrap_or_default().split_whitespace()).arg(&temp),
        )?;
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

/// FFmpeg's arguments for `--bitrate`, as in Python spotDL: `auto` encodes at the source's bitrate (128k when yt-dlp
/// does not know it), digits are a VBR quality (`-q:a`, LAME's V0 to V9), anything else a bitrate, and `disable` leaves
/// the encoder's default. FLAC and WAV are lossless, so a bitrate means nothing to them.
fn rate_args(bitrate: &str, format: &str, abr: Option<f64>) -> Vec<String> {
    match bitrate {
        "disable" => vec![],
        "auto" if matches!(format, "flac" | "wav") => vec![],
        "auto" => vec!["-b:a".into(), format!("{}k", abr.filter(|abr| *abr >= 1.0).map_or(128, |abr| abr as u32))],
        _ if !bitrate.is_empty() && bitrate.bytes().all(|b| b.is_ascii_digit()) => {
            vec!["-q:a".into(), bitrate.into()]
        }
        _ => vec!["-b:a".into(), bitrate.into()],
    }
}

/// Removes the temporary files an interrupted download of `out` left: yt-dlp's `<stem>.<id>.<ext>` (and `.part`) and
/// `fetch`'s `<stem>.<id>.converted.<format>`. Only names that start with `<stem>.<id>.` go.
fn remove_temp_files(out: &Path, id: &str) {
    let prefix = out.with_extension(format!("{id}."));
    let (Some(dir), Some(prefix)) = (prefix.parent(), prefix.file_name()) else {
        return;
    };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name().as_encoded_bytes().starts_with(prefix.as_encoded_bytes()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The id in `fetch`'s temporary names (`<stem>.<id>.converted.<format>`). The stem may fill 200 bytes, so a Spotify id
/// (22 bytes) or YouTube id (11) is kept, while a missing or longer id (SoundCloud slugs have no fixed length) is
/// hashed to 16 hex digits: the names then stay under the 255-byte file name limit.
fn temp_id(song: &Song) -> String {
    match song.song_id.as_str() {
        id if !id.is_empty() && id.len() <= 22 => id.into(),
        _ => format!("{:x}", Sha1::digest(&song.url))[..16].into(),
    }
}

/// Downloads the pinned URL, else the search candidates in order; returns the URL that worked and the downloaded
/// (untagged) temporary file.
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

/// The file `op` writes a song to: `meta --redownload` replaces a local song's own file (its name with the selected
/// format's extension), not a file at the output template's path, which would leave the old file as a duplicate.
pub(crate) fn out_path(s: &Song, op: &str) -> (PathBuf, Option<&'static PathBuf>) {
    match REDOWNLOADS.get().filter(|_| op == "redownload").and_then(|files| files.get(&s.url)) {
        Some(file) => (file.with_extension(settings().format.as_str()), Some(file)),
        None => (path(s), None),
    }
}

/// Saves, prints or downloads one song; `redownload` is a download that replaces the file.
fn finish(s: &mut Song, op: &str) -> R<PathBuf> {
    if op == "save" {
        if settings().preload && s.download_url.is_none() {
            // Spotify gives no ISRC; Deezer's is what lets the search find the release's own recording, as a download
            // does. The save file keeps it, so a later download searches by it too.
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
    let early = ENRICHED.iter().any(|field| settings().output.contains(field));
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
    let scanned = settings().scan_for_songs.then(|| SCANNED.get_or_init(scan).get(&s.url).cloned()).flatten();
    let detected = settings()
        .detect_formats
        .iter()
        .map(String::as_str)
        .map(|format| out.with_extension(format))
        .find(|path| path.is_file());
    let mut target = local
        .cloned()
        .or(scanned)
        .or_else(|| out.is_file().then(|| out.clone()))
        .or(detected)
        .unwrap_or_else(|| out.clone());
    let archived = settings().archive.as_deref().is_some_and(|file| {
        let read = || fs::read_to_string(file).unwrap_or_default().lines().map(String::from).collect();
        ARCHIVED.get_or_init(read).contains(&s.url)
    });
    let skip = settings().respect_skip_file && skip_path(&target).exists();
    let (exists, overwrite) = (target.is_file(), settings().overwrite.as_str());
    // A moved file is not downloaded again, but its tags are written again for its new title, album or year.
    let moved = exists && MOVED.get().is_some_and(|moved| moved.contains(&target));
    let keep = archived || skip || exists && overwrite == "skip" && !settings().force_update_metadata;
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
        // The audio is tagged before it gets its name: a file at `out` is a finished song, which later runs skip and
        // archive, so a failed or interrupted tagging must not leave an untagged one there.
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
    if settings().create_skip_file {
        fs::write(skip_path(&target), "")?;
    }
    archive(&s.url)?;
    Ok(target)
}

/// Adds Deezer's metadata, warning when Deezer failed: the song is then tagged without genres, ISRC or barcode.
pub(crate) fn enrich(s: &mut Song) -> bool {
    let result = spotify::enrich(s);
    if let Err(error) = &result {
        log("WARNING", &format!("{} - {}: no Deezer genres, ISRC or barcode: {error}", s.artists.join(", "), s.name));
    }
    result.is_ok()
}

pub(crate) fn process(mut song: Song, op: &str) -> (Song, R<PathBuf>) {
    let label = format!("{} - {}", song.artists.join(", "), song.name);
    let result = finish(&mut song, op);
    match &result {
        Ok(_) if op == "url" => println!("{}", song.download_url.as_deref().unwrap_or_default()),
        Ok(path) if settings().simple_tui => println!("{label}: {}", path.display()),
        Ok(path) => log("INFO", &format!("{label}: {}", path.display())),
        Err(error) => {
            log("ERROR", &format!("{label}: {error}"));
            if settings().add_unavailable && error.to_string().starts_with("no match for ") {
                let _ = archive(&song.url);
            }
        }
    }
    (song, result)
}

/// Moves (or, for songs no longer listed, deletes) the files of songs whose output path changed.
pub(crate) fn prune(old: &[Song], new: &[Song]) {
    let next: HashMap<&str, &Song> = new.iter().rev().map(|song| (song.url.as_str(), song)).collect();
    // A file a listed song writes is that song's, also when its URL changed (Spotify gives re-released songs new IDs):
    // it is kept for the song's job, not deleted before a download that can fail.
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
        let lrc = to.is_some() || settings().sync_remove_lrc;
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

/// Removes the folders of `file` (up to the output's fixed folder) that hold nothing more than the folder images of
/// songs that are gone, so that a renamed or removed album leaves no empty album in the library.
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

    /// A song without a Spotify id got its URL's 40-digit SHA1 as the temporary names' id, and a SoundCloud song its
    /// slug: with a 200-byte stem the names went over 255 bytes and FFmpeg failed with "File name too long".
    #[test]
    fn temp_names_fit_the_file_name_limit() {
        crate::settings::use_defaults();
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
