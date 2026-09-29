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

/// Default cap on how many envelopes a single [`Mailbox::check`] call returns
/// when the caller doesn't pass an explicit `limit` (#188). This is a
/// *response-size* bound, distinct from [`MAX_UNCONSUMED_PER_PERSONA`]'s
/// *storage* bound above: a persona with a stale cursor in a busy broadcast
/// room can legitimately have thousands of unconsumed rows on disk, but
/// handing all of them back in one `safehouse_check` reply produces a
/// multi-megabyte payload no bounded-context MCP client (an LLM agent) can
/// consume — the one tool this substrate exists to make reliable for agents
/// becomes unusable for exactly the caller who most needs it (a first/
/// returning agent in an active room). A caller that wants more than this in
/// one shot still can — pass an explicit `limit` (`check`'s cap on an
/// explicit value is 1000, set in `rpc.rs`) — this only bounds the *unset*
/// case. `more_available`/`remaining` in [`MailboxCheckResult`] tell the
/// caller when a cap (default or explicit) left unread rows behind, so it can
/// call again with a smaller/larger `limit` as needed instead of discovering
/// the truncation only by counting.
pub(crate) const DEFAULT_CHECK_LIMIT: u32 = 200;

/// Size budget, in bytes, on the envelopes a single [`Mailbox::check`] call
/// returns (#190) — applied *in addition to* the row cap
/// ([`DEFAULT_CHECK_LIMIT`] or an explicit `limit`), whichever bites first.
///
/// The row cap is only a proxy for what actually breaks a bounded MCP
/// client: the reply's *size*. #188's repro measured ~619 chars/envelope in a
/// busy room, so 200 of those is ~124K chars (~31K tokens at ~4 chars/token)
/// — over Claude Code's default 25K-token `MAX_MCP_OUTPUT_TOKENS`. 64 KiB
/// keeps a reply under ~22K tokens even at a pessimistic ~3 chars/token for
/// JSON-heavy text, leaving headroom for the reply's own framing and for
/// clients with somewhat smaller budgets. Each entry's cost is estimated by
/// [`entry_cost`] (stored envelope JSON + routing fields + a fixed allowance
/// for keys/pretty-printing), so this is an approximate bound on the
/// rendered reply, not an exact byte count.
///
/// The budget never drops mail: at least one envelope is always returned (a
/// single envelope larger than the budget comes back alone rather than
/// wedging the mailbox), the cursor only advances past what was returned, and
/// a budget-triggered truncation is reported through the same
/// `more_available`/`remaining` signal as a row-cap truncation.
pub(crate) const DEFAULT_CHECK_BYTE_BUDGET: usize = 64 * 1024;

/// Fixed per-entry allowance for what [`entry_cost`] can't see from the raw
/// stored strings: the `room_id`/`event_id`/`sender`/`envelope` keys, JSON
/// punctuation, and the indentation the MCP shim adds when it pretty-prints
/// the reply (`serde_json::to_string_pretty`) — each envelope field lands on
/// its own indented line there. Measured at ~154 bytes for a typical
/// five-field envelope (`v`/`from`/`to`/`type`/`body`); 192 leaves room for
/// the optional `task_id`/`wake` fields while keeping a default-cap reply of
/// 200 short envelopes (~50 KB rendered) under the budget, so #188's row cap
/// remains the binding bound for ordinary chat traffic.
const ENTRY_OVERHEAD_BYTES: usize = 192;

/// Estimated contribution of one mailbox row to a `check` reply, for
/// [`DEFAULT_CHECK_BYTE_BUDGET`] accounting.
fn entry_cost(room_id: &str, event_id: &str, sender: &str, envelope_json: &str) -> usize {
    room_id.len() + event_id.len() + sender.len() + envelope_json.len() + ENTRY_OVERHEAD_BYTES
}

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

/// Result of [`Mailbox::check`]: the entries returned, plus whether a cap
/// (default or caller-supplied) left more unread rows behind (#188). Derefs
/// to `Vec<MailboxEntry>` so existing call sites that only care about the
/// entries (`.len()`, `.is_empty()`, indexing, `.iter()`) don't need to
/// change; a caller that needs the backlog signal reaches for `.more_available`
/// / `.remaining` explicitly.
#[derive(Clone, Debug)]
pub struct MailboxCheckResult {
    pub entries: Vec<MailboxEntry>,
    /// True when at least one unread row exists beyond what `entries`
    /// contains — i.e. the row cap (default or explicit `limit`) or the
    /// size budget ([`DEFAULT_CHECK_BYTE_BUDGET`], #190) was the reason fewer
    /// than the full unread backlog came back, not that there was nothing
    /// more to return.
    pub more_available: bool,
    /// Count of unread rows beyond `entries` — `0` iff `more_available` is
    /// `false`. Lets a caller decide how much bigger a follow-up `limit`
    /// needs to be instead of only learning that *some* backlog remains.
    pub remaining: i64,
}

impl std::ops::Deref for MailboxCheckResult {
    type Target = Vec<MailboxEntry>;

    fn deref(&self) -> &Vec<MailboxEntry> {
        &self.entries
    }
}

impl std::ops::Index<usize> for MailboxCheckResult {
    type Output = MailboxEntry;

    fn index(&self, idx: usize) -> &MailboxEntry {
        &self.entries[idx]
    }
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
    /// peek is idempotent. `limit`, when set, caps how many are returned; when
    /// unset, [`DEFAULT_CHECK_LIMIT`] applies instead of an unbounded return
    /// (#188) — the cursor only ever advances to cover what was actually
    /// returned, so a capped check never skips unread mail, it just may take
    /// more than one call to fully drain a large backlog. The returned
    /// [`MailboxCheckResult`] reports whether a cap (default or explicit)
    /// left more unread rows behind, so the caller can tell a genuinely empty
    /// mailbox apart from a truncated read.
    ///
    /// Independently of the row cap, the reply is also bounded by
    /// [`DEFAULT_CHECK_BYTE_BUDGET`] (#190), so a backlog of large envelopes
    /// may come back in fewer rows than `limit`; see [`Self::check_bounded`].
    pub async fn check(
        &self,
        persona: &str,
        advance: bool,
        limit: Option<u32>,
    ) -> Result<MailboxCheckResult> {
        self.check_bounded(persona, advance, limit, DEFAULT_CHECK_BYTE_BUDGET)
            .await
    }

    /// [`Self::check`] with an explicit size budget (`max_bytes`, measured by
    /// [`entry_cost`]). Rows are taken oldest-first until either the row cap
    /// is reached or including the next row would push the accumulated cost
    /// over `max_bytes` — except that the first row is always included, even
    /// if it alone exceeds the budget, so one oversized envelope can never
    /// wedge a persona's mailbox. Rows left behind by either bound are
    /// reported via `more_available`/`remaining` and stay unread.
    pub async fn check_bounded(
        &self,
        persona: &str,
        advance: bool,
        limit: Option<u32>,
        max_bytes: usize,
    ) -> Result<MailboxCheckResult> {
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

        let cap: i64 = limit
            .map(i64::from)
            .unwrap_or(i64::from(DEFAULT_CHECK_LIMIT));
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
        let mut used_bytes: usize = 0;
        for (seq, room_id, event_id, sender, envelope) in rows {
            // #190: stop before the row that would push the reply over the
            // size budget — but never before the first row, so an envelope
            // that alone exceeds the budget is still delivered (alone).
            let cost = entry_cost(&room_id, &event_id, &sender, &envelope);
            if !entries.is_empty() && used_bytes.saturating_add(cost) > max_bytes {
                break;
            }
            used_bytes = used_bytes.saturating_add(cost);
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

        // Rows still unread beyond what this call is about to return — same
        // "seq > N" shape as the primary query above, anchored to the last
        // seq actually included (or the original cursor, when `entries` is
        // empty) so this is correct whether the row cap, the byte budget, or
        // neither was the limiting factor.
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE persona = ?1 AND seq > ?2",
                params![persona, max_seq],
                |row| row.get(0),
            )
            .context("counting remaining unread mailbox rows")?;

        if advance && max_seq > cursor {
            conn.execute(
                "INSERT INTO cursors (persona, seq) VALUES (?1, ?2) \
                 ON CONFLICT(persona) DO UPDATE SET seq = excluded.seq",
                params![persona, max_seq],
            )
            .context("advancing mailbox cursor")?;
        }
        Ok(MailboxCheckResult {
            entries,
            more_available: remaining > 0,
            remaining,
        })
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
        assert!(
            first.more_available,
            "3 rows remain unread beyond the explicit limit of 2"
        );
        assert_eq!(first.remaining, 3);

        let rest = mailbox.check("writer_agent", true, None).await.unwrap();
        let bodies: Vec<_> = rest.iter().map(|e| e.envelope.body.clone()).collect();
        assert_eq!(bodies, vec!["msg 2", "msg 3", "msg 4"]);
        assert!(
            !rest.more_available,
            "nothing left unread after draining the rest"
        );
        assert_eq!(rest.remaining, 0);
    }

    /// #188: an unset `limit` must not return an unbounded backlog — a
    /// persona with a stale cursor in a busy room gets `DEFAULT_CHECK_LIMIT`
    /// envelopes per call, with `more_available`/`remaining` telling it a
    /// bigger backlog is still waiting so it can call again.
    #[tokio::test]
    async fn check_with_no_limit_applies_the_default_cap_and_reports_more_available() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        let total = DEFAULT_CHECK_LIMIT as usize + 50;
        for i in 0..total {
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

        let first = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(
            first.len(),
            DEFAULT_CHECK_LIMIT as usize,
            "an unset limit must be capped at DEFAULT_CHECK_LIMIT, not unbounded"
        );
        assert_eq!(first[0].envelope.body, "msg 0");
        assert!(
            first.more_available,
            "50 rows remain beyond the default cap"
        );
        assert_eq!(first.remaining, 50);

        // The cursor only advanced past what was actually returned, so a
        // follow-up call (still uncapped by the caller) drains the rest.
        let rest = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(rest.len(), 50);
        assert!(!rest.more_available);
        assert_eq!(rest.remaining, 0);
    }

    /// #188: a mailbox whose entire unread backlog fits under the default cap
    /// must report `more_available: false` — the signal only fires when a cap
    /// actually truncated the result, never unconditionally.
    #[tokio::test]
    async fn check_more_available_is_false_when_the_whole_backlog_fits() {
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
        assert_eq!(unread.len(), 3);
        assert!(!unread.more_available);
        assert_eq!(unread.remaining, 0);
    }

    async fn deliver_bodies(mailbox: &Mailbox, persona: &str, bodies: &[String]) {
        for (i, body) in bodies.iter().enumerate() {
            mailbox
                .deliver(
                    persona,
                    "!room:x",
                    &format!("$event{i}"),
                    "@robb:x",
                    &env("@robb:x", persona, body),
                )
                .await
                .unwrap();
        }
    }

    /// #190: a backlog of large envelopes is bounded by the byte budget well
    /// before the row cap, with the truncation reported through the same
    /// `more_available`/`remaining` signal, and nothing is skipped across
    /// the follow-up calls that drain it.
    #[tokio::test]
    async fn byte_budget_truncates_before_the_row_cap_and_reports_more_available() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        // 50 envelopes of ~4 KiB body each (~200 KiB total) — far under the
        // 200-row default cap, well over the 64 KiB budget.
        let bodies: Vec<String> = (0..50)
            .map(|i| format!("msg {i:02} {}", "x".repeat(4096)))
            .collect();
        deliver_bodies(&mailbox, "writer_agent", &bodies).await;

        let first = mailbox.check("writer_agent", true, None).await.unwrap();
        assert!(!first.is_empty());
        assert!(
            first.len() < bodies.len(),
            "the byte budget, not the row cap, must bound this reply"
        );
        let first_bytes: usize = first
            .iter()
            .map(|e| serde_json::to_string(&e.envelope).unwrap().len())
            .sum();
        assert!(
            first_bytes <= DEFAULT_CHECK_BYTE_BUDGET,
            "returned envelopes ({first_bytes} bytes) must fit the budget"
        );
        assert!(first.more_available);
        assert_eq!(first.remaining, (bodies.len() - first.len()) as i64);

        // Drain the rest; every envelope comes back exactly once, in order.
        let mut seen: Vec<String> = first.iter().map(|e| e.envelope.body.clone()).collect();
        let mut more = first.more_available;
        while more {
            let next = mailbox.check("writer_agent", true, None).await.unwrap();
            assert!(!next.is_empty(), "more_available must imply progress");
            seen.extend(next.iter().map(|e| e.envelope.body.clone()));
            more = next.more_available;
        }
        assert_eq!(seen, bodies);
    }

    /// #190: an envelope whose own size exceeds the budget is still returned
    /// — alone — rather than wedging the mailbox, and the envelopes behind it
    /// are reported as remaining.
    #[tokio::test]
    async fn single_oversized_envelope_is_returned_alone() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        let bodies = vec![
            "y".repeat(DEFAULT_CHECK_BYTE_BUDGET * 2),
            "small 1".to_owned(),
            "small 2".to_owned(),
        ];
        deliver_bodies(&mailbox, "writer_agent", &bodies).await;

        let first = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(first.len(), 1, "the oversized envelope comes back alone");
        assert_eq!(first[0].envelope.body, bodies[0]);
        assert!(first.more_available);
        assert_eq!(first.remaining, 2);

        let rest = mailbox.check("writer_agent", true, None).await.unwrap();
        let got: Vec<_> = rest.iter().map(|e| e.envelope.body.clone()).collect();
        assert_eq!(got, vec!["small 1", "small 2"]);
        assert!(!rest.more_available);
    }

    /// #190: an oversized envelope *behind* smaller ones is deferred to the
    /// next call (where it is returned alone), not dropped.
    #[tokio::test]
    async fn oversized_envelope_after_small_ones_is_deferred_not_dropped() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        let bodies = vec![
            "small 0".to_owned(),
            "z".repeat(DEFAULT_CHECK_BYTE_BUDGET + 1),
            "small 2".to_owned(),
        ];
        deliver_bodies(&mailbox, "writer_agent", &bodies).await;

        let first = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].envelope.body, "small 0");
        assert_eq!(first.remaining, 2);

        let second = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].envelope.body, bodies[1]);
        assert_eq!(second.remaining, 1);

        let third = mailbox.check("writer_agent", true, None).await.unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].envelope.body, "small 2");
        assert!(!third.more_available);
    }

    /// #190: the byte budget applies to an explicit `limit` too — a large
    /// explicit limit over large envelopes is still size-bounded — while a
    /// small explicit limit under budget still bounds by rows as before.
    #[tokio::test]
    async fn explicit_limit_is_also_subject_to_the_byte_budget() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        let bodies: Vec<String> = (0..40)
            .map(|i| format!("msg {i:02} {}", "x".repeat(4096)))
            .collect();
        deliver_bodies(&mailbox, "writer_agent", &bodies).await;

        // Small explicit limit: the row cap bites first, as before #190.
        let peek = mailbox.check("writer_agent", false, Some(3)).await.unwrap();
        assert_eq!(peek.len(), 3);
        assert_eq!(peek.remaining, 37);

        // Large explicit limit: the byte budget bites first.
        let big = mailbox
            .check("writer_agent", true, Some(1000))
            .await
            .unwrap();
        assert!(big.len() > 3 && big.len() < bodies.len());
        assert!(big.more_available);
        assert_eq!(big.remaining, (bodies.len() - big.len()) as i64);
        assert_eq!(big[0].envelope.body, bodies[0]);
    }

    /// #190: `check_bounded` exposes the mechanism with an arbitrary budget;
    /// a backlog entirely under budget is returned unchanged.
    #[tokio::test]
    async fn backlog_under_budget_is_unchanged_and_small_budget_truncates() {
        let mailbox = Mailbox::open_in_memory().unwrap();
        let bodies: Vec<String> = (0..5).map(|i| format!("msg {i}")).collect();
        deliver_bodies(&mailbox, "writer_agent", &bodies).await;

        let all = mailbox.check("writer_agent", false, None).await.unwrap();
        let got: Vec<_> = all.iter().map(|e| e.envelope.body.clone()).collect();
        assert_eq!(got, bodies, "an under-budget backlog is returned whole");
        assert!(!all.more_available);
        assert_eq!(all.remaining, 0);

        // A budget of zero still returns exactly one row (never wedges).
        let one = mailbox
            .check_bounded("writer_agent", false, None, 0)
            .await
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one.remaining, 4);
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
