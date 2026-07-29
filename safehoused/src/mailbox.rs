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

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;

use crate::envelope::Envelope;

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
    pub async fn deliver(
        &self,
        persona: &str,
        room_id: &str,
        event_id: &str,
        sender: &str,
        env: &Envelope,
    ) -> Result<()> {
        let payload = serde_json::to_string(env).context("serializing envelope for mailbox")?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO messages (persona, room_id, event_id, sender, envelope) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![persona, room_id, event_id, sender, payload],
        )
        .context("inserting mailbox row")?;
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
        Envelope {
            v: 1,
            from: from.to_owned(),
            to: to.to_owned(),
            kind: "chat".to_owned(),
            task_id: None,
            body: body.to_owned(),
            wake: None,
            meta: None,
        }
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

    #[tokio::test]
    async fn survives_a_restart_mid_gap() {
        // Simulates the acceptance criterion end-to-end at the durable-store
        // layer: messages arrive, the daemon process "restarts" (the Mailbox
        // handle is dropped and a fresh one opens the same on-disk file), and
        // a persona that only checks in after the restart still gets exactly
        // what it missed.
        let dir = tempdir();
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

    /// A unique scratch directory under the OS temp dir, cleaned up by the
    /// caller. Avoids pulling in a `tempfile` dependency for one test.
    ///
    /// pid + wall-clock nanos alone are not sufficient uniqueness under
    /// parallel test execution — see the sibling helper in `egress.rs` for
    /// the full collision analysis (#55). A process-wide atomic counter
    /// makes each call unique regardless of clock resolution.
    fn tempdir() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "safehoused-mailbox-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
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
}
