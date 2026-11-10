//! Fuzz tests for the retry pauses of `http.rs`.

use super::*;
use crate::gen::runs;

#[test]
fn retry_pauses_stay_capped_for_any_attempt() {
    runs("backoff", |rng| {
        let attempt = [0, 1, 5, 63, 64, u64::MAX][rng.below(6)];
        let after = [None, Some(0), Some(7), Some(u64::MAX)][rng.below(4)];
        assert!(backoff(attempt, after) <= Duration::from_secs(60));
    });
}
