//! Lyrics: the providers asked in turn, which words are a song's own, and the `.lrc` file next to it.

use crate::genius::{genius_key, genius_same, genius_titled, instrumental, released_on, Page, GENIUS_FOUND};
use crate::html::html;
use crate::lrclib::{lrclib, lrclib_agrees, synced_first};
use crate::tags::audio;
use crate::{http::Service, list, log, on, search, text, Song, R};
use lofty::prelude::*;
use std::{
    collections::HashSet,
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Mutex, PoisonError,
    },
};

/// azlyrics and musixmatch are asked by every thread of a run at once otherwise: such bursts get a client blocked.
static AZLYRICS: Service = Service::new("azlyrics", 1500, false);

static MUSIXMATCH: Service = Service::new("musixmatch", 1500, false);

/// The page at `url` as text, asked of `service`.
pub(crate) fn page(service: &Service, url: &str) -> Option<String> {
    String::from_utf8(service.get(url, &[])?).ok()
}

/// The title that lyrics sites list: without " - Remastered", "(feat. ...)", "(Uncensored)" or other notes.
pub(crate) fn lyrics_title(song: &Song) -> &str {
    let title = search::versionless(&song.name);
    let end = [" (", " ["].iter().filter_map(|note| title.find(note)).min().unwrap_or(title.len());
    title[..end].trim()
}

/// A lowercase title of letters and digits, without notes in brackets.
pub(crate) fn plain_title(title: &str) -> String {
    let title = title.to_lowercase();
    let end = [" (", " [", " - "].iter().filter_map(|note| title.find(note)).min().unwrap_or(title.len());
    search::slug(&title[..end], "")
}

/// The songs whose lyrics were left unchecked because Genius could not be asked, for [`report_unchecked`].
static UNCHECKED: std::sync::LazyLock<Mutex<HashSet<String>>> = std::sync::LazyLock::new(Default::default);

pub(crate) fn unchecked(song: &Song) {
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
pub(crate) fn genius_answered(song: &Song) -> bool {
    if !list("lyrics_providers").any(|name| name == "genius" || name == "synced") {
        return false;
    }
    // Only the song's own page without words is "no lyrics": a search with no such page, or one that found
    // nothing, leaves a vocal song to be looked up again later.
    let pages =
        GENIUS_FOUND.lock().unwrap_or_else(PoisonError::into_inner).get(&genius_key(song, &song.artist, 1)).cloned();
    pages.is_some_and(|pages| {
        pages.iter().any(|page| instrumental(song, page))
            || (instrumental_title(&song.name) && pages.iter().all(|page| words(&page.lyrics).is_empty()))
    })
}

/// An azlyrics page came without the marker before its lyrics: its layout changed, which is said once.
static AZLYRICS_CHANGED: AtomicBool = AtomicBool::new(false);

/// The lyrics on an azlyrics page, or nothing (and a warning, once) for its access check or a page it no longer
/// lays out as expected.
pub(crate) fn azlyrics(page: &str) -> Option<String> {
    if page.contains("AZLyrics - request for access") {
        // A page (a redirect to b.azlyrics.com, with status 200) instead of lyrics: azlyrics takes this client for a
        // bot. Without this, every song found "no lyrics" there and nothing said why.
        AZLYRICS.block("asks for access (a bot check)");
        return None;
    }
    let lyrics = html(page, "<!-- Usage of azlyrics.com", "</div>");
    if lyrics.is_none() && !AZLYRICS_CHANGED.swap(true, Relaxed) {
        log("WARNING", "azlyrics: a page without its lyrics marker; its layout may have changed");
    }
    lyrics
}

/// Lyrics from one provider: `synced` (LRCLIB), `genius`, `azlyrics` or `musixmatch`.
pub(crate) fn provider(name: &str, song: &Song) -> Option<String> {
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
            // An instrumental without a page of its own has no words: LRCLIB files the sung song's under its title.
            let wordless = |page: &Page| words(&page.lyrics).is_empty();
            if page.as_ref().is_some_and(|page| instrumental(song, page))
                || (instrumental_title(&song.name) && page.as_ref().is_none_or(wordless))
            {
                return Some(String::new());
            }
            let checks = page.map(|page| page.lyrics).filter(|lyrics| !words(lyrics).is_empty());
            let agrees = |lyrics: &String| lrclib_agrees(lyrics, checks.as_ref());
            return synced_first(lrclib(song, true), agrees).map(|lyrics| lyrics.trim().to_string());
        }
        // A page without words is the song's answer that it has none, or else another song's.
        "genius" => {
            let Some(page) = genius_titled(song, artist, 1)?.into_iter().next() else {
                return instrumental_title(&song.name).then(String::new);
            };
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
            azlyrics(&page(&AZLYRICS, &url)?)?
        }
        _ => {
            let url =
                format!("https://www.musixmatch.com/lyrics/{}/{}", search::slug(artist, "-"), search::slug(title, "-"));
            let body = page(&MUSIXMATCH, &url)?.split_once("\"body\":")?.1.to_string();
            serde_json::Deserializer::from_str(&body).into_iter::<String>().next()?.ok()?
        }
    };
    Some(lyrics.trim().to_string()).filter(|lyrics| !lyrics.is_empty())
}

/// The lyrics of the first provider that has them. `Some(None)`: a provider found that the song has no words (an
/// instrumental), so the providers after it, which go by the title alone, are not asked: azlyrics would give
/// "Interstellar Overdrive" the words of another Pink Floyd song.
pub(crate) fn found_lyrics(song: &Song) -> Option<Option<String>> {
    let found = list("lyrics_providers").find_map(|name| provider(name, song))?;
    Some(Some(found).filter(|lyrics| !lyrics.is_empty()))
}

pub(crate) fn find_lyrics(song: &Song) -> Option<String> {
    found_lyrics(song).flatten()
}

/// Whether the lyrics have `[mm:ss` timestamps.
pub(crate) fn is_synced(lyrics: &str) -> bool {
    lyrics.lines().any(|line| {
        let minutes = line.bytes().skip(1).take_while(u8::is_ascii_digit).count();
        line.starts_with('[') && minutes > 0 && line.as_bytes().get(minutes + 1) == Some(&b':')
    })
}

/// The lyrics without `[mm:ss.xx]` timestamps.
pub(crate) fn plain_lyrics(lyrics: &str) -> String {
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
pub(crate) fn same_lyrics(a: &str, b: &str) -> bool {
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
pub(crate) fn words(lyrics: &str) -> Vec<String> {
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

/// Whether a note of the title says that the recording has no singing: "Big Poppa - Instrumental; 2014 Remaster",
/// "Girl (Take 2 - Instrumental Backing Track)", "Paperback Writer - Takes 1 & 2 / Backing Track". A song called
/// "Instrumental" is not one.
pub(crate) fn instrumental_title(title: &str) -> bool {
    let Some(end) = [" (", " [", " - "].iter().filter_map(|note| title.find(note)).min() else { return false };
    let notes = search::slug(&title[end..], "-");
    let words: Vec<&str> = notes.split('-').collect();
    words.iter().any(|word| matches!(*word, "instrumental" | "instrumentals" | "karaoke"))
        || words.windows(2).any(|pair| pair == ["backing", "track"])
}

/// Lyrics without Genius's ticket ad: "See Don Toliver Live", "Get tickets as low as $462", and the song list that
/// follows it ("You might also like", then titles and artists) up to the next section or blank line.
fn without_ads(lyrics: &str) -> String {
    let lines: Vec<&str> = lyrics.lines().collect();
    let (mut kept, mut i) = (Vec::new(), 0);
    while i < lines.len() {
        let ad = |line: &str| line.trim_start().starts_with("Get tickets as low as");
        if lines[i].trim_start().starts_with("See ") && lines.get(i + 1).is_some_and(|line| ad(line)) {
            i += 2;
            if lines.get(i).is_some_and(|line| line.trim() == "You might also like") {
                i += 1;
                while i < lines.len() && !lines[i].trim().is_empty() && !lines[i].trim_start().starts_with('[') {
                    i += 1;
                }
            }
            continue;
        }
        kept.push(lines[i]);
        i += 1;
    }
    kept.join("\n")
}

/// The words of a `.lrc` file without the "You might also like" marker that earlier Genius scrapes glued into lines
/// ("[02:39.30]You might also like[Chorus]").
pub(crate) fn clean_lrc(file: &Path) -> R<()> {
    const LIST: &str = "You might also like";
    let lrc = file.with_extension("lrc");
    let Ok(text) = fs::read_to_string(&lrc) else { return Ok(()) };
    if !text.contains(LIST) {
        return Ok(());
    }
    let lines =
        text.lines().map(|line| if line.contains(&format!("{LIST} ")) { line.into() } else { line.replace(LIST, "") });
    Ok(fs::write(&lrc, lines.collect::<Vec<_>>().join("\n") + "\n")?)
}

/// Lyrics without the "You might also like" song list that Genius puts between its lyrics blocks, which earlier
/// scrapes of its pages glued into the text ("I love youYou might also like[Chorus]"). One followed by a space is
/// a line of lyrics ("You might also like me").
pub(crate) fn unlisted(lyrics: &str) -> String {
    const LIST: &str = "You might also like";
    let lyrics = &without_ads(lyrics);
    let (mut out, mut rest) = (String::new(), lyrics.as_str());
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
pub(crate) fn reference(song: &Song, own: Option<&String>) -> Option<Option<String>> {
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
    // An instrumental without a page of its own has no words to check others by: none are its.
    asked.then(|| instrumental_title(&song.name).then(|| "[Instrumental]".to_string()))
}

/// The `.lrc` next to the file. Synced lyrics are timed to one recording: they are used when they were timed to a
/// recording as long as the file (no line after its end, see [`timed_for`]) and their words are the song's (see
/// [`reference`]): the song's own, the old `.lrc`, or LRCLIB's for the file's length. Without such lyrics, an old
/// `.lrc` with the words of another song, or timed to a longer recording, is removed. Without an answer from
/// Genius, nothing can be checked: no `.lrc` is written and none is removed.
pub(crate) fn write_lrc(file: &Path, song: &Song) -> R<()> {
    let sidecar = file.with_extension("lrc");
    let replace = matches!(text("overwrite"), Some("force" | "metadata"));
    if !on("generate_lrc") || (sidecar.exists() && !replace) {
        return Ok(());
    }
    let length = audio(file)?.properties().duration().as_secs_f64();
    let recording = Song { duration: length, ..song.clone() };
    let fits = (length - song.duration).abs() < 4.0;
    let own = song.lyrics.clone().filter(|lyrics| !lyrics.trim().is_empty());
    let Some(reference) = reference(song, own.as_ref()) else {
        unchecked(song);
        return Ok(());
    };
    let agrees = |lyrics: &String| reference.as_ref().is_none_or(|words| same_lyrics(lyrics, words));
    let timed = |lyrics: &String| is_synced(lyrics) && timed_for(lyrics, length);
    // The old `.lrc` goes before LRCLIB, which is often busy.
    let own_synced = own.filter(|lyrics| fits && timed(lyrics));
    let old = fs::read_to_string(&sidecar).ok();
    let found = std::iter::once_with(|| lrclib(&recording, false)).flatten().filter(timed);
    let kept = old.clone().filter(timed);
    if let Some(lrc) = own_synced.into_iter().chain(kept).chain(found).find(agrees) {
        if old.as_ref() != Some(&lrc) {
            fs::write(&sidecar, lrc)?;
        }
    } else if let Some(old) = old.filter(|_| replace) {
        let why = if !agrees(&old) { "its lyrics are another song's" } else { "it is timed to a longer recording" };
        if !agrees(&old) || !timed_for(&old, length) {
            log("WARNING", &format!("{}: removed, {why}", sidecar.display()));
            fs::remove_file(&sidecar)?;
        }
    }
    Ok(())
}

/// Whether synced lyrics fit a recording `length` seconds long: no line comes long after its end. LRCLIB files
/// lyrics timed to an album version or another concert under a shorter edit's length ("Lucid Dreams" on Hits To The
/// Head runs 4 minutes past the song). Lyrics a little off are still the song's timing: up to 15 seconds over is
/// another master's fade or a slow upload ("Walk Away - 7"" is 3 seconds over, "This Fire" 13). Plain lyrics fit
/// any recording.
pub(crate) fn timed_for(lyrics: &str, length: f64) -> bool {
    last_time(lyrics).is_none_or(|last| last <= length + SLACK)
}

/// Seconds that the last line of synced lyrics may come after the end of the recording.
const SLACK: f64 = 15.0;

/// The time of the last `[mm:ss.xx]` line, in seconds.
fn last_time(lyrics: &str) -> Option<f64> {
    let time = |line: &str| {
        let (stamp, _) = line.trim_start().strip_prefix('[')?.split_once(']')?;
        let (minutes, seconds) = stamp.split_once(':')?;
        // Digits only: `parse` also reads "inf", "nan" and "1e999", which no `.lrc` file has as a time.
        let number =
            |text: &str| text.bytes().all(|b| b.is_ascii_digit() || b == b'.').then(|| text.parse::<f64>().ok())?;
        Some(number(minutes)? * 60.0 + number(seconds)?)
    };
    lyrics.lines().filter_map(time).reduce(f64::max)
}

#[cfg(test)]
#[path = "fuzz/lyrics.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::genius::GENIUS;
    use crate::lrclib::lrclib_lyrics;

    const BULLY_KING: &str =
        "[Intro]\nThe time is now, right now\nThis is the hour, this is the new dawn, this is the new day\n\
        Now is the time, for nature and all her glory have named you her king\nShe has named you the king\n\
        This that feelin' we need more of\nThe hatin' just brought me more love";
    const VULTURES_KING: &str =
        "[00:05.86]After everything said, huh, man, man\n[00:12.00]After everything said, huh\n\
        [00:18.27]\"Crazy, bipolar, antisemite\"\n[00:23.04]And I'm still the king\n\
        [00:31.56]They thought headlines was my kryptonite\n[00:36.14]Still the king";

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

    #[test]
    fn titles_with_an_instrumental_note_are_instrumentals() {
        for title in [
            "Big Poppa - Instrumental; 2014 Remaster",
            "Girl (Take 2 - Instrumental Backing Track)",
            "Paperback Writer - Takes 1 & 2 / Backing Track",
            "No Complaints (Instrumental) (Bonus)",
            "Juicy - Remix Instrumental; 2014 Remaster",
        ] {
            assert!(instrumental_title(title), "{title}");
        }
        for title in [
            "Instrumental",
            "Big Poppa",
            "Track 1 - Live",
            "Backing Track Blues",
            "Robbery - Live from the Sydney Opera House",
        ] {
            assert!(!instrumental_title(title), "{title}");
        }
    }

    /// LRCLIB files lyrics timed to a longer version under a shorter edit's length: such synced lyrics give only
    /// their words. A last line up to 15 seconds after the end is the same recording, a little off.
    #[test]
    fn synced_lyrics_with_lines_after_the_end_are_plain() {
        assert!(timed_for("[00:10.00]a\n[03:20.00]b", 199.0));
        assert!(timed_for("[00:10.00]a\n[03:33.00]b", 199.0));
        assert!(!timed_for("[00:10.00]a\n[03:35.00]b", 199.0));
        assert!(!timed_for("[00:10.00]a\n[07:20.00]b\n[03:20.00]c", 199.0));
        assert!(timed_for("plain words\nno times", 1.0));
        let song = Song { name: "Lucid Dreams".into(), duration: 199.0, ..Song::default() };
        let track = serde_json::json!({"duration": 199.0, "syncedLyrics": "[07:20.00]Lucid dreams", "plainLyrics": "Lucid dreams"});
        assert_eq!(lrclib_lyrics(&song, &[track]), vec!["Lucid dreams".to_string()]);
    }

    /// Genius's "You might also like", glued into lyrics by earlier scrapes, is not lyrics.
    #[test]
    fn genius_song_lists_are_dropped_from_lyrics() {
        assert_eq!(unlisted("[Instrumental and Screams]You might also like"), "[Instrumental and Screams]");
        assert_eq!(unlisted("I love youYou might also like[Chorus]\nOh"), "I love you\n[Chorus]\nOh");
        assert_eq!(unlisted("Hey\nYou might also like[Bridge]"), "Hey\n[Bridge]");
        assert_eq!(unlisted("You might also like me"), "You might also like me");
        // Genius's ticket ad and the song list after it go; the lyrics around them stay.
        let ad = "[Verse]\nI got it\nSee Don Toliver Live\nGet tickets as low as $462\nYou might also like\nOpposite\nDon Toliver\n\n[Chorus]\nYeah";
        assert_eq!(unlisted(ad), "[Verse]\nI got it\n\n[Chorus]\nYeah");
        assert_eq!(unlisted("See you later\nGet up"), "See you later\nGet up");
    }

    /// A Genius that cannot be asked (its website answers 403) is not a Genius without the song's page, which
    /// would let LRCLIB's lyrics in unchecked: BULLY's "KING" would get VULTURES 1's.
    #[test]
    fn lyrics_are_not_checked_against_a_genius_that_cannot_be_asked() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        GENIUS.block("test");
        let artists = vec!["¥$".to_string(), "Kanye West".to_string()];
        let song = Song { name: "KING".into(), artist: "¥$".into(), artists, year: 2026, ..Song::default() };
        assert_eq!(reference(&song, None), None);
        assert_eq!(provider("synced", &song), None);
        // The song's own plain lyrics still check synced lyrics.
        assert_eq!(reference(&song, Some(&BULLY_KING.into())), Some(Some(BULLY_KING.into())));
    }

    /// azlyrics's access check comes as a 200 page (from b.azlyrics.com): it was taken for a song without lyrics,
    /// on every song, with nothing logged. It turns azlyrics off for the run; a lyrics page still gives its lyrics.
    #[test]
    fn azlyrics_access_check_turns_it_off() {
        let _ = crate::CONFIG.set(serde_json::from_str(crate::DEFAULTS).unwrap());
        let lyrics = "<div class=\"ringtone\"></div><div>\n<!-- Usage of azlyrics.com content by any third-party \
            lyrics provider is prohibited. -->\nIs this the real life?<br>\nIs this just fantasy?\n</div>";
        assert_eq!(azlyrics(lyrics).as_deref(), Some("Is this the real life?\nIs this just fantasy?"));
        assert!(!AZLYRICS.blocked());
        let gate =
            "<html><head><title>AZLyrics - request for access</title></head><body><form>...</form></body></html>";
        assert_eq!(azlyrics(gate), None);
        assert!(AZLYRICS.blocked());
        // Not asked again: no request is sent.
        let song = Song { name: "Creep".into(), artist: "Radiohead".into(), ..Song::default() };
        assert_eq!(provider("azlyrics", &song), None);
    }
}
