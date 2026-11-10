use crate::settings::settings;
use crate::{cached, files, http, log, now, spotify, Song, R};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering::Relaxed},
    time::Duration,
};

const REJECTED: &str =
    "bassboost bassboosted boosted 8d 8daudio 3d 16d slowed sped spedup speedup nightcore \
    daycore reverb karaoke cover tribute instrumental acapella drumless chipmunk chipmunks \
    live concert remix acoustic extended clean mashup originally reversed reverse amv";
const PENALIZED: &str = "remaster remastered lyrics lyric visualizer video edit";
// "album", "lp", "edited" and "explicit" make " - Album Version (Edited)" and " - LP Version"
// version notes: kept in the title, they sank the exact clean track below the explicit one. The
// explicit badge tells the edits apart.
const SUFFIXES: &str =
    "remaster remastered mono stereo mix version edit edited explicit single radio album lp \
    deluxe anniversary original bonus";

/// Until when (Unix seconds) every thread holds off YouTube after it blocked us.
static PAUSE: AtomicU64 = AtomicU64::new(0);

/// YouTube Music's classification of a video (`musicVideoType`).
#[derive(Clone, Copy, Default, PartialEq)]
enum Kind {
    /// Label-provided audio: the exact studio recording.
    Atv,
    /// Official music video: the right recording, but maybe with skits, intros or another edit.
    Omv,
    /// User upload, or a result with no type (yt-dlp searches).
    #[default]
    Other,
}

#[derive(Default)]
struct Hit {
    song: Song,
    verified: bool,
    isrc: bool,
    kind: Kind,
    /// YouTube Music's explicit badge; only meaningful for song results.
    explicit: bool,
}

fn hit(url: String, name: String, artists: Vec<String>, album: String, duration: f64) -> Song {
    let song_id = url
        .rsplit(['=', '/'])
        .next()
        .unwrap_or_default()
        .to_string();
    let artist = artists.first().cloned().unwrap_or_default();
    Song {
        name,
        artist,
        artists,
        album_name: album,
        duration,
        song_id,
        url: url.clone(),
        download_url: Some(url),
        ..Song::default()
    }
}

/// Runs a YouTube step, retrying transient errors; when YouTube blocks us, every thread pauses.
pub(crate) fn youtube<T>(mut step: impl FnMut() -> Result<T, String>) -> R<T> {
    let mut attempt = 0;
    loop {
        let wait = PAUSE.load(Relaxed).saturating_sub(now());
        if wait > 0 {
            log(
                "WARNING",
                &format!("YouTube is blocking requests; waiting {wait} s"),
            );
            std::thread::sleep(Duration::from_secs(wait));
        }
        let error = match step() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let has = |signs: &[&str]| signs.iter().any(|sign| error.contains(sign));
        let blocked = has(&[
            "not a bot",
            "HTTP Error 403",
            "HTTP 403",
            "HTTP Error 429",
            "HTTP 429",
            "Too Many Requests",
        ]);
        let transient = has(&[
            "Requested format is not available",
            "Failed to extract any player response",
        ]);
        if attempt >= settings().max_retries || !(blocked || transient) {
            return Err(error.into());
        }
        if blocked && attempt > 0 {
            PAUSE.fetch_max(now() + (30 << attempt.min(2)), Relaxed);
        } else {
            std::thread::sleep(Duration::from_secs(3));
        }
        attempt += 1;
    }
}

pub(crate) fn yt_dlp() -> Command {
    let settings = settings();
    let mut command = Command::new(&settings.yt_dlp);
    // Deno is on by default; these are used when it is missing.
    command.args(["--js-runtimes", "node", "--js-runtimes", "bun"]);
    for (value, flag) in [
        (&settings.cookie_file, "--cookies"),
        (&settings.proxy, "--proxy"),
    ] {
        command.args(value.iter().flat_map(|value| [flag, value.as_str()]));
    }
    command.args(
        settings
            .yt_dlp_args
            .as_deref()
            .unwrap_or_default()
            .split_whitespace(),
    );
    command
}

/// Runs `command`, naming the program when it cannot be started: the OS error alone ("No such file
/// or directory") does not say that yt-dlp or FFmpeg is missing.
pub(crate) fn run(command: &mut Command) -> Result<Output, String> {
    command.output().map_err(|error| {
        format!(
            "cannot run {}: {error}",
            command.get_program().to_string_lossy()
        )
    })
}

pub(crate) fn slug(s: &str, sep: &str) -> String {
    let s = deunicode::deunicode(s).to_lowercase();
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Similarity in percent: twice the longest common subsequence over the total length.
pub(crate) fn ratio(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let row = a.iter().fold(vec![0usize; b.len() + 1], |row, x| {
        b.iter().enumerate().fold(vec![0], |mut next, (j, y)| {
            next.push(if x == y {
                row[j] + 1
            } else {
                row[j + 1].max(next[j])
            });
            next
        })
    });
    let n = (a.len() + b.len()) as f64;
    if n == 0.0 {
        100.0
    } else {
        200.0 * row[b.len()] as f64 / n
    }
}

/// Words and joined word pairs of the title, artists and (with `album`) album.
fn words(song: &Song, album: bool) -> HashSet<String> {
    let mut words = HashSet::new();
    let album = album.then_some(&song.album_name);
    for value in [&song.name].into_iter().chain(album).chain(&song.artists) {
        let slugged = slug(value, "-");
        let tokens: Vec<&str> = slugged.split('-').filter(|word| !word.is_empty()).collect();
        words.extend(tokens.iter().map(|word| word.to_string()));
        words.extend(tokens.windows(2).map(|pair| pair.concat()));
    }
    words
}

/// A word that names another recording: "remix", "live", "sped", "432hz".
pub(crate) fn altered(word: &str) -> bool {
    let hertz = word
        .strip_suffix("hz")
        .is_some_and(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()));
    hertz || REJECTED.split(' ').any(|version| version == word)
}

fn is_version_suffix(suffix: &str) -> bool {
    let words = slug(suffix, "-");
    !words.is_empty()
        && words
            .split('-')
            .all(|w| w.bytes().all(|b| b.is_ascii_digit()) || SUFFIXES.split(' ').any(|s| s == w))
}

/// A credit or soundtrack note rather than another recording: "feat. X", "with X", "From "Top
/// Gun"".
fn is_note(text: &str) -> bool {
    matches!(
        slug(text, "-").split('-').next(),
        Some("feat" | "ft" | "featuring" | "with" | "from")
    )
}

/// The title without a " - Remastered 2011" or " - From "Black Panther"" suffix.
pub(crate) fn versionless(name: &str) -> &str {
    let suffix = |(_, suffix): &(&str, &str)| is_version_suffix(suffix) || is_note(suffix);
    name.rsplit_once(" - ")
        .filter(suffix)
        .map_or(name, |(title, _)| title)
}

/// The slugged title without version notes, featured artists or a " - Remix" suffix, to compare the
/// editions of an album: "Getting Better - Remix", "Getting Better - 2017 Mix" and "Getting Better -
/// Remastered 2009" are all "getting-better", and "Norwegian Wood (This Bird Has Flown) [2026 Mix]"
/// is "norwegian-wood-this-bird-has-flown". Remix editions date their remix: "Dogs - 2018 Remix"
/// and "Sorrow - Remix 2019" are "dogs" and "sorrow". They also write "Pt." as "Part": "A New
/// Machine Part 1 - Remix 2019" is "A New Machine, Pt. 1".
pub(crate) fn edition_title(name: &str) -> String {
    let name = clean_name(versionless(name), &[]);
    let mut words: Vec<&str> = name
        .split('-')
        .map(|word| if word == "part" { "pt" } else { word })
        .collect();
    let year = |word: &str| word.len() == 4 && word.bytes().all(|b| b.is_ascii_digit());
    let note = match words[..] {
        [.., date, "remix"] | [.., "remix", date] if year(date) => 2,
        [.., "remix"] => 1,
        _ => 0,
    };
    words.truncate(words.len() - note);
    words.join("-")
}

/// The slugged title without featured artists, soundtrack notes and version groups.
pub(crate) fn clean_name(name: &str, artists: &[String]) -> String {
    let (mut clean, mut rest) = (String::new(), name);
    while let Some(open) = rest.find(['(', '[']) {
        let Some(close) = rest[open..].find([')', ']']).map(|close| open + close) else {
            break;
        };
        clean.push_str(&rest[..open]);
        let group = &rest[open + 1..close];
        if !is_note(group) && !is_version_suffix(group) {
            clean.push_str(&rest[open..=close]);
        }
        rest = &rest[close + 1..];
    }
    clean.push_str(rest);
    let name = artists
        .iter()
        .fold(format!("-{}-", slug(&clean, "-")), |name, a| {
            name.replace(&format!("-{}-", slug(a, "-")), "-")
        });
    name.trim_matches('-').into()
}

/// The numbered markers of a title: "Side 2, Pt. 2: Skins" has ("side", 2) and ("pt", 2), "Part II"
/// has ("pt", 2).
fn markers(name: &str) -> Vec<(&'static str, usize)> {
    const ROMAN: [&str; 10] = ["i", "ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"];
    let slugged = slug(name, "-");
    let words: Vec<&str> = slugged.split('-').collect();
    let marker = |pair: &[&str]| {
        let marker = match pair[0] {
            "side" => "side",
            "pt" | "part" => "pt",
            "take" => "take",
            "vol" | "volume" => "vol",
            "no" | "number" => "no",
            _ => return None,
        };
        let number = pair[1]
            .parse()
            .ok()
            .or_else(|| ROMAN.iter().position(|r| *r == pair[1]).map(|i| i + 1))?;
        Some((marker, number))
    };
    words.windows(2).filter_map(marker).collect()
}

/// The title after a prefix of only numbered markers: "Skins" for "Side 2, Pt. 2: Skins", as
/// official uploads name it.
fn unnumbered(name: &str) -> Option<&str> {
    let (head, tail) = name.split_once(": ")?;
    (markers(head).len() * 2 == slug(head, "-").split('-').count()).then_some(tail)
}

/// How well a result matches the song, 0 for a reject; official audio and the right edit rank
/// first.
fn score(s: &Song, h: &Hit) -> f64 {
    let hit = &h.song;
    let (rname, sname) = (slug(&hit.name, "-"), slug(&s.name, "-"));
    let versionless_name = versionless(&s.name);
    let versionless = slug(versionless_name, "-");
    let (stripped, bare) = (
        clean_name(&hit.name, &s.artists),
        clean_name(versionless_name, &s.artists),
    );
    let tail = clean_name(
        unnumbered(versionless_name).unwrap_or(versionless_name),
        &s.artists,
    );
    let pairs = [
        (&sname, &rname),
        (&sname, &stripped),
        (&versionless, &rname),
        (&versionless, &stripped),
        (&bare, &stripped),
        (&tail, &stripped),
    ];
    // "Side 2, Pt. 2: Skins" and "Side 4, Pt. 3: Surfacing" share most of their letters, so the
    // ratio passed another part: a marker that both titles number must have the same first number
    // ("Take 2 With Solo From Take 1" is take 2).
    let (song_markers, hit_markers) = (markers(&s.name), markers(&hit.name));
    let first = |markers: &[(&str, usize)], kind| markers.iter().find(|m| m.0 == kind).map(|m| m.1);
    let misnumbered = song_markers
        .iter()
        .any(|m| first(&hit_markers, m.0).is_some_and(|n| Some(n) != first(&song_markers, m.0)));
    let name = pairs.iter().map(|(a, b)| ratio(a, b)).fold(0.0, f64::max);
    let gap = (s.duration - hit.duration).abs();
    // The ISRC search finds the label's track under another credit ("Tupac" for "2Pac", the Beatle
    // without the BBC presenter Spotify lists first) and on another album ("Best of British
    // (tribute to Coldplay)" for Tyler Ward's "The Scientist"), but also unrelated songs: such a
    // hit needs the same title and length, not the artists.
    let isrc = h.isrc && name > 85.0 && gap <= 3.0;
    let (result_words, allowed_words) = (words(hit, !isrc), words(s, true));
    let rejected = result_words
        .iter()
        .any(|word| altered(word) && !allowed_words.contains(word));
    let penalized = PENALIZED
        .split(' ')
        .filter(|w| result_words.contains(*w) && !allowed_words.contains(*w))
        .count() as f64;
    let combined_artists = slug(&hit.artists.join(" "), "-");
    // One name as whole words of the other ("ADIAM" / "Adiam Dymott"); short names could match
    // anything.
    let same_artist = |a: &str, b: &str| {
        let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
        ratio(a, b) > 85.0
            || (short.len() >= 4 && format!("-{long}-").contains(&format!("-{short}-")))
    };
    let found = |a: &String| {
        let artist = slug(a, "-");
        hit.artists
            .iter()
            .any(|r| same_artist(&artist, &slug(r, "-")))
            || ratio(&artist, &combined_artists) > 85.0
            || rname.contains(&artist)
    };
    let count = s.artists.len().max(1) as f64;
    let artists = 100.0 * s.artists.iter().filter(|a| found(a)).count() as f64 / count;
    let main = |a: &str| {
        let a = slug(a, "-");
        hit.artists
            .iter()
            .chain([&hit.name, &combined_artists])
            .map(|r| ratio(&a, &slug(r, "-")))
            .fold(0.0, f64::max)
    };
    let album = !hit.album_name.is_empty()
        && ratio(&slug(&s.album_name, "-"), &slug(&hit.album_name, "-")) > 80.0;
    // Spotify may list a presenter or band member first ("Lee Peters", "Oliver Leimbach") where
    // YouTube Music credits only the act, and YouTube Music may credit an alias ("Tupac" for
    // "2Pac"): a hit as long as the song is theirs when its lead artist is another of Spotify's
    // artists (scored below a full credit, which then wins), or when it has the song's title on the
    // song's album.
    let lead = gap <= 3.0
        && s.artists
            .iter()
            .any(|a| same_artist(&slug(a, "-"), &slug(&hit.artist, "-")));
    let artists = if found(&s.artist) {
        artists.max(main(&s.artist)).max(160.0 / count).min(100.0)
    } else if gap <= 3.0 && album && name > 85.0 {
        100.0
    } else if lead {
        s.artists
            .iter()
            .map(|a| main(a))
            .fold(0.0, f64::max)
            .min(85.0)
    } else {
        main(&s.artist).min(artists)
    };
    // A long shared note (" - Live At The BBC For "Pop Go The Beatles" / 23rd July, 1963") lifted
    // the name ratio of another song ("And Here We Are Again" for "Lower 5E"): the title before
    // such a note must be in the result's. Featured artists are left out on both sides: YouTube
    // Music writes "Hustler's Story [Instrumental] (feat. Akon, Big Gee & Scarface)" for "Hustler's
    // Story (feat. Scarface, Akon & Big Gee) - Instrumental". An ISRC hit has the song's title and
    // length already.
    let title = clean_name(s.name.split(" - ").next().unwrap_or_default(), &s.artists);
    let parts = hit
        .name
        .split(" - ")
        .flat_map(|part| part.split(['(', ')', '[', ']']));
    let titled = isrc
        || !versionless_name.contains(" - ")
        || format!("-{}-", clean_name(&hit.name, &s.artists)).contains(&format!("-{title}-"))
        || parts
            .map(|part| ratio(&title, &slug(part, "-")))
            .any(|r| r > 60.0);
    let time = (-0.1 * gap).exp() * 100.0;
    let avg = (artists + name) / 2.0;
    let skip = rejected
        || !titled
        || misnumbered
        || name <= 60.0
        || (artists < 70.0 && !isrc)
        || time < 25.0
        || (time < 50.0 && avg < 75.0);
    let avg = if !h.isrc && avg <= 85.0 {
        (avg + time) / 2.0
    } else {
        avg
    };
    // The same recording is on singles, deluxe editions and compilations, so a matching album only
    // breaks ties; penalizing other albums (as Python spotDL does) made album-less uploads beat the
    // label's own audio.
    let album = if album { 3.0 } else { 0.0 };
    // Clean and explicit edits are separate tracks with the same title; YouTube Music badges them.
    let edit = if h.verified && h.explicit != s.explicit {
        -10.0
    } else {
        0.0
    };
    // Without this, a perfect name and artist match would tie an original master with its remaster.
    let closeness = -(0.5 * gap).min(5.0);
    let kind = match h.kind {
        Kind::Atv => 0.0,
        Kind::Omv => -5.0,
        Kind::Other => -12.0,
    };
    let adjusted = avg.min(100.0) - 15.0 * penalized + album + edit + closeness + kind;
    adjusted.max(1.0) * f64::from(u8::from(!skip))
}

fn ytm_hits(reply: &Value, songs: bool, isrc: bool) -> Vec<Hit> {
    let tab =
        &reply["contents"]["tabbedSearchResultsRenderer"]["tabs"][0]["tabRenderer"]["content"];
    let shelves = tab["sectionListRenderer"]["contents"]
        .as_array()
        .into_iter()
        .flatten();
    let items = shelves.flat_map(|shelf| {
        shelf["musicShelfRenderer"]["contents"]
            .as_array()
            .into_iter()
            .flatten()
    });
    let parse = |item: &Value| {
        let r = &item["musicResponsiveListItemRenderer"];
        let column = |i: usize| {
            &r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"]
        };
        let meta = column(1).as_array()?;
        let linked = |kind: &str| -> Vec<String> {
            let runs = meta.iter().filter(|run| {
                run["navigationEndpoint"]["browseEndpoint"]
                    .to_string()
                    .contains(kind)
            });
            runs.filter_map(|run| run["text"].as_str().map(String::from))
                .collect()
        };
        let time = meta.last()?["text"]
            .as_str()?
            .split(':')
            .try_fold(0.0, |a, p| p.parse::<f64>().map(|p| a * 60.0 + p));
        let artists = Some(linked("ARTIST"))
            .filter(|a| !a.is_empty())
            .or_else(|| Some(vec![meta.first()?["text"].as_str()?.into()]))?;
        let config = "/watchEndpoint/watchEndpointMusicSupportedConfigs\
            /watchEndpointMusicConfig/musicVideoType";
        let overlay = "/overlay/musicItemThumbnailOverlayRenderer/content\
            /musicPlayButtonRenderer/playNavigationEndpoint";
        let first_column = "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0\
            /navigationEndpoint";
        let paths = [
            format!("{overlay}{config}"),
            format!("{first_column}{config}"),
        ];
        let kind = match paths.iter().find_map(|path| r.pointer(path)?.as_str()) {
            Some("MUSIC_VIDEO_TYPE_ATV") => Kind::Atv,
            Some("MUSIC_VIDEO_TYPE_OMV") => Kind::Omv,
            _ => Kind::Other,
        };
        let mut badges = r["badges"].as_array().into_iter().flatten();
        let explicit = badges.any(|badge| {
            badge["musicInlineBadgeRenderer"]["icon"]["iconType"] == "MUSIC_EXPLICIT_BADGE"
        });
        let url = format!(
            "https://music.youtube.com/watch?v={}",
            r["playlistItemData"]["videoId"].as_str()?
        );
        let album = linked("ALBUM").into_iter().next().unwrap_or_default();
        let song = hit(
            url,
            column(0)[0]["text"].as_str()?.into(),
            artists,
            album,
            time.ok()?,
        );
        Some(Hit {
            song,
            verified: songs,
            isrc,
            kind,
            explicit,
        })
    };
    items.filter_map(parse).collect()
}

/// YouTube Music's song results (`songs`) or video results for a query.
fn ytm(query: &str, songs: bool, isrc: bool) -> R<Vec<Hit>> {
    let params = if songs {
        "EgWKAQIIAWoMEAMQBBAJEA4QChAF"
    } else {
        "EgWKAQIQAWoMEA4QChADEAQQCRAF"
    };
    let client = json!({
        "clientName": "WEB_REMIX", "clientVersion": "1.20251001.01.00", "hl": "en", "gl": "US",
    });
    let body = json!({"context": {"client": client}, "query": query, "params": params}).to_string();
    let reply = cached(&format!("ytm{body}"), 7, || {
        let url = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
        let reply: Value = youtube(|| {
            http::post_json(url, &[("Content-Type", "application/json")], &body)
                .map_err(|error| error.to_string())
        })?;
        reply
            .get("contents")
            .is_some()
            .then_some(reply)
            .ok_or_else(|| "unexpected YouTube Music response".into())
    })?;
    Ok(ytm_hits(&reply, songs, isrc))
}

/// yt-dlp's flat listing of a search (`ytsearch10:...`), video or playlist.
fn ytdlp(target: &str) -> R<Vec<Hit>> {
    let stdout = youtube(|| {
        let out = run(yt_dlp().args(["--flat-playlist", "-J", "--no-warnings", target]))?;
        let error = || format!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).trim());
        out.status
            .success()
            .then(|| out.stdout.clone())
            .ok_or_else(error)
    })?;
    let mut reply: Value = serde_json::from_slice(&stdout)?;
    let entries = match reply["entries"].take() {
        Value::Array(entries) => entries,
        _ => vec![reply],
    };
    let parse = |entry: &Value| {
        let artist = ["artist", "uploader", "channel"]
            .iter()
            .find_map(|key| entry[key].as_str())
            .unwrap_or_default();
        let url = entry["webpage_url"]
            .as_str()
            .or(entry["url"].as_str())?
            .into();
        let name = entry["track"].as_str().or(entry["title"].as_str())?.into();
        let album = entry["album"].as_str().unwrap_or_default().into();
        Some(Hit {
            song: hit(
                url,
                name,
                vec![artist.into()],
                album,
                entry["duration"].as_f64().unwrap_or_default(),
            ),
            ..Hit::default()
        })
    };
    Ok(entries.iter().filter_map(parse).collect())
}

/// The ISRC search's hits that are the song, ranked above every text hit. YouTube Music fills an
/// ISRC search it has no match for with unrelated songs ("Purple Reign" for "Swimming Pools
/// (Drank)"), so they are scored even when `--dont-filter-results` turns scoring off for text hits;
/// they must also be the same length and edit.
fn isrc_picks(song: &Song, hits: Vec<Hit>) -> impl Iterator<Item = (f64, String)> + '_ {
    let same =
        |h: &Hit| (h.song.duration - song.duration).abs() < 10.0 && h.explicit == song.explicit;
    hits.into_iter()
        .filter(same)
        .map(|h| (score(song, &h) + 1000.0, h.song.url))
        .filter(|c| c.0 > 1000.0)
}

/// Up to three URLs to try, best first: YouTube Music's ISRC match, then the text search of each
/// provider.
pub(crate) fn find(song: &Song) -> R<Vec<String>> {
    let query = files::fill(
        settings()
            .search_query
            .as_deref()
            .unwrap_or("{artists} - {title}"),
        song,
    );
    let filter = settings().filter_results;
    let rank = |hit: &Hit| if filter { score(song, hit) } else { 100.0 };
    let verified = settings().only_verified_results;
    let (mut found, mut error) = (Vec::<(f64, String)>::new(), None::<String>);
    let mut note = |result: R<Vec<Hit>>| {
        result.unwrap_or_else(|e| {
            error.get_or_insert(e.to_string());
            Vec::new()
        })
    };
    for provider in settings().audio_providers.iter().map(String::as_str) {
        let mut hits = match provider {
            "youtube-music" => note(ytm(&query, true, false)),
            "youtube" => note(ytdlp(&format!("ytsearch10:{query}"))),
            _ => note(ytdlp(&format!("scsearch10:{query}"))),
        };
        if provider == "youtube-music" {
            let isrc = song
                .isrc
                .as_deref()
                .map_or_else(|| Ok(Vec::new()), |isrc| ytm(isrc, true, true));
            found.extend(isrc_picks(song, note(isrc)));
            if !verified && hits.iter().map(rank).fold(0.0, f64::max) < 80.0 {
                hits.extend(note(ytm(&query, false, false)));
            }
        }
        let hits = hits.into_iter().filter(|hit| !verified || hit.verified);
        found.extend(
            hits.map(|hit| (rank(&hit), hit.song.url))
                .filter(|candidate| candidate.0 > 0.0),
        );
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut seen = HashSet::new();
    let found: Vec<String> = found
        .into_iter()
        .map(|c| c.1)
        .filter(|url| seen.insert(url.clone()))
        .take(3)
        .collect();
    match (found.is_empty(), error) {
        (false, _) => Ok(found),
        (true, Some(error)) => Err(format!("search failed for {query}: {error}").into()),
        (true, None) => Err(format!("no match for {query}").into()),
    }
}

/// The Spotify song with a video's URL, and with its title, artists and length when `ytm_data` is
/// on.
pub(crate) fn with_video(song: Song, video: Song) -> Song {
    let song = match settings().ytm_data {
        true => Song {
            name: video.name,
            artist: video.artist,
            artists: video.artists,
            duration: video.duration,
            album_name: video.album_name,
            ..song
        },
        false => song,
    };
    Song {
        download_url: video.download_url,
        ..song
    }
}

/// Songs for a YouTube or SoundCloud URL; YouTube Music songs get Spotify metadata when `lookup` is
/// set.
pub(crate) fn source(url: &str, lookup: bool) -> R<Vec<Song>> {
    let single = url.contains("youtu") && !(url.contains("list=") && !url.contains("watch?"));
    let lookup = lookup && url.contains("music.youtube.com");
    let song = |hit: Hit| match lookup {
        false => Some(hit.song),
        true => {
            let found = spotify::resolve(&format!(
                "{} - {}",
                hit.song.artists.join(", "),
                hit.song.name
            ))
            .ok()?;
            Some(with_video(found.into_iter().next()?, hit.song))
        }
    };
    Ok(ytdlp(url)?
        .into_iter()
        .take(if single { 1 } else { usize::MAX })
        .filter_map(song)
        .collect())
}

#[cfg(test)]
#[path = "fuzz/search.rs"]
mod fuzz;

#[cfg(test)]
mod tests {
    use super::{edition_title, isrc_picks, score, versionless, Hit, Kind, Song};

    fn song(name: &str, album: &str, duration: f64, explicit: bool) -> Song {
        let artists = vec!["2Pac".to_string()];
        Song {
            name: name.into(),
            artist: "2Pac".into(),
            artists,
            album_name: album.into(),
            duration,
            explicit,
            ..Song::default()
        }
    }

    fn atv(name: &str, duration: f64, explicit: bool) -> Hit {
        let song = song(name, "Until The End Of Time", duration, explicit);
        Hit {
            song,
            verified: true,
            isrc: false,
            kind: Kind::Atv,
            explicit,
        }
    }

    #[test]
    fn album_version_edited_is_a_version_note() {
        assert_eq!(
            versionless("Lil' Homies - Album Version (Edited)"),
            "Lil' Homies"
        );
        assert_eq!(versionless("Breathin - LP Version"), "Breathin");
        assert_eq!(
            edition_title("M.O.B. - Album Version (Edited)"),
            edition_title("M.O.B.")
        );
        // Remix editions date their remix and write "Pt." as "Part"; a remix with a name is another
        // song.
        assert_eq!(
            edition_title("Pigs on the Wing 1 - 2018 Remix"),
            "pigs-on-the-wing-1"
        );
        assert_eq!(
            edition_title("A New Machine Part 1 - Remix 2019"),
            edition_title("A New Machine, Pt. 1")
        );
        assert_eq!(
            edition_title("Pigs (Three Different Ones) - 2018 Remix"),
            "pigs-three-different-ones"
        );
        assert_eq!(
            edition_title("Until The End Of Time - RP Remix"),
            "until-the-end-of-time-rp"
        );
        assert_eq!(edition_title("Song 2 - Remix"), "song-2");
        // The clean track was scored 0 against its own exact clean audio.
        let homies = song(
            "Lil' Homies - Album Version (Edited)",
            "Until The End Of Time",
            224.0,
            false,
        );
        assert!(score(&homies, &atv("Lil' Homies", 224.0, false)) > 90.0);
        // The clean edit beats the explicit edit with a featured artist.
        let happy = song(
            "Happy Home II - Album Version (Edited)",
            "Until The End Of Time",
            235.0,
            false,
        );
        let (clean, explicit) = (
            atv("Happy Home II", 235.0, false),
            atv("Happy Home II (feat. Yusef Sharid)", 235.0, true),
        );
        assert!(score(&happy, &clean) > score(&happy, &explicit));
    }

    fn track(name: &str, artists: &[&str], album: &str, duration: f64) -> Song {
        let artists: Vec<String> = artists.iter().map(|artist| artist.to_string()).collect();
        Song {
            name: name.into(),
            artist: artists[0].clone(),
            artists,
            album_name: album.into(),
            duration,
            ..Song::default()
        }
    }

    fn result(name: &str, artists: &[&str], album: &str, duration: f64, isrc: bool) -> Hit {
        Hit {
            song: track(name, artists, album, duration),
            verified: true,
            isrc,
            kind: Kind::Atv,
            explicit: false,
        }
    }

    #[test]
    fn official_tracks_with_other_credits() {
        // YouTube Music credits 2Pac's live album to "Tupac": its ISRC hit and its track on the
        // song's album count, but unrelated ISRC hits and a Tupac track on another album do not.
        let ridah = track(
            "Ambitionz Az A Ridah - Live",
            &["2Pac", "Outlawz"],
            "Live at the House Of Blues",
            133.5,
        );
        let tupac = |album, isrc| {
            result(
                "Ambitionz Az A Ridah (Live) (feat. Outlawz)",
                &["Tupac"],
                album,
                134.0,
                isrc,
            )
        };
        assert!(score(&ridah, &tupac("Tupac: Live At The House Of Blues", true)) > 0.0);
        assert!(score(&ridah, &tupac("Live at the House Of Blues", false)) > 0.0);
        assert_eq!(score(&ridah, &tupac("Greatest Hits", false)), 0.0);
        assert_eq!(
            score(
                &ridah,
                &result("Khat", &["Navjot Ahuja"], "Khat", 134.0, true)
            ),
            0.0
        );
        // ISRC filler of the same length and edit is not picked, whether or not text hits are
        // filtered.
        let filler = result("Khat", &["Navjot Ahuja"], "Khat", 134.0, true);
        let picks = |hit: Hit| isrc_picks(&ridah, vec![hit]).count();
        assert_eq!(
            (
                picks(filler),
                picks(tupac("Tupac: Live At The House Of Blues", true))
            ),
            (0, 1)
        );
        // The ISRC hit of Tyler Ward's cover is on a "tribute" album; a text hit on such an album
        // is still a cover.
        let scientist = track(
            "The Scientist",
            &["Tyler Ward", "Lindsey Stirling", "Kina Grannis"],
            "Covers",
            276.6,
        );
        let british = |isrc| {
            result(
                "The Scientist",
                &["Tyler Ward"],
                "Best of British (tribute to Coldplay)",
                277.0,
                isrc,
            )
        };
        assert!(score(&scientist, &british(true)) > 0.0);
        assert_eq!(score(&scientist, &british(false)), 0.0);
        // Spotify lists the BBC presenter first, YouTube Music only the Beatle.
        let note = |title: &str, date| {
            format!("{title} - Live At The BBC For \"Pop Go The Beatles\" / {date}, 1963")
        };
        let hit = |title: &str, date, artists, duration| {
            let name =
                format!("{title} (Live At The BBC For \"Pop Go The Beatles\" / {date}, 1963)");
            result(
                &name,
                artists,
                "On Air: Live at the BBC, Vol.2",
                duration,
                false,
            )
        };
        let paul = track(
            &note("Hey Paul….", "25th June"),
            &["Lee Peters", "Paul McCartney"],
            "On Air",
            21.1,
        );
        assert!(
            score(
                &paul,
                &hit("Hey Paul….", "25th June", &["Paul McCartney"], 22.0)
            ) > 0.0
        );
        // The long shared note must not carry another song by the presenter.
        let lower = track(
            &note("Lower 5E", "10th September"),
            &["Rodney Burke", "Paul McCartney"],
            "On Air",
            23.3,
        );
        assert!(
            score(
                &lower,
                &hit("Lower 5E", "10th September", &["Paul McCartney"], 24.0)
            ) > 0.0
        );
        assert_eq!(
            score(
                &lower,
                &hit(
                    "And Here We Are Again",
                    "23rd July",
                    &["Rodney Burke"],
                    16.0
                )
            ),
            0.0
        );
        // A title with featured artists before its version note keeps its official track, from the
        // ISRC search or from the text search, where YouTube Music writes the version in brackets
        // and orders the features otherwise.
        let story = track(
            "Hustler's Story (feat. Scarface, Akon & Big Gee) - Instrumental",
            &["The Notorious B.I.G.", "Scarface", "Akon", "Big Gee"],
            "Duets: The Final Chapter (20th Anniversary Edition)",
            249.0,
        );
        let atv = |isrc| {
            let name = "Hustler's Story [Instrumental] (feat. Akon, Big Gee & Scarface)";
            result(
                name,
                &["The Notorious B.I.G."],
                "Duets: The Final Chapter",
                249.2,
                isrc,
            )
        };
        assert!(score(&story, &atv(true)) > 0.0);
        assert!(score(&story, &atv(false)) > 0.0);
        let praise = track(
            "Praise The Lord (Da Shine) (feat. Skepta) - Durdenhauer Edit",
            &["A$AP Rocky", "Skepta"],
            "",
            200.0,
        );
        let edit = result(
            "Praise The Lord (Da Shine) [Durdenhauer Edit] (feat. Skepta)",
            &["A$AP Rocky"],
            "",
            200.0,
            false,
        );
        assert!(score(&praise, &edit) > 0.0);
    }

    #[test]
    fn numbered_parts() {
        // An upload of "Side 4, Pt. 3: Surfacing" passed the name ratio for "Side 2, Pt. 2: Skins"
        // and was downloaded, while Pink Floyd's own video, titled only "Skins", failed it.
        let skins = track(
            "Side 2, Pt. 2: Skins",
            &["Pink Floyd"],
            "The Endless River",
            157.8,
        );
        let video = |name: &str, artist: &str, duration, kind| {
            let song = track(name, &[artist], "", duration);
            Hit {
                song,
                verified: false,
                isrc: false,
                kind,
                explicit: false,
            }
        };
        let surfacing = video(
            "Pink Floyd   Side 4, Pt  3 Surfacing",
            "Gabriele Cordovani",
            167.0,
            Kind::Other,
        );
        assert_eq!(score(&skins, &surfacing), 0.0);
        assert!(score(&skins, &video("Skins", "PINK FLOYD", 158.0, Kind::Omv)) > 80.0);
        assert!(
            score(
                &skins,
                &video(
                    "Pink Floyd - Side 2, Part II: Skins",
                    "Fan",
                    158.0,
                    Kind::Other
                )
            ) > 0.0
        );
        let wall = track(
            "Another Brick in the Wall, Pt. 2",
            &["Pink Floyd"],
            "The Wall",
            238.0,
        );
        assert!(
            score(
                &wall,
                &result(
                    "Another Brick in the Wall (Part 2)",
                    &["Pink Floyd"],
                    "",
                    238.0,
                    false
                )
            ) > 0.0
        );
        assert_eq!(
            score(
                &wall,
                &result(
                    "Another Brick in the Wall (Part 3)",
                    &["Pink Floyd"],
                    "",
                    238.0,
                    false
                )
            ),
            0.0
        );
        let love = track(
            "Can't Buy Me Love - Take 2 With Solo From Take 1",
            &["The Beatles"],
            "Anthology 1",
            151.0,
        );
        assert!(
            score(
                &love,
                &result(
                    "Can't Buy Me Love (Take 2)",
                    &["The Beatles"],
                    "",
                    151.0,
                    false
                )
            ) > 0.0
        );
        assert_eq!(
            score(
                &love,
                &result(
                    "Can't Buy Me Love (Take 1)",
                    &["The Beatles"],
                    "",
                    151.0,
                    false
                )
            ),
            0.0
        );
    }
}
