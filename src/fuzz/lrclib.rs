//! Fuzz tests for LRCLIB's replies: of any shape they give lyrics or nothing.

use super::*;
use crate::gen::{json, lyrics, runs, song};

#[test]
fn lrclib_replies_of_any_shape_give_lyrics_or_nothing() {
    runs("lrclib", |rng| {
        let s = song(rng);
        let reply: Vec<_> = (0..rng.below(4)).map(|_| json(rng, 3)).collect();
        let versions = lrclib_lyrics(&s, &uploads(reply));
        assert!(versions.lyrics.iter().all(|lyrics| !lyrics.trim().is_empty()));
        let _ = versions.pick(Some(&lyrics(rng)));
    });
}
