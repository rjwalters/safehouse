//! Shared exponential-backoff formula used by both retry loops in the daemon:
//! the egress publisher's sink-retry path (`egress.rs`) and the sync loop's
//! reconnect path (`main.rs`). Both want the same shape — 2s base, 60s cap —
//! so this lives in one place rather than as two independently-maintained
//! copies (#146).

/// Exponential backoff (seconds) before the `attempt`-th consecutive retry,
/// capped so a long outage doesn't push the delay absurdly far out.
/// `attempt` is clamped into `1..=6` before the formula is applied, so
/// callers may pass `0` or values beyond `6` without special-casing.
pub(crate) fn exponential_backoff_secs(attempt: u32) -> u64 {
    const BASE_SECS: u64 = 2;
    const CAP_SECS: u64 = 60;
    BASE_SECS.saturating_pow(attempt.clamp(1, 6)).min(CAP_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_monotonic_and_capped() {
        // Increasing, then pinned at the 60s cap — never unbounded.
        let seq: Vec<u64> = (1..=8).map(exponential_backoff_secs).collect();
        assert_eq!(seq, vec![2, 4, 8, 16, 32, 60, 60, 60]);
        for w in seq.windows(2) {
            assert!(w[1] >= w[0], "backoff must be non-decreasing");
        }
        assert!(
            seq.iter().all(|&s| s <= 60),
            "backoff must be capped so a long outage can't push it out unboundedly"
        );
    }

    #[test]
    fn clamps_out_of_range_attempts() {
        // Below the clamp floor and above the clamp ceiling both land on the
        // in-range endpoint's value rather than under/overflowing.
        assert_eq!(exponential_backoff_secs(0), exponential_backoff_secs(1));
        assert_eq!(exponential_backoff_secs(100), exponential_backoff_secs(6));
    }
}
