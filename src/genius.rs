//! Genius, the authority for a song's lyrics: its pages and which of them are the song's own.

use crate::html::html;
use crate::lyrics::{instrumental_title, lyrics_title, page, plain_title, same_album, words, Answers};
use crate::settings::settings;
use crate::{
    http::{self, HttpError, Service},
    lock, search, url_encode, Song,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// Genius's website answers 403 when it blocks this client; the lyrics pages are on the website, so without it Genius
/// cannot be asked at all.
pub(crate) static GENIUS: Service = Service::new("Genius", 500, true);

/// Genius lyrics: the text of each `data-lyrics-container` div. The page puts song lists ("You might also like") and
/// ads between these divs, so only the divs are read.
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

/// Genius could not be asked: it blocks this client, or still fails after the waits. That is not "no page": no page
/// lets LRCLIB's lyrics in unchecked, while a Genius that cannot be asked leaves them for a later run that can check
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unavailable;

/// What Genius answered, if it could be asked.
pub(crate) type Asked<T> = Result<T, Unavailable>;

/// Genius's page of the song, only one with the song's title: the check that replaces lyrics must not take another song
/// with a like title ("In the Flesh?" for The Wall's other "In the Flesh").
pub(crate) fn genius_same(song: &Song, artist: &str, answers: &Answers) -> Asked<Option<Page>> {
    Ok(genius_page(song, artist, answers)?.filter(|page| page.titled))
}

/// Whether the page is the song's recording and not another of the artist's songs with that title: Genius dates it to
/// the release's year or has it on the release's album. Spotify calls Until the End of Time's "When I Get Free II
/// (Remix)" "When I Get Free", the title of a song from R U Still Down; Genius has that song on R U Still Down and
/// Euthanasia.
pub(crate) fn released_on(song: &Song, page: &Page) -> Asked<bool> {
    if page.same_year {
        return Ok(true);
    }
    let url = format!("https://genius.com/api/songs/{}", page.id);
    let reply = serde_json::from_str::<Value>(&genius_get(&url)?).ok();
    let albums = reply.as_ref().and_then(|reply| reply["response"]["song"]["albums"].as_array());
    let mut names = albums.into_iter().flatten().filter_map(|other| other["name"].as_str());
    Ok(names.any(|other| same_album(&song.album_name, other)))
}

pub(crate) fn genius_key(song: &Song, artist: &str) -> String {
    format!("{artist}\n{}\n{}", song.name, song.year)
}

/// Genius's page for the song's title by `artist`: its best hit with that artist and title (a song can have several
/// pages: "Everybody", "Everybody (Original)", "Everybody (Demo 3)"). The lyrics provider and the `.lrc` writer both
/// ask; Genius is paced, so the page is fetched once and stays in the song's `answers`. A failed request is not kept:
/// the next ask tries again.
pub(crate) fn genius_page(song: &Song, artist: &str, answers: &Answers) -> Asked<Option<Page>> {
    let key = genius_key(song, artist);
    if let Some(page) = lock(&answers.genius).get(&key) {
        return Ok(page.clone());
    }
    let page = fetch_page(song, artist)?;
    lock(&answers.genius).insert(key, page.clone());
    Ok(page)
}

/// The API's token is out of requests (the shared docs client has 100 an hour): the website's search is asked.
static GENIUS_API_SPENT: AtomicBool = AtomicBool::new(false);

/// A Genius page or search.
fn genius_get(url: &str) -> Asked<String> {
    page(&GENIUS, url).ok_or(Unavailable)
}

/// Whether the artist `name` is one of `names` ("¥$" of "¥$, Kanye West & Ty Dolla $ign"): its slug is a run of whole
/// words there, however they are spaced ("Lil Wayne", "LilWayne"). As a substring, "¥$" (slug "y") was in every name
/// with a y, Genius Ukrainian Translations' too, and "Nas" in Jonas Brothers. A slug of a letter or two could still be
/// another name's word ("Y"), so such a name must also be spelled the same there.
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

/// Genius's own accounts ("Genius Ukrainian Translations (Український переклад)", "Genius Romanizations", "Genius
/// Traducciones al Español") post translations of other artists' songs, never the songs' own lyrics.
fn genius_account(primary: &str, artist: &str) -> bool {
    primary.starts_with("Genius ") && !among(primary, artist)
}

/// A search hit, as far as it is read.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Hit {
    title: String,
    artist_names: String,
    primary_artist: Named,
    url: Option<String>,
    id: Option<u64>,
    instrumental: Option<bool>,
    release_date_components: Option<Released>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Named {
    name: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Released {
    year: Option<u64>,
}

/// The hits of a search reply's list; one that is not as Genius writes them is left out.
fn hits(list: &Value) -> Option<Vec<Hit>> {
    Some(list.as_array()?.iter().filter_map(|hit| Hit::deserialize(&hit["result"]).ok()).collect())
}

/// The song's hits among Genius's search hits: the same artist and the same title. The artist's first hit can be
/// another of the artist's songs, and the artist may be one of several on the page ("¥$, Kanye West & Ty Dolla $ign").
fn is_song(hit: &Hit, song: &Song, artist: &str) -> bool {
    let primary = &hit.primary_artist.name;
    let (hit_plain, song_plain) = (plain_title(&hit.title), plain_title(lyrics_title(song)));
    // An instrumental ("Big Poppa - Instrumental", "Girl (Take 2 - Instrumental Backing Track)") takes only a page of
    // an instrumental version (Genius has the studio talk of the Beatles' takes), never the sung song's words.
    let wordless = instrumental_title(&song.name);
    (among(artist, &hit.artist_names) || among(primary, artist))
        && !genius_account(primary, artist)
        && !hit_plain.is_empty()
        && (hit_plain.contains(&song_plain) || song_plain.contains(&hit_plain))
        && (!wordless || instrumental_title(&hit.title) || hit.instrumental == Some(true))
}

/// How Genius's hits for a song are ordered. The page of the song's whole title goes first: "WHISKEY (RELEASE ME)"
/// before Maroon 5's "Whiskey" (feat. A$AP Rocky), and "For No One" before "For No One (Take 10 / Backing Track)", a
/// session take from the 2022 box set whose "For No One - 2022 Mix" is the song. Then the artist's own page, then the
/// same title without notes ("Intro" before "Intro (Skit)"), and as an artist can have two songs with one title ("KING"
/// on VULTURES 1 in 2024 and on BULLY in 2026), the song of the release's year.
struct Ranking<'a> {
    artist: &'a str,
    year: u32,
    title: String,
    whole_title: String,
    artists: Vec<String>,
    marks: String,
}

impl<'a> Ranking<'a> {
    fn new(song: &Song, artist: &'a str) -> Self {
        // Artists' names are taken out of titles, but not names of a letter or two: "¥$" would take out every "y".
        let artists: Vec<String> = song.artists.iter().filter(|a| search::slug(a, "").len() >= 3).cloned().collect();
        let whole_title = search::clean_name(search::versionless(&song.name), &artists);
        Self {
            artist,
            year: song.year,
            title: plain_title(lyrics_title(song)),
            whole_title,
            artists,
            marks: marks(search::versionless(&song.name)),
        }
    }

    fn whole(&self, hit: &Hit) -> bool {
        search::clean_name(&hit.title, &self.artists) == self.whole_title
    }

    /// The hit has the song's whole title and its question and exclamation marks, which tell two titles apart where the
    /// words are the same ("In the Flesh?").
    fn titled(&self, hit: &Hit) -> bool {
        self.whole(hit) && marks(&hit.title) == self.marks
    }

    fn same_year(&self, hit: &Hit) -> bool {
        let year = hit.release_date_components.as_ref().and_then(|released| released.year);
        self.year != 0 && year == Some(u64::from(self.year))
    }

    fn key(&self, hit: &Hit) -> (bool, bool, bool, bool, bool) {
        (
            !self.titled(hit),
            !self.whole(hit),
            !among(&hit.primary_artist.name, self.artist),
            plain_title(&hit.title) != self.title,
            !self.same_year(hit),
        )
    }
}

/// The question and exclamation marks of a title, without its notes.
fn marks(title: &str) -> String {
    title[..search::note_at(title, false).unwrap_or(title.len())].chars().filter(|c| matches!(c, '?' | '!')).collect()
}

/// Genius's search hits for `artist` and `title`. With a `genius_token`, Genius's API (as spotDL asks it); it counts
/// requests apart from the website's search, which is asked when the API cannot answer (a token that many people use is
/// often out of requests). A token out of requests stays so for the hour, and a rejected one (401) for good: neither is
/// asked again in this run.
fn search_hits(artist: &str, title: &str) -> Asked<Vec<Hit>> {
    let query = url_encode(&format!("{artist} {title}"));
    let token =
        Some(settings().genius_token.as_str()).filter(|token| !token.is_empty() && !GENIUS_API_SPENT.load(Relaxed));
    if let Some(hits) = token.and_then(|token| api_hits(&query, token)) {
        return Ok(hits);
    }
    let reply = serde_json::from_str::<Value>(&genius_get(&format!("https://genius.com/api/search/song?q={query}"))?)
        .map_err(|_| Unavailable)?;
    Ok(hits(&reply["response"]["sections"][0]["hits"]).unwrap_or_default())
}

fn api_hits(query: &str, token: &str) -> Option<Vec<Hit>> {
    GENIUS.wait();
    let url = format!("https://api.genius.com/search?q={query}");
    let reply = http::request(&url, &[("Authorization", &format!("Bearer {token}"))], None, 0, None);
    if reply.as_ref().is_err_and(|error| matches!(HttpError::status(&**error), Some(429 | 401))) {
        GENIUS_API_SPENT.store(true, Relaxed);
    }
    hits(&serde_json::from_slice::<Value>(&reply.ok()?).ok()?["response"]["hits"])
}

/// The page of a search hit, or nothing for a hit without a page or lyrics.
fn page_of(hit: &Hit, ranking: &Ranking) -> Asked<Option<Page>> {
    let (Some(url), Some(id)) = (hit.url.as_deref(), hit.id) else {
        return Ok(None);
    };
    // An instrumental's page has no lyrics blocks, only "This song is an instrumental", and the API says so too.
    let page = genius_get(url)?;
    let said = hit.instrumental == Some(true) || page.contains("This song is an instrumental");
    let lyrics = genius_lyrics(&page).or_else(|| said.then(|| "[Instrumental]".into()));
    Ok(lyrics.map(|lyrics| Page { lyrics, id, titled: ranking.titled(hit), same_year: ranking.same_year(hit) }))
}

fn fetch_page(song: &Song, artist: &str) -> Asked<Option<Page>> {
    if GENIUS.blocked() {
        return Err(Unavailable);
    }
    let title = lyrics_title(song);
    let mut hits = search_hits(artist, title)?;
    hits.retain(|hit| is_song(hit, song, artist));
    // A title with a note after " - " ("Robbery - Live from the Sydney Opera House") has no page of its own: the song's
    // page has its words.
    let base = title.split(" - ").next().unwrap_or(title).trim();
    if hits.is_empty() && base != title && !base.is_empty() {
        hits = search_hits(artist, base)?;
        hits.retain(|hit| is_song(hit, song, artist));
    }
    let ranking = Ranking::new(song, artist);
    hits.sort_by_key(|hit| ranking.key(hit));
    hits.first().map_or(Ok(None), |hit| page_of(hit, &ranking))
}

/// Whether Genius's page is the song's own (see [`genius_same`] and [`released_on`]) and has no words, only section
/// names ("[Instrumental]"): the song is an instrumental. A page of another song with a like title ("Bohemian Rhapsody
/// (Instrumental)") says nothing about the song.
pub(crate) fn instrumental(song: &Song, page: &Page) -> bool {
    page.titled && words(&page.lyrics).is_empty() && released_on(song, page) == Ok(true)
}

#[cfg(test)]
#[path = "fuzz/genius.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::*;

    /// An artist is matched by whole words: "¥$" (slug "y") took Genius Ukrainian Translations' "CARNIVAL" as its own,
    /// as that name has a y in it.
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
