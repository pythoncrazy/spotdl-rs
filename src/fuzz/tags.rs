//! Fuzz tests for the tags of damaged audio files: they fail or work, and never panic.

use super::*;
use crate::gen::{runs, song};
use std::{fs, process::Command};

/// One second of audio in each format that `meta` reads, made with FFmpeg; empty without it.
fn sample_files(dir: &Path) -> Vec<Vec<u8>> {
    ["opus", "mp3", "m4a", "flac", "ogg", "wav"]
        .iter()
        .filter_map(|ext| {
            let file = dir.join(format!("sample.{ext}"));
            let made = Command::new("ffmpeg")
                .args(["-v", "quiet", "-y", "-f", "lavfi", "-i", "sine=duration=1", "-metadata", "title=Sample"])
                .arg(&file)
                .status();
            made.ok()?.success().then(|| fs::read(&file).ok()).flatten()
        })
        .collect()
}

/// Flips, drops, repeats and adds bytes, and cuts the end off, as a bad download or a failing disk does.
fn damaged(rng: &mut crate::gen::Rng, original: &[u8]) -> Vec<u8> {
    let mut bytes = original.to_vec();
    for _ in 0..1 + rng.below(6) {
        let at = rng.below(bytes.len().max(1));
        match rng.below(5) {
            0 => bytes.truncate(at),
            1 if at < bytes.len() => bytes[at] ^= 1 << rng.below(8),
            2 if at < bytes.len() => drop(bytes.remove(at)),
            3 => bytes.insert(at.min(bytes.len()), rng.below(256) as u8),
            _ if at + 64 < bytes.len() => bytes.copy_within(at..at + 32, at + 32),
            _ => {}
        }
    }
    bytes
}

#[test]
fn damaged_audio_files_fail_or_work_but_never_panic() {
    let dir = std::env::temp_dir().join(format!("spotdl-fuzz-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let samples = sample_files(&dir);
    runs("files", |rng| {
        if samples.is_empty() {
            return;
        }
        let original = rng.pick(&samples);
        let file = dir.join(format!("damaged.{}", ["opus", "mp3", "m4a", "flac", "ogg", "wav"][rng.below(6)]));
        fs::write(&file, damaged(rng, original)).unwrap();
        let s = song(rng);
        let _ = (write_tags(&file, &s), write_lyrics(&file, s.lyrics.clone()), write_lyrics(&file, None));
        if let Ok(audio) = audio(&file) {
            let _ = audio.primary_tag().map(source_url);
        }
    });
    fs::remove_dir_all(&dir).unwrap();
}

/// `lofty` 0.22 panics on an Opus header with no channels ("Channel count is valid"), which aborted a whole run.
#[test]
fn an_opus_file_without_channels_fails_the_song_instead_of_ending_the_run() {
    let dir = std::env::temp_dir().join(format!("spotdl-opus-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let Some(mut bytes) = sample_files(&dir).into_iter().next() else {
        return;
    };
    let head = bytes.windows(8).position(|window| window == b"OpusHead").unwrap();
    bytes[head + 9] = 0;
    let file = dir.join("no-channels.opus");
    fs::write(&file, bytes).unwrap();
    let error = audio(&file).err().expect("a file without channels is damaged").to_string();
    assert!(error.contains("damaged") || error.contains("hannel"), "{error}");
    fs::remove_dir_all(&dir).unwrap();
}
