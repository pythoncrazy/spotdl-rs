//! Fuzz tests for the title matching of `search.rs`: nothing may panic, and the scores and ratios stay in range.

use super::*;
use crate::gen::{json, runs, song, text};

#[test]
fn title_matching_holds_its_properties_for_any_title() {
    runs("titles", |rng| {
        let (a, b) = (text(rng, 8), text(rng, 8));
        let (similar, back) = (ratio(&a, &b), ratio(&b, &a));
        assert!((0.0..=100.0).contains(&similar) && (similar - back).abs() < 1e-9, "{a:?} {b:?}");
        assert_eq!(ratio(&a, &a), 100.0, "{a:?}");
        assert!(slug(&a, "-").bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'), "{a:?}");
        let artists = [text(rng, 2), text(rng, 2)];
        let _ = (clean_name(&a, &artists), versionless(&a), edition_title(&a), markers(&a), unnumbered(&a));
        let _ = (is_note(&a), is_version_suffix(&a), altered(&a));
    });
}

#[test]
fn scores_are_finite_and_never_negative() {
    runs("score", |rng| {
        let (wanted, found) = (song(rng), song(rng));
        let kind = *rng.pick(&[Kind::Atv, Kind::Omv, Kind::Other]);
        let hit = Hit {
            song: found,
            verified: rng.below(2) == 0,
            isrc: rng.below(2) == 0,
            kind,
            explicit: rng.below(2) == 0,
        };
        let score = score(&wanted, &hit);
        assert!(score.is_finite() && score >= 0.0, "{score} for {:?} and {:?}", wanted.name, hit.song.name);
        let _ = isrc_picks(&wanted, vec![hit]).count();
    });
}

#[test]
fn youtube_replies_of_any_shape_give_hits_or_nothing() {
    runs("ytm", |rng| {
        let reply = json(rng, 5);
        let _ = ytm_hits(&reply, rng.below(2) == 0, rng.below(2) == 0);
    });
}
