//! Fuzz tests for the reply and title code of `spotify.rs`: replies of any shape must give a value or nothing, never a
//! panic, since one panic aborts a whole run.

use super::*;
use crate::gen::{json, runs, song, text};

#[test]
fn replies_of_any_shape_never_panic() {
    runs("replies", |rng| {
        let reply = json(rng, 5);
        let _ = (release_date(&reply), cover(&reply), complete(&reply), Track::parse(&reply));
        let _ = Album::parse(&reply).disc_tracks(rng.below(5) as u64);
        // What is kept of a reply is read back as it was written.
        let kept = serde_json::to_value((Album::parse(&reply), [Track::parse(&reply)])).unwrap();
        let (album, _): (Album, Vec<Track>) = serde_json::from_value(kept.clone()).unwrap();
        assert_eq!(serde_json::to_value(album).unwrap(), kept[0]);
    });
}

#[test]
fn titles_and_keys_never_panic() {
    runs("titles", |rng| {
        let title = text(rng, 8);
        let _ = (song_key(&title), base_title(&title), totp(title.as_bytes()));
        let base = base_title(&title);
        assert!(title.starts_with(base), "{title:?}");
        let albums: Vec<Vec<Song>> =
            (0..rng.below(4)).map(|_| (0..rng.below(4)).map(|_| song(rng)).collect()).collect();
        let names: Vec<String> = albums.iter().map(|_| text(rng, 4)).collect();
        let picked = editions(&names.iter().map(String::as_str).collect::<Vec<_>>(), &albums);
        assert!(picked.iter().all(|&i| i < albums.len()));
    });
}
