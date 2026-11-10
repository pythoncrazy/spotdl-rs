//! Seeded fuzzing helpers, for tests only: hostile text, API-shaped JSON and songs, and a runner
//! that names the seed of a failing input. `FUZZ_ITERS` (default 3000) sets the inputs per test and
//! `FUZZ_SEED` the first seed: `FUZZ_ITERS=2000000 cargo test --release fuzz` runs for minutes.

use crate::Song;
use serde_json::{json, Value};

pub struct Rng(u64);

impl Rng {
    pub fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as usize % n.max(1)
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// Pieces that the parsers treat specially, and the kind of text that has broken tools before:
/// multi-byte and combining characters, no-break spaces, bidi marks, NUL and other controls.
const PIECES: &[&str] = &[
    "",
    " ",
    "  ",
    "\n",
    "\r\n",
    "\t",
    "\0",
    "\u{7f}",
    "\u{200b}",
    "\u{202e}",
    "\u{a0}",
    "é",
    "e\u{301}",
    "ß",
    "İ",
    "ǅ",
    "日本語",
    "🎵",
    "👨‍👩‍👧",
    "\u{10ffff}",
    "'",
    "’",
    "\"",
    "&",
    "&amp;",
    "&lt;",
    "&#",
    "&#x",
    "&#x41;",
    "&#0;",
    "&#1114112;",
    "&;",
    ";",
    "<",
    ">",
    "<br/>",
    "<div>",
    "</div>",
    "<div data-exclude-from-selection=\"true\">",
    "<div",
    "data-lyrics-container=\"true\"",
    "[",
    "]",
    "[]",
    "(",
    ")",
    "()",
    "{",
    "}",
    "{title}",
    "{artists}",
    "{album}",
    "{output-ext}",
    "{list[0]}",
    "{",
    "/",
    "\\",
    "..",
    ".",
    "...",
    "../",
    ":",
    "*",
    "?",
    "%",
    "|",
    " - ",
    " (",
    " [",
    "(feat. X)",
    "(Live)",
    "[Chorus]",
    "[00:01.00]",
    "[0:1]",
    "[99:99.99]",
    "[nan:00]",
    "[inf:00]",
    "[-1:00]",
    "[1e999:00]",
    "[00:",
    "Pt. 2",
    "Part II",
    "Side 2",
    ": ",
    "Instrumental",
    "Karaoke",
    "Backing Track",
    "Remix",
    "Remastered 2011",
    "2017",
    "You might also like",
    "See ",
    "Get tickets as low as $1",
    "hz",
    "432hz",
    "0",
    "-1",
    "1e999",
    "NaN",
    "inf",
    "9999999999999999999999",
    "a",
    "Z",
    "the",
    "I",
    "love",
    "you",
];

/// Text from 0 to `max` pieces and loose characters.
pub fn text(rng: &mut Rng, max: usize) -> String {
    let mut out = String::new();
    for _ in 0..rng.below(max + 1) {
        match rng.below(8) {
            0 => out.push(char::from_u32(rng.below(0x11_0000) as u32).unwrap_or('?')),
            1 => out.push(char::from(rng.below(128) as u8)),
            _ => out.push_str(rng.pick(PIECES)),
        }
    }
    out
}

/// Lyrics: lines of text, often with a timestamp in front.
pub fn lyrics(rng: &mut Rng) -> String {
    let line = |rng: &mut Rng| {
        let stamp = if rng.below(2) == 0 {
            format!(
                "[{:02}:{:02}.{:02}]",
                rng.below(8),
                rng.below(60),
                rng.below(100)
            )
        } else {
            String::new()
        };
        stamp + &text(rng, 6)
    };
    (0..rng.below(12))
        .map(|_| line(rng))
        .collect::<Vec<_>>()
        .join("\n")
}

const KEYS: &[&str] = &[
    "data",
    "items",
    "uri",
    "name",
    "profile",
    "artists",
    "album",
    "discs",
    "number",
    "tracks",
    "totalCount",
    "tracksV2",
    "duration",
    "trackDuration",
    "totalMilliseconds",
    "isoString",
    "precision",
    "coverArt",
    "sources",
    "url",
    "track",
    "releases",
    "date",
    "copyright",
    "genres",
    "response",
    "sections",
    "hits",
    "result",
    "id",
    "title",
    "primary_artist",
    "release_date_components",
    "year",
    "month",
    "day",
    "contents",
    "tabbedSearchResultsRenderer",
    "tabs",
    "runs",
    "text",
    "flexColumns",
    "musicResponsiveListItemFlexColumnRenderer",
    "syncedLyrics",
    "plainLyrics",
    "instrumental",
    "albumName",
    "trackName",
    "artistName",
    "entries",
    "uploader",
    "view_count",
    "record_type",
    "nb_tracks",
    "explicit_lyrics",
];

/// JSON like the web players' and lyrics sites' replies, with the keys the code reads, in the wrong
/// places and with the wrong types: a changed or half-empty reply must fail a song, not abort the
/// run.
pub fn json(rng: &mut Rng, depth: usize) -> Value {
    match rng.below(if depth == 0 { 7 } else { 10 }) {
        0 => Value::Null,
        1 => json!(rng.below(3) == 0),
        2 => json!(rng.below(5000) as i64 - 10),
        3 => json!(rng.below(100_000) as f64 / 7.0),
        4 => json!(u64::MAX - rng.below(3) as u64),
        5 => json!(text(rng, 4)),
        6 => json!(-1.5e300),
        7 | 8 => Value::Object(
            (0..rng.below(5))
                .map(|_| (rng.pick(KEYS).to_string(), json(rng, depth - 1)))
                .collect(),
        ),
        _ => Value::Array((0..rng.below(4)).map(|_| json(rng, depth - 1)).collect()),
    }
}

pub fn song(rng: &mut Rng) -> Song {
    let artists: Vec<String> = (0..rng.below(4)).map(|_| text(rng, 3)).collect();
    Song {
        name: text(rng, 6),
        artist: artists.first().cloned().unwrap_or_default(),
        artists,
        album_name: text(rng, 4),
        album_artist: text(rng, 2),
        duration: [0.0, 1.0, 215.5, f64::NAN, f64::INFINITY, -3.0][rng.below(6)],
        track_number: rng.below(100) as u32,
        disc_number: rng.below(4) as u32,
        disc_count: rng.below(4) as u32,
        explicit: rng.below(2) == 0,
        date: text(rng, 2),
        lyrics: (rng.below(2) == 0).then(|| lyrics(rng)),
        ..Song::default()
    }
}

/// Runs `test` on `FUZZ_ITERS` inputs, each from its own seed, and says which seed failed.
pub fn runs(name: &str, test: impl Fn(&mut Rng)) {
    let var = |key: &str, default: u64| {
        std::env::var(key)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    crate::settings::use_defaults();
    let first = var("FUZZ_SEED", 1);
    for seed in first..first + var("FUZZ_ITERS", 3000) {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        if let Err(panic) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(&mut rng)))
        {
            eprintln!("fuzz {name}: FUZZ_SEED={seed} FUZZ_ITERS=1 reproduces it");
            std::panic::resume_unwind(panic);
        }
    }
}
