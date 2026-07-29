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
//! ## Transport (#31)
//!
//! The sink is one of two mutually-exclusive targets, chosen at boot from
//! [`EgressConfig`] (`sink_url` wins if both are set): a local JSON-lines file
//! (`sink_path`, the #30 default, kept for backward compatibility) or an
//! outbound HTTP `POST` (`sink_url`) — e.g. a Workers/Pages endpoint or an
//! R2-backed feed. **The HTTP sink only ever originates outbound connections;
//! it never binds a listening socket** (D8 — see `docs/design.md` §4.1.2).
//!
//! The network POST never runs while holding the delay-buffer's sqlite
//! [`Mutex`]: [`Egress::publish_due`] selects due rows under the lock, releases
//! it, then publishes each row (the only part that may block on the network),
//! and re-acquires the lock just to record the outcome. This keeps a slow or
//! unreachable sink from stalling `consider`/`retract`, which run on the live
//! sync-event path.
//!
//! Retry fits the existing 1s poll loop rather than an in-line sleep: a
//! transient failure (network error or `5xx`) bumps `publish_after` into the
//! future by an exponential backoff and increments `attempts`; after
//! [`MAX_PUBLISH_ATTEMPTS`] the row is marked `failed` for operator inspection
//! (`failed` rows are never auto-retried again). A `4xx` is treated as a
//! config/schema problem, not a transient fault, and is marked `failed`
//! immediately without consuming retry attempts — the operator needs to look
//! at it, not have the daemon hammer a broken endpoint.
//!
//! **At-least-once delivery.** A crash between a successful sink write and the
//! `published = 1` update re-emits that row on restart. The wrapper object
//! POSTed (`{"room_id", "event_id", "payload"}`) carries the natural dedup key
//! `(room_id, event_id)` — receivers MUST tolerate the same pair arriving more
//! than once.

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

/// How long the HTTP sink waits for a response before treating the attempt as
/// a (retryable) network failure. Bounded so an unreachable sink can never
/// hang the flush loop indefinitely.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// How many total attempts a row gets against a transiently-failing sink
/// before it is given up on and marked `failed`. Chosen to bound backoff to a
/// few minutes rather than retry forever against a sink that is down for an
/// extended outage — an operator can requeue a `failed` row manually once the
/// sink is back (out of scope here; see the design-doc note on `failed`).
const MAX_PUBLISH_ATTEMPTS: i64 = 5;

/// Exponential backoff (seconds) after `attempts` failed tries, capped so a
/// long outage doesn't push `publish_after` absurdly far out.
fn retry_backoff_secs(attempts: i64) -> i64 {
    const BASE_SECS: i64 = 2;
    const CAP_SECS: i64 = 60;
    BASE_SECS
        .saturating_pow(attempts.clamp(1, 6) as u32)
        .min(CAP_SECS)
}

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
    /// The #30 local JSON-lines sink file. Optional as of #31 — kept only for
    /// backward compatibility with existing configs; a config written from
    /// scratch should prefer `sink_url`. Ignored when `sink_url` is set.
    #[serde(default)]
    pub sink_path: Option<PathBuf>,
    /// The #31 outbound HTTP sink: every published record is `POST`ed here as
    /// `{"room_id", "event_id", "payload"}` JSON. Takes priority over
    /// `sink_path` when both are configured. Strictly outbound — the daemon
    /// never listens on this or any address (D8).
    #[serde(default)]
    pub sink_url: Option<String>,
}

/// Boot-time fail-safe guards.
///
/// - (from #28's acceptance sketch) if the operator opted any room into
///   egress, they MUST also supply a non-empty deny-pattern list. A
///   configured-but-unfiltered egress room is the exact
///   leak-everything-by-omission footgun this refuses.
/// - (#31) an egress block MUST configure at least one sink target
///   (`sink_path` and/or `sink_url`) — an egress block with no publish target
///   is either a config mistake or a copy-paste leftover, and the daemon
///   refuses to boot silently-inert rather than guess.
pub fn validate_egress_config(cfg: &EgressConfig) -> std::result::Result<(), String> {
    if !cfg.rooms.is_empty() && cfg.deny_patterns.is_empty() {
        return Err(
            "egress.rooms is non-empty but egress.deny_patterns is empty — refusing to \
             publish unredacted content (add at least one deny pattern)"
                .to_owned(),
        );
    }
    if cfg.sink_path.is_none() && cfg.sink_url.is_none() {
        return Err(
            "egress is configured but neither sink_path nor sink_url is set — configure at \
             least one publish target"
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

/// One row that cleared the delay buffer and was published to the sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedRecord {
    pub room_id: String,
    pub event_id: String,
    /// The redacted `completion-v1` payload actually published.
    pub payload: Value,
}

/// Where a published record actually goes. Chosen once at construction from
/// [`EgressConfig`]; `sink_url` wins if both are configured.
enum Sink {
    /// The #30 local JSON-lines file.
    File(PathBuf),
    /// The #31 outbound HTTP POST target. `client` is a single shared
    /// `reqwest::Client` (connection pooling, one place to bound the
    /// request timeout) — never used to accept a connection, only to
    /// originate one.
    Http {
        url: String,
        client: reqwest::Client,
    },
}

/// Why a publish attempt to the sink failed, distinguishing whether a retry is
/// warranted.
enum SinkError {
    /// Network error or `5xx` — may well succeed on a later attempt.
    Retryable(String),
    /// `4xx`, or a local file-sink error (bad path/permissions) — retrying
    /// without operator intervention won't help; surface it instead.
    Terminal(String),
}

impl std::fmt::Display for SinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SinkError::Retryable(msg) | SinkError::Terminal(msg) => write!(f, "{msg}"),
        }
    }
}

/// The egress runtime: allowlist config + a durable delay buffer + the sink.
/// Constructed only when `egress` is present in config; cloned into both the
/// event-dispatch path (`consider`/`retract`) and the background flush task
/// (`run`).
pub struct Egress {
    rooms: HashSet<String>,
    deny_patterns: Vec<String>,
    delay_seconds: u64,
    sink: Sink,
    /// Durable delay buffer, mirroring `mailbox.rs`'s rusqlite-behind-a-Mutex
    /// shape. Keyed by `(room_id, event_id)` so an edit/redaction of the source
    /// event can find and suppress the pending row before it publishes.
    ///
    /// **Never held across a sink publish.** The network POST (the only part
    /// of this subsystem that can block on something outside our control) runs
    /// with this lock released — see [`Egress::publish_due`].
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
        // #31: guarded migration for `egress.sqlite3` files written by #30,
        // which predates the `attempts`/`failed` columns. `CREATE TABLE IF NOT
        // EXISTS` above is a no-op against an existing file, so these columns
        // must be added out-of-band rather than folded into the CREATE.
        add_column_if_missing(
            &conn,
            "pending_publish",
            "attempts",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column_if_missing(
            &conn,
            "pending_publish",
            "failed",
            "INTEGER NOT NULL DEFAULT 0",
        )?;

        let sink = match config.sink_url {
            Some(url) => Sink::Http {
                url,
                client: reqwest::Client::builder()
                    .timeout(HTTP_TIMEOUT)
                    .build()
                    .context("building egress HTTP client")?,
            },
            None => Sink::File(
                config
                    .sink_path
                    .context("egress config has neither sink_url nor sink_path (validate_egress_config should have caught this)")?,
            ),
        };

        Ok(Arc::new(Self {
            rooms: config.rooms.into_iter().collect(),
            deny_patterns: config.deny_patterns,
            delay_seconds: config.delay_seconds,
            sink,
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

    /// Flush every row whose delay has elapsed (`publish_after <= now`), was
    /// neither retracted, already published, nor given up on (`failed`):
    /// publish each to the sink and mark the outcome. Returns what was
    /// actually published. Idempotent across calls — a published row is never
    /// re-emitted.
    ///
    /// Shape: **select due rows under the lock, release it, publish (the only
    /// part that may hit the network), then re-acquire the lock per row just
    /// to record the outcome.** The lock is never held across a sink publish,
    /// so a slow/unreachable HTTP sink cannot stall `consider`/`retract`, which
    /// run on the live sync-event path and share this same connection.
    pub async fn publish_due(&self, now: i64) -> Result<Vec<PublishedRecord>> {
        let rows = {
            let conn = self.conn.lock().await;
            let mut stmt = conn
                .prepare(
                    "SELECT id, room_id, event_id, payload_json, attempts FROM pending_publish \
                     WHERE publish_after <= ?1 AND retracted = 0 AND published = 0 \
                     AND failed = 0 ORDER BY id ASC",
                )
                .context("preparing pending_publish query")?;
            let rows = stmt
                .query_map(params![now], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .context("querying pending_publish rows")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("reading pending_publish rows")?;
            rows
            // `conn`/`stmt` (and the lock guard) drop here, at the end of the
            // block — deliberately, before any row is published.
        };

        let mut published = Vec::with_capacity(rows.len());
        for (id, room_id, event_id, payload_json, attempts) in rows {
            let payload: Value =
                serde_json::from_str(&payload_json).context("decoding buffered egress payload")?;
            match self.publish_to_sink(&room_id, &event_id, &payload).await {
                Ok(()) => {
                    let conn = self.conn.lock().await;
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
                Err(SinkError::Terminal(msg)) => {
                    eprintln!(
                        "safehoused: egress publish for {event_id} in {room_id} failed \
                         permanently (non-retryable): {msg}"
                    );
                    let conn = self.conn.lock().await;
                    conn.execute(
                        "UPDATE pending_publish SET failed = 1 WHERE id = ?1",
                        params![id],
                    )
                    .context("marking pending_publish row failed")?;
                }
                Err(SinkError::Retryable(msg)) => {
                    let next_attempts = attempts + 1;
                    let conn = self.conn.lock().await;
                    if next_attempts >= MAX_PUBLISH_ATTEMPTS {
                        eprintln!(
                            "safehoused: egress publish for {event_id} in {room_id} failed \
                             permanently after {next_attempts} attempts: {msg}"
                        );
                        conn.execute(
                            "UPDATE pending_publish SET failed = 1, attempts = ?2 WHERE id = ?1",
                            params![id, next_attempts],
                        )
                        .context("marking pending_publish row failed after max attempts")?;
                    } else {
                        let backoff = retry_backoff_secs(next_attempts);
                        eprintln!(
                            "safehoused: egress publish for {event_id} in {room_id} failed \
                             (attempt {next_attempts}/{MAX_PUBLISH_ATTEMPTS}), retrying in \
                             {backoff}s: {msg}"
                        );
                        conn.execute(
                            "UPDATE pending_publish SET attempts = ?2, publish_after = ?3 \
                             WHERE id = ?1",
                            params![id, next_attempts, now + backoff],
                        )
                        .context("scheduling pending_publish retry")?;
                    }
                }
            }
        }
        Ok(published)
    }

    /// Publish one record to the configured sink. Never holds `self.conn`'s
    /// lock — callers (`publish_due`) must have already released it before
    /// calling this.
    async fn publish_to_sink(
        &self,
        room_id: &str,
        event_id: &str,
        payload: &Value,
    ) -> std::result::Result<(), SinkError> {
        match &self.sink {
            Sink::File(path) => Self::write_to_file_sink(path, room_id, event_id, payload)
                .map_err(|e| SinkError::Terminal(e.to_string())),
            Sink::Http { url, client } => {
                let body = json!({
                    "room_id": room_id,
                    "event_id": event_id,
                    "payload": payload,
                });
                let response = client.post(url).json(&body).send().await;
                match response {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            Ok(())
                        } else if status.is_server_error() {
                            // 5xx: the sink is having a bad time, not a bad
                            // request — worth retrying.
                            Err(SinkError::Retryable(format!(
                                "sink returned {status} (transient)"
                            )))
                        } else {
                            // 4xx and anything else unexpected: our request
                            // was rejected on its merits — a config/schema
                            // problem to surface, not a fault to retry away.
                            Err(SinkError::Terminal(format!(
                                "sink returned {status} (not retrying)"
                            )))
                        }
                    }
                    // A transport-level failure (DNS, connection refused,
                    // timeout, TLS handshake failure, ...) is always treated
                    // as transient — the operator's network/sink may recover.
                    Err(err) => Err(SinkError::Retryable(err.to_string())),
                }
            }
        }
    }

    /// Append one published record to the local JSON-lines sink (the #30
    /// default, kept for `sink_path`-only configs).
    fn write_to_file_sink(
        sink_path: &Path,
        room_id: &str,
        event_id: &str,
        payload: &Value,
    ) -> Result<()> {
        if let Some(parent) = sink_path.parent() {
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
            .open(sink_path)
            .with_context(|| format!("opening sink {}", sink_path.display()))?;
        writeln!(file, "{line}")
            .with_context(|| format!("writing to sink {}", sink_path.display()))?;
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

/// Add `column` to `table` if it isn't already present. `ALTER TABLE ... ADD
/// COLUMN` has no `IF NOT EXISTS` form, so this checks `pragma_table_info`
/// first — needed to safely open an `egress.sqlite3` file written by #30
/// (before `attempts`/`failed` existed) without erroring on every later boot.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let exists: bool = conn
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
        ))
        .context("preparing pragma_table_info query")?
        .exists(params![column])
        .context("checking for existing column")?;
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )
        .with_context(|| format!("adding {table}.{column}"))?;
    }
    Ok(())
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
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        sync::Mutex as TokioMutex,
    };

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
            sink_path: Some(sink),
            sink_url: None,
        }
    }

    fn http_config(rooms: &[&str], deny: &[&str], delay_seconds: u64, url: String) -> EgressConfig {
        EgressConfig {
            rooms: rooms.iter().map(|s| (*s).to_owned()).collect(),
            deny_patterns: deny.iter().map(|s| (*s).to_owned()).collect(),
            delay_seconds,
            sink_path: None,
            sink_url: Some(url),
        }
    }

    /// A unique scratch directory under the OS temp dir. Mirrors `mailbox.rs`'s
    /// test helper to avoid pulling in a `tempfile` dependency.
    ///
    /// pid + wall-clock nanos alone are not sufficient uniqueness under
    /// parallel test execution: macOS/APFS's `SystemTime::now()` granularity
    /// is coarser than 1ns, so two tests starting in the same burst can
    /// observe identical nanos and collide on the same directory name.
    /// `create_dir_all` succeeds silently on an existing dir, so the tests
    /// then share one `egress.sqlite3` — and whichever test finishes (and
    /// runs its trailing `remove_dir_all`) first deletes the file out from
    /// under the other test's still-open connection, which SQLite reports as
    /// `SQLITE_READONLY_DBMOVED` (#55). A process-wide atomic counter makes
    /// each call unique regardless of clock resolution.
    fn tempdir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "safehoused-egress-test-{}-{}-{}",
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

    /// A minimal single-purpose HTTP/1.1 server for tests — deliberately not a
    /// crate dependency (`wiremock`/`httpmock`): a bare `TcpListener` plus a
    /// hand-rolled request line/header/body reader is enough to assert what
    /// this feature needs (received JSON bodies, controllable status codes)
    /// without adding a new dev-dependency for it. Purely test-scope; this
    /// process still never listens as part of the shipped daemon (D8).
    ///
    /// Returns the sink URL and a handle to the JSON bodies POSTed to it, in
    /// arrival order. `statuses` is consumed one response per accepted
    /// connection; once exhausted, the last status repeats.
    async fn spawn_mock_sink(statuses: Vec<u16>) -> (String, Arc<TokioMutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received: Arc<TokioMutex<Vec<Value>>> = Arc::new(TokioMutex::new(Vec::new()));
        let received_task = received.clone();
        tokio::spawn(async move {
            let mut idx = 0usize;
            while let Ok((mut stream, _)) = listener.accept().await {
                let status = statuses
                    .get(idx)
                    .or_else(|| statuses.last())
                    .copied()
                    .unwrap_or(200);
                idx += 1;
                handle_mock_conn(&mut stream, status, &received_task).await;
            }
        });
        (format!("http://{addr}"), received)
    }

    async fn handle_mock_conn(
        stream: &mut tokio::net::TcpStream,
        status: u16,
        received: &Arc<TokioMutex<Vec<Value>>>,
    ) {
        let (read_half, mut write_half) = stream.split();
        let mut reader = BufReader::new(read_half);
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
            return;
        }
        if let Ok(json) = serde_json::from_slice::<Value>(&body) {
            received.lock().await.push(json);
        }
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let response =
            format!("HTTP/1.1 {status} {reason}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let _ = write_half.write_all(response.as_bytes()).await;
        let _ = write_half.flush().await;
    }

    /// A local address nothing is listening on — connecting to it fails fast
    /// (connection refused) rather than hanging, so the "unreachable sink"
    /// test doesn't have to wait out a full timeout.
    fn unreachable_addr() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
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

    #[test]
    fn config_guard_rejects_no_sink_configured() {
        let cfg = EgressConfig {
            rooms: vec![],
            deny_patterns: vec![],
            delay_seconds: 0,
            sink_path: None,
            sink_url: None,
        };
        assert!(validate_egress_config(&cfg).is_err());
    }

    #[test]
    fn config_guard_accepts_sink_url_alone() {
        let cfg = http_config(&[], &[], 0, "https://example.com/feed".to_owned());
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

    // ---- pre-existing `egress.sqlite3` migration (attempts/failed) ---------

    #[tokio::test]
    async fn opening_a_pre_31_store_adds_attempts_and_failed_columns() {
        let dir = tempdir();
        let db = dir.join("egress.sqlite3");
        {
            // Simulate a #30-vintage store: the base table, no attempts/failed.
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE pending_publish (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    room_id TEXT NOT NULL,
                    event_id TEXT NOT NULL,
                    publish_after INTEGER NOT NULL,
                    payload_json TEXT NOT NULL,
                    retracted INTEGER NOT NULL DEFAULT 0,
                    published INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
        }
        // Opening through Egress::open must migrate in the new columns rather
        // than erroring on the pre-existing table, and the migrated schema
        // must work end to end (enqueue + publish).
        let egress = Egress::open(
            config(&["!r:x"], &["nothing"], 0, dir.join("sink.jsonl")),
            &db,
        )
        .unwrap();
        egress
            .enqueue("!r:x", "$1", 0, &json!({"schema": "completion-v1"}))
            .await
            .unwrap();
        let due = egress.publish_due(0).await.unwrap();
        assert_eq!(due.len(), 1, "a migrated store must still publish");
        std::fs::remove_dir_all(dir).ok();
    }

    // ---- HTTP sink (#31) ----------------------------------------------------

    #[tokio::test]
    async fn http_sink_posts_expected_payload_only_after_delay() {
        let (url, received) = spawn_mock_sink(vec![200]).await;
        let egress = Egress::open_in_memory(http_config(&["!r:x"], &["nothing"], 0, url)).unwrap();
        let payload = redact(&completion_meta(), &["nothing".to_owned()]);
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();

        // Before the delay elapses: no request at all.
        let early = egress.publish_due(999).await.unwrap();
        assert!(early.is_empty());
        assert!(received.lock().await.is_empty(), "must not POST early");

        // At/after publish_after: exactly one POST, with the redacted payload
        // wrapped in the (room_id, event_id, payload) dedup envelope.
        let due = egress.publish_due(1_000).await.unwrap();
        assert_eq!(due.len(), 1);
        let bodies = received.lock().await;
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["room_id"], "!r:x");
        assert_eq!(bodies[0]["event_id"], "$1");
        assert_eq!(bodies[0]["payload"]["schema"], "completion-v1");
    }

    #[tokio::test]
    async fn http_sink_5xx_triggers_bounded_retry_then_succeeds() {
        let (url, received) = spawn_mock_sink(vec![500, 500, 200]).await;
        let egress = Egress::open_in_memory(http_config(&["!r:x"], &["nothing"], 0, url)).unwrap();
        let payload = redact(&completion_meta(), &["nothing".to_owned()]);
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();

        // Attempt 1: 500 -> retryable, row rescheduled (not published, not failed).
        let due = egress.publish_due(1_000).await.unwrap();
        assert!(due.is_empty());
        // A big future `now` clears whatever backoff was applied, so the next
        // poll retries immediately without the test needing to know the exact
        // backoff duration.
        let due = egress.publish_due(1_000_000).await.unwrap();
        assert!(due.is_empty(), "attempt 2 (still 500) must not publish");
        let due = egress.publish_due(2_000_000).await.unwrap();
        assert_eq!(due.len(), 1, "attempt 3 (200) must publish");
        assert_eq!(received.lock().await.len(), 3, "exactly 3 POSTs total");
    }

    #[tokio::test]
    async fn http_sink_4xx_does_not_retry() {
        let (url, received) = spawn_mock_sink(vec![400]).await;
        let egress = Egress::open_in_memory(http_config(&["!r:x"], &["nothing"], 0, url)).unwrap();
        let payload = redact(&completion_meta(), &["nothing".to_owned()]);
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();

        let due = egress.publish_due(1_000).await.unwrap();
        assert!(due.is_empty(), "a 4xx must not count as published");
        assert_eq!(received.lock().await.len(), 1, "exactly one attempt made");

        // A much later poll must not retry a 4xx-failed row at all.
        let due = egress.publish_due(1_000_000).await.unwrap();
        assert!(due.is_empty());
        assert_eq!(
            received.lock().await.len(),
            1,
            "a 4xx row must never be retried, even much later"
        );
    }

    #[tokio::test]
    async fn http_sink_gives_up_after_max_attempts_on_repeated_5xx() {
        let (url, received) = spawn_mock_sink(vec![500]).await;
        let egress = Egress::open_in_memory(http_config(&["!r:x"], &["nothing"], 0, url)).unwrap();
        let payload = redact(&completion_meta(), &["nothing".to_owned()]);
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();

        let mut now = 1_000i64;
        for _ in 0..MAX_PUBLISH_ATTEMPTS {
            let due = egress.publish_due(now).await.unwrap();
            assert!(due.is_empty());
            now += 1_000_000; // clear backoff unconditionally between polls
        }
        let attempts_made = received.lock().await.len();
        assert_eq!(
            attempts_made as i64, MAX_PUBLISH_ATTEMPTS,
            "must stop at the attempt cap"
        );

        // One more poll: the row is `failed` now, so no further request fires.
        let due = egress.publish_due(now).await.unwrap();
        assert!(due.is_empty());
        assert_eq!(
            received.lock().await.len(),
            attempts_made,
            "a failed row must never be retried again"
        );
    }

    #[tokio::test]
    async fn http_sink_unreachable_does_not_hang_or_crash() {
        let egress =
            Egress::open_in_memory(http_config(&["!r:x"], &["nothing"], 0, unreachable_addr()))
                .unwrap();
        let payload = redact(&completion_meta(), &["nothing".to_owned()]);
        egress.enqueue("!r:x", "$1", 1_000, &payload).await.unwrap();

        // Bounded by the test harness itself: if this hangs, the test times
        // out rather than the process. A connection-refused failure returns
        // near-instantly, well inside HTTP_TIMEOUT.
        let due = tokio::time::timeout(Duration::from_secs(5), egress.publish_due(1_000))
            .await
            .expect("publish_due must not hang against an unreachable sink")
            .unwrap();
        assert!(
            due.is_empty(),
            "an unreachable sink must not count as published"
        );
    }

    #[tokio::test]
    async fn http_sink_failure_does_not_block_consider_on_other_rooms() {
        // The core "must not stall the sync loop" property: even mid-retry
        // against a bad sink, `consider`/`retract` (the live event-dispatch
        // path, sharing the same connection) keep working.
        let (url, _received) = spawn_mock_sink(vec![500]).await;
        let egress = Egress::open_in_memory(http_config(&["!r:x"], &["nothing"], 0, url)).unwrap();
        let payload = redact(&completion_meta(), &["nothing".to_owned()]);
        egress
            .enqueue("!r:x", "$stuck", 1_000, &payload)
            .await
            .unwrap();
        let _ = egress.publish_due(1_000).await.unwrap();

        // consider() must still work immediately after a publish attempt.
        let queued = egress
            .consider("!r:x", "$new", &env("completion", Some(completion_meta())))
            .await
            .unwrap();
        assert!(queued);
    }
}
