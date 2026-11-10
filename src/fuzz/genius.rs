//! Fuzz tests for the artist matching of Genius.

use super::*;
use crate::gen::{runs, song};

#[test]
fn artist_matching_never_panics() {
    runs("artists", |rng| {
        let s = song(rng);
        let _ = (among(&s.name, &s.album_name), genius_account(&s.name, &s.album_name));
    });
}
