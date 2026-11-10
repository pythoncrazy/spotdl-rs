//! LRCLIB, a crowd's uploads of lyrics, and the synced lyrics among them.

use crate::lyrics::{is_synced, lyrics_title, plain_title, same_lyrics, timed_for, Lyrics};
use crate::{http::Service, url_encode, Song};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{Mutex, PoisonError},
};

pub(crate) static LRCLIB: Service = Service::new("LRCLIB", 750, false);

/// LRCLIB's search results for a URL. LRCLIB refuses requests when it is busy; [`Service::get`] waits and asks again,
/// so that a busy moment does not replace synced lyrics with plain ones.
fn lrclib_search(url: &str) -> Option<Vec<Value>> {
    serde_json::from_slice(&LRCLIB.get(url, &[])?).ok()
}

/// What LRCLIB has for a song, among the versions as long as it.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Versions {
    /// Every version says that the song has no words.
    pub(crate) instrumental: bool,
    /// The lyrics of the versions: those of the song's album first, then synced ones.
    pub(crate) lyrics: Vec<String>,
}

impl Versions {
    fn is_empty(&self) -> bool {
        !self.instrumental && self.lyrics.is_empty()
    }

    /// The lyrics that the `synced` provider takes, synced ones first: [`lrclib`] puts the song's album first, and that
    /// album's version often has plain lyrics only ("Lucid Dreams" on Hits To The Head) while others as long as the
    /// song are synced. They must agree with the Genius lyrics `checks`. Without those, only synced lyrics (timed to a
    /// recording as long as the song) and LRCLIB's word that the song is an instrumental count: plain lyrics are
    /// anyone's text filed under any title, and all 7 LRCLIB versions of Lee Fields's "Funky Screw", which has no
    /// Genius page, have the plain words of a Japanese comic song.
    pub(crate) fn pick(self, checks: Option<&str>) -> Option<Lyrics> {
        if self.instrumental {
            return checks.is_none().then_some(Lyrics::Instrumental);
        }
        let agrees = |lyrics: &&String| checks.map_or_else(|| is_synced(lyrics), |by| same_lyrics(lyrics, by));
        let best = self.lyrics.iter().find(|lyrics| is_synced(lyrics) && agrees(lyrics));
        let best = best.or_else(|| self.lyrics.iter().find(agrees))?;
        Some(Lyrics::Words(best.trim().to_string()))
    }
}

/// LRCLIB's lyrics for versions as long as the song. `keep`: the `.lrc` writer will search again (see
/// [`lrclib_tracks`]).
pub(crate) fn lrclib(song: &Song, keep: bool) -> Versions {
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

fn lrclib_by(song: &Song, artist: &str, keep: bool) -> Versions {
    let url = format!(
        "https://lrclib.net/api/search?artist_name={}&track_name={}",
        url_encode(artist),
        url_encode(lyrics_title(song))
    );
    let Some(tracks) = lrclib_tracks(&url, keep, lrclib_search) else {
        return Versions::default();
    };
    lrclib_lyrics(song, &tracks)
}

/// LRCLIB's search results kept for the `.lrc` writer, by URL, oldest first.
type Kept = std::collections::VecDeque<(String, Vec<Value>)>;

static LRCLIB_FOUND: std::sync::LazyLock<Mutex<Kept>> = std::sync::LazyLock::new(Default::default);

/// A search reply can be over 100 KB; this is room for the songs in progress, not for a library.
const LRCLIB_KEPT: usize = 256;

/// `search`'s results for a URL. The lyrics provider and the `.lrc` writer search for the same song; LRCLIB is often
/// busy, so it is asked once: the provider keeps its results (`keep`) and the writer takes them. The writer keeps
/// nothing, since nothing searches after it, and results the writer does not take (no `.lrc` to write) are dropped when
/// newer ones come: once kept by everyone, they filled gigabytes over a library.
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

/// [`lrclib_by`]'s versions of LRCLIB's search results. When every version as long as the song is marked instrumental,
/// LRCLIB answers that the song has no words, so that the lyrics providers stop and do not take another song's words
/// from a site that goes by the title alone.
pub(crate) fn lrclib_lyrics(song: &Song, tracks: &[Value]) -> Versions {
    let near: Vec<&Value> = tracks
        .iter()
        .filter(|track| (track["duration"].as_f64().unwrap_or_default() - song.duration).abs() < 4.0)
        .collect();
    if !near.is_empty() && near.iter().all(|track| track["instrumental"].as_bool() == Some(true)) {
        return Versions { instrumental: true, lyrics: vec![] };
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
            // Synced lyrics timed to a longer recording keep their words, as plain lyrics.
            let synced = text(track, "syncedLyrics").filter(|lyrics| timed_for(lyrics, song.duration));
            Some((!same_album, synced.is_none(), synced.or_else(|| text(track, "plainLyrics"))?))
        })
        .collect();
    found.sort_by_key(|(other_album, plain, _)| (*other_album, *plain));
    let mut seen = HashSet::new();
    let lyrics = found.into_iter().map(|(_, _, lyrics)| lyrics).filter(|lyrics| seen.insert(lyrics.clone()));
    Versions { instrumental: false, lyrics: lyrics.collect() }
}

#[cfg(test)]
#[path = "fuzz/lrclib.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;

    const BULLY_KING: &str =
        "[Intro]\nThe time is now, right now\nThis is the hour, this is the new dawn, this is the new day\n\
        Now is the time, for nature and all her glory have named you her king\nShe has named you the king\n\
        This that feelin' we need more of\nThe hatin' just brought me more love";

    /// The `synced` provider takes LRCLIB's synced lyrics before the song's album's plain ones, which [`lrclib`] lists
    /// first: Franz Ferdinand's "Lucid Dreams" on Hits To The Head got plain lyrics in its tags.
    #[test]
    fn synced_provider_takes_synced_lyrics_first() {
        use serde_json::json;
        let song = Song {
            name: "Lucid Dreams".into(),
            album_name: "Hits To The Head".into(),
            duration: 224.0,
            ..Song::default()
        };
        let track = |album: &str, key: &str, text: &str| json!({"albumName": album, "duration": 224.0, key: text});
        let tracks = [
            track("Tonight: Franz Ferdinand", "syncedLyrics", "[00:12.00] Lucid dreams"),
            track("Hits To The Head", "plainLyrics", "Lucid dreams"),
        ];
        let found = lrclib_lyrics(&song, &tracks);
        assert_eq!(found.lyrics[0], "Lucid dreams");
        let words = |text: &str| Some(Lyrics::Words(text.into()));
        // Genius's lyrics are those of the plain version: the synced one still agrees by its words.
        assert_eq!(found.pick(Some("Lucid dreams")), words("[00:12.00] Lucid dreams"));
    }

    /// Without Genius lyrics to check them by, LRCLIB's plain lyrics are not taken (Lee Fields's "Funky Screw" got a
    /// Japanese comic song's), but its synced lyrics and its word that the song is an instrumental are.
    #[test]
    fn unchecked_lrclib_lyrics_must_be_synced() {
        let song = Song { name: "Funky Screw".into(), duration: 211.0, ..Song::default() };
        let plain = serde_json::json!({"duration": 210.6, "syncedLyrics": "", "plainLyrics": "半チャーハン 一升半"});
        assert_eq!(lrclib_lyrics(&song, std::slice::from_ref(&plain)).pick(None), None);
        let synced = serde_json::json!({"duration": 211.0, "syncedLyrics": "[00:12.00] Screw"});
        let found = lrclib_lyrics(&song, &[plain, synced]);
        assert_eq!(found.pick(None), Some(Lyrics::Words("[00:12.00] Screw".into())));
        let instrumental = Versions { instrumental: true, lyrics: vec![] };
        assert_eq!(instrumental.pick(None), Some(Lyrics::Instrumental));
        // Genius's lyrics still check plain ones, and an instrumental's word does not stand against them.
        let agreeing = Versions { instrumental: false, lyrics: vec![BULLY_KING.into()] };
        assert_eq!(agreeing.pick(Some(BULLY_KING)), Some(Lyrics::Words(BULLY_KING.into())));
        assert_eq!(Versions { instrumental: true, lyrics: vec![] }.pick(Some(BULLY_KING)), None);
    }

    /// LRCLIB's results are kept only for the `.lrc` writer's search after the provider's, and only for a while: every
    /// search's results, kept until the run ended, filled gigabytes over a library.
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

    /// LRCLIB's word that a song is an instrumental is an answer, but only when every version as long as the song says
    /// so: all 20 results for Pink Floyd's "Interstellar Overdrive" are instrumental.
    #[test]
    fn lrclib_instrumentals_are_an_answer() {
        let song = Song { name: "Interstellar Overdrive".into(), duration: 581.0, ..Song::default() };
        let track = |d: f64, i: bool, p: &str| serde_json::json!({"duration": d, "instrumental": i, "plainLyrics": p});
        let instrumental = [track(582.0, true, ""), track(580.0, true, ""), track(300.0, false, "Marigolds")];
        assert_eq!(lrclib_lyrics(&song, &instrumental), Versions { instrumental: true, lyrics: vec![] });
        let mixed = [track(582.0, true, ""), track(580.0, false, "Some words")];
        assert_eq!(lrclib_lyrics(&song, &mixed).lyrics, vec!["Some words".to_string()]);
        assert!(lrclib_lyrics(&song, &[track(300.0, true, "")]).is_empty());
    }
}
