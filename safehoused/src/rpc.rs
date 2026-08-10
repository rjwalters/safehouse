//! Unix-socket RPC — the only door agents have into the room.
//!
//! JSON lines, one object per line. First request must be
//! `{"op":"hello","persona":"..."}`; the daemon gates the persona against the
//! configured allowlist and stamps it into every outbound envelope (§6 — the
//! socket connection, not a message field, is the identity). After hello, the
//! daemon also pushes inbound room events to the connection as
//! `{"event":"message", ...}` lines (no `id`).

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use matrix_sdk::{
    deserialized_responses::SyncOrStrippedState,
    room::{MessagesOptions, ParentSpace},
    ruma::{
        api::client::room::create_room::v3::{CreationContent, Request as CreateRoomRequest},
        events::{
            space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
            SyncStateEvent,
        },
        room::RoomType,
        serde::Raw,
        OwnedServerName, OwnedUserId, RoomId,
    },
    Client, Room, RoomState,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, Mutex},
};

use crate::{
    envelope::{self, Envelope},
    mailbox::{Mailbox, MailboxEntry},
};

pub struct Registry {
    conns: Mutex<HashMap<u64, ConnHandle>>,
    next_id: std::sync::atomic::AtomicU64,
    pub personas: Vec<String>,
    /// `(matrix_sender, version)` pairs already surfaced to the human for an
    /// unsupported-version envelope (§7.2). Deduped so a remote daemon sending
    /// a stream of bad-version envelopes surfaces once, not once per event.
    surfaced_unsupported: Mutex<HashSet<(String, u64)>>,
    /// §2/§5.2 thread-relation bookkeeping, populated from the synced event
    /// stream (D6 — one code path) and consulted both when composing outbound
    /// `m.relates_to` and when routing un-tokened human thread replies.
    pub threads: ThreadState,
    /// The durable per-persona mailbox (D16/D17) — populated as the room
    /// stream is dispatched (`mailbox_deliver`), consumed via the `check` op.
    /// Delivery to a live connection above is a low-latency convenience; the
    /// mailbox is what makes receipt independent of being connected at all.
    mailbox: Mailbox,
    /// #85 — liveness/staleness observability, exposed via the `status` RPC
    /// op. `Instant`, not wall-clock time: the daemon only ever reports "how
    /// long ago", never an absolute timestamp, which sidesteps clock-skew
    /// concerns and is what an operator diagnosing "is this cut off" actually
    /// wants. Plain `std::sync::Mutex`, not the `tokio::sync::Mutex` used
    /// elsewhere in this struct — every critical section here is a single
    /// field read/write with no `.await` inside it, so a blocking mutex adds
    /// no async-runtime risk and keeps the call sites (an event handler hot
    /// path, a per-sync-response callback) synchronous.
    last_event_received_at: std::sync::Mutex<Option<Instant>>,
    /// The instant the most recently *completed* sync cycle finished (#85) —
    /// tracked separately from `last_event_received_at` so "the room is
    /// quiet" (events stopped, sync still completing) and "sync is not
    /// returning" (nothing completing at all) are distinguishable from each
    /// other, not just from healthy.
    last_sync_completed_at: std::sync::Mutex<Option<Instant>>,
    /// The in-progress sync retry/backoff attempt, if any (#85) — mirrors the
    /// existing `"sync error (attempt N), retrying in Ms"` log line
    /// (`main.rs`'s `retry_sync_attempts`). Cleared back to `None` the moment
    /// a sync cycle completes successfully, so `status`'s `connected` field
    /// reflects live reality, not a stale in-backoff snapshot.
    retry_state: std::sync::Mutex<Option<RetryState>>,
}

/// A snapshot of an in-progress sync retry/backoff cycle (#85). See
/// `Registry::retry_state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryState {
    pub attempt: u32,
    pub backoff_secs: u64,
}

/// In-memory thread bookkeeping. All state here is derived from observed
/// room events (`ThreadState::observe`, called once per dispatched envelope
/// in `on_message`) — never persisted, since it's rebuildable from room
/// history and the daemon is long-running (D6: the room, not local state, is
/// the source of truth; this is a local index over it, not a second copy).
#[derive(Default)]
pub struct ThreadState {
    /// `task_id` -> the event id of that task's thread root (§2). Recorded
    /// the first time a `task_id` is seen; never overwritten, so every
    /// message sharing a `task_id` threads under the same root.
    task_roots: Mutex<HashMap<String, String>>,
    /// thread root event id -> the most recent event id observed in that
    /// thread, for the `m.in_reply_to` rich-reply fallback (§2).
    latest_in_thread: Mutex<HashMap<String, String>>,
    /// thread root event id -> the persona most recently addressed within
    /// that thread, so a human's un-tokened in-thread reply (§5.2) still
    /// routes correctly.
    thread_target: Mutex<HashMap<String, String>>,
}

impl ThreadState {
    /// The thread root already recorded for `task_id`, if any.
    pub async fn root_for_task(&self, task_id: &str) -> Option<String> {
        self.task_roots.lock().await.get(task_id).cloned()
    }

    /// The most recent event id observed in the thread rooted at
    /// `root_event_id`, if any.
    pub async fn latest_in_thread(&self, root_event_id: &str) -> Option<String> {
        self.latest_in_thread
            .lock()
            .await
            .get(root_event_id)
            .cloned()
    }

    /// The persona currently addressed within the thread rooted at
    /// `root_event_id` (§5.2), if any.
    pub async fn target_for_thread(&self, root_event_id: &str) -> Option<String> {
        self.thread_target.lock().await.get(root_event_id).cloned()
    }

    /// Record that `event_id` (belonging to the thread rooted at
    /// `thread_root`) carried `env`. Called for every dispatched envelope —
    /// own, remote, or human-synthesized — so thread state always reflects
    /// what actually round-tripped through the room (D6).
    pub async fn observe(&self, thread_root: &str, event_id: &str, env: &Envelope) {
        self.latest_in_thread
            .lock()
            .await
            .insert(thread_root.to_owned(), event_id.to_owned());
        if let Some(task_id) = &env.task_id {
            self.task_roots
                .lock()
                .await
                .entry(task_id.clone())
                .or_insert_with(|| thread_root.to_owned());
        }
        // Only a persona-shaped `to` (never "*", never a Matrix user id) is
        // useful as a §5.2 routing target.
        if envelope::valid_persona(&env.to) {
            self.thread_target
                .lock()
                .await
                .insert(thread_root.to_owned(), env.to.clone());
        }
    }
}

struct ConnHandle {
    persona: String,
    tx: mpsc::UnboundedSender<String>,
}

impl Registry {
    pub fn new(personas: Vec<String>, mailbox: Mailbox) -> Arc<Self> {
        Arc::new(Self {
            conns: Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
            personas,
            surfaced_unsupported: Mutex::new(HashSet::new()),
            threads: ThreadState::default(),
            mailbox,
            last_event_received_at: std::sync::Mutex::new(None),
            last_sync_completed_at: std::sync::Mutex::new(None),
            retry_state: std::sync::Mutex::new(None),
        })
    }

    /// Record that a room event (message, invite, redaction, or
    /// undecryptable-event notice) was just received over sync (#85). Called
    /// from every relevant event handler in `main.rs` — not only
    /// `on_message` — since narration chatter, not only agent completions, is
    /// the sensitive liveness signal the issue calls out.
    pub fn record_event_received(&self) {
        *self.last_event_received_at.lock().unwrap() = Some(Instant::now());
    }

    /// Record that a sync cycle just completed successfully (#85), and clear
    /// any in-progress retry/backoff state — a completed cycle is definitive
    /// proof the connection is no longer in backoff.
    pub fn record_sync_completed(&self) {
        *self.last_sync_completed_at.lock().unwrap() = Some(Instant::now());
        *self.retry_state.lock().unwrap() = None;
    }

    /// Record that the sync loop is about to sleep before retry attempt
    /// `attempt`, backing off for `backoff_secs` (#85) — called alongside the
    /// existing `"sync error (attempt N), retrying in Ms"` log line in
    /// `main.rs::retry_sync_attempts`, so the two never drift apart.
    pub fn record_retry_attempt(&self, attempt: u32, backoff_secs: u64) {
        *self.retry_state.lock().unwrap() = Some(RetryState {
            attempt,
            backoff_secs,
        });
    }

    /// The `status` RPC op's payload (#85): how long ago the last room event
    /// and last completed sync cycle were, plus the in-progress retry attempt
    /// if the sync loop is currently backing off. Deliberately reports
    /// elapsed seconds, never an absolute timestamp — an operator
    /// disambiguating "healthy and idle" from "cut off" wants "how long ago",
    /// which sidesteps clock-skew entirely. A field that has never fired
    /// (e.g. no sync has completed yet) is `null`, not a bogus zero.
    pub fn status(&self) -> Value {
        let now = Instant::now();
        let secs_ago = |at: Option<Instant>| at.map(|t| now.saturating_duration_since(t).as_secs());
        let last_event = secs_ago(*self.last_event_received_at.lock().unwrap());
        let last_sync = secs_ago(*self.last_sync_completed_at.lock().unwrap());
        let retry = *self.retry_state.lock().unwrap();
        json!({
            "ok": true,
            "connected": retry.is_none(),
            "last_event_received_secs_ago": last_event,
            "last_sync_completed_secs_ago": last_sync,
            "retry_attempt": retry.map(|r| r.attempt),
            "retry_backoff_secs": retry.map(|r| r.backoff_secs),
            // #95: same advertisement as the `hello` reply, on the one op that
            // needs no persona — so "connected, but which types does it know?"
            // is answerable by a healthcheck, not only by an agent that has
            // already handshaked.
            "known_types": envelope::KNOWN_TYPES,
        })
    }

    /// Record that an unsupported-version envelope from `sender` at `version`
    /// is about to be surfaced, returning `true` only the first time the pair
    /// is seen. Callers surface to the human iff this returns `true`, so a
    /// flood of bad-version events from one sender is logged but surfaced once.
    pub async fn mark_unsupported_surfaced(&self, sender: &str, version: u64) -> bool {
        self.surfaced_unsupported
            .lock()
            .await
            .insert((sender.to_owned(), version))
    }

    /// Deliver an inbound envelope to connected agents. The author persona is
    /// excluded when the event came from this daemon's own Matrix user — that
    /// is the loop-back rule (§7) refined for same-host traffic: local agents
    /// still hear each other; only the author is skipped.
    pub async fn dispatch(&self, line: &str, own_event: bool, author: &str) {
        let conns = self.conns.lock().await;
        for handle in conns.values() {
            if own_event && handle.persona == author {
                continue;
            }
            let _ = handle.tx.send(line.to_owned());
        }
    }

    /// Route one inbound envelope into the durable mailbox of every
    /// locally-hosted persona it's addressed to, per envelope-v1 §7's
    /// resolution rules — a broadcast (`to: "*"`) fans out to every
    /// registered persona; a direct `to:` lands only in that persona's
    /// mailbox; anything else (a persona hosted elsewhere, or a Matrix user
    /// id) is not ours to keep. This is what makes receipt independent of
    /// whether any agent is connected right now (D16/D17) — unlike the live
    /// `dispatch` above, this always runs, for every event.
    pub async fn mailbox_deliver(
        &self,
        own_event: bool,
        room_id: &str,
        event_id: &str,
        sender: &str,
        env: &Envelope,
    ) -> anyhow::Result<()> {
        for persona in self.mailbox_recipients(own_event, env) {
            self.mailbox
                .deliver(persona, room_id, event_id, sender, env)
                .await?;
        }
        Ok(())
    }

    /// The set of locally-registered personas that should receive `env` in
    /// their mailbox (§7). Same-host loop-back rule: when the Matrix sender
    /// is this daemon's own user, the authoring persona is skipped — same
    /// nuance as `dispatch`'s `own_event`/`author` handling.
    fn mailbox_recipients(&self, own_event: bool, env: &Envelope) -> Vec<&str> {
        if env.to == "*" {
            self.personas
                .iter()
                .map(String::as_str)
                .filter(|p| !(own_event && *p == env.from))
                .collect()
        } else if own_event && env.to == env.from {
            // Nonsensical (an agent addressing itself) but guard it anyway —
            // never let an event deliver to its own author on same-host loop
            // back.
            Vec::new()
        } else {
            self.personas
                .iter()
                .map(String::as_str)
                .filter(|p| *p == env.to)
                .collect()
        }
    }

    /// Unread mailbox envelopes for `persona` — the `check` op / MCP tool.
    /// See [`Mailbox::check`] for the peek/limit/cursor semantics.
    pub async fn check(
        &self,
        persona: &str,
        advance: bool,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<MailboxEntry>> {
        self.mailbox.check(persona, advance, limit).await
    }
}

pub async fn serve(client: Client, registry: Arc<Registry>, socket_path: PathBuf) -> Result<()> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;
    println!("safehoused: rpc listening on {}", socket_path.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let client = client.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_conn(stream, client, registry).await {
                eprintln!("safehoused: rpc connection error: {err:#}");
            }
        });
    }
}

async fn handle_conn(stream: UnixStream, client: Client, registry: Arc<Registry>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let mut conn_id: Option<u64> = None;
    let mut persona: Option<String> = None;

    loop {
        tokio::select! {
            pushed = rx.recv() => {
                let Some(pushed) = pushed else { break };
                write_half.write_all(pushed.as_bytes()).await?;
                write_half.write_all(b"\n").await?;
            }
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                if line.trim().is_empty() { continue; }
                let req: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        let reply = json!({"ok": false, "error": format!("bad json: {e}")});
                        write_half.write_all(reply.to_string().as_bytes()).await?;
                        write_half.write_all(b"\n").await?;
                        continue;
                    }
                };
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let op = req.get("op").and_then(Value::as_str).unwrap_or("");

                let mut reply = if op == "hello" {
                    match req.get("persona").and_then(Value::as_str) {
                        Some(p) if envelope::valid_persona(p)
                            && registry.personas.iter().any(|x| x == p) =>
                        {
                            let cid = registry
                                .next_id
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            registry.conns.lock().await.insert(cid, ConnHandle {
                                persona: p.to_owned(),
                                tx: tx.clone(),
                            });
                            conn_id = Some(cid);
                            persona = Some(p.to_owned());
                            json!({
                                "ok": true,
                                "user_id": client.user_id().map(|u| u.to_string()),
                                "device_id": client.device_id().map(|d| d.to_string()),
                                // #95: advertise the envelope `type`s this
                                // build knows, so a newer caller can detect
                                // skew at handshake instead of inferring it
                                // from a degraded message (or a log line on
                                // someone else's host). Purely additive to
                                // the *local socket* reply — the Matrix wire
                                // format is untouched, and a caller that
                                // ignores this field behaves exactly as
                                // before.
                                "known_types": envelope::KNOWN_TYPES,
                            })
                        }
                        Some(p) => json!({
                            "ok": false,
                            "error": format!("persona {p:?} not in allowlist (config `personas`)"),
                        }),
                        None => json!({"ok": false, "error": "hello requires persona"}),
                    }
                } else if op == "status" {
                    // #85: deliberately queryable *before* `hello` — unlike
                    // every other op, which requires an authenticated persona
                    // (gated below). A wedged daemon is exactly the scenario
                    // where a lower-friction healthcheck matters most, and
                    // `status` carries no persona-specific data to protect —
                    // it's daemon-wide liveness, safe for any local caller
                    // that can open the unix socket at all.
                    registry.status()
                } else if persona.is_none() {
                    json!({"ok": false, "error": "hello first"})
                } else {
                    match handle_op(op, &req, persona.as_deref().unwrap(), &client, &registry).await
                    {
                        Ok(v) => v,
                        Err(e) => json!({"ok": false, "error": format!("{e:#}")}),
                    }
                };
                if let Some(obj) = reply.as_object_mut() {
                    obj.insert("id".into(), id);
                }
                write_half.write_all(reply.to_string().as_bytes()).await?;
                write_half.write_all(b"\n").await?;
            }
        }
    }
    if let Some(cid) = conn_id {
        registry.conns.lock().await.remove(&cid);
    }
    Ok(())
}

async fn handle_op(
    op: &str,
    req: &Value,
    persona: &str,
    client: &Client,
    registry: &Registry,
) -> Result<Value> {
    match op {
        "send" => {
            let OutboundSend { env, degraded_from } = build_send_envelope(persona, req)?;
            let room = resolve_room(client, req.get("room").and_then(Value::as_str))?;

            // §2: task/handoff chains sharing a `task_id` thread under that
            // task's root event. If we already know the root (from an
            // earlier send, or from having observed one over sync — D6),
            // attach native `m.thread` threading; otherwise this send
            // *becomes* the root and carries no relation.
            let known_root = match &env.task_id {
                Some(task_id) => registry.threads.root_for_task(task_id).await,
                None => None,
            };
            let relates_to = if let Some(root) = &known_root {
                let latest = registry
                    .threads
                    .latest_in_thread(root)
                    .await
                    .unwrap_or_else(|| root.clone());
                Some(envelope::thread_relation(root, &latest))
            } else {
                None
            };

            let content = envelope::to_event_content(&env, relates_to);
            let response = room
                .send_raw("m.room.message", content)
                .await
                .context("sending to room")?;
            let event_id = response.response.event_id.to_string();

            // Self-register the thread state immediately rather than only
            // relying on this event round-tripping back through sync — that
            // closes the race where two rapid sends for a brand-new
            // `task_id` would each miss the other's root (§2).
            if env.task_id.is_some() {
                let root = known_root.unwrap_or_else(|| event_id.clone());
                registry.threads.observe(&root, &event_id, &env).await;
            }

            Ok(send_reply(
                &event_id,
                room.room_id().as_str(),
                &env.kind,
                degraded_from.as_deref(),
            ))
        }
        "create_room" => {
            let name = req
                .get("name")
                .and_then(Value::as_str)
                .context("create_room requires `name`")?;
            // A Space (`m.space`) is a container of rooms, not a message room.
            let is_space = req.get("space").and_then(Value::as_bool).unwrap_or(false);
            // Optionally create the room already linked under an existing space,
            // resolved through the same id/name/alias path as everything else.
            let parent = match req.get("parent").and_then(Value::as_str) {
                Some(p) => {
                    let parent = resolve_room(client, Some(p))?;
                    // Symmetry with `add_to_space`: refuse to write an
                    // `m.space.child` into a plain message room.
                    anyhow::ensure!(
                        parent.is_space(),
                        "parent room {:?} is not a Space (m.space)",
                        parent.room_id().as_str()
                    );
                    Some(parent)
                }
                None => None,
            };
            let mut request = CreateRoomRequest::new();
            request.name = Some(name.to_owned());
            if is_space {
                // `m.space` is set via the `m.room.create` content's `type`,
                // carried through `creation_content` (there is no top-level
                // `room_type` on the createRoom request in this ruma version).
                let mut creation = CreationContent::new();
                creation.room_type = Some(RoomType::Space);
                request.creation_content =
                    Some(Raw::new(&creation).context("serializing creation_content")?);
            }
            if let Some(invites) = req.get("invite").and_then(Value::as_array) {
                for user in invites {
                    let user = user
                        .as_str()
                        .context("invite entries must be user id strings")?;
                    request
                        .invite
                        .push(user.try_into().context("invalid user id in invite")?);
                }
            }
            let room = client.create_room(request).await?;
            // Encryption decision (see issue #27 / D5): a Space carries no
            // messages — only `m.space.child`/`m.space.parent` state — so D5's
            // "every meaningful message goes through the encrypted room"
            // rationale does not apply, and Element's own convention leaves
            // Spaces unencrypted. Only message rooms get encryption enabled.
            if !is_space {
                room.enable_encryption().await?;
            }
            if let Some(parent) = &parent {
                link_room_to_space(client, parent, &room).await?;
            }
            // Read-your-writes (#58): the SDK stores the new room immediately,
            // but its name (`m.room.name`) and room type (`m.room.create`) only
            // land with the next sync — until then `resolve_room` by name and
            // `is_space` cannot see what this op just reported creating, and
            // the natural "create Space, then create child into it by name"
            // sequence fails. Wait (bounded) for the state to arrive before
            // acknowledging, so a follow-up op can address the room by `name`.
            await_room_name_visible(client, room.room_id(), name).await;
            Ok(json!({
                "ok": true,
                "room_id": room.room_id(),
                "name": name,
                "type": if is_space { "space" } else { "room" },
                "parent_space": parent.as_ref().map(|p| p.room_id().to_string()),
            }))
        }
        "invite" => {
            // New-host onboarding (issue #39): validate `user` before
            // resolving the room, so a malformed/missing user id is reported
            // on its own terms rather than being masked by room-resolution
            // failure. The daemon on the *receiving* end auto-joins via
            // `on_invite` (main.rs) — this op is only the sending half.
            let user = req
                .get("user")
                .and_then(Value::as_str)
                .context("invite requires `user`")?;
            let user_id: OwnedUserId = user
                .try_into()
                .with_context(|| format!("invalid user id {user:?}"))?;
            let room = resolve_room(client, req.get("room").and_then(Value::as_str))
                .context("resolving `room`")?;
            room.invite_user_by_id(&user_id)
                .await
                .with_context(|| format!("inviting {user} to {}", room.room_id()))?;
            Ok(json!({
                "ok": true,
                "room_id": room.room_id(),
                "user": user,
            }))
        }
        "add_to_space" => {
            let space = resolve_room(client, req.get("space").and_then(Value::as_str))
                .context("resolving `space`")?;
            anyhow::ensure!(
                space.is_space(),
                "room {:?} is not a Space (m.space)",
                space.room_id().as_str()
            );
            let room = resolve_room(client, req.get("room").and_then(Value::as_str))
                .context("resolving `room`")?;
            // Idempotent: only skip the write when *both* halves are already
            // present. A half-link (one side written, the other lost to a
            // mid-write crash) reports `false` and is repaired by re-running the
            // overwrite-idempotent link — retrying must never error or duplicate.
            let already = space_child_fully_linked(&space, &room).await?;
            if !already {
                link_room_to_space(client, &space, &room).await?;
            }
            Ok(json!({
                "ok": true,
                "space": space.room_id(),
                "room": room.room_id(),
                "already_linked": already,
            }))
        }
        "list_rooms" => {
            let mut rooms = Vec::new();
            for room in client.joined_rooms() {
                rooms.push(json!({
                    "room_id": room.room_id(),
                    "name": room.name(),
                    "encrypted": room.latest_encryption_state().await.map(|s| s.is_encrypted()).unwrap_or(false),
                    // A client can render/verify the fleet hierarchy from these
                    // two fields: whether this entry is a Space container, and
                    // (for a message room) the id of its confirmed parent Space.
                    "type": if room.is_space() { "space" } else { "room" },
                    "parent_space": reciprocal_parent_space(&room).await,
                }));
            }
            Ok(json!({"ok": true, "rooms": rooms}))
        }
        "read" => {
            let room = resolve_room(client, req.get("room").and_then(Value::as_str))?;
            let limit = req
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .min(100);
            let mut options = MessagesOptions::backward();
            options.limit = (limit as u32).into();
            let batch = room.messages(options).await?;
            let own = client.user_id().map(|u| u.to_string()).unwrap_or_default();
            let mut messages = Vec::new();
            for event in batch.chunk.iter().rev() {
                let Ok(parsed) = serde_json::from_str::<Value>(event.raw().json().get()) else {
                    continue;
                };
                if parsed.get("type").and_then(Value::as_str) != Some("m.room.message") {
                    continue;
                }
                let sender = parsed
                    .get("sender")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let content = parsed.get("content").cloned().unwrap_or(Value::Null);
                let mut message = json!({
                    "event_id": parsed.get("event_id"),
                    "sender": sender,
                    "own": sender == own,
                    "ts": parsed.get("origin_server_ts"),
                });
                let obj = message.as_object_mut().expect("json object literal");
                // §5.2: resolve the thread agent the same way the live
                // dispatch path does, so a replayed thread reply synthesizes
                // the same envelope it would have gotten in real time.
                let event_id = parsed.get("event_id").and_then(Value::as_str);
                let thread_root = envelope::thread_root_from_content(&content)
                    .or_else(|| event_id.map(str::to_owned));
                let thread_agent = match &thread_root {
                    Some(root) => registry.threads.target_for_thread(root).await,
                    None => None,
                };
                // §7.2: never hand an agent a guessed envelope for a version we
                // don't support — mark it so the agent can surface, not act.
                match envelope::from_event_json(
                    &content,
                    &sender,
                    &registry.personas,
                    thread_agent.as_deref(),
                ) {
                    envelope::Inbound::Envelope(env, _unknown_persona) => {
                        obj.insert("envelope".into(), serde_json::to_value(env)?);
                    }
                    envelope::Inbound::UnsupportedVersion(v) => {
                        obj.insert("unsupported_version".into(), json!(v));
                    }
                }
                messages.push(message);
            }
            Ok(json!({"ok": true, "room_id": room.room_id(), "messages": messages}))
        }
        "check" => {
            // Peek mode (no-advance) and `limit`, per the issue spec.
            // Default is to advance the cursor past everything returned.
            let peek = req.get("peek").and_then(Value::as_bool).unwrap_or(false);
            let limit = req
                .get("limit")
                .and_then(Value::as_u64)
                .map(|l| l.min(1000) as u32);
            let entries = registry.check(persona, !peek, limit).await?;
            let messages: Vec<Value> = entries
                .into_iter()
                .map(|e| {
                    json!({
                        "room_id": e.room_id,
                        "event_id": e.event_id,
                        "sender": e.sender,
                        "envelope": e.envelope,
                    })
                })
                .collect();
            Ok(json!({"ok": true, "advanced": !peek, "messages": messages}))
        }
        other => anyhow::bail!("unknown op {other:?}"),
    }
}

/// Build the outbound envelope for a `send` request. `persona` comes from the
/// authenticated connection (§6 — the socket connection is the identity), not
/// from `req`; the request is never even inspected for a `from` field, so a
/// client cannot spoof it no matter what it sends.
fn build_send_envelope(persona: &str, req: &Value) -> Result<OutboundSend> {
    let to = req
        .get("to")
        .and_then(Value::as_str)
        .context("send requires `to`")?;
    let body = req
        .get("body")
        .and_then(Value::as_str)
        .context("send requires `body`")?;
    let requested = req.get("type").and_then(Value::as_str).unwrap_or("chat");
    // §9 (#95): a type this build doesn't know is **degraded to `chat`, not
    // rejected** — on this path too, not just on ingest. New types are additive
    // and don't bump `v`, so the common cause is a caller that is simply newer
    // than this daemon; refusing the send is the one outcome that actually
    // loses the message, since the RPC caller does not retry as `chat` itself.
    // The trade-off is deliberate: an agent that mistypes `type` no longer gets
    // an error. The daemon's warning goes to *its* stderr, which a caller on the
    // far side of the socket never reads, and `known_types` only helps a caller
    // that diffs it up front — so the degrade is also reported back in-band on
    // the `send` reply (see `OutboundSend::degraded_from`).
    let kind = envelope::degrade_unknown_type(requested, "send");
    let degraded_from = (kind != requested).then(|| requested.to_owned());
    let task_id = req
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(t) = &task_id {
        anyhow::ensure!(
            t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "task_id must be [A-Za-z0-9_]"
        );
    }
    // Advisory only (D16) — the daemon never acts on this, it just carries it
    // through so a receiver's `check` output preserves the sender's hint.
    let wake = req.get("wake").and_then(Value::as_bool);
    // §4a: agent-originated `completion` meta (#30). Unlike ingest — which
    // degrades an invalid completion to `chat` (D18) — an agent's *own* send is
    // rejected outright on bad meta rather than silently downgraded, so the
    // agent learns its completion won't be feed-eligible instead of discovering
    // it vanished. `meta` is only meaningful for `completion`; supplying it on
    // any other type is a request error, and a `completion` without valid
    // `completion-v1` meta is refused here.
    let meta = match req.get("meta") {
        Some(m) if !m.is_null() => {
            anyhow::ensure!(
                kind == "completion",
                "`meta` is only valid for type \"completion\", not {requested:?}"
            );
            envelope::validate_completion_meta(m)
                .map_err(|e| anyhow::anyhow!("invalid completion-v1 meta: {e}"))?;
            Some(m.clone())
        }
        _ => {
            anyhow::ensure!(
                kind != "completion",
                "type \"completion\" requires a `meta` object (completion-v1)"
            );
            None
        }
    };
    Ok(OutboundSend {
        env: Envelope {
            v: 1,
            from: persona.to_owned(), // stamped here; never taken from the request
            to: to.to_owned(),
            kind: kind.to_owned(),
            task_id,
            body: body.to_owned(),
            wake,
            meta,
        },
        degraded_from,
    })
}

/// The `"send"` success reply.
///
/// `type` is always present — it is what actually went on the wire, which is
/// not necessarily what the caller asked for (§9 degrade). `degraded_from`
/// appears **only** when the two differ, so an honored send keeps exactly the
/// reply shape it has always had (purely additive `type`), while a degraded one
/// is detectable with a single key lookup instead of by reading the daemon's
/// stderr on another host (#95).
fn send_reply(event_id: &str, room_id: &str, kind: &str, degraded_from: Option<&str>) -> Value {
    let mut reply = json!({
        "ok": true,
        "event_id": event_id,
        "room_id": room_id,
        "type": kind,
    });
    if let Some(requested) = degraded_from {
        reply["degraded_from"] = json!(requested);
    }
    reply
}

/// The result of building an outbound `send`: the envelope to put on the wire,
/// plus the §9 skew signal the RPC reply owes the caller.
#[derive(Debug)]
struct OutboundSend {
    env: Envelope,
    /// `Some(requested)` when the caller named a `type` this build does not
    /// know and it was degraded to `chat` (#95). Without this the reply is
    /// byte-identical to a fully honored send, so a typo'd or newer-than-daemon
    /// `type` is 100% silent from the caller's seat: the once-per-session
    /// warning lands on the daemon's stderr (possibly another host), and the
    /// `known_types` advertisement only helps a caller that actively diffs it.
    degraded_from: Option<String>,
}

/// A joined room's addressable identifiers, projected out of a `Room` so the
/// resolution logic (`pick_room_index`) is pure and unit-testable without a
/// live homeserver — a `matrix_sdk::Room` can't be constructed offline.
struct RoomAddr {
    id: String,
    name: Option<String>,
    canonical_alias: Option<String>,
    alt_aliases: Vec<String>,
}

impl RoomAddr {
    fn of(room: &Room) -> Self {
        RoomAddr {
            id: room.room_id().to_string(),
            name: room.name(),
            canonical_alias: room.canonical_alias().map(|a| a.as_str().to_owned()),
            alt_aliases: room
                .alt_aliases()
                .iter()
                .map(|a| a.as_str().to_owned())
                .collect(),
        }
    }

    /// Whether `s` names this room — by id, display name, canonical alias, or
    /// any alt alias (the issue's accepted address forms).
    fn matches(&self, s: &str) -> bool {
        self.id == s
            || self.name.as_deref() == Some(s)
            || self.canonical_alias.as_deref() == Some(s)
            || self.alt_aliases.iter().any(|a| a == s)
    }
}

/// Decide which room a spec resolves to. Returns the index into `rooms`.
///
/// Ambiguity is an **error**, never a silent first-match: once the fleet has
/// several similarly-named rooms, guessing would misroute. A `None` spec is
/// only valid when exactly one room is joined.
fn pick_room_index(rooms: &[RoomAddr], spec: Option<&str>) -> Result<usize> {
    match spec {
        Some(s) => {
            let matches: Vec<usize> = rooms
                .iter()
                .enumerate()
                .filter(|(_, r)| r.matches(s))
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                [only] => Ok(*only),
                [] => anyhow::bail!("no joined room matching {s:?}"),
                many => anyhow::bail!(
                    "ambiguous room spec {s:?}: {} joined rooms match",
                    many.len()
                ),
            }
        }
        None if rooms.len() == 1 => Ok(0),
        None => anyhow::bail!("`room` required: {} rooms joined", rooms.len()),
    }
}

/// How long `create_room` waits for the concurrent sync loop to deliver the
/// new room's state before acknowledging anyway (#58). Normally one sync
/// round-trip (well under a second); the ceiling only matters when the
/// homeserver is struggling, where blocking the RPC longer helps nobody.
const CREATE_ROOM_VISIBILITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll the store until the just-created `room_id` resolves to `name`, or the
/// bounded wait expires. Timeout is soft: the room exists either way, only
/// by-name addressing lags until the next sync — log it and move on.
async fn await_room_name_visible(client: &Client, room_id: &RoomId, name: &str) {
    let deadline = tokio::time::Instant::now() + CREATE_ROOM_VISIBILITY_TIMEOUT;
    loop {
        if let Some(room) = client.get_room(room_id) {
            if room.name().as_deref() == Some(name) {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "safehoused: created room {room_id} still not name-resolvable after \
                 {CREATE_ROOM_VISIBILITY_TIMEOUT:?} — by-name addressing lags until the next sync"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Room by id, name, or alias, or the sole joined room when unambiguous.
/// Ambiguous specs (more than one match) error rather than guessing.
fn resolve_room(client: &Client, spec: Option<&str>) -> Result<Room> {
    let joined: Vec<Room> = client
        .joined_rooms()
        .into_iter()
        .filter(|r| r.state() == RoomState::Joined)
        .collect();
    let addrs: Vec<RoomAddr> = joined.iter().map(RoomAddr::of).collect();
    let idx = pick_room_index(&addrs, spec)?;
    Ok(joined.into_iter().nth(idx).expect("index within bounds"))
}

/// The server name to advertise in `m.space.child`/`m.space.parent` `via`
/// lists — this daemon's own homeserver, the one guaranteed to be in the room.
fn own_server_name(client: &Client) -> Result<OwnedServerName> {
    Ok(client
        .user_id()
        .context("client has no user id (not logged in?)")?
        .server_name()
        .to_owned())
}

/// Link `child` under `space` by writing both halves of the reciprocal
/// `m.space` relationship (spec §m.space.child / §m.space.parent): the
/// `m.space.child` in the space keyed by the child's id, and the
/// `m.space.parent` in the child keyed by the space's id. State events are
/// keyed by (type, state_key), so re-writing is inherently idempotent — no
/// duplicate rows, just an overwrite with identical content.
async fn link_room_to_space(client: &Client, space: &Room, child: &Room) -> Result<()> {
    let via = vec![own_server_name(client)?];
    space
        .send_state_event_for_key(child.room_id(), SpaceChildEventContent::new(via.clone()))
        .await
        .context("writing m.space.child in the space")?;
    let mut parent = SpaceParentEventContent::new(via);
    parent.canonical = true; // the only parent we set, so it's canonical
    child
        .send_state_event_for_key(space.room_id(), parent)
        .await
        .context("writing m.space.parent in the child room")?;
    Ok(())
}

/// Whether `space` and `child` already advertise *both* halves of the
/// reciprocal `m.space` link — the child-side `m.space.child` in the space and
/// the parent-side `m.space.parent` in the child, each with a non-empty `via`.
/// This is the idempotency guard for `add_to_space`: only a *full* link short-
/// circuits the write. A half-link (one side written, the other missing because
/// a crash landed between the two `send_state_event_for_key` calls) reports
/// `false`, so the caller re-runs the overwrite-idempotent link and repairs the
/// missing half rather than skipping it.
async fn space_child_fully_linked(space: &Room, child: &Room) -> Result<bool> {
    let child_side = space
        .get_state_event_static_for_key::<SpaceChildEventContent, _>(child.room_id())
        .await?;
    let has_child_side = matches!(
        child_side.map(|raw| raw.deserialize()),
        Some(Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(e)))) if !e.content.via.is_empty()
    );
    if !has_child_side {
        return Ok(false);
    }
    let parent_side = child
        .get_state_event_static_for_key::<SpaceParentEventContent, _>(space.room_id())
        .await?;
    Ok(matches!(
        parent_side.map(|raw| raw.deserialize()),
        Some(Ok(SyncOrStrippedState::Sync(SyncStateEvent::Original(e)))) if !e.content.via.is_empty()
    ))
}

/// The room id of `room`'s confirmed parent Space, if any: the first
/// `ParentSpace::Reciprocal` from `room.parent_spaces()`. Only a reciprocal
/// relationship (parent and child both advertise each other) is a *confirmed*
/// parent — `WithPowerlevel`/`Illegitimate`/`Unverifiable` are not presented.
async fn reciprocal_parent_space(room: &Room) -> Option<String> {
    let stream = room.parent_spaces().await.ok()?;
    futures_util::pin_mut!(stream);
    while let Some(parent) = stream.next().await {
        if let Ok(ParentSpace::Reciprocal(space)) = parent {
            return Some(space.room_id().to_string());
        }
    }
    None
}

/// Integration tests for the unix-socket RPC protocol.
///
/// These drive the real `handle_conn` connection loop over a genuine
/// `tokio::net::UnixStream` pair, exactly as `serve()` does per accepted
/// connection — no code path is duplicated for testing. The one piece that
/// would otherwise require a live homeserver, `matrix_sdk::Client`, is built
/// against an unroutable loopback URL: `Client::builder().build()` performs
/// no network I/O by itself (no login, no sync), so this is a genuine
/// no-network mock client, not a live/gated test. The trade-off is that ops
/// requiring a joined room (`send`'s room resolution, `read`) can't complete
/// end-to-end here — those are exercised at the unit level instead
/// (`build_send_envelope` below, and `envelope::from_event_json`'s tests).
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
    };

    use super::*;

    /// A `Client` that performs no network I/O: `homeserver_url` skips
    /// server-name discovery, and `build()` does not log in or sync.
    async fn offline_client() -> Client {
        Client::builder()
            .homeserver_url("http://127.0.0.1:1")
            .build()
            .await
            .expect("building a client does not require network access")
    }

    /// Spawn `handle_conn` on one end of a socket pair, returning the other
    /// end (split into a writer and a buffered line reader) for the test to
    /// drive, plus the `Registry` so tests can exercise `dispatch` directly.
    async fn spawn_conn(
        personas: Vec<String>,
    ) -> (
        tokio::net::unix::OwnedWriteHalf,
        BufReader<tokio::net::unix::OwnedReadHalf>,
        Arc<Registry>,
    ) {
        let (server, client_side) = UnixStream::pair().expect("socketpair");
        let mailbox = Mailbox::open_in_memory().expect("in-memory mailbox for tests");
        let registry = Registry::new(personas, mailbox);
        let client = offline_client().await;
        let conn_registry = registry.clone();
        tokio::spawn(async move {
            let _ = handle_conn(server, client, conn_registry).await;
        });
        let (read_half, write_half) = client_side.into_split();
        (write_half, BufReader::new(read_half), registry)
    }

    async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, req: Value) {
        let mut line = req.to_string();
        line.push('\n');
        write
            .write_all(line.as_bytes())
            .await
            .expect("write request line");
    }

    async fn recv(read: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
        let mut line = String::new();
        read.read_line(&mut line).await.expect("read response line");
        serde_json::from_str(line.trim()).expect("response is valid json")
    }

    #[tokio::test]
    async fn hello_accepts_allowlisted_persona() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], true);
    }

    #[tokio::test]
    async fn hello_rejects_persona_not_in_allowlist() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "research_agent"}),
        )
        .await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"]
            .as_str()
            .unwrap()
            .contains("not in allowlist"));
    }

    #[tokio::test]
    async fn hello_rejects_syntactically_invalid_persona_even_if_configured() {
        // A misconfigured allowlist entry (uppercase/hyphenated) must never
        // let a connection through: `valid_persona` is enforced independently
        // of whatever the operator put in `personas`.
        let (mut write, mut read, _registry) = spawn_conn(vec!["Bad-Name".to_owned()]).await;
        send(&mut write, json!({"op": "hello", "persona": "Bad-Name"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
    }

    #[tokio::test]
    async fn hello_requires_persona_field() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(&mut write, json!({"op": "hello"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"]
            .as_str()
            .unwrap()
            .contains("requires persona"));
    }

    /// #95: the handshake advertises the type vocabulary, so a caller newer
    /// than this daemon can see the skew up front instead of inferring it from
    /// a message that came back degraded.
    #[tokio::test]
    async fn hello_advertises_known_types() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["known_types"], json!(envelope::KNOWN_TYPES));
        assert!(reply["known_types"]
            .as_array()
            .unwrap()
            .contains(&json!("digest")));
    }

    // ---- `status` op (#85) — liveness/staleness observability -------------

    /// #95: same advertisement on the one op that needs no persona, so a
    /// healthcheck can report skew alongside "connected".
    #[tokio::test]
    async fn status_advertises_known_types_before_hello() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(&mut write, json!({"op": "status"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["known_types"], json!(envelope::KNOWN_TYPES));
    }

    #[tokio::test]
    async fn status_is_queryable_before_hello() {
        // Deliberate design choice: unlike every other op, `status` needs no
        // authenticated persona — a wedged daemon reachable enough to accept
        // a connection but never getting to persona auth is exactly the case
        // liveness checking needs to survive.
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(&mut write, json!({"op": "status"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], true);
    }

    #[tokio::test]
    async fn status_reports_null_fields_before_anything_has_happened() {
        // Edge case from the issue's test plan: a daemon that has never
        // completed a sync (or seen an event) reports absent fields, not a
        // bogus zero timestamp.
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(&mut write, json!({"op": "status"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["connected"], true);
        assert!(reply["last_event_received_secs_ago"].is_null());
        assert!(reply["last_sync_completed_secs_ago"].is_null());
        assert!(reply["retry_attempt"].is_null());
        assert!(reply["retry_backoff_secs"].is_null());
    }

    #[tokio::test]
    async fn status_reflects_recorded_event_and_sync_timestamps() {
        let (mut write, mut read, registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        registry.record_event_received();
        registry.record_sync_completed();

        send(&mut write, json!({"op": "status"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["connected"], true);
        assert_eq!(reply["last_event_received_secs_ago"], 0);
        assert_eq!(reply["last_sync_completed_secs_ago"], 0);
    }

    #[tokio::test]
    async fn status_surfaces_an_in_progress_retry_attempt() {
        let (mut write, mut read, registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        registry.record_retry_attempt(3, 8);

        send(&mut write, json!({"op": "status"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(
            reply["connected"], false,
            "an in-progress retry means not currently connected"
        );
        assert_eq!(reply["retry_attempt"], 3);
        assert_eq!(reply["retry_backoff_secs"], 8);
    }

    #[tokio::test]
    async fn status_a_completed_sync_clears_a_stale_retry_state() {
        // A sync succeeding after a retry must clear the retry snapshot —
        // otherwise `status` would keep reporting a stale in-backoff state
        // after the connection actually recovered.
        let (mut write, mut read, registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        registry.record_retry_attempt(2, 4);
        registry.record_sync_completed();

        send(&mut write, json!({"op": "status"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["connected"], true);
        assert!(reply["retry_attempt"].is_null());
        assert!(reply["retry_backoff_secs"].is_null());
    }

    #[tokio::test]
    async fn ops_before_hello_are_rejected() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(&mut write, json!({"op": "list_rooms"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "hello first");
    }

    #[tokio::test]
    async fn bad_json_line_gets_an_error_reply_and_the_connection_survives() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        write
            .write_all(b"not json\n")
            .await
            .expect("write raw line");
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("bad json"));

        // The connection is still alive: a well-formed hello afterward
        // succeeds.
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], true);
    }

    #[tokio::test]
    async fn unknown_op_after_hello_returns_an_error() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;
        send(&mut write, json!({"op": "not_a_real_op"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("unknown op"));
    }

    // ---- `invite` op request-parsing (issue #39) --------------------------
    //
    // `resolve_room` needs a live homeserver's joined rooms, which the
    // offline test client never has (see `offline_client` above) — so these
    // exercise the parts of the op that fail *before* room resolution
    // (missing/malformed `user`) plus the room-resolution failure itself
    // (unknown room), matching the issue's test plan.

    #[tokio::test]
    async fn invite_requires_user_field() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;

        send(&mut write, json!({"op": "invite", "room": "!x:y"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("requires `user`"));
    }

    #[tokio::test]
    async fn invite_rejects_malformed_user_id() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;

        send(
            &mut write,
            json!({"op": "invite", "room": "!x:y", "user": "not-a-user-id"}),
        )
        .await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("invalid user id"));
    }

    #[tokio::test]
    async fn invite_reports_unknown_room() {
        // A well-formed `user` clears the parsing check above, so this
        // exercises `resolve_room`'s failure path: the offline test client
        // has no joined rooms at all.
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;

        send(
            &mut write,
            json!({"op": "invite", "room": "!nope:x", "user": "@new-bot:example.org"}),
        )
        .await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], false);
        assert!(reply["error"]
            .as_str()
            .unwrap()
            .contains("no joined room matching"));
    }

    #[tokio::test]
    async fn reply_echoes_the_request_id() {
        let (mut write, mut read, _registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent", "id": 42}),
        )
        .await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["id"], 42);
    }

    #[tokio::test]
    async fn dispatch_skips_only_the_authoring_persona_on_own_events() {
        // §7 refined: own-host events still dispatch to local agents; only
        // the authoring persona is skipped, so same-host agent-to-agent
        // traffic still flows.
        let (mut write, mut read, registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;

        registry
            .dispatch(r#"{"event":"from-self"}"#, true, "writer_agent")
            .await;
        let nothing = tokio::time::timeout(Duration::from_millis(200), recv(&mut read)).await;
        assert!(
            nothing.is_err(),
            "the authoring persona must not hear its own event"
        );

        registry
            .dispatch(r#"{"event":"from-other"}"#, true, "research_agent")
            .await;
        let pushed = tokio::time::timeout(Duration::from_millis(200), recv(&mut read))
            .await
            .expect("a different author's own-event should still be delivered");
        assert_eq!(pushed["event"], "from-other");
        assert!(pushed.get("id").is_none(), "pushed events carry no `id`");
    }

    // ---- build_send_envelope — `from` stamping (§6) -----------------------

    #[test]
    fn build_send_envelope_stamps_from_the_authenticated_persona() {
        let req = json!({"to": "research_agent", "body": "hi"});
        let env = build_send_envelope("writer_agent", &req).unwrap().env;
        assert_eq!(env.from, "writer_agent");
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.kind, "chat");
        assert_eq!(env.body, "hi");
    }

    #[test]
    fn build_send_envelope_ignores_a_spoofed_from_field() {
        // The request is never even read for `from` — an agent claiming to
        // be someone else has no effect at all.
        let req = json!({"to": "research_agent", "body": "hi", "from": "admin"});
        let env = build_send_envelope("writer_agent", &req).unwrap().env;
        assert_eq!(env.from, "writer_agent");
    }

    #[test]
    fn build_send_envelope_requires_to_and_body() {
        assert!(build_send_envelope("writer_agent", &json!({"body": "hi"})).is_err());
        assert!(build_send_envelope("writer_agent", &json!({"to": "research_agent"})).is_err());
    }

    /// #95 (was `..._rejects_unknown_type`): an unrecognized `type` on an
    /// agent's own send is a version skew, not a fatal request error — it
    /// degrades to `chat` and the send proceeds, matching the ingest path.
    #[test]
    fn build_send_envelope_degrades_unknown_type_to_chat() {
        let req = json!({"to": "research_agent", "body": "hi", "type": "smoke_signal"});
        let env = build_send_envelope("writer_agent", &req).unwrap().env;
        assert_eq!(env.kind, "chat");
        assert_eq!(env.body, "hi");
        assert_eq!(env.to, "research_agent");
    }

    // ---- the degrade is visible to the caller (#95 review) ----------------

    /// A degraded send reports what the caller *asked* for. Without this the
    /// daemon's only signal is its own stderr, which the RPC caller on the far
    /// side of the socket never sees.
    #[test]
    fn build_send_envelope_reports_the_degraded_from_type() {
        let req = json!({"to": "research_agent", "body": "hi", "type": "smoke_signal"});
        let built = build_send_envelope("writer_agent", &req).unwrap();
        assert_eq!(built.env.kind, "chat");
        assert_eq!(built.degraded_from.as_deref(), Some("smoke_signal"));
    }

    /// An honored send carries no degrade signal — the field must not fire on
    /// the ordinary path, explicit type or defaulted.
    #[test]
    fn build_send_envelope_reports_no_degrade_for_a_known_type() {
        for req in [
            json!({"to": "research_agent", "body": "hi", "type": "digest"}),
            json!({"to": "research_agent", "body": "hi", "type": "chat"}),
            json!({"to": "research_agent", "body": "hi"}),
        ] {
            let built = build_send_envelope("writer_agent", &req).unwrap();
            assert_eq!(built.degraded_from, None, "req {req} must not degrade");
        }
    }

    /// The `send` reply always names the type that actually went on the wire,
    /// and adds `degraded_from` only when it differs from what was requested —
    /// so a caller can detect a silent degrade with one key lookup.
    #[test]
    fn send_reply_surfaces_a_degrade_to_the_caller() {
        let honored = send_reply("$evt", "!room:h", "digest", None);
        assert_eq!(honored["ok"], json!(true));
        assert_eq!(honored["event_id"], json!("$evt"));
        assert_eq!(honored["room_id"], json!("!room:h"));
        assert_eq!(honored["type"], json!("digest"));
        assert!(
            honored.get("degraded_from").is_none(),
            "an honored send must not claim a degrade"
        );

        let degraded = send_reply("$evt", "!room:h", "chat", Some("smoke_signal"));
        assert_eq!(degraded["type"], json!("chat"));
        assert_eq!(degraded["degraded_from"], json!("smoke_signal"));
        // Still a success: degrade-don't-drop, the message did go out (D19).
        assert_eq!(degraded["ok"], json!(true));
        assert_eq!(degraded["event_id"], json!("$evt"));
    }

    /// #95: `digest` is a known type now, so it is sent as itself rather than
    /// being either rejected or degraded.
    #[test]
    fn build_send_envelope_accepts_digest() {
        let req = json!({"to": "*", "body": "3 PRs merged, 1 blocked.", "type": "digest"});
        let env = build_send_envelope("writer_agent", &req).unwrap().env;
        assert_eq!(env.kind, "digest");
        assert_eq!(env.from, "writer_agent");
    }

    /// The one send-side rejection that survives the #95 degrade: `meta` is
    /// still refused for anything but `completion`, and the error names the
    /// type the caller actually asked for rather than the degraded one.
    #[test]
    fn build_send_envelope_rejects_meta_on_an_unknown_type() {
        let req = json!({
            "to": "*",
            "body": "hi",
            "type": "smoke_signal",
            "meta": {"schema": "smoke-v1"},
        });
        let err = build_send_envelope("writer_agent", &req).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("only valid for type"), "was {msg:?}");
        assert!(msg.contains("smoke_signal"), "was {msg:?}");
    }

    #[test]
    fn build_send_envelope_defaults_type_to_chat() {
        let env = build_send_envelope(
            "writer_agent",
            &json!({"to": "research_agent", "body": "hi"}),
        )
        .unwrap()
        .env;
        assert_eq!(env.kind, "chat");
    }

    #[test]
    fn build_send_envelope_rejects_malformed_task_id() {
        let req = json!({"to": "research_agent", "body": "hi", "task_id": "not-valid!"});
        assert!(build_send_envelope("writer_agent", &req).is_err());
    }

    #[test]
    fn build_send_envelope_carries_the_wake_hint_through() {
        // Advisory only (D16) — the daemon never acts on it, it's just
        // preserved for optional external wakers to read later via `check`.
        let req = json!({"to": "research_agent", "body": "hi", "wake": true});
        let env = build_send_envelope("writer_agent", &req).unwrap().env;
        assert_eq!(env.wake, Some(true));
    }

    #[test]
    fn build_send_envelope_defaults_wake_to_none_when_absent() {
        let env = build_send_envelope(
            "writer_agent",
            &json!({"to": "research_agent", "body": "hi"}),
        )
        .unwrap()
        .env;
        assert_eq!(env.wake, None);
    }

    #[test]
    fn build_send_envelope_accepts_well_formed_task_id() {
        let req = json!({"to": "research_agent", "body": "hi", "task_id": "source_check_1"});
        let env = build_send_envelope("writer_agent", &req).unwrap().env;
        assert_eq!(env.task_id.as_deref(), Some("source_check_1"));
    }

    // ---- build_send_envelope — completion meta wiring (#30) ----------------

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

    #[test]
    fn build_send_envelope_carries_valid_completion_meta_intact() {
        let req = json!({
            "to": "*",
            "body": "shipped it",
            "type": "completion",
            "meta": completion_meta(),
        });
        let env = build_send_envelope("writer_agent", &req).unwrap().env;
        assert_eq!(env.kind, "completion");
        assert_eq!(env.meta.as_ref().unwrap()["schema"], "completion-v1");
        assert_eq!(env.meta.as_ref().unwrap()["repo"], "rjwalters/safehouse");
    }

    #[test]
    fn build_send_envelope_rejects_completion_without_meta() {
        let req = json!({"to": "*", "body": "shipped it", "type": "completion"});
        let err = build_send_envelope("writer_agent", &req).unwrap_err();
        assert!(err.to_string().contains("requires a `meta`"));
    }

    #[test]
    fn build_send_envelope_rejects_completion_with_invalid_meta() {
        let mut meta = completion_meta();
        meta["schema"] = json!("wrong-schema");
        let req = json!({"to": "*", "body": "shipped it", "type": "completion", "meta": meta});
        let err = build_send_envelope("writer_agent", &req).unwrap_err();
        assert!(err.to_string().contains("invalid completion-v1 meta"));
    }

    #[test]
    fn build_send_envelope_rejects_meta_on_non_completion_type() {
        let req = json!({
            "to": "research_agent",
            "body": "hi",
            "type": "chat",
            "meta": completion_meta(),
        });
        let err = build_send_envelope("writer_agent", &req).unwrap_err();
        assert!(err.to_string().contains("only valid for type"));
    }

    #[test]
    fn build_send_envelope_non_completion_without_meta_is_unaffected() {
        // Regression: the common chat/task path never sets meta and never errors.
        let env = build_send_envelope(
            "writer_agent",
            &json!({"to": "research_agent", "body": "hi"}),
        )
        .unwrap()
        .env;
        assert!(env.meta.is_none());
    }

    // ---- room resolution: id/name/alias matching + ambiguity (AC #3) ------
    //
    // `resolve_room` needs a live `Client`'s joined rooms, but its decision
    // logic is factored into the pure `pick_room_index`/`RoomAddr::matches`,
    // which are exercised here with no homeserver — the same match predicate
    // and ambiguity rule `resolve_room` runs against real `Room`s.

    fn addr(id: &str, name: Option<&str>, canonical: Option<&str>, alt: &[&str]) -> RoomAddr {
        RoomAddr {
            id: id.to_owned(),
            name: name.map(str::to_owned),
            canonical_alias: canonical.map(str::to_owned),
            alt_aliases: alt.iter().map(|a| (*a).to_owned()).collect(),
        }
    }

    #[test]
    fn pick_room_ambiguous_name_errors_instead_of_first_match() {
        // The latent bug this fixes: two joined rooms sharing a display name
        // must error, not silently route to whichever one happens to be first.
        let rooms = [
            addr("!a:x", Some("fleet"), None, &[]),
            addr("!b:x", Some("fleet"), None, &[]),
        ];
        let err = pick_room_index(&rooms, Some("fleet")).unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "expected an ambiguity error, got: {err}"
        );
    }

    #[test]
    fn pick_room_resolves_by_canonical_alias() {
        let rooms = [
            addr("!a:x", Some("General"), Some("#fleet-vibesql:x"), &[]),
            addr("!b:x", Some("Other"), Some("#other:x"), &[]),
        ];
        assert_eq!(
            pick_room_index(&rooms, Some("#fleet-vibesql:x")).unwrap(),
            0
        );
    }

    #[test]
    fn pick_room_resolves_by_alt_alias() {
        let rooms = [
            addr("!a:x", Some("General"), Some("#canon:x"), &["#alt-fleet:x"]),
            addr("!b:x", Some("Other"), None, &[]),
        ];
        assert_eq!(pick_room_index(&rooms, Some("#alt-fleet:x")).unwrap(), 0);
    }

    #[test]
    fn pick_room_resolves_by_id_and_name_unchanged() {
        // Regression: the existing id/name paths still resolve as before.
        let rooms = [
            addr("!a:x", Some("writer"), None, &[]),
            addr("!b:x", Some("research"), None, &[]),
        ];
        assert_eq!(pick_room_index(&rooms, Some("!b:x")).unwrap(), 1);
        assert_eq!(pick_room_index(&rooms, Some("research")).unwrap(), 1);
    }

    #[test]
    fn pick_room_unknown_spec_errors() {
        let rooms = [addr("!a:x", Some("writer"), None, &[])];
        let err = pick_room_index(&rooms, Some("nope")).unwrap_err();
        assert!(err.to_string().contains("no joined room matching"));
    }

    #[test]
    fn pick_room_none_spec_defaults_only_when_single_room() {
        let one = [addr("!a:x", Some("writer"), None, &[])];
        assert_eq!(pick_room_index(&one, None).unwrap(), 0);

        let two = [
            addr("!a:x", Some("writer"), None, &[]),
            addr("!b:x", Some("research"), None, &[]),
        ];
        assert!(pick_room_index(&two, None).is_err());

        let none: [RoomAddr; 0] = [];
        assert!(pick_room_index(&none, None).is_err());
    }

    // ---- unsupported-version surfacing dedup (§7.2, §9) --------------------

    #[tokio::test]
    async fn unsupported_surface_dedups_per_sender_and_version() {
        let registry = Registry::new(vec![], Mailbox::open_in_memory().unwrap());
        // First sighting of a (sender, version) pair surfaces.
        assert!(registry.mark_unsupported_surfaced("@evil:remote", 2).await);
        // Repeats from the same sender at the same version are suppressed —
        // a flood of bad-version events surfaces once, not once per event.
        assert!(!registry.mark_unsupported_surfaced("@evil:remote", 2).await);
        assert!(!registry.mark_unsupported_surfaced("@evil:remote", 2).await);
        // A different version from the same sender is meaningfully new.
        assert!(registry.mark_unsupported_surfaced("@evil:remote", 3).await);
        // A different sender at a seen version is also new.
        assert!(registry.mark_unsupported_surfaced("@other:remote", 2).await);
    }

    // ---- ThreadState — §2 task threading / §5.2 thread-reply routing -------

    fn thread_env(to: &str, task_id: Option<&str>) -> Envelope {
        Envelope {
            v: 1,
            from: "writer_agent".to_owned(),
            to: to.to_owned(),
            kind: "task".to_owned(),
            task_id: task_id.map(str::to_owned),
            body: "go".to_owned(),
            wake: None,
            meta: None,
        }
    }

    // ---- mailbox routing (D16/D17 §7 addressing) --------------------------

    fn env_to(from: &str, to: &str) -> Envelope {
        Envelope {
            v: 1,
            from: from.to_owned(),
            to: to.to_owned(),
            kind: "chat".to_owned(),
            task_id: None,
            body: "hi".to_owned(),
            wake: None,
            meta: None,
        }
    }

    #[tokio::test]
    async fn thread_state_records_root_the_first_time_a_task_id_is_seen() {
        let threads = ThreadState::default();
        assert!(threads.root_for_task("source_check").await.is_none());

        threads
            .observe(
                "$root",
                "$root",
                &thread_env("research_agent", Some("source_check")),
            )
            .await;
        assert_eq!(
            threads.root_for_task("source_check").await.as_deref(),
            Some("$root")
        );
    }

    #[tokio::test]
    async fn mailbox_deliver_routes_direct_address_to_only_that_persona() {
        let registry = Registry::new(
            vec!["writer_agent".to_owned(), "research_agent".to_owned()],
            Mailbox::open_in_memory().unwrap(),
        );
        registry
            .mailbox_deliver(
                false,
                "!room:x",
                "$1",
                "@robb:x",
                &env_to("@robb:x", "research_agent"),
            )
            .await
            .unwrap();
        assert_eq!(
            registry
                .check("research_agent", true, None)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(registry
            .check("writer_agent", true, None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn mailbox_deliver_broadcast_reaches_every_local_persona() {
        let registry = Registry::new(
            vec!["writer_agent".to_owned(), "research_agent".to_owned()],
            Mailbox::open_in_memory().unwrap(),
        );
        registry
            .mailbox_deliver(false, "!room:x", "$1", "@robb:x", &env_to("@robb:x", "*"))
            .await
            .unwrap();
        assert_eq!(
            registry
                .check("writer_agent", true, None)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            registry
                .check("research_agent", true, None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn thread_state_never_overwrites_an_established_root() {
        // Every subsequent event for the same task_id must thread under the
        // *original* root, even if it arrives with a different root (which
        // shouldn't happen in practice, but the map must not drift).
        let threads = ThreadState::default();
        threads
            .observe(
                "$root",
                "$root",
                &thread_env("research_agent", Some("source_check")),
            )
            .await;
        threads
            .observe(
                "$root",
                "$reply1",
                &thread_env("writer_agent", Some("source_check")),
            )
            .await;
        assert_eq!(
            threads.root_for_task("source_check").await.as_deref(),
            Some("$root")
        );
    }

    #[tokio::test]
    async fn thread_state_tracks_latest_event_in_thread() {
        let threads = ThreadState::default();
        threads
            .observe("$root", "$root", &thread_env("research_agent", Some("t")))
            .await;
        assert_eq!(
            threads.latest_in_thread("$root").await.as_deref(),
            Some("$root")
        );

        threads
            .observe("$root", "$reply1", &thread_env("writer_agent", Some("t")))
            .await;
        assert_eq!(
            threads.latest_in_thread("$root").await.as_deref(),
            Some("$reply1")
        );
    }

    #[tokio::test]
    async fn thread_state_tracks_last_addressed_persona_for_thread_replies() {
        // §5.2: whoever was last addressed within a thread is who an
        // un-tokened human reply should route to.
        let threads = ThreadState::default();
        threads
            .observe("$root", "$root", &thread_env("research_agent", Some("t")))
            .await;
        assert_eq!(
            threads.target_for_thread("$root").await.as_deref(),
            Some("research_agent")
        );

        threads
            .observe("$root", "$reply1", &thread_env("writer_agent", Some("t")))
            .await;
        assert_eq!(
            threads.target_for_thread("$root").await.as_deref(),
            Some("writer_agent")
        );
    }

    #[tokio::test]
    async fn thread_state_ignores_broadcast_and_matrix_user_targets() {
        // "*" and a Matrix user id are not persona-shaped — never useful as
        // a §5.2 routing target.
        let threads = ThreadState::default();
        threads
            .observe("$root", "$root", &thread_env("*", None))
            .await;
        assert!(threads.target_for_thread("$root").await.is_none());

        threads
            .observe("$root", "$root", &thread_env("@robb:safehouse.local", None))
            .await;
        assert!(threads.target_for_thread("$root").await.is_none());
    }

    #[tokio::test]
    async fn mailbox_deliver_broadcast_skips_only_the_authoring_persona_on_own_events() {
        let registry = Registry::new(
            vec!["writer_agent".to_owned(), "research_agent".to_owned()],
            Mailbox::open_in_memory().unwrap(),
        );
        registry
            .mailbox_deliver(
                true,
                "!room:x",
                "$1",
                "@safehoused:x",
                &env_to("writer_agent", "*"),
            )
            .await
            .unwrap();
        assert!(registry
            .check("writer_agent", true, None)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            registry
                .check("research_agent", true, None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn mailbox_deliver_ignores_a_persona_not_hosted_locally() {
        let registry = Registry::new(
            vec!["writer_agent".to_owned()],
            Mailbox::open_in_memory().unwrap(),
        );
        registry
            .mailbox_deliver(
                false,
                "!room:x",
                "$1",
                "@safehoused-hostb:x",
                &env_to("remote_agent", "not_a_local_persona"),
            )
            .await
            .unwrap();
        assert!(registry
            .check("writer_agent", true, None)
            .await
            .unwrap()
            .is_empty());
    }

    // ---- `check` op end-to-end over the socket (acceptance criteria) ------

    #[tokio::test]
    async fn check_delivers_exactly_what_was_missed_while_disconnected() {
        // The persona was never connected while N messages arrived — they
        // land straight in the mailbox via `mailbox_deliver`, exactly as
        // `on_message` does on every inbound event.
        let (mut write, mut read, registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        for i in 0..3 {
            registry
                .mailbox_deliver(
                    false,
                    "!room:x",
                    &format!("$event{i}"),
                    "@robb:x",
                    &env_to("@robb:x", "writer_agent"),
                )
                .await
                .unwrap();
        }

        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;

        send(&mut write, json!({"op": "check"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["messages"].as_array().unwrap().len(), 3);

        // A second immediate check returns nothing — the cursor advanced.
        send(&mut write, json!({"op": "check"})).await;
        let reply = recv(&mut read).await;
        assert!(reply["messages"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn check_peek_mode_does_not_advance_the_cursor() {
        let (mut write, mut read, registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        registry
            .mailbox_deliver(
                false,
                "!room:x",
                "$1",
                "@robb:x",
                &env_to("@robb:x", "writer_agent"),
            )
            .await
            .unwrap();
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;

        send(&mut write, json!({"op": "check", "peek": true})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["messages"].as_array().unwrap().len(), 1);
        assert_eq!(reply["advanced"], false);

        send(&mut write, json!({"op": "check", "peek": true})).await;
        let reply = recv(&mut read).await;
        assert_eq!(
            reply["messages"].as_array().unwrap().len(),
            1,
            "a repeated peek must be idempotent"
        );

        send(&mut write, json!({"op": "check"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["messages"].as_array().unwrap().len(), 1);
        assert_eq!(reply["advanced"], true);
    }

    #[tokio::test]
    async fn check_respects_limit() {
        let (mut write, mut read, registry) = spawn_conn(vec!["writer_agent".to_owned()]).await;
        for i in 0..3 {
            registry
                .mailbox_deliver(
                    false,
                    "!room:x",
                    &format!("$event{i}"),
                    "@robb:x",
                    &env_to("@robb:x", "writer_agent"),
                )
                .await
                .unwrap();
        }
        send(
            &mut write,
            json!({"op": "hello", "persona": "writer_agent"}),
        )
        .await;
        recv(&mut read).await;

        send(&mut write, json!({"op": "check", "limit": 2})).await;
        let reply = recv(&mut read).await;
        assert_eq!(reply["messages"].as_array().unwrap().len(), 2);

        send(&mut write, json!({"op": "check"})).await;
        let reply = recv(&mut read).await;
        assert_eq!(
            reply["messages"].as_array().unwrap().len(),
            1,
            "the remaining unread message must still be there"
        );
    }
}
