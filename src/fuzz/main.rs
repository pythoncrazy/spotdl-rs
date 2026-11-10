//! Fuzz tests for the command line and the playlist code of `main.rs`.

use super::*;
use crate::gen::{runs, song, text};

#[test]
fn arranging_songs_never_panics_or_adds_songs() {
    runs("arrange", |rng| {
        let songs: Vec<Song> = (0..rng.below(5))
            .map(|_| {
                let mut song = song(rng);
                song.list_name = (rng.below(2) == 0).then(|| text(rng, 3));
                song.list_position = (rng.below(2) == 0).then(|| rng.below(1000));
                song.list_length = (rng.below(2) == 0).then(|| usize::MAX - rng.below(2));
                song
            })
            .collect();
        assert!(arrange(songs.clone()).len() <= songs.len());
    });
}
