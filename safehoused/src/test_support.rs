//! Shared `#[cfg(test)]`-only helpers used by more than one module's test
//! suite. Consolidated here so the collision-avoidance rationale below lives
//! in exactly one place instead of drifting between per-module copies (#105).

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// A unique scratch directory under the OS temp dir, cleaned up by the
/// caller. Avoids pulling in a `tempfile` dependency for a handful of tests.
///
/// pid + wall-clock nanos alone are not sufficient uniqueness under parallel
/// test execution: macOS/APFS's `SystemTime::now()` granularity is coarser
/// than 1ns, so two tests starting in the same burst can observe identical
/// nanos and collide on the same directory name. `create_dir_all` succeeds
/// silently on an existing dir, so the tests then share one on-disk file —
/// and whichever test finishes (and runs its trailing `remove_dir_all`)
/// first deletes the file out from under the other test's still-open
/// connection, which SQLite reports as `SQLITE_READONLY_DBMOVED` (#55). A
/// process-wide atomic counter makes each call unique regardless of clock
/// resolution.
///
/// `prefix` distinguishes callers' scratch directories (e.g.
/// `"safehoused-mailbox-test"` / `"safehoused-egress-test"`) so directories
/// from different test suites remain easy to tell apart on disk.
pub(crate) fn tempdir(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        seq
    ));
    assert!(
        !dir.exists(),
        "tempdir collision: {dir:?} already exists (uniqueness invariant violated)"
    );
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
