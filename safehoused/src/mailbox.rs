//! Per-persona mailbox (D16/D17) — the pull-model delivery primitive.
//!
//! `safehoused` never spawns, wakes, or push-notifies an agent (D16). Instead,
//! for each registered persona it keeps a durable mailbox: the envelopes
//! addressed to that persona (`to: <persona>` or `to: "*"`), populated from
//! the same synced room timeline that already drives live dispatch
//! (`on_message` in `main.rs`). An agent calls the `check` RPC op (surfaced as
//! the `safehouse_check` MCP tool) on its own cadence and gets exactly what it
//! missed, whether or not it was ever connected.
//!
//! **This is a derived view, not a second source of truth (D6).** Every row
//! here is reconstructible from the room: if this database were deleted, a
//! fresh mailbox would repopulate correctly as the daemon resyncs (matrix-sdk
//! persists its own sync position, so a restart replays exactly the events
//! the daemon hadn't processed yet — see `main.rs`'s boot sequence). The one
//! genuinely new piece of local state is the read cursor: what a given
//! persona has already consumed. That's what must survive a restart, and
//! that's what's persisted here (sqlite, in `state_dir`, per D17).
//!
//! **Storage is bounded, not unbounded (#60).** Being a derived/rebuildable
//! view (above) means it is always safe for `safehoused` to be *lossy* about
//! what it retains locally — the room, not the mailbox, is the durable
//! record. Two policies keep a persona's mailbox from growing without bound
//! when nothing ever consumes it:
//!
//! 1. **Ephemeral coordination broadcasts are never persisted per-persona**
//!    (see [`is_ephemeral_body`]). A `to: "*"` broadcast whose `body` is a
//!    JSON object carrying the [`EPHEMERAL_BODY_MARKER`] key is a live
//!    coordination heartbeat (e.g. loom-daemon's cross-host claim
//!    advertisement, re-published every ~30s per in-flight issue) — a stale
//!    copy has negative value to an agent that checks in after the fact, so
//!    it is dropped at `deliver()` rather than fanned out into every local
//!    persona's mailbox. Live delivery to a *connected* socket
//!    (`Registry::dispatch`) is unaffected — this only changes what gets
//!    durably stored for later pickup.
//! 2. **Per-persona retention is capped** (see [`gc_persona`]): every
//!    `deliver()` also (a) drops rows the persona has already consumed
//!    (`seq <= cursor` — dead weight, `check` never returns them again
//!    regardless of retention) and (b) caps the remaining unconsumed backlog
//!    at [`MAX_UNCONSUMED_PER_PERSONA`], dropping the oldest excess. A
//!    persona with a live, regularly-advancing cursor never approaches the
//!    cap in practice, so this only bites a mailbox nobody has ever consumed
//!    from.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;

use crate::envelope::Envelope;

/// JSON object key that marks a `body` payload as an ephemeral coordination
/// broadcast rather than agent-facing content — e.g. loom-daemon's
/// cross-host claim-advertisement heartbeat (`peer_claims.rs`'s
/// `PEER_CLAIM_MARKER`), which rides a `to: "*"` `task` envelope and is
/// re-published on a short interval for as long as an issue is in flight. A
/// message matching this marker is never written into any persona's mailbox
/// (see [`is_ephemeral_body`]) — `safehoused` is a generic protocol daemon
/// and does not otherwise interpret `body`; this is a narrow, documented
/// storage-policy exception motivated by #60 (studio's mailbox held 208k+
/// rows, 92% of which were stale claim heartbeats with an empty `cursors`
/// table), not a protocol change.
const EPHEMERAL_BODY_MARKER: &str = "loom_claim";

/// Cap on how many *unconsumed* rows (`seq` greater than the persona's
/// cursor) a single persona's mailbox may hold. Chosen generously above any
/// plausible legitimate backlog for a persona that checks in at all, while
/// still bounding worst-case storage for one that never does (#60).
const MAX_UNCONSUMED_PER_PERSONA: i64 = 5_000;

/// True when `body` parses as a JSON object carrying [`EPHEMERAL_BODY_MARKER`]
/// as a top-level key. Anything that isn't valid JSON, or is JSON but not an
/// object, or is an object without the marker, is never ephemeral — plain
/// prose `body` (the overwhelming common case) fails the `serde_json::from_str`
/// parse immediately and this is a cheap no-op.
fn is_ephemeral_body(body: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(body),
        Ok(serde_json::Value::Object(map)) if map.contains_key(EPHEMERAL_BODY_MARKER)
    )
}

/// Bound `persona`'s mailbox in `conn`, run after every delivery (#60):
///
/// 1. Delete rows already covered by `persona`'s read cursor (`seq <=
///    cursor`) — `check` never returns them again regardless of retention,
///    so keeping them around is pure dead weight. A persona with no cursor
///    yet (never checked in) has nothing deleted by this step.
/// 2. Cap the remaining (unconsumed) rows at `cap`, deleting the oldest
///    excess first. A persona whose cursor advances promptly never
///    accumulates more than a handful of unconsumed rows, so this step is a
///    no-op for a live consumer — it only bites a mailbox nobody has ever
///    drained.
///
/// Any rows actually dropped are logged (`safehoused: mailbox GC …`) so the
/// bounded-retention behavior is observable in operation, not just inferred
/// from a static row count.
fn gc_persona(conn: &Connection, persona: &str, cap: i64) -> Result<()> {
    let cursor: i64 = conn
        .query_row(
            "SELECT seq FROM cursors WHERE persona = ?1",
            params![persona],
            |row| row.get(0),
        )
        .optional()
        .context("reading mailbox cursor for gc")?
        .unwrap_or(0);

    let consumed_dropped = conn
        .execute(
            "DELETE FROM messages WHERE persona = ?1 AND seq <= ?2",
            params![persona, cursor],
        )
        .context("dropping consumed mailbox rows")?;

    let capped_dropped = conn
        .execute(
            "DELETE FROM messages WHERE persona = ?1 AND seq NOT IN ( \
                 SELECT seq FROM messages WHERE persona = ?1 ORDER BY seq DESC LIMIT ?2 \
             )",
            params![persona, cap],
        )
        .context("capping unconsumed mailbox backlog")?;

    if consumed_dropped > 0 || capped_dropped > 0 {
        println!(
            "safehoused: mailbox GC for persona {persona:?}: dropped {consumed_dropped} \
             consumed row(s), {capped_dropped} over-cap row(s) (cap {cap})"
        );
    }
    Ok(())
}

/// One delivered-and-stored envelope, as returned by [`Mailbox::check`].
#[derive(Clone, Debug)]
pub struct MailboxEntry {
    pub room_id: String,
    pub event_id: String,
    /// The Matrix sender of the underlying event (may be a human, this
    /// daemon's own user, or a remote host's daemon user — see envelope-v1
    /// §6 on why this is surfaced alongside `envelope.from`).
    pub sender: String,
    pub envelope: Envelope,
}

pub struct Mailbox {
    conn: Mutex<Connection>,
}

impl Mailbox {
    /// Open (creating if needed) the durable mailbox store at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening mailbox store {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// An in-memory mailbox — only used by tests (`rpc.rs`'s included), which
    /// need a scratch mailbox with no durability requirement.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory mailbox store")?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS messages (
                seq       INTEGER PRIMARY KEY AUTOINCREMENT,
                persona   TEXT NOT NULL,
                room_id   TEXT NOT NULL,
                event_id  TEXT NOT NULL,
                sender    TEXT NOT NULL,
                envelope  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_persona_seq ON messages(persona, seq);
            CREATE TABLE IF NOT EXISTS cursors (
                persona TEXT PRIMARY KEY,
                seq     INTEGER NOT NULL
            );
            ",
        )
        .context("creating mailbox schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record one delivery of `env` into `persona`'s mailbox. Called once per
    /// (envelope, recipient persona) pair as the room stream is dispatched
    /// (mirrors the addressing rules in envelope-v1 §7); a broadcast fans out
    /// to one row per locally-hosted persona.
    ///
    /// A no-op when `env.body` matches [`is_ephemeral_body`] — see the module
    /// doc for why (#60). Otherwise inserts, then runs [`gc_persona`] to keep
    /// `persona`'s mailbox bounded.
    pub async fn deliver(
        &self,
        persona: &str,
        room_id: &str,
        event_id: &str,
        sender: &str,
        env: &Envelope,
    ) -> Result<()> {
        if is_ephemeral_body(&env.body) {
            return Ok(());
        }
        let payload = serde_json::to_string(env).context("serializing envelope for mailbox")?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO messages (persona, room_id, event_id, sender, envelope) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![persona, room_id, event_id, sender, payload],
        )
        .context("inserting mailbox row")?;
        gc_persona(&conn, persona, MAX_UNCONSUMED_PER_PERSONA)
            .context("garbage-collecting mailbox after delivery")?;
        Ok(())
    }

    /// Unread envelopes for `persona`, oldest first (so the caller can render
    /// "newest last", per the issue's `safehouse_check` spec). When `advance`
    /// is true the persona's read cursor moves past everything returned; a
    /// peek (`advance = false`) leaves the cursor untouched, so a repeated
    /// peek is idempotent. `limit`, when set, caps how many are returned —
    /// the cursor only ever advances to cover what was actually returned, so
    /// a limited check never skips unread mail.
    pub async fn check(
        &self,
        persona: &str,
        advance: bool,
        limit: Option<u32>,
    ) -> Result<Vec<MailboxEntry>> {
        let conn = self.conn.lock().await;
        let cursor: i64 = conn
            .query_row(
                "SELECT seq FROM cursors WHERE persona = ?1",
                params![persona],
                |row| row.get(0),
            )
            .optional()
            .context("reading mailbox cursor")?
            .unwrap_or(0);

        let cap: i64 = limit.map(i64::from).unwrap_or(i64::MAX);
        let mut stmt = conn
            .prepare(
                "SELECT seq, room_id, event_id, sender, envelope FROM messages \
                 WHERE persona = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
            )
            .context("preparing mailbox query")?;
        let rows = stmt
            .query_map(params![persona, cursor, cap], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .context("querying mailbox rows")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading mailbox rows")?;

        let mut entries = Vec::with_capacity(rows.len());
        let mut max_seq = cursor;
        for (seq, room_id, event_id, sender, envelope) in rows {
            max_seq = max_seq.max(seq);
            let envelope: Envelope =
                serde_json::from_str(&envelope).context("decoding stored envelope")?;
            entries.push(MailboxEntry {
                room_id,
                event_id,
                sender,
                envelope,
            });
        }

        if advance && max_seq > cursor {
            conn.execute(
                "INSERT INTO cursors (persona, seq) VALUES (?1, ?2) \
                 ON CONFLICT(persona) DO UPDATE SET seq = excluded.seq",
                params![persona, max_seq],
            )
            .context("advancing mailbox cursor")?;
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(from: &str, to: &str, body: &str) -> Envelope {
        crate::test_support::envelope(from, to, "chat", body)
    }

    #[tokio::test]
    async fn check_returns_exactly_what_was_missed_oldest_first() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        for i in 0..3 {
            mailbox
                .deliver(
                    "writer_agent",
                    "!room:x",
                    &format!("$event{i}"),
                    "@robb:x",
                    &env("@robb:x", "writer_agent", &format!("msg {i}")),
                )
                .await
                .unwrap();
        }
        let unread = mailbox.check("writer_agent", true, None).await.unwrap();
        let bodies: Vec<_> = unread.iter().map(|e| e.envelope.body.clone()).collect();
        assert_eq!(bodies, vec!["msg 0", "msg 1", "msg 2"]);
    }

    #[tokio::test]
    async fn second_immediate_check_returns_none() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        mailbox
            .deliver(
                "writer_agent",
                "!room:x",
                "$1",
                "@robb:x",
                &env("@robb:x", "writer_agent", "hi"),
            )
            .await
            .unwrap();
        let first = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(first.len(), 1);
        let second = mailbox.check("writer_agent", true, None).await.unwrap();
        assert!(second.is_empty(), "second immediate check must be empty");
    }

    #[tokio::test]
    async fn peek_does_not_advance_the_cursor() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        mailbox
            .deliver(
                "writer_agent",
                "!room:x",
                "$1",
                "@robb:x",
                &env("@robb:x", "writer_agent", "hi"),
            )
            .await
            .unwrap();
        let peek1 = mailbox.check("writer_agent", false, None).await.unwrap();
        let peek2 = mailbox.check("writer_agent", false, None).await.unwrap();
        assert_eq!(peek1.len(), 1);
        assert_eq!(peek2.len(), 1, "a repeated peek must be idempotent");
        // The genuine (advancing) check afterward still sees it, then clears.
        let real = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(real.len(), 1);
        let after = mailbox.check("writer_agent", true, None).await.unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn limit_caps_results_and_cursor_only_advances_past_what_was_returned() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        for i in 0..5 {
            mailbox
                .deliver(
                    "writer_agent",
                    "!room:x",
                    &format!("$event{i}"),
                    "@robb:x",
                    &env("@robb:x", "writer_agent", &format!("msg {i}")),
                )
                .await
                .unwrap();
        }
        let first = mailbox.check("writer_agent", true, Some(2)).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].envelope.body, "msg 0");
        assert_eq!(first[1].envelope.body, "msg 1");

        let rest = mailbox.check("writer_agent", true, None).await.unwrap();
        let bodies: Vec<_> = rest.iter().map(|e| e.envelope.body.clone()).collect();
        assert_eq!(bodies, vec!["msg 2", "msg 3", "msg 4"]);
    }

    #[tokio::test]
    async fn mailboxes_are_independent_per_persona() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        mailbox
            .deliver(
                "writer_agent",
                "!room:x",
                "$1",
                "@robb:x",
                &env("@robb:x", "writer_agent", "for writer"),
            )
            .await
            .unwrap();
        mailbox
            .deliver(
                "research_agent",
                "!room:x",
                "$2",
                "@robb:x",
                &env("@robb:x", "research_agent", "for research"),
            )
            .await
            .unwrap();
        let writer = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(writer.len(), 1);
        assert_eq!(writer[0].envelope.body, "for writer");

        let research = mailbox.check("research_agent", true, None).await.unwrap();
        assert_eq!(research.len(), 1);
        assert_eq!(research[0].envelope.body, "for research");
    }

    /// Mirrors loom-daemon's `ClaimAd::to_body_json` shape closely enough to
    /// exercise the detector faithfully (see `peer_claims.rs` upstream) —
    /// `to: "*"`, `type: "task"`, and a `body` that is a JSON object carrying
    /// the `loom_claim` marker key.
    fn claim_env(issue: u32) -> Envelope {
        Envelope {
            v: 1,
            from: "loom_daemon".to_owned(),
            to: "*".to_owned(),
            kind: "task".to_owned(),
            task_id: Some(issue.to_string()),
            body: format!(
                r#"{{"loom_claim":1,"kind":"advertise","issue":{issue},"repo":"r","host":"h"}}"#
            ),
            wake: None,
            meta: None,
        }
    }

    #[test]
    fn is_ephemeral_body_matches_only_the_marker_shape() {
        assert!(is_ephemeral_body(
            r#"{"loom_claim":1,"kind":"advertise","issue":5,"repo":"r","host":"h"}"#
        ));
        // Plain prose — the overwhelming common case — is never ephemeral.
        assert!(!is_ephemeral_body("hmm, the timeline looks off"));
        // Valid JSON without the marker key is left alone.
        assert!(!is_ephemeral_body(r#"{"foo":"bar"}"#));
        // Valid JSON that isn't an object (e.g. an array) is left alone.
        assert!(!is_ephemeral_body("[1,2,3]"));
    }

    #[tokio::test]
    async fn ephemeral_loom_claim_broadcasts_are_never_persisted() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        mailbox
            .deliver(
                "loom_builder_1",
                "!room:x",
                "$claim1",
                "@loom:x",
                &claim_env(60),
            )
            .await
            .unwrap();
        let unread = mailbox.check("loom_builder_1", true, None).await.unwrap();
        assert!(
            unread.is_empty(),
            "a loom_claim heartbeat must never land in a persona's durable mailbox"
        );
    }

    #[tokio::test]
    async fn json_body_without_the_marker_is_persisted_normally() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        mailbox
            .deliver(
                "writer_agent",
                "!room:x",
                "$1",
                "@robb:x",
                &env("@robb:x", "writer_agent", r#"{"foo":"bar"}"#),
            )
            .await
            .unwrap();
        let unread = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(
            unread.len(),
            1,
            "only the exact loom_claim marker shape is treated as ephemeral"
        );
    }

    #[tokio::test]
    async fn gc_drops_rows_already_covered_by_the_cursor() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        for i in 0..3 {
            mailbox
                .deliver(
                    "writer_agent",
                    "!room:x",
                    &format!("$event{i}"),
                    "@robb:x",
                    &env("@robb:x", "writer_agent", &format!("msg {i}")),
                )
                .await
                .unwrap();
        }
        // Consume everything, advancing the cursor past all three rows.
        mailbox.check("writer_agent", true, None).await.unwrap();

        // One more delivery triggers the next gc pass, which should sweep the
        // three now-consumed rows away, leaving only the new one.
        mailbox
            .deliver(
                "writer_agent",
                "!room:x",
                "$event3",
                "@robb:x",
                &env("@robb:x", "writer_agent", "msg 3"),
            )
            .await
            .unwrap();

        let row_count: i64 = {
            let conn = mailbox.conn.lock().await;
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE persona = 'writer_agent'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            row_count, 1,
            "already-consumed rows are garbage-collected on the next delivery"
        );

        // The consumer's own view is unaffected — the new row is still there.
        let unread = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].envelope.body, "msg 3");
    }

    #[tokio::test]
    async fn bounded_retention_caps_an_unconsumed_backlog() {
        // A persona that never calls `check` (the exact #60 scenario: 8
        // loom_builder_N personas, an empty `cursors` table) must not
        // accumulate an unbounded backlog. Exercise `gc_persona` directly
        // with a small cap — the mechanism, not the production constant.
        let mailbox = Mailbox::open_in_memory().unwrap();
        for i in 0..10 {
            mailbox
                .deliver(
                    "loom_builder_1",
                    "!room:x",
                    &format!("$event{i}"),
                    "@robb:x",
                    &env("@robb:x", "loom_builder_1", &format!("msg {i}")),
                )
                .await
                .unwrap();
        }
        {
            let conn = mailbox.conn.lock().await;
            gc_persona(&conn, "loom_builder_1", 3).unwrap();
        }

        let remaining = mailbox.check("loom_builder_1", false, None).await.unwrap();
        let bodies: Vec<_> = remaining.iter().map(|e| e.envelope.body.clone()).collect();
        assert_eq!(
            bodies,
            vec!["msg 7", "msg 8", "msg 9"],
            "the cap keeps only the most recent rows, dropping the oldest excess"
        );
    }

    #[tokio::test]
    async fn a_live_consumer_never_hits_the_retention_cap() {
        // Existing consumers with live cursors are unaffected (#60 AC3): a
        // persona that checks in regularly keeps a small unconsumed backlog
        // (well under the production cap), so gc never trims anything it
        // hasn't already delivered and had the chance to advance past.
        let mailbox = Mailbox::open_in_memory().unwrap();
        for round in 0..5 {
            mailbox
                .deliver(
                    "writer_agent",
                    "!room:x",
                    &format!("$event{round}"),
                    "@robb:x",
                    &env("@robb:x", "writer_agent", &format!("msg {round}")),
                )
                .await
                .unwrap();
            let unread = mailbox.check("writer_agent", true, None).await.unwrap();
            assert_eq!(unread.len(), 1);
            assert_eq!(unread[0].envelope.body, format!("msg {round}"));
        }
        // A fresh check confirms nothing was skipped or duplicated by gc.
        let after = mailbox.check("writer_agent", true, None).await.unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn survives_a_restart_mid_gap() {
        // Simulates the acceptance criterion end-to-end at the durable-store
        // layer: messages arrive, the daemon process "restarts" (the Mailbox
        // handle is dropped and a fresh one opens the same on-disk file), and
        // a persona that only checks in after the restart still gets exactly
        // what it missed.
        let dir = crate::test_support::tempdir("safehoused-mailbox-test");
        let db_path = dir.join("mailbox.sqlite3");
        {
            let mailbox = Mailbox::open(&db_path).unwrap();
            for i in 0..3 {
                mailbox
                    .deliver(
                        "writer_agent",
                        "!room:x",
                        &format!("$event{i}"),
                        "@robb:x",
                        &env("@robb:x", "writer_agent", &format!("missed {i}")),
                    )
                    .await
                    .unwrap();
            }
            // Persona never checked in before "restart".
        }
        // Fresh Mailbox instance over the same file — this is what a daemon
        // restart looks like from the mailbox's point of view.
        let reopened = Mailbox::open(&db_path).unwrap();
        let unread = reopened.check("writer_agent", true, None).await.unwrap();
        let bodies: Vec<_> = unread.iter().map(|e| e.envelope.body.clone()).collect();
        assert_eq!(bodies, vec!["missed 0", "missed 1", "missed 2"]);

        let second = reopened.check("writer_agent", true, None).await.unwrap();
        assert!(second.is_empty());

        std::fs::remove_dir_all(dir).ok();
    }
}
