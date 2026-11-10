//! Where songs and their cover images go: the `--output` template, and the folder images.

use crate::{
    http::{self},
    on, search, spotify, text, Song, R,
};
use lofty::picture::PictureInformation;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

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
        // A name with one leading dot is hidden, and Navidrome's scanner skips it (`...And Justice for All` is not).
        let value = if value.starts_with('.') && !value.starts_with("..") { format!("_{value}") } else { value };
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
pub(crate) fn output_root() -> PathBuf {
    let output = Path::new(text("output").unwrap_or_default());
    output.components().take_while(|part| !part.as_os_str().to_string_lossy().contains('{')).collect()
}

pub(crate) fn skip_path(file: &Path) -> PathBuf {
    let mut path = file.as_os_str().to_os_string();
    path.push(".skip");
    path.into()
}

pub(crate) fn audio_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for path in fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path()) {
        let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if path.is_dir() {
            audio_files(&path, files);
        } else if AUDIO.contains(&extension.as_str()) && !temp_file(&path) {
            files.push(path);
        }
    }
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
pub(crate) fn art(url: &str) -> Option<Vec<u8>> {
    let hash = url.strip_prefix("https://i.scdn.co/image/ab67616d").and_then(|rest| rest.get(8..));
    let original = hash.map(|hash| format!("https://i.scdn.co/image/ab67616d000082c1{hash}"));
    let larger = |bytes: &Vec<u8>| PictureInformation::from_jpeg(bytes).is_ok_and(|info| info.width > 640);
    original.and_then(|original| http::get(&original, &[]).ok().filter(larger)).or_else(|| http::get(url, &[]).ok())
}

/// The images that `--folder-art` puts next to the songs, which music servers (Navidrome, Jellyfin) show:
/// `cover.jpg` in the song's folder, and the album artist's Spotify picture as `artist.jpg` in the artist's
/// folder, whose URL (`None`) is looked up only when it is wanted. An album without a cover gets no `cover.jpg`:
/// the artist's picture there would be shown as the album's cover, and never replaced.
pub(crate) fn folder_images(file: &Path, s: &Song) -> Vec<(PathBuf, Option<String>)> {
    let cover = file.parent().zip(s.cover_url.clone()).map(|(dir, url)| (dir.join("cover.jpg"), Some(url)));
    let artist =
        artist_folder(file, s).filter(|_| !s.artist_id.is_empty()).map(|folder| (folder.join("artist.jpg"), None));
    cover.into_iter().chain(artist).filter(|(image, _)| !image.exists()).collect()
}

pub(crate) fn write_folder_art(file: &Path, s: &Song) -> R<()> {
    if !on("folder_art") {
        return Ok(());
    }
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

pub(crate) const AUDIO: [&str; 6] = ["mp3", "flac", "ogg", "opus", "m4a", "wav"];

/// Whether `file` is one of `fetch`'s temporary files (`<stem>.<id>.converted.<format>`), which an interrupted
/// run can leave behind: it is no song of the library.
pub(crate) fn temp_file(file: &Path) -> bool {
    file.file_stem().map(Path::new).and_then(Path::extension).is_some_and(|extension| extension == "converted")
}

#[cfg(test)]
#[path = "fuzz/files.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;

    /// Covers were the 640 px image that Spotify lists, though it serves the same cover at 1500 px.
    #[test]
    fn album_covers_are_taken_in_their_original_size() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let cover = art("https://i.scdn.co/image/ab67616d0000b273b694e89ba937dd2631ff584c").unwrap();
        assert_eq!(lofty::picture::PictureInformation::from_jpeg(&cover).unwrap().width, 1500);
    }

    /// An album without a cover URL got its artist's picture as `cover.jpg`, which servers show as its cover.
    #[test]
    fn albums_without_a_cover_get_no_cover_image() {
        let file = std::env::temp_dir().join("spotdl-no-such-folder/Artist/Album/Artist - Song.opus");
        let song = |cover: Option<&str>| Song {
            album_artist: "Artist".into(),
            artist_id: "id".into(),
            cover_url: cover.map(String::from),
            ..Song::default()
        };
        let artist = (file.parent().unwrap().parent().unwrap().join("artist.jpg"), None);
        let cover = (file.parent().unwrap().join("cover.jpg"), Some("url".to_string()));
        assert_eq!(folder_images(&file, &song(Some("url"))), [cover, artist.clone()]);
        assert_eq!(folder_images(&file, &song(None)), [artist]);
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

    /// Slipknot's `.5: The Gray Chapter` made the hidden folder `.5 The Gray Chapter (2014)` and files starting with
    /// `.5`, which Navidrome's scanner and file browsers skip, so the album never showed up.
    #[test]
    fn leading_dot_names_are_not_hidden() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let album = |name: &str| Song {
            name: "Song A".into(),
            artists: vec!["Slipknot".into()],
            album_artist: "Slipknot".into(),
            album_name: name.into(),
            year: 2014,
            track_number: 1,
            ..Song::default()
        };
        let out = |name: &str| {
            template_path("{album-artist}/{album} ({year})/{album} - {track-number} - {title}", &album(name))
        };
        assert_eq!(
            out(".5: The Gray Chapter"),
            PathBuf::from("Slipknot/_.5 The Gray Chapter (2014)/_.5 The Gray Chapter - 01 - Song A.opus")
        );
        // Names that start with `..` are shown and scanned, and stay as they are.
        assert_eq!(
            out("...And Justice for All"),
            PathBuf::from("Slipknot/...And Justice for All (2014)/...And Justice for All - 01 - Song A.opus")
        );
    }
}
