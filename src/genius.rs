//! Genius, the authority for a song's lyrics: its pages and which of them are the song's own.

use crate::html::html;
use crate::lyrics::{instrumental_title, lyrics_title, page, plain_title, words};
use crate::{
    http::{self, HttpError, Service},
    search, text, url_encode, Song,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Mutex, PoisonError,
    },
};

/// Genius's website answers 403 when it blocks this client; the lyrics pages are on the website, so without it Genius
/// cannot be asked at all.
pub(crate) static GENIUS: Service = Service::new("Genius", 500, true);

/// Genius lyrics: the text of each `data-lyrics-container` div. The page puts song lists ("You might also
/// like") and ads between these divs, so only the divs are read.
pub(crate) fn genius_lyrics(page: &str) -> Option<String> {
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

/// A Genius page of lyrics.
#[derive(Clone)]
pub(crate) struct Page {
    pub(crate) lyrics: String,
    /// Genius's song id.
    pub(crate) id: u64,
    /// The page has the song's title.
    pub(crate) titled: bool,
    /// Genius dates the song to the year of the song's release.
    pub(crate) same_year: bool,
}

/// Genius's page of the song, only one with the song's title: the check that replaces lyrics must not take
/// another song with a like title ("In the Flesh?" for The Wall's other "In the Flesh"). The outer `None`: Genius
/// could not be asked.
pub(crate) fn genius_same(song: &Song, artist: &str) -> Option<Option<Page>> {
    Some(genius_titled(song, artist, 1)?.into_iter().next().filter(|page| page.titled))
}

/// Whether the page is the song's recording and not another of the artist's songs with that title: Genius dates it
/// to the release's year or has it on the release's album. Spotify calls Until the End of Time's "When I Get Free
/// II (Remix)" "When I Get Free", the title of a song from R U Still Down; Genius has that song on R U Still Down
/// and Euthanasia. `None` when Genius could not be asked.
pub(crate) fn released_on(song: &Song, page: &Page) -> Option<bool> {
    if page.same_year {
        return Some(true);
    }
    let url = format!("https://genius.com/api/songs/{}", page.id);
    let album = plain_title(&song.album_name);
    let reply = serde_json::from_str::<Value>(&genius_get(&url)?).ok();
    let albums = reply.as_ref().and_then(|reply| reply["response"]["song"]["albums"].as_array());
    Some(albums.into_iter().flatten().filter_map(|other| other["name"].as_str()).map(plain_title).any(|other| {
        !album.is_empty() && !other.is_empty() && (other.starts_with(&album) || album.starts_with(&other))
    }))
}

/// The pages Genius gave for each song and artist. The lyrics provider and the `.lrc` writer both ask; Genius is
/// paced, so a song's pages are fetched once. A failed request is not kept: the next ask tries again.
pub(crate) static GENIUS_FOUND: std::sync::LazyLock<Mutex<HashMap<String, Vec<Page>>>> =
    std::sync::LazyLock::new(Default::default);

pub(crate) fn genius_key(song: &Song, artist: &str, max: usize) -> String {
    format!("{artist}\n{}\n{}\n{max}", song.name, song.year)
}

/// Genius's pages for the song's title by `artist`: up to `max` hits with that artist and title (a song can have
/// several pages: "Everybody", "Everybody (Original)", "Everybody (Demo 3)"). `None` (Genius could not be asked) is not "no page": no page lets LRCLIB's lyrics in
/// unchecked, while a Genius that cannot be asked leaves them for a later run that can check them.
pub(crate) fn genius_titled(song: &Song, artist: &str, max: usize) -> Option<Vec<Page>> {
    let key = genius_key(song, artist, max);
    if let Some(pages) = GENIUS_FOUND.lock().unwrap_or_else(PoisonError::into_inner).get(&key) {
        return Some(pages.clone());
    }
    let pages = genius_pages(song, artist, max)?;
    GENIUS_FOUND.lock().unwrap_or_else(PoisonError::into_inner).insert(key, pages.clone());
    Some(pages)
}

/// The API's token is out of requests (the shared docs client has 100 an hour): the website's search is asked.
static GENIUS_API_SPENT: AtomicBool = AtomicBool::new(false);

/// A Genius page or search.
fn genius_get(url: &str) -> Option<String> {
    page(&GENIUS, url)
}

/// Whether the artist `name` is one of `names` ("¥$" of "¥$, Kanye West & Ty Dolla $ign"): its slug is a run of
/// whole words there, however they are spaced ("Lil Wayne", "LilWayne"). As a substring, "¥$" (slug "y") was in
/// every name with a y, Genius Ukrainian Translations' too, and "Nas" in Jonas Brothers. A slug of a letter or two
/// could still be another name's word ("Y"), so such a name must also be spelled the same there.
pub(crate) fn among(name: &str, names: &str) -> bool {
    let (slug, words) = (search::slug(name, ""), search::slug(names, " "));
    let words: Vec<&str> = words.split(' ').collect();
    let run = (0..words.len()).any(|start| {
        let mut joined = String::new();
        words[start..].iter().any(|word| {
            joined.push_str(word);
            joined == slug
        })
    });
    !slug.is_empty() && run && (slug.len() >= 3 || names.to_lowercase().contains(&name.to_lowercase()))
}

/// Genius's own accounts ("Genius Ukrainian Translations (Український переклад)", "Genius Romanizations",
/// "Genius Traducciones al Español") post translations of other artists' songs, never the songs' own lyrics.
fn genius_account(primary: &str, artist: &str) -> bool {
    primary.starts_with("Genius ") && !among(primary, artist)
}

/// The pages, or nothing when Genius could not be asked.
fn genius_pages(song: &Song, artist: &str, max: usize) -> Option<Vec<Page>> {
    if GENIUS.blocked() {
        return None;
    }
    let title = lyrics_title(song);
    // With a `genius_token`, Genius's API (as spotDL asks it); it counts requests apart from the website's search,
    // which is asked when the API cannot answer (a token that many people use is often out of requests).
    // A token out of requests stays so for the hour, and a rejected one (401) for good: neither is asked again in
    // this run.
    let search = |title: &str| -> Option<Vec<Value>> {
        let query = url_encode(&format!("{artist} {title}"));
        let token = text("genius_token").filter(|token| !token.is_empty() && !GENIUS_API_SPENT.load(Relaxed));
        let from_api = token.map(|token| format!("Bearer {token}")).and_then(|auth| {
            GENIUS.wait();
            let url = format!("https://api.genius.com/search?q={query}");
            let reply = http::request(&url, &[("Authorization", &auth)], None, 0, None);
            if reply.as_ref().is_err_and(|error| matches!(HttpError::status(&**error), Some(429 | 401))) {
                GENIUS_API_SPENT.store(true, Relaxed);
            }
            serde_json::from_slice::<Value>(&reply.ok()?).ok()?["response"]["hits"].as_array().cloned()
        });
        match from_api {
            Some(hits) => Some(hits),
            None => {
                let url = format!("https://genius.com/api/search/song?q={query}");
                let reply = serde_json::from_str::<Value>(&genius_get(&url)?).ok()?;
                Some(reply["response"]["sections"][0]["hits"].as_array().cloned().unwrap_or_default())
            }
        }
    };
    let song_title = plain_title(title);
    fn primary(hit: &Value) -> &str {
        hit["result"]["primary_artist"]["name"].as_str().unwrap_or_default()
    }
    // An instrumental ("Big Poppa - Instrumental", "Girl (Take 2 - Instrumental Backing Track)") takes only a page
    // of an instrumental version (Genius has the studio talk of the Beatles' takes), never the sung song's words.
    let wordless = instrumental_title(&song.name);
    // The same artist and the same title: the artist's first hit can be another of the artist's songs. The
    // artist may be one of several on the page ("¥$, Kanye West & Ty Dolla $ign").
    let same = |hit: &Value| {
        let artists = hit["result"]["artist_names"].as_str().unwrap_or_default();
        let full_title = hit["result"]["title"].as_str().unwrap_or_default();
        let hit_title = plain_title(full_title);
        (among(artist, artists) || among(primary(hit), artist))
            && !genius_account(primary(hit), artist)
            && !hit_title.is_empty()
            && (hit_title.contains(&song_title) || song_title.contains(&hit_title))
            && (!wordless || instrumental_title(full_title) || hit["result"]["instrumental"].as_bool() == Some(true))
    };
    let mut hits = search(title)?;
    hits.retain(same);
    // A title with a note after " - " ("Robbery - Live from the Sydney Opera House") has no page of its own: the
    // song's page has its words.
    let base = title.split(" - ").next().unwrap_or(title).trim();
    if hits.is_empty() && base != title && !base.is_empty() {
        hits = search(base)?;
        hits.retain(same);
    }
    // The page of the song's whole title goes first: "WHISKEY (RELEASE ME)" before Maroon 5's "Whiskey" (feat.
    // A$AP Rocky), and "For No One" before "For No One (Take 10 / Backing Track)", a session take from the 2022 box
    // set whose "For No One - 2022 Mix" is the song. Then the artist's own page, then the same title without notes
    // ("Intro" before "Intro (Skit)"), and as an artist can have two songs with one title ("KING" on VULTURES 1 in
    // 2024 and on BULLY in 2026), the song of the release's year.
    let year = |hit: &Value| hit["result"]["release_date_components"]["year"].as_u64();
    fn hit_title(hit: &Value) -> &str {
        hit["result"]["title"].as_str().unwrap_or_default()
    }
    // Artists' names are taken out of titles, but not names of a letter or two: "¥$" would take out every "y".
    let artists: Vec<String> = song.artists.iter().filter(|a| search::slug(a, "").len() >= 3).cloned().collect();
    let whole_title = search::clean_name(search::versionless(&song.name), &artists);
    let whole = |hit: &Value| search::clean_name(hit_title(hit), &artists) == whole_title;
    // A question or exclamation mark tells two titles apart where the words are the same ("In the Flesh?").
    let marks = |title: &str| -> String {
        let end = [" (", " ["].iter().filter_map(|note| title.find(note)).min().unwrap_or(title.len());
        title[..end].chars().filter(|c| matches!(c, '?' | '!')).collect()
    };
    let song_marks = marks(search::versionless(&song.name));
    let titled = |hit: &Value| whole(hit) && marks(hit_title(hit)) == song_marks;
    let own = |hit: &Value| among(primary(hit), artist);
    let exact = |hit: &Value| plain_title(hit_title(hit)) == song_title;
    hits.sort_by_key(|hit| {
        (!titled(hit), !whole(hit), !own(hit), !exact(hit), song.year == 0 || year(hit) != Some(u64::from(song.year)))
    });
    let same_year = |hit: &Value| song.year != 0 && year(hit) == Some(u64::from(song.year));
    let mut pages = vec![];
    for hit in hits.iter().take(max) {
        let (Some(url), Some(id)) = (hit["result"]["url"].as_str(), hit["result"]["id"].as_u64()) else { continue };
        // An instrumental's page has no lyrics blocks, only "This song is an instrumental", and the API says so too.
        let page = genius_get(url)?;
        let said =
            hit["result"]["instrumental"].as_bool() == Some(true) || page.contains("This song is an instrumental");
        let Some(lyrics) = genius_lyrics(&page).or_else(|| said.then(|| "[Instrumental]".into())) else { continue };
        pages.push(Page { lyrics, id, titled: titled(hit), same_year: same_year(hit) });
    }
    Some(pages)
}

/// Whether Genius's page is the song's own (see [`genius_same`] and [`released_on`]) and has no words, only
/// section names ("[Instrumental]"): the song is an instrumental. A page of another song with a like title
/// ("Bohemian Rhapsody (Instrumental)") says nothing about the song.
pub(crate) fn instrumental(song: &Song, page: &Page) -> bool {
    page.titled && words(&page.lyrics).is_empty() && released_on(song, page) == Some(true)
}

#[cfg(test)]
#[path = "fuzz/genius.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;

    /// An artist is matched by whole words: "¥$" (slug "y") took Genius Ukrainian Translations' "CARNIVAL" as its
    /// own, as that name has a y in it.
    #[test]
    fn genius_artists_are_matched_by_whole_names() {
        assert!(among("¥$", "¥$, Kanye West & Ty Dolla $ign"));
        assert!(!among("¥$", "Genius Ukrainian Translations (Український переклад)"));
        assert!(!among("¥$", "Kanye West & Ty Dolla $ign"));
        assert!(!among("¥$", "Y & Kanye West"));
        assert!(!among("Nas", "Jonas Brothers"));
        assert!(among("Lil Wayne", "Drake & LilWayne"));
        assert!(among("Kanye West", "¥$, Kanye West & Ty Dolla $ign"));
        assert!(!among("", "Kanye West"));
        assert!(genius_account("Genius Ukrainian Translations (Український переклад)", "¥$"));
        assert!(genius_account("Genius Traducciones al Español", "Bad Bunny"));
        assert!(!genius_account("¥$", "¥$"));
    }

    /// Only `&#x27;`, `&quot;` and `&amp;` were decoded, so a Genius "<3" or "->" stayed "&lt;3" or "-&gt;".
    #[test]
    fn lyrics_decode_every_html_entity_once() {
        let page = "<div data-lyrics-container=\"true\">I &lt;3 you<br/>Don&#x27;t go -&gt; &quot;there&quot; \
            &amp; back<br/>&amp;lt;em&amp;gt; &#39;Til&nbsp;dawn&#x2019; &copy; &#xZZ; R&B</div>";
        let lyrics = "I <3 you\nDon't go -> \"there\" & back\n&lt;em&gt; 'Til dawn\u{2019} &copy; &#xZZ; R&B";
        assert_eq!(genius_lyrics(page).unwrap(), lyrics);
    }
}
