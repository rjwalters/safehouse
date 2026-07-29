//! Egress publisher core (#30) — the trust-boundary filter that decides what,
//! if anything, leaves safehouse for the public feed, and when.
//!
//! This is the single most security-sensitive component in the daemon: it is
//! the one code path that copies content *out* of an end-to-end-encrypted room
//! toward an external sink. Every design choice here is fail-safe:
//!
//! - **Off by default.** With no `egress` block in config the daemon behaves
//!   exactly as before — nothing here runs (see [`Config`](crate::Config)).
//! - **Explicit per-room opt-in.** Only rooms whose id appears in
//!   `egress.rooms` are ever considered; everything else is ignored.
//! - **Allowlist by type + strict re-validation.** Only a well-formed
//!   `completion` envelope (§4a `completion-v1`) is ever eligible — see
//!   [`is_allowlisted`], which re-runs [`validate_completion_meta`] as
//!   defense-in-depth even though ingest already degraded invalid completions
//!   to `chat` (envelope-v1.md §4a / D18).
//! - **Mandatory redaction.** Configured deny patterns are stripped from every
//!   string in the payload *before* it is queued ([`redact`]).
//! - **Delay buffer with retraction.** A completion is not published
//!   immediately; it sits in a durable buffer for `delay_seconds`, during which
//!   a native Matrix edit (`m.replace`) or redaction (`m.room.redaction`) of the
//!   source event suppresses it entirely ([`Egress::retract`]).
//!
//! This phase writes to a local JSON-lines file sink; real network transport is
//! #31. Isolating the filtering logic behind a local sink lets the hardest part
//! to get right — *what* leaves the boundary and *when* — be reviewed and tested
//! without also reviewing an HTTP client.

use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::envelope::{validate_completion_meta, Envelope};

/// How often the background publisher wakes to flush rows whose delay has
/// elapsed. The delay itself is per-completion (`delay_seconds`); this is just
/// the polling granularity, kept short so the buffer feels near-real-time.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The optional egress block on the daemon [`Config`](crate::Config). Absent =
/// the whole egress subsystem is disabled (zero behavior change). Matches the
/// repo's flat, explicit-config style (`#[serde(deny_unknown_fields)]`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressConfig {
    /// Room-id allowlist — only completions observed in one of these rooms are
    /// ever eligible for the feed. Empty = nothing is ever published (the whole
    /// block is effectively inert), which is the safe default.
    #[serde(default)]
    pub rooms: Vec<String>,
    /// Mandatory deny-pattern list (literal substrings, see [`redact`]) applied
    /// to every string in a completion payload before it is queued. Required to
    /// be non-empty whenever `rooms` is non-empty — the daemon refuses to boot
    /// otherwise ([`validate_egress_config`]).
    #[serde(default)]
    pub deny_patterns: Vec<String>,
    /// How long a completion sits in the delay buffer before it is published,
    /// during which an edit/redaction of the source event suppresses it.
    #[serde(default)]
    pub delay_seconds: u64,
    /// The local JSON-lines sink file for this phase. #31 replaces this with a
    /// real HTTP POST target.
    pub sink_path: PathBuf,
}

/// Boot-time fail-safe guard (from #28's acceptance sketch): if the operator
/// opted any room into egress, they MUST also supply a non-empty deny-pattern
/// list. A configured-but-unfiltered egress room is the exact
/// leak-everything-by-omission footgun this refuses.
pub fn validate_egress_config(cfg: &EgressConfig) -> std::result::Result<(), String> {
    if !cfg.rooms.is_empty() && cfg.deny_patterns.is_empty() {
        return Err(
            "egress.rooms is non-empty but egress.deny_patterns is empty — refusing to \
             publish unredacted content (add at least one deny pattern)"
                .to_owned(),
        );
    }
    Ok(())
}

/// **The** security-critical gate. A payload is feed-eligible **iff** it is a
/// `completion` envelope carrying a `meta` that strictly validates as
/// `completion-v1`. Everything else — `chat`, `task`, `handoff`, `ack`, or a
/// `completion` whose `meta` is missing/malformed — returns `false`,
/// unconditionally.
///
/// Re-validating via [`validate_completion_meta`] here is deliberate
/// defense-in-depth, not redundancy: ingest already degrades an invalid
/// completion to `chat` (envelope-v1.md §4a / D18), so by the time egress sees
/// it `meta` has validated once — this makes the egress path correct in
/// isolation regardless of what the ingest path did.
pub fn is_allowlisted(env: &Envelope) -> bool {
    env.kind == "completion"
        && env
            .meta
            .as_ref()
            .is_some_and(|m| validate_completion_meta(m).is_ok())
}

/// Apply the deny-pattern list to every string value in `meta`, recursively.
///
/// **Matching is literal substring** (not regex): a deny pattern matches
/// anywhere it appears inside any string, and every match is replaced with
/// `[REDACTED]`. This was chosen over regex to avoid a new dependency and to
/// keep the security-critical behavior trivially auditable — an operator's
/// deny list means exactly what it says, with no metacharacter surprises.
/// Empty patterns are ignored (they would otherwise "match" everywhere).
pub fn redact(meta: &Value, deny_patterns: &[String]) -> Value {
    let mut cloned = meta.clone();
    redact_in_place(&mut cloned, deny_patterns);
    cloned
}

fn redact_in_place(value: &mut Value, deny_patterns: &[String]) {
    match value {
        Value::String(s) => {
            for pattern in deny_patterns {
                if !pattern.is_empty() && s.contains(pattern.as_str()) {
                    *s = s.replace(pattern.as_str(), "[REDACTED]");
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_in_place(item, deny_patterns);
            }
        }
        Value::Object(map) => {
            for v in map.values_mut() {
                redact_in_place(v, deny_patterns);
            }
        }
        // Numbers, bools, and null carry nothing to redact.
        _ => {}
    }
}

/// If `content` is a native Matrix edit (`m.relates_to.rel_type == "m.replace"`),
/// the event id of the *source* event it replaces. Used to suppress a pending
/// completion whose source message a human/agent edited inside the delay window
/// — the same "undo" Element already gives them, reused rather than inventing a
/// bespoke retract envelope.
pub fn edit_target(content: &Value) -> Option<String> {
    let relates_to = content.get("m.relates_to")?;
    if relates_to.get("rel_type").and_then(Value::as_str) != Some("m.replace") {
        return None;
    }
    relates_to
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// One row that cleared the delay buffer and was written to the sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedRecord {
    pub room_id: String,
    pub event_id: String,
    /// The redacted `completion-v1` payload actually published.
    pub payload: Value,
}

/// The egress runtime: allowlist config + a durable delay buffer + the local
/// sink. Constructed only when `egress` is present in config; cloned into both
/// the event-dispatch path (`consider`/`retract`) and the background flush task
/// (`run`).
pub struct Egress {
    rooms: HashSet<String>,
    deny_patterns: Vec<String>,
    delay_seconds: u64,
    sink_path: PathBuf,
    /// Durable delay buffer, mirroring `mailbox.rs`'s rusqlite-behind-a-Mutex
    /// shape. Keyed by `(room_id, event_id)` so an edit/redaction of the source
    /// event can find and suppress the pending row before it publishes.
    conn: Mutex<Connection>,
}

impl Egress {
    /// Open the egress subsystem: validate config, open (creating if needed) the
    /// durable delay-buffer store at `db_path`.
    pub fn open(config: EgressConfig, db_path: &Path) -> Result<Arc<Self>> {
        validate_egress_config(&config)
            .map_err(|e| anyhow::anyhow!("invalid egress config: {e}"))?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening egress store {}", db_path.display()))?;
        Self::from_connection(config, conn)
    }

    #[cfg(test)]
    fn open_in_memory(config: EgressConfig) -> Result<Arc<Self>> {
        let conn = Connection::open_in_memory().context("opening in-memory egress store")?;
        Self::from_connection(config, conn)
    }

    fn from_connection(config: EgressConfig, conn: Connection) -> Result<Arc<Self>> {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS pending_publish (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                room_id       TEXT NOT NULL,
                event_id      TEXT NOT NULL,
                publish_after INTEGER NOT NULL,
                payload_json  TEXT NOT NULL,
                retracted     INTEGER NOT NULL DEFAULT 0,
                published     INTEGER NOT NULL DEFAULT 0
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_pending_room_event
                ON pending_publish(room_id, event_id);
            ",
        )
        .context("creating egress schema")?;
        Ok(Arc::new(Self {
            rooms: config.rooms.into_iter().collect(),
            deny_patterns: config.deny_patterns,
            delay_seconds: config.delay_seconds,
            sink_path: config.sink_path,
            conn: Mutex::new(conn),
        }))
    }

    /// Whether `room_id` is opted into egress.
    pub fn is_egress_room(&self, room_id: &str) -> bool {
        self.rooms.contains(room_id)
    }

    /// Consider one observed envelope for the feed. Returns `Ok(true)` when the
    /// envelope was redacted and enqueued into the delay buffer, `Ok(false)`
    /// when it was ignored (wrong room, wrong type, or not feed-eligible).
    ///
    /// This is where allowlist and redaction compose: the room must be opted in
    /// **and** [`is_allowlisted`] must pass; only then is the payload redacted
    /// and buffered. Duplicate `(room_id, event_id)` enqueues are ignored, so a
    /// re-observed event never double-publishes.
    pub async fn consider(&self, room_id: &str, event_id: &str, env: &Envelope) -> Result<bool> {
        if !self.is_egress_room(room_id) || !is_allowlisted(env) {
            return Ok(false);
        }
        // `is_allowlisted` guarantees `meta` is present and valid.
        let meta = env
            .meta
            .as_ref()
            .expect("is_allowlisted guarantees meta is present");
        let redacted = redact(meta, &self.deny_patterns);
        let publish_after = unix_now() + self.delay_seconds as i64;
        self.enqueue(room_id, event_id, publish_after, &redacted)
            .await?;
        Ok(true)
    }

    /// Enqueue a redacted payload into the delay buffer. Split out from
    /// [`consider`](Self::consider) so tests can drive the store with explicit
    /// `publish_after` timestamps.
    async fn enqueue(
        &self,
        room_id: &str,
        event_id: &str,
        publish_after: i64,
        payload: &Value,
    ) -> Result<()> {
        let payload_json = serde_json::to_string(payload).context("serializing egress payload")?;
        let conn = self.conn.lock().await;
        // INSERT OR IGNORE against the unique (room_id, event_id) index: a row
        // already buffered for this source event is authoritative — never
        // clobber its `retracted`/`published` state with a re-observation.
        conn.execute(
            "INSERT OR IGNORE INTO pending_publish \
             (room_id, event_id, publish_after, payload_json) VALUES (?1, ?2, ?3, ?4)",
            params![room_id, event_id, publish_after, payload_json],
        )
        .context("inserting pending_publish row")?;
        Ok(())
    }

    /// Suppress a pending completion whose source event was edited or redacted
    /// inside the delay window (retraction). No-op if there is no matching
    /// un-published row — a redaction of some unrelated event, or one that
    /// arrives after the completion already published, changes nothing.
    pub async fn retract(&self, room_id: &str, event_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE pending_publish SET retracted = 1 \
             WHERE room_id = ?1 AND event_id = ?2 AND published = 0",
            params![room_id, event_id],
        )
        .context("marking pending_publish row retracted")?;
        Ok(())
    }

    /// Flush every row whose delay has elapsed (`publish_after <= now`) and that
    /// was neither retracted nor already published: append each to the sink and
    /// mark it published. Returns what was written. Idempotent across calls — a
    /// published row is never re-emitted.
    pub async fn publish_due(&self, now: i64) -> Result<Vec<PublishedRecord>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn
            .prepare(
                "SELECT id, room_id, event_id, payload_json FROM pending_publish \
                 WHERE publish_after <= ?1 AND retracted = 0 AND published = 0 ORDER BY id ASC",
            )
            .context("preparing pending_publish query")?;
        let rows = stmt
            .query_map(params![now], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("querying pending_publish rows")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading pending_publish rows")?;

        let mut published = Vec::with_capacity(rows.len());
        for (id, room_id, event_id, payload_json) in rows {
            let payload: Value =
                serde_json::from_str(&payload_json).context("decoding buffered egress payload")?;
            self.write_to_sink(&room_id, &event_id, &payload)?;
            conn.execute(
                "UPDATE pending_publish SET published = 1 WHERE id = ?1",
                params![id],
            )
            .context("marking pending_publish row published")?;
            published.push(PublishedRecord {
                room_id,
                event_id,
                payload,
            });
        }
        Ok(published)
    }

    /// Append one published record to the local JSON-lines sink. #31 swaps this
    /// for a real network POST.
    fn write_to_sink(&self, room_id: &str, event_id: &str, payload: &Value) -> Result<()> {
        if let Some(parent) = self.sink_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating sink dir {}", parent.display()))?;
            }
        }
        let line = json!({
            "room_id": room_id,
            "event_id": event_id,
            "payload": payload,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.sink_path)
            .with_context(|| format!("opening sink {}", self.sink_path.display()))?;
        writeln!(file, "{line}")
            .with_context(|| format!("writing to sink {}", self.sink_path.display()))?;
        Ok(())
    }

    /// Background flush loop: every [`POLL_INTERVAL`] publish whatever is due.
    /// Errors are logged, never fatal — a transient sink failure must not take
    /// the daemon down.
    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            match self.publish_due(unix_now()).await {
                Ok(records) => {
                    for record in records {
                        println!(
                            "safehoused: egress published completion {} from {}",
                            record.event_id, record.room_id
                        );
                    }
                }
                Err(err) => {
                    eprintln!("safehoused: egress publish poll failed: {err:#}");
                }
            }
        }
    }
}

/// Current wall-clock time in whole seconds since the Unix epoch.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_meta() -> Value {
        json!({
            "schema": "completion-v1",
            "agent": "writer_agent",
            "repo": "rjwalters/safehouse",
            "ref": "https://github.com/rjwalters/safehouse/pull/99",
            "result": "success",
            "started_at": "2026-07-29T10:00:00Z",
            "completed_at": "2026-07-29T10:05:00Z",
        })
    }

    fn env(kind: &str, meta: Option<Value>) -> Envelope {
        Envelope {
            v: 1,
            from: "writer_agent".to_owned(),
            to: "*".to_owned(),
            kind: kind.to_owned(),
            task_id: None,
            body: "done".to_owned(),
            wake: None,
            meta,
        }
    }

    fn config(rooms: &[&str], deny: &[&str], delay_seconds: u64, sink: PathBuf) -> EgressConfig {
        EgressConfig {
            rooms: rooms.iter().map(|s| (*s).to_owned()).collect(),
            deny_patterns: deny.iter().map(|s| (*s).to_owned()).collect(),
            delay_seconds,
            sink_path: sink,
        }
    }

    /// A unique scratch directory under the OS temp dir. Mirrors `mailbox.rs`'s
    /// test helper to avoid pulling in a `tempfile` dependency.
    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "safehoused-egress-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- is_allowlisted — the security-critical gate (heaviest coverage) ---

    #[test]
    fn is_allowlisted_accepts_well_formed_completion() {
        assert!(is_allowlisted(&env("completion", Some(completion_meta()))));
    }

    #[test]
    fn is_allowlisted_rejects_completion_with_no_meta() {
        assert!(!is_allowlisted(&env("completion", None)));
    }

    #[test]
    fn is_allowlisted_rejects_completion_with_malformed_meta() {
        let mut meta = completion_meta();
        meta["schema"] = json!("something-else");
        assert!(!is_allowlisted(&env("completion", Some(meta))));

        let mut missing = completion_meta();
        missing.as_object_mut().unwrap().remove("repo");
        assert!(!is_allowlisted(&env("completion", Some(missing))));

        let mut bad_ts = completion_meta();
        bad_ts["started_at"] = json!("last tuesday");
        assert!(!is_allowlisted(&env("completion", Some(bad_ts))));
    }

    #[test]
    fn is_allowlisted_rejects_every_non_completion_type() {
        // Even carrying a valid completion meta, a non-`completion` type is
        // never feed-eligible — the type is the gate, not the payload.
        for kind in ["chat", "task", "handoff", "ack"] {
            assert!(
                !is_allowlisted(&env(kind, Some(completion_meta()))),
                "type {kind:?} must never be allowlisted"
            );
            assert!(!is_allowlisted(&env(kind, None)));
        }
    }

    // ---- redact ------------------------------------------------------------

    #[test]
    fn redact_strips_matches_from_every_string_field() {
        let meta = json!({
            "schema": "completion-v1",
            "agent": "secret_agent",
            "repo": "acme/secret-repo",
            "ref": "https://internal.acme.example/pr/1",
            "nested": { "note": "contains secret token here" },
            "list": ["secret", "clean"],
        });
        let out = redact(&meta, &["secret".to_owned(), "acme".to_owned()]);
        assert_eq!(out["agent"], "[REDACTED]_agent");
        assert_eq!(out["repo"], "[REDACTED]/[REDACTED]-repo");
        assert_eq!(out["ref"], "https://internal.[REDACTED].example/pr/1");
        assert_eq!(out["nested"]["note"], "contains [REDACTED] token here");
        assert_eq!(out["list"][0], "[REDACTED]");
        assert_eq!(out["list"][1], "clean");
        // Non-string values are untouched.
        assert_eq!(out["schema"], "completion-v1");
    }

    #[test]
    fn redact_with_no_patterns_is_identity() {
        let meta = completion_meta();
        assert_eq!(redact(&meta, &[]), meta);
    }

    #[test]
    fn redact_ignores_empty_patterns() {
        let meta = json!({ "agent": "writer" });
        // An empty pattern must not "match everywhere" and blow the string away.
        assert_eq!(redact(&meta, &[String::new()]), meta);
    }

    // ---- validate_egress_config — boot guard -------------------------------

    #[test]
    fn config_guard_rejects_rooms_without_deny_patterns() {
        let cfg = config(&["!room:x"], &[], 0, PathBuf::from("/tmp/sink.jsonl"));
        assert!(validate_egress_config(&cfg).is_err());
    }

    #[test]
    fn config_guard_accepts_rooms_with_deny_patterns() {
        let cfg = config(
            &["!room:x"],
            &["secret"],
            0,
            PathBuf::from("/tmp/sink.jsonl"),
        );
        assert!(validate_egress_config(&cfg).is_ok());
    }

    #[test]
    fn config_guard_accepts_empty_rooms_even_without_deny_patterns() {
        // An empty allowlist can never publish, so an empty deny list is moot.
        let cfg = config(&[], &[], 0, PathBuf::from("/tmp/sink.jsonl"));
        assert!(validate_egress_config(&cfg).is_ok());
    }

    // ---- edit_target -------------------------------------------------------

    #[test]
    fn edit_target_extracts_replace_relation() {
        let content = json!({
            "body": "* edited",
            "m.relates_to": { "rel_type": "m.replace", "event_id": "$orig" },
        });
        assert_eq!(edit_target(&content).as_deref(), Some("$orig"));
    }

    #[test]
    fn edit_target_ignores_non_replace_relations() {
        let thread = json!({
            "m.relates_to": { "rel_type": "m.thread", "event_id": "$root" },
        });
        assert!(edit_target(&thread).is_none());
        assert!(edit_target(&json!({ "body": "no relation" })).is_none());
    }

    // ---- delay buffer: timing + retraction (the core behavior) -------------

    #[tokio::test]
    async fn consider_ignores_rooms_not_in_the_allowlist() {
        let dir = tempdir();
        let egress = Egress::open_in_memory(config(
            &["!allowed:x"],
            &["secret"],
            0,
            dir.join("sink.jsonl"),
        ))
        .unwrap();
        let queued = egress
            .consider(
                "!other:x",
                "$1",
                &env("completion", Some(completion_meta())),
            )
            .await
            .unwrap();
        assert!(!queued, "a room outside the allowlist must never queue");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn consider_ignores_non_feed_eligible_envelopes() {
        let dir = tempdir();
        let egress =
            Egress::open_in_memory(config(&["!r:x"], &["secret"], 0, dir.join("sink.jsonl")))
                .unwrap();
        assert!(!egress
            .consider("!r:x", "$1", &env("chat", None))
            .await
            .unwrap());
        assert!(!egress
            .consider("!r:x", "$2", &env("completion", None))
            .await
            .unwrap());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn publish_only_after_delay_elapses() {
        let dir = tempdir();
        let sink = dir.join("sink.jsonl");
        let egress =
            Egress::open_in_memory(config(&["!r:x"], &["nothing"], 0, sink.clone())).unwrap();
        // Enqueue with an explicit future publish_after so timing is deterministic.
        let payload = redact(&completion_meta(), &egress.deny_patterns);
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();

        // Before the delay elapses: nothing publishes, sink stays absent.
        let early = egress.publish_due(999).await.unwrap();
        assert!(early.is_empty(), "must not publish before publish_after");
        assert!(!sink.exists(), "sink must not be written before the delay");

        // At/after publish_after: exactly one record, written to the sink.
        let due = egress.publish_due(1_000).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_id, "$1");
        let sink_body = std::fs::read_to_string(&sink).unwrap();
        assert_eq!(sink_body.lines().count(), 1);
        let line: Value = serde_json::from_str(sink_body.lines().next().unwrap()).unwrap();
        assert_eq!(line["room_id"], "!r:x");
        assert_eq!(line["event_id"], "$1");
        assert_eq!(line["payload"]["schema"], "completion-v1");

        // Idempotent: a later poll does not re-publish the same row.
        let again = egress.publish_due(2_000).await.unwrap();
        assert!(again.is_empty(), "a published row must never re-emit");
        assert_eq!(std::fs::read_to_string(&sink).unwrap().lines().count(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn retraction_inside_the_window_suppresses_publish() {
        let dir = tempdir();
        let sink = dir.join("sink.jsonl");
        let egress =
            Egress::open_in_memory(config(&["!r:x"], &["nothing"], 0, sink.clone())).unwrap();
        let payload = redact(&completion_meta(), &egress.deny_patterns);
        egress
            .enqueue("!r:x", "$src", 1_000, &payload)
            .await
            .unwrap();

        // Source event edited/redacted before publish_after -> retracted.
        egress.retract("!r:x", "$src").await.unwrap();

        let due = egress.publish_due(5_000).await.unwrap();
        assert!(due.is_empty(), "a retracted completion must never publish");
        assert!(!sink.exists(), "the sink must stay empty for a retraction");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn retraction_of_unrelated_event_does_not_suppress() {
        let dir = tempdir();
        let sink = dir.join("sink.jsonl");
        let egress =
            Egress::open_in_memory(config(&["!r:x"], &["nothing"], 0, sink.clone())).unwrap();
        let payload = redact(&completion_meta(), &egress.deny_patterns);
        egress
            .enqueue("!r:x", "$src", 1_000, &payload)
            .await
            .unwrap();

        // A redaction of some *other* event must not touch this pending row.
        egress.retract("!r:x", "$unrelated").await.unwrap();

        let due = egress.publish_due(5_000).await.unwrap();
        assert_eq!(due.len(), 1, "unrelated retraction must not suppress");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn consider_redacts_before_queuing() {
        let dir = tempdir();
        let sink = dir.join("sink.jsonl");
        // Deny the repo owner substring; it must be gone from the queued payload.
        let egress =
            Egress::open_in_memory(config(&["!r:x"], &["rjwalters"], 0, sink.clone())).unwrap();
        assert!(egress
            .consider("!r:x", "$1", &env("completion", Some(completion_meta())))
            .await
            .unwrap());
        let due = egress.publish_due(unix_now() + 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].payload["repo"], "[REDACTED]/safehouse");
        assert!(!due[0].payload["ref"]
            .as_str()
            .unwrap()
            .contains("rjwalters"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn buffer_is_durable_across_reopen() {
        // Mirrors mailbox.rs's restart test: a completion queued before a
        // "restart" (fresh handle over the same file) still publishes after.
        let dir = tempdir();
        let db = dir.join("egress.sqlite3");
        let sink = dir.join("sink.jsonl");
        {
            let egress =
                Egress::open(config(&["!r:x"], &["nothing"], 0, sink.clone()), &db).unwrap();
            let payload = redact(&completion_meta(), &["nothing".to_owned()]);
            egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();
        }
        let reopened = Egress::open(config(&["!r:x"], &["nothing"], 0, sink.clone()), &db).unwrap();
        let due = reopened.publish_due(2_000).await.unwrap();
        assert_eq!(due.len(), 1, "a buffered row must survive a restart");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn duplicate_enqueue_is_ignored() {
        let dir = tempdir();
        let egress =
            Egress::open_in_memory(config(&["!r:x"], &["nothing"], 0, dir.join("sink.jsonl")))
                .unwrap();
        let payload = redact(&completion_meta(), &["nothing".to_owned()]);
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();
        let due = egress.publish_due(2_000).await.unwrap();
        assert_eq!(
            due.len(),
            1,
            "the same source event must queue at most once"
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
