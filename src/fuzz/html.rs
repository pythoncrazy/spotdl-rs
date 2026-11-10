//! Fuzz tests for the page parsers: nothing may panic, and decoded entities only shrink the text.

use super::*;
use crate::gen::{runs, text};
use crate::genius::genius_lyrics;
use crate::lyrics::azlyrics;

#[test]
fn page_parsers_never_panic_and_entities_only_shrink_text() {
    runs("html", |rng| {
        let page = text(rng, 20);
        assert!(entities(&page).len() <= page.len());
        let _ = (html(&page, &text(rng, 2), &text(rng, 2)), genius_lyrics(&page), azlyrics(&page));
        let _ = genius_lyrics(&format!("<div data-lyrics-container=\"true\">{page}</div>"));
    });
}
