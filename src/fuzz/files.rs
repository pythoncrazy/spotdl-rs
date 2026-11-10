//! Fuzz tests for the output paths: whatever a song is called, the file stays in its folder.

use super::*;
use crate::gen::{runs, song, text};
use std::path::Component;

#[test]
fn path_parts_are_safe_and_the_template_cannot_leave_its_folder() {
    runs("template_path", |rng| {
        let s = song(rng);
        assert!(path_safe(&s.name)
            .chars()
            .all(|c| !"/\\?%*:|\"<>".contains(c) && !c.is_control()));
        let file = template_path(
            "{album-artist}/{album}/{artists} - {title}.{output-ext}",
            &s,
        );
        for part in file.components() {
            assert!(matches!(part, Component::Normal(_)), "{file:?} for {s:?}");
            assert!(part.as_os_str().len() <= 200 + ".opus".len(), "{file:?}");
        }
        assert!(
            file.extension().is_some_and(|ext| ext == "opus"),
            "{file:?}"
        );
        let hostile = template_path(&text(rng, 6), &s);
        assert!(hostile.as_os_str().len() < 4096);
        let _ = (fill(&text(rng, 6), &s), output_root());
    });
}
