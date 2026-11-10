//! Fuzz tests for the lyrics text code, which must hold its properties for any lyrics and title.

use super::*;
use crate::gen::{lyrics, runs, song};

#[test]
fn lyrics_helpers_hold_their_properties_for_any_lyrics() {
    runs("lyrics", |rng| {
        let (a, b) = (lyrics(rng), lyrics(rng));
        let plain = plain_lyrics(&a);
        assert!(!is_synced(&plain), "{a:?} -> {plain:?}");
        // A stray carriage return and a last empty line are not lyrics: only the words must settle.
        let flat = |text: &str| text.replace('\r', "").trim_end().to_string();
        assert_eq!(flat(&plain_lyrics(&plain)), flat(&plain), "{a:?}");
        assert!(same_lyrics(&a, &a), "{a:?}");
        assert_eq!(same_lyrics(&a, &b), same_lyrics(&b, &a), "{a:?} {b:?}");
        let length = rng.pick(&[0.0, 90.0, 215.0, f64::NAN, f64::INFINITY]).to_owned();
        let _ = (timed_for(&a, length), last_time(&a), words(&a), without_ads(&a), unlisted(&a));
        assert!(last_time(&a).is_none_or(|last| last.is_finite() && last >= 0.0), "{a:?}");
        // More time never makes lyrics fit worse.
        assert!(!timed_for(&a, 100.0) || timed_for(&a, 200.0), "{a:?}");
    });
}

#[test]
fn title_helpers_never_panic() {
    runs("titles", |rng| {
        let s = song(rng);
        let _ = (lyrics_title(&s), plain_title(&s.name), instrumental_title(&s.name));
    });
}
