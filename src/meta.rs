//! `meta`: tags the audio files that are already there, and downloads again the songs that are asked for.

use crate::download::{enrich, out_path, process};
use crate::files::{audio_files, folder_images, write_folder_art};
use crate::lyrics::{
    clean_lrc, find_lyrics, found_lyrics, genius_answered, is_synced, plain_lyrics, recheck, report_unchecked,
    timed_for, unlisted, write_lrc, Check, Lyrics,
};
use crate::replaygain::{album_gains, write_track_gain};
use crate::settings::settings;
use crate::tags::{advisory_key, audio, no_lyrics_key, source_url, write_lyrics, write_tags};
use crate::{log, search, spotify, Song, R};
use lofty::{prelude::*, tag::ItemKey};
use rayon::prelude::*;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// The files `meta --redownload` downloads again, by song URL.
pub(crate) static REDOWNLOADS: OnceLock<HashMap<String, PathBuf>> = OnceLock::new();

/// What the settings ask `meta` to do about files that already have their tags.
#[derive(Clone, Copy)]
struct Mode {
    /// Tags are written again and lyrics looked for again (`--force-update-metadata`).
    force: bool,
    /// The songs are downloaded again.
    redownload: bool,
    /// `--overwrite metadata` or `force`: the lyrics in the files are checked again.
    replace: bool,
}

impl Mode {
    fn current() -> Self {
        let replace = matches!(settings().overwrite.as_str(), "force" | "metadata");
        Self { force: settings().force_update_metadata, redownload: settings().redownload, replace }
    }
}

/// A file's tags: the song they describe, and what they lack.
struct Tagged {
    /// The song as the file's own tags describe it.
    local: Song,
    /// The lyrics as the tags store them.
    stored: Option<String>,
    /// The stored lyrics without Genius's song lists and, for synced lyrics timed to a longer recording, without the
    /// times: their words stay.
    lyrics: Option<String>,
    /// The URL the audio was downloaded from (the comment); tags written again keep it.
    source: Option<String>,
    /// The song's Spotify URL.
    spotify_url: Option<String>,
    /// The date of the recording as the tags have it.
    date: Option<String>,
    /// Title, artists, cover and (for a Spotify song) the explicit flag are there: a Spotify song written before the
    /// explicit flag reached Vorbis comments has its tags written again.
    tagged: bool,
    /// The lyrics were looked for and not found (an instrumental), and are not looked for again on each run.
    no_lyrics: bool,
    /// Tagged, and with lyrics or the note that there are none.
    complete: bool,
    /// ReplayGain is asked for and the file has no measured peak (Deezer's gain has none).
    measure: bool,
    /// The folder's cover image is asked for and missing.
    folder_art_missing: bool,
}

fn read_tags(file: &Path) -> R<Tagged> {
    let audio = audio(file)?;
    let tag = audio.primary_tag().ok_or("no tags")?;
    let has = |key: ItemKey| tag.get_string(&key).is_some_and(|value| !value.is_empty());
    let spotify_url = source_url(tag).filter(|url| url.contains("spotify"));
    let tagged = has(ItemKey::TrackTitle)
        && has(ItemKey::TrackArtist)
        && has(ItemKey::TrackArtists)
        && (settings().skip_album_art || !tag.pictures().is_empty())
        && (spotify_url.is_none() || has(advisory_key(tag.tag_type())));
    let no_lyrics = has(no_lyrics_key(tag.tag_type()));
    let complete = tagged
        && (settings().lyrics_providers.iter().map(String::as_str).next().is_none()
            || has(ItemKey::Lyrics)
            || no_lyrics);
    let album_artist = tag.get_string(&ItemKey::AlbumArtist).unwrap_or_default().to_string();
    let wanted = Song { album_artist, artist_id: "-".into(), cover_url: Some(String::new()), ..Song::default() };
    let stored = tag.get_string(&ItemKey::Lyrics).filter(|lyrics| !lyrics.is_empty()).map(String::from);
    let length = audio.properties().duration().as_secs_f64();
    let untimed = |lyrics: String| match timed_for(&lyrics, length) {
        true => lyrics,
        false => plain_lyrics(&lyrics).trim().to_string(),
    };
    let lyrics = stored.as_deref().map(unlisted).map(untimed).filter(|lyrics| !lyrics.is_empty());
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
    Ok(Tagged {
        local,
        stored,
        lyrics,
        source: tag.comment().filter(|comment| comment.starts_with("http")).map(String::from),
        spotify_url,
        date: tag.get_string(&ItemKey::RecordingDate).map(String::from),
        tagged,
        no_lyrics,
        complete,
        measure: settings().replaygain && !has(ItemKey::ReplayGainTrackPeak),
        folder_art_missing: settings().folder_art && !folder_images(file, &wanted).is_empty(),
    })
}

/// What a file that has all its tags still needs: its ReplayGain measured, its lyrics cleaned, an `.lrc` file written.
/// Whether that is all, and `meta` is done with the file.
fn tidy(file: &Path, state: &Tagged) -> R<bool> {
    if state.measure {
        write_track_gain(file)?;
    }
    // Lyrics with Genius's song list or ticket ad in them are cleaned, also when nothing else is written.
    if state.stored.is_some() && state.lyrics != state.stored {
        write_lyrics(file, state.lyrics.clone())?;
    }
    if !settings().generate_lrc || file.with_extension("lrc").exists() || (state.lyrics.is_none() && state.no_lyrics) {
        return Ok(true);
    }
    if state.lyrics.as_deref().is_some_and(is_synced) {
        write_lrc(file, &state.local)?;
        return Ok(true);
    }
    Ok(false)
}

/// The Spotify song of a file without a Spotify URL. The match must be the same song on the same release, so that a
/// song that is not on Spotify (an unreleased one) does not take the tags of a released song with the same title,
/// and an album's song does not take those of a compilation or another edition. The track search finds a song's most
/// popular release (Three Little Birds of Exodus is on Legend), so the album is searched too.
fn find_on_spotify(local: &Song) -> Option<Song> {
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

/// Sets the lyrics of `song` and says whether they were removed because the song is an instrumental: its old lyrics
/// are another song's ("See-Saw"'s under "Interstellar Overdrive"). `force` fetches the lyrics again, for example to
/// get synced lyrics that were not available before; so does `--overwrite metadata` for a song whose lyrics were not
/// found before. With `--overwrite metadata`, lyrics in the tags that are another song's (LRCLIB's "KING" of VULTURES
/// 1 under BULLY's) are replaced by the song's Genius lyrics.
fn look_up_lyrics(file: &Path, song: &mut Song, state: &Tagged, mode: Mode) -> bool {
    let mut cleared = false;
    song.lyrics = if mode.force {
        match found_lyrics(song) {
            Some(Lyrics::Instrumental) => {
                cleared = state.lyrics.is_some();
                None
            }
            found => found.and_then(Lyrics::into_words).or(state.lyrics.clone()),
        }
    } else {
        let look = !state.no_lyrics || mode.replace;
        state.lyrics.clone().or_else(|| look.then(|| find_lyrics(song)).flatten())
    };
    let genius =
        settings().lyrics_providers.iter().map(String::as_str).any(|name| name == "genius" || name == "synced");
    if mode.replace && !mode.force && genius {
        match recheck(song) {
            Check::Keep => {}
            Check::Replace(Some(words)) => {
                log("WARNING", &format!("{}: lyrics replaced, they were another song's", file.display()));
                song.lyrics = Some(words);
            }
            Check::Replace(None) => {
                song.lyrics = None;
                cleared = true;
            }
        }
    }
    if cleared {
        log("WARNING", &format!("{}: lyrics removed, the song is an instrumental", file.display()));
    }
    cleared
}

/// Writes what `meta` found: for a Spotify song, the tags, the lyrics, the folder art and the `.lrc` file; for another,
/// only its lyrics, since it keeps its own tags.
fn write(file: &Path, song: &Song, state: &Tagged, mode: Mode, cleared: bool) -> R<()> {
    let on_spotify = !song.song_id.is_empty();
    if on_spotify {
        write_folder_art(file, song)?;
    }
    // A song whose tags are not written again (tagged, or not on Spotify) is measured on its own.
    if state.measure && (!on_spotify || (state.tagged && !mode.force)) {
        write_track_gain(file)?;
    }
    if on_spotify {
        // Missing lyrics alone do not write the tags again: only the lyrics, or the key that none were found. Tags
        // written again keep the file's lyrics when the song has none, so an instrumental's are removed after them.
        // `--overwrite metadata` writes the tags again from Spotify and Deezer (fixed track totals, dates and
        // flags reach files tagged before), keeping the file's lyrics, source URL and measured gain.
        if !state.tagged || mode.force || mode.replace {
            write_tags(file, song)?;
        }
        let changed = song.lyrics != state.stored || (!state.complete && genius_answered(song));
        if cleared || (state.tagged && !mode.force && changed) {
            write_lyrics(file, song.lyrics.clone())?;
        }
        write_lrc(file, song)
    } else {
        let new = song.lyrics.clone().filter(|_| song.lyrics != state.stored);
        let none_found = song.lyrics.is_none() && (cleared || !state.no_lyrics && genius_answered(song));
        write_lrc(file, song)?;
        if new.is_some() || none_found {
            write_lyrics(file, new)?;
        }
        Ok(())
    }
}

/// Tags one file. The song to download again, when `--redownload` asks for it.
fn meta_file(file: &Path) -> R<Option<Song>> {
    let state = read_tags(file)?;
    let mode = Mode::current();
    clean_lrc(file)?;
    let tidy_only = state.complete && !mode.force && !mode.redownload && !mode.replace && !state.folder_art_missing;
    if tidy_only && tidy(file, &state)? {
        return Ok(None);
    }
    let found = match &state.spotify_url {
        Some(url) => spotify::resolve(url)?.into_iter().next(),
        None => find_on_spotify(&state.local),
    };
    let Some(mut song) = found.or_else(|| state.spotify_url.is_none().then(|| state.local.clone())) else {
        return Err("no match".into());
    };
    let on_spotify = !song.song_id.is_empty();
    song.download_url = song.download_url.or(state.source.clone());
    if on_spotify && !enrich(&mut song) {
        // Without Deezer the date is Spotify's, which can be only a year or month: the file's Deezer day stays.
        let date = state.date.as_deref();
        if let Some(date) = date.filter(|date| date.len() > song.date.len() && date.starts_with(&song.date)) {
            song.date = date.into();
        }
    }
    let cleared = look_up_lyrics(file, &mut song, &state, mode);
    write(file, &song, &state, mode, cleared)?;
    Ok((mode.redownload && on_spotify).then_some(song))
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

pub(crate) fn meta(paths: &[String]) -> R<()> {
    let mut files = Vec::new();
    for path in paths.iter().map(Path::new) {
        match path.is_file() {
            true => files.push(path.to_path_buf()),
            false => audio_files(path, &mut files),
        }
    }
    // `threads` files at a time, as downloads: most of the time goes to waiting for lyrics sites.
    let pool = rayon::ThreadPoolBuilder::new().num_threads(settings().threads.max(1) as usize).build()?;
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
    if settings().fetch_albums {
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
    use crate::files::path;

    /// `meta --redownload --fetch-albums` downloaded an album once for each of its files, from several threads into
    /// the same file, and saved songs at the output template's path, leaving the local `.mp3` beside the new `.opus`.
    #[test]
    fn redownloads_replace_local_files_and_download_each_song_once() {
        crate::settings::use_defaults();
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
}
