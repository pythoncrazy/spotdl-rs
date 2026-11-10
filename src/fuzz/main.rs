//! Fuzz tests for the command line and the playlist code of `main.rs`.

use super::*;
use crate::gen::{runs, song, text};

#[test]
fn any_command_line_gives_settings_or_an_error() {
    const OPTIONS: &[&str] = &[
        "--format",
        "--output",
        "--threads",
        "--bitrate",
        "--lyrics",
        "--overwrite",
        "--ignore-albums",
        "--m3u",
        "--archive",
        "--restrict",
        "--album-type",
        "--max-filename-length",
        "--generate-lrc",
        "--yt-dlp-args",
        "--no-such-option",
        "--",
        "--=",
        "--format=",
        "-",
        "",
    ];
    runs("settings", |rng| {
        let args: Vec<String> = (0..rng.below(8))
            .map(|_| if rng.below(2) == 0 { rng.pick(OPTIONS).to_string() } else { text(rng, 3) })
            .filter(|arg| !arg.starts_with("--config") && !arg.contains("load"))
            .collect();
        let _ = settings(&args);
    });
}

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
