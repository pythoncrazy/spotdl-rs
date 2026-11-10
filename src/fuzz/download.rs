//! Fuzz tests for the text, path and lyrics code of `download.rs`: nothing may panic, and the properties that the
//! rest of the program relies on must hold for any input. See `gen.rs` for how to run longer.

use super::*;
use crate::gen::{json, lyrics, runs, song, text};
use std::{fs, path::Component, process::Command};

#[test]
fn path_parts_are_safe_and_the_template_cannot_leave_its_folder() {
    runs("template_path", |rng| {
        let s = song(rng);
        assert!(path_safe(&s.name).chars().all(|c| !"/\\?%*:|\"<>".contains(c) && !c.is_control()));
        let file = template_path("{album-artist}/{album}/{artists} - {title}.{output-ext}", &s);
        for part in file.components() {
            assert!(matches!(part, Component::Normal(_)), "{file:?} for {s:?}");
            assert!(part.as_os_str().len() <= 200 + ".opus".len(), "{file:?}");
        }
        assert!(file.extension().is_some_and(|ext| ext == "opus"), "{file:?}");
        let hostile = template_path(&text(rng, 6), &s);
        assert!(hostile.as_os_str().len() < 4096);
        let _ = (fill(&text(rng, 6), &s), output_root());
    });
}

#[test]
fn page_parsers_never_panic_and_entities_only_shrink_text() {
    runs("html", |rng| {
        let page = text(rng, 20);
        assert!(entities(&page).len() <= page.len());
        let _ = (html(&page, &text(rng, 2), &text(rng, 2)), genius_lyrics(&page), azlyrics(&page));
        let _ = genius_lyrics(&format!("<div data-lyrics-container=\"true\">{page}</div>"));
    });
}

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
        let _ = (lyrics_title(&s), plain_title(&s.name), instrumental_title(&s.name), among(&s.name, &s.album_name));
        let _ = (genius_account(&s.name, &s.album_name), rate_args(&text(rng, 2), &text(rng, 1), s.duration.into()));
        let _ = temp_id(&s);
    });
}

#[test]
fn lrclib_replies_of_any_shape_give_lyrics_or_nothing() {
    runs("lrclib", |rng| {
        let s = song(rng);
        let tracks: Vec<_> = (0..rng.below(4)).map(|_| json(rng, 3)).collect();
        for lyrics in lrclib_lyrics(&s, &tracks) {
            assert!(lyrics.is_empty() || !lyrics.trim().is_empty());
        }
        let _ = synced_first(vec![lyrics(rng), lyrics(rng)], |lyrics| lyrics.len() % 2 == 0);
    });
}

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
    let Some(mut bytes) = sample_files(&dir).into_iter().next() else { return };
    let head = bytes.windows(8).position(|window| window == b"OpusHead").unwrap();
    bytes[head + 9] = 0;
    let file = dir.join("no-channels.opus");
    fs::write(&file, bytes).unwrap();
    let error = audio(&file).err().expect("a file without channels is damaged").to_string();
    assert!(error.contains("damaged") || error.contains("hannel"), "{error}");
    fs::remove_dir_all(&dir).unwrap();
}
