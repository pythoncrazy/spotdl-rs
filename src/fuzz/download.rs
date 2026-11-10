//! Fuzz tests for the download helpers: any title or bitrate gives a name or arguments, never a panic.

use super::*;
use crate::gen::{runs, song, text};

#[test]
fn file_names_and_encoder_arguments_never_panic() {
    runs("names", |rng| {
        let s = song(rng);
        let _ = (temp_id(&s), rate_args(&text(rng, 2), &text(rng, 1), s.duration.into()));
    });
}
