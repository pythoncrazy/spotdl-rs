//! LRCLIB, a crowd's uploads of lyrics, and the synced lyrics among them.

use crate::lyrics::{is_synced, lyrics_title, same_album, same_lyrics, timed_for, Answers, Lyrics};
use crate::{http::Service, lock, url_encode, Song};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};

pub(crate) static LRCLIB: Service = Service::new("LRCLIB", 750, false);

/// One upload of a song's lyrics.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Upload {
    duration: Option<f64>,
    instrumental: Option<bool>,
    album_name: Option<String>,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

/// The uploads of a search reply; one that is not as LRCLIB writes them is left out.
pub(crate) fn uploads(reply: Vec<Value>) -> Vec<Upload> {
    reply.into_iter().filter_map(|upload| serde_json::from_value(upload).ok()).collect()
}

/// LRCLIB's search results for a URL. LRCLIB refuses requests when it is busy; [`Service::get`] waits and asks again,
/// so that a busy moment does not replace synced lyrics with plain ones.
fn lrclib_search(url: &str) -> Option<Vec<Upload>> {
    Some(uploads(serde_json::from_slice(&LRCLIB.get(url, &[])?).ok()?))
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

/// LRCLIB's lyrics for versions as long as the song, one [`Versions`] for each of the song's artists that LRCLIB has
/// any for, found one artist at a time as the caller asks for more. LRCLIB files a song under one of its artists
/// ("KING" of ¥$ is under Kanye West), and the first artist's versions can be wrong: under Jack Harlow, the only
/// version as long as his WHATS POPPIN remix has the words of the original, and the 20 right ones are under Tory Lanez.
pub(crate) fn lrclib<'a>(song: &'a Song, answers: &'a Answers) -> impl Iterator<Item = Versions> + 'a {
    lrclib_with(song, answers, lrclib_search)
}

fn lrclib_with<'a>(
    song: &'a Song,
    answers: &'a Answers,
    search: fn(&str) -> Option<Vec<Upload>>,
) -> impl Iterator<Item = Versions> + 'a {
    let artists = std::iter::once(&song.artist).chain(song.artists.iter().filter(|artist| **artist != song.artist));
    artists
        .filter(|artist| !artist.is_empty())
        .map(move |artist| lrclib_by(song, artist, answers, search))
        .filter(|found| !found.is_empty())
}

fn lrclib_by(song: &Song, artist: &str, answers: &Answers, search: fn(&str) -> Option<Vec<Upload>>) -> Versions {
    let url = format!(
        "https://lrclib.net/api/search?artist_name={}&track_name={}",
        url_encode(artist),
        url_encode(lyrics_title(song))
    );
    asked_once(&url, answers, search).map(|uploads| lrclib_lyrics(song, &uploads)).unwrap_or_default()
}

/// `search`'s results for a URL. The lyrics provider and the `.lrc` writer search for the same song; LRCLIB is often
/// busy, so it is asked once, and the results stay in the song's `answers`: kept for the run, they filled gigabytes
/// over a library. A failed search is not kept.
fn asked_once(url: &str, answers: &Answers, search: fn(&str) -> Option<Vec<Upload>>) -> Option<Arc<Vec<Upload>>> {
    let known = lock(&answers.lrclib).get(url).cloned();
    known.or_else(|| {
        let uploads = Arc::new(search(url)?);
        lock(&answers.lrclib).insert(url.into(), uploads.clone());
        Some(uploads)
    })
}

/// [`lrclib_by`]'s versions of LRCLIB's search results. When every version as long as the song is marked instrumental,
/// LRCLIB answers that the song has no words, so that the lyrics providers stop and do not take another song's words
/// from a site that goes by the title alone.
pub(crate) fn lrclib_lyrics(song: &Song, uploads: &[Upload]) -> Versions {
    let near: Vec<&Upload> =
        uploads.iter().filter(|upload| (upload.duration.unwrap_or_default() - song.duration).abs() < 4.0).collect();
    if !near.is_empty() && near.iter().all(|upload| upload.instrumental == Some(true)) {
        return Versions { instrumental: true, lyrics: vec![] };
    }
    let text = |lyrics: &Option<String>| lyrics.clone().filter(|lyrics| !lyrics.trim().is_empty());
    let mut found: Vec<(bool, bool, String)> = near
        .into_iter()
        .filter_map(|upload| {
            let same_album = same_album(&song.album_name, upload.album_name.as_deref().unwrap_or_default());
            // Synced lyrics timed to a longer recording keep their words, as plain lyrics.
            let synced = text(&upload.synced_lyrics).filter(|lyrics| timed_for(lyrics, song.duration));
            Some((!same_album, synced.is_none(), synced.or_else(|| text(&upload.plain_lyrics))?))
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
    use crate::lyrics::plain_lyrics;

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
        let tracks = vec![
            track("Tonight: Franz Ferdinand", "syncedLyrics", "[00:12.00] Lucid dreams"),
            track("Hits To The Head", "plainLyrics", "Lucid dreams"),
        ];
        let found = lrclib_lyrics(&song, &uploads(tracks));
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
        assert_eq!(lrclib_lyrics(&song, &uploads(vec![plain.clone()])).pick(None), None);
        let synced = serde_json::json!({"duration": 211.0, "syncedLyrics": "[00:12.00] Screw"});
        let found = lrclib_lyrics(&song, &uploads(vec![plain, synced]));
        assert_eq!(found.pick(None), Some(Lyrics::Words("[00:12.00] Screw".into())));
        let instrumental = Versions { instrumental: true, lyrics: vec![] };
        assert_eq!(instrumental.pick(None), Some(Lyrics::Instrumental));
        // Genius's lyrics still check plain ones, and an instrumental's word does not stand against them.
        let agreeing = Versions { instrumental: false, lyrics: vec![BULLY_KING.into()] };
        assert_eq!(agreeing.pick(Some(BULLY_KING)), Some(Lyrics::Words(BULLY_KING.into())));
        assert_eq!(Versions { instrumental: true, lyrics: vec![] }.pick(Some(BULLY_KING)), None);
    }

    /// LRCLIB's first artist's only version as long as the song was another song's words, and the search stopped
    /// there: Jack Harlow's WHATS POPPIN remix got no `.lrc` file, as the 20 right versions are under Tory Lanez.
    #[test]
    fn a_later_artists_versions_are_asked_when_the_first_artists_are_not_the_songs() {
        let song = Song {
            name: "WHATS POPPIN - Remix".into(),
            artist: "Jack Harlow".into(),
            artists: vec!["Jack Harlow".into(), "Tory Lanez".into()],
            duration: 227.5,
            ..Song::default()
        };
        // Thirty lines of words that only one of the two songs has: fewer say nothing (see `same_lyrics`).
        fn lyrics(word: &str, end: u32) -> String {
            let lines = (0..30).map(|i| format!("[00:{i:02}.00] {word}{i} {word}{}", i + 1));
            lines.chain([format!("[{end}:00.00] {word}end")]).collect::<Vec<_>>().join("\n")
        }
        fn search(url: &str) -> Option<Vec<Upload>> {
            let (words, album) = match url.contains("artist_name=Jack%20Harlow") {
                true => (lyrics("original", 2), "Sweet Action"),
                false => (lyrics("remix", 3), "WHATS POPPIN"),
            };
            Some(uploads(vec![serde_json::json!({"duration": 227.5, "syncedLyrics": words, "albumName": album})]))
        }
        let plain = plain_lyrics(&lyrics("remix", 3));
        let found = lrclib_with(&song, &Answers::default(), search).find_map(|versions| versions.pick(Some(&plain)));
        assert_eq!(found, Some(Lyrics::Words(lyrics("remix", 3))));
    }

    /// The lyrics provider and the `.lrc` writer of a song ask LRCLIB once between them; the next song asks again,
    /// and a failed search is not an answer. Every search's results, kept until the run ended, filled gigabytes.
    #[test]
    fn lrclib_is_asked_once_for_a_song() {
        let reply = |_: &str| Some(uploads(vec![serde_json::json!({"duration": 200.0, "plainLyrics": "Words"})]));
        let none = |_: &str| None;
        let answers = Answers::default();
        assert!(asked_once("test:url", &answers, none).is_none());
        assert_eq!(asked_once("test:url", &answers, reply).unwrap().len(), 1);
        assert_eq!(asked_once("test:url", &answers, none).unwrap().len(), 1);
        assert!(asked_once("test:url", &Answers::default(), none).is_none());
    }

    /// LRCLIB's word that a song is an instrumental is an answer, but only when every version as long as the song says
    /// so: all 20 results for Pink Floyd's "Interstellar Overdrive" are instrumental.
    #[test]
    fn lrclib_instrumentals_are_an_answer() {
        let song = Song { name: "Interstellar Overdrive".into(), duration: 581.0, ..Song::default() };
        let track = |d: f64, i: bool, p: &str| serde_json::json!({"duration": d, "instrumental": i, "plainLyrics": p});
        let instrumental = vec![track(582.0, true, ""), track(580.0, true, ""), track(300.0, false, "Marigolds")];
        assert_eq!(lrclib_lyrics(&song, &uploads(instrumental)), Versions { instrumental: true, lyrics: vec![] });
        let mixed = vec![track(582.0, true, ""), track(580.0, false, "Some words")];
        assert_eq!(lrclib_lyrics(&song, &uploads(mixed)).lyrics, vec!["Some words".to_string()]);
        assert!(lrclib_lyrics(&song, &uploads(vec![track(300.0, true, "")])).is_empty());
    }
}
