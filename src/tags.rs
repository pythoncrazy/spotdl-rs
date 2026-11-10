//! The tags of audio files: reading, writing, and the keys that each format keeps them under.

use crate::files::art;
use crate::lyrics::genius_answered;
use crate::replaygain::track_gain;
use crate::settings::settings;
use crate::{Song, R};
use lofty::{
    config::WriteOptions,
    picture::{MimeType, Picture, PictureType},
    prelude::*,
    tag::{ItemKey, ItemValue, Tag, TagItem, TagType},
};
use std::path::Path;

/// The file's audio and tags. `lofty` panics on some damaged files (an Opus header with no channels, an M4A whose atoms
/// end early); that fails the song, as an unreadable file does, and does not end the run.
pub(crate) fn audio(file: &Path) -> R<lofty::file::TaggedFile> {
    damaged(file, || Ok(lofty::read_from_path(file)?))
}

/// Writes the tags of `audio` to the file; see [`audio`].
pub(crate) fn save(audio: &lofty::file::TaggedFile, file: &Path) -> R<()> {
    damaged(file, || Ok(audio.save_to_path(file, WriteOptions::default())?))
}

fn damaged<T>(file: &Path, work: impl FnOnce() -> R<T>) -> R<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(format!("{}: damaged audio file", file.display()).into()))
}

/// Where the Spotify URL goes: the keys Python spotDL uses, so either tool can rescan the other's files.
fn url_key(format: TagType) -> ItemKey {
    match format {
        TagType::Id3v2 => ItemKey::AudioSourceUrl,
        TagType::Mp4Ilst => ItemKey::Unknown("----:spotdl:WOAS".into()),
        _ => ItemKey::Unknown("WOAS".into()),
    }
}

/// The key that says the lyrics providers found no lyrics for the song, so that `meta` does not look for them on every
/// run. It is not the lyrics key, which music servers would show as lyrics.
pub(crate) fn no_lyrics_key(format: TagType) -> ItemKey {
    match format {
        TagType::Mp4Ilst => ItemKey::Unknown("----:spotdl:NOLYRICS".into()),
        _ => ItemKey::Unknown("SPOTDL_NOLYRICS".into()),
    }
}

/// Writes only the lyrics into the tags, or without them (none found, or an instrumental's removed), the key that says
/// that none were found.
pub(crate) fn write_lyrics(file: &Path, lyrics: Option<String>) -> R<()> {
    let mut audio = audio(file)?;
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
    save(&audio, file)
}

/// The Spotify URL of a tagged file, also from the `WWWAUDIOFILE` keys that earlier spotdl-rs versions wrote.
pub(crate) fn source_url(tag: &Tag) -> Option<String> {
    let names = ["WOAS", "----:spotdl:WOAS", "WWWAUDIOFILE", "----:com.apple.iTunes:WWWAUDIOFILE"];
    let known = |key: &ItemKey| match key {
        ItemKey::AudioSourceUrl | ItemKey::AudioFileUrl => true,
        ItemKey::Unknown(key) => names.iter().any(|name| name.eq_ignore_ascii_case(key)),
        _ => false,
    };
    let urls = tag.items().filter(|item| known(item.key()));
    urls.filter_map(|item| item.value().text().or(item.value().locator())).find(|url| !url.is_empty()).map(String::from)
}

/// The key of the explicit flag: lofty writes `ITUNESADVISORY` for ID3 and `rtng` for MP4, but drops it from Vorbis
/// comments (Opus, FLAC, Ogg), where music servers read `ITUNESADVISORY` too.
pub(crate) fn advisory_key(format: TagType) -> ItemKey {
    match format {
        TagType::VorbisComments => ItemKey::Unknown("ITUNESADVISORY".into()),
        _ => ItemKey::ParentalAdvisory,
    }
}

/// The key of the release type (album, single, EP, compilation): lofty drops a plain `RELEASETYPE` from MP4 files,
/// which keep only atom names, so there it is Picard's freeform atom, which TagLib (and so Navidrome) reads as
/// `RELEASETYPE`.
fn release_type_key(format: TagType) -> ItemKey {
    match format {
        TagType::Mp4Ilst => ItemKey::Unknown("----:com.apple.iTunes:MusicBrainz Album Type".into()),
        _ => ItemKey::Unknown("RELEASETYPE".into()),
    }
}

/// The compilation flag marks a Various Artists release, as Picard and iTunes set it, and is the same for every song of
/// the album. Spotify calls an artist's own anthologies and best-ofs (the 2018 White Album, 2Pac's Greatest Hits)
/// compilations too: flagged, they are filed under Various Artists by players. `RELEASETYPE` keeps Spotify's word.
pub(crate) fn compilation(s: &Song) -> bool {
    s.album_type == "compilation" && s.album_artist.eq_ignore_ascii_case("Various Artists")
}

pub(crate) fn write_tags(file: &Path, s: &Song) -> R<()> {
    let mut audio = audio(file)?;
    let format = audio.primary_tag_type();
    let id3 = format == TagType::Id3v2;
    // spotdl's keys go into the file's own tag, so that the keys of other tools (MusicBrainz IDs, composers, sort
    // names, album gains) stay.
    let mut tag = audio.primary_tag().cloned().unwrap_or_else(|| Tag::new(format));
    let (gain, peak) = track_gain(file, s);
    // A measured gain (one with a peak) stays unless it is measured again: Deezer's gain is only an estimate.
    let measured = tag.get_string(&ItemKey::ReplayGainTrackPeak).is_some_and(|peak| !peak.is_empty());
    let gain = if peak.is_empty() && measured { String::new() } else { gain };
    let has_lyrics = s.lyrics.as_deref().is_some_and(|lyrics| !lyrics.is_empty());
    tag.set_title(s.name.clone());
    tag.set_artist(s.artists.join(settings().id3_separator.as_str()));
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
        (release_type_key(format), single(s.album_type.clone())),
        (url_key(format), single(s.url.clone())),
    ];
    // Other tools may write unknown Vorbis keys in lower case.
    let same_key = |a: &ItemKey, b: &ItemKey| match (a, b) {
        (ItemKey::Unknown(a), ItemKey::Unknown(b)) => a.eq_ignore_ascii_case(b),
        _ => a == b,
    };
    for (key, values) in items {
        let values: Vec<String> = values.into_iter().filter(|value| !value.is_empty()).collect();
        // A value spotdl does not have leaves the file's own, but the compilation flag is spotdl's to clear, and lyrics
        // clear the key that says none were found.
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
    let cover = s.cover_url.as_deref().filter(|_| !settings().skip_album_art).and_then(art);
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
    save(&audio, file)
}

#[cfg(test)]
#[path = "fuzz/tags.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;

    /// The release type was written as a plain `RELEASETYPE` key, which lofty silently drops from M4A files.
    #[test]
    fn release_type_is_written_to_every_format() {
        crate::settings::use_defaults();
        let dir = std::env::temp_dir().join(format!("spotdl-release-type-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = Song { name: "Song".into(), album_type: "single".into(), ..Song::default() };
        for (extension, codec) in [("m4a", "aac"), ("opus", "libopus")] {
            let file = dir.join(format!("t.{extension}"));
            let made = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=mono", "-t", "1", "-c:a", codec])
                .arg(&file)
                .status()
                .unwrap();
            assert!(made.success());
            write_tags(&file, &song).unwrap();
            let audio = lofty::read_from_path(&file).unwrap();
            let tag = lofty::file::TaggedFileExt::primary_tag(&audio).unwrap();
            assert_eq!(tag.get_string(&release_type_key(tag.tag_type())), Some("single"), "{extension}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
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
}
