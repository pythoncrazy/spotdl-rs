//! Fuzz tests for the reply and title code of `spotify.rs`: replies of any shape must give a value or nothing, never
//! a panic, since one panic aborts a whole run.

use super::*;
use crate::gen::{json, runs, song, text};

#[test]
fn replies_of_any_shape_never_panic() {
    runs("replies", |rng| {
        let (reply, other) = (json(rng, 5), json(rng, 5));
        let _ = (release_date(&reply), cover(&reply), disc_tracks(&reply, rng.below(5) as u64), deezer_error(&reply));
        let _ = (genres(&reply), complete(&reply), dates(&text(rng, 3), &text(rng, 3)));
        let hits: Vec<Value> = (0..rng.below(4)).map(|_| json(rng, 3)).collect();
        let _ = ranked(&hits, &text(rng, 4), &text(rng, 1), rng.below(30) as u64, rng.below(2) == 0);
        let lengths: HashMap<String, f64> = (0..rng.below(4)).map(|i| (i.to_string(), rng.below(300) as f64)).collect();
        if let Value::Object(tracks) = other {
            let _ = agrees(&lengths, &tracks);
        }
    });
}

#[test]
fn titles_keys_and_dates_never_panic() {
    runs("titles", |rng| {
        let title = text(rng, 8);
        let _ = (song_key(&title), base_title(&title), version_note(&title), totp(title.as_bytes()));
        let base = base_title(&title);
        assert!(title.starts_with(base), "{title:?}");
        let _ = dates(&text(rng, 2), &text(rng, 2));
        let albums: Vec<Vec<Song>> =
            (0..rng.below(4)).map(|_| (0..rng.below(4)).map(|_| song(rng)).collect()).collect();
        let names: Vec<String> = albums.iter().map(|_| text(rng, 4)).collect();
        let picked = editions(&names.iter().map(String::as_str).collect::<Vec<_>>(), &albums);
        assert!(picked.iter().all(|&i| i < albums.len()));
    });
}
