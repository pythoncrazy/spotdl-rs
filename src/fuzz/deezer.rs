//! Fuzz tests for the reply code of `deezer.rs`: replies of any shape must give a value or nothing, never a panic.

use super::*;
use crate::gen::{json, runs, text};

#[test]
fn replies_of_any_shape_never_panic() {
    runs("replies", |rng| {
        let (reply, other) = (json(rng, 5), json(rng, 5));
        let _ =
            (deezer_error(&reply), genres(&reply), dates(&text(rng, 3), &text(rng, 3)), version_note(&text(rng, 8)));
        let hits: Vec<Value> = (0..rng.below(4)).map(|_| json(rng, 3)).collect();
        let _ = ranked(&hits, &text(rng, 4), &text(rng, 1), rng.below(30) as u64, rng.below(2) == 0);
        let lengths: HashMap<String, f64> = (0..rng.below(4)).map(|i| (i.to_string(), rng.below(300) as f64)).collect();
        // A cached album of any shape is read or fetched again.
        if let Ok(Some(album)) = serde_json::from_value::<Option<Album>>(other) {
            let _ = agrees(&lengths, &album.tracks);
        }
    });
}
