//! Fuzz tests for the command line: any arguments give settings or an error.

use super::*;
use crate::gen::{runs, text};

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
            .map(|_| {
                if rng.below(2) == 0 {
                    rng.pick(OPTIONS).to_string()
                } else {
                    text(rng, 3)
                }
            })
            .filter(|arg| !arg.starts_with("--config") && !arg.contains("load"))
            .collect();
        let _ = load(&args);
    });
}
