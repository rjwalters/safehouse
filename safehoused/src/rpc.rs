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
};

use anyhow::{Context, Result};
use matrix_sdk::{
    room::MessagesOptions, ruma::api::client::room::create_room::v3::Request as CreateRoomRequest,
    Client, Room, RoomState,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, Mutex},
};

use crate::envelope::{self, Envelope};

pub struct Registry {
    conns: Mutex<HashMap<u64, ConnHandle>>,
    next_id: std::sync::atomic::AtomicU64,
    pub personas: Vec<String>,
    /// `(matrix_sender, version)` pairs already surfaced to the human for an
    /// unsupported-version envelope (§7.2). Deduped so a remote daemon sending
    /// a stream of bad-version envelopes surfaces once, not once per event.
    surfaced_unsupported: Mutex<HashSet<(String, u64)>>,
}

struct ConnHandle {
    persona: String,
    tx: mpsc::UnboundedSender<String>,
}

impl Registry {
    pub fn new(personas: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            conns: Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
            personas,
            surfaced_unsupported: Mutex::new(HashSet::new()),
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
                            })
                        }
                        Some(p) => json!({
                            "ok": false,
                            "error": format!("persona {p:?} not in allowlist (config `personas`)"),
                        }),
                        None => json!({"ok": false, "error": "hello requires persona"}),
                    }
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
            let env = build_send_envelope(persona, req)?;
            let room = resolve_room(client, req.get("room").and_then(Value::as_str))?;
            let content = envelope::to_event_content(&env);
            let response = room
                .send_raw("m.room.message", content)
                .await
                .context("sending to room")?;
            Ok(
                json!({"ok": true, "event_id": response.response.event_id, "room_id": room.room_id()}),
            )
        }
        "create_room" => {
            let name = req
                .get("name")
                .and_then(Value::as_str)
                .context("create_room requires `name`")?;
            let mut request = CreateRoomRequest::new();
            request.name = Some(name.to_owned());
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
            room.enable_encryption().await?;
            Ok(json!({"ok": true, "room_id": room.room_id(), "name": name}))
        }
        "list_rooms" => {
            let mut rooms = Vec::new();
            for room in client.joined_rooms() {
                rooms.push(json!({
                    "room_id": room.room_id(),
                    "name": room.name(),
                    "encrypted": room.latest_encryption_state().await.map(|s| s.is_encrypted()).unwrap_or(false),
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
                // §7.2: never hand an agent a guessed envelope for a version we
                // don't support — mark it so the agent can surface, not act.
                match envelope::from_event_json(&content, &sender, &registry.personas) {
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
        other => anyhow::bail!("unknown op {other:?}"),
    }
}

/// Build the outbound envelope for a `send` request. `persona` comes from the
/// authenticated connection (§6 — the socket connection is the identity), not
/// from `req`; the request is never even inspected for a `from` field, so a
/// client cannot spoof it no matter what it sends.
fn build_send_envelope(persona: &str, req: &Value) -> Result<Envelope> {
    let to = req
        .get("to")
        .and_then(Value::as_str)
        .context("send requires `to`")?;
    let body = req
        .get("body")
        .and_then(Value::as_str)
        .context("send requires `body`")?;
    let kind = req.get("type").and_then(Value::as_str).unwrap_or("chat");
    anyhow::ensure!(
        envelope::KNOWN_TYPES.contains(&kind),
        "unknown type {kind:?} (v1 types: {:?})",
        envelope::KNOWN_TYPES
    );
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
    Ok(Envelope {
        v: 1,
        from: persona.to_owned(), // stamped here; never taken from the request
        to: to.to_owned(),
        kind: kind.to_owned(),
        task_id,
        body: body.to_owned(),
    })
}

/// Room by id, by name, or the sole joined room when unambiguous.
fn resolve_room(client: &Client, spec: Option<&str>) -> Result<Room> {
    let joined: Vec<Room> = client
        .joined_rooms()
        .into_iter()
        .filter(|r| r.state() == RoomState::Joined)
        .collect();
    match spec {
        Some(s) => joined
            .into_iter()
            .find(|r| r.room_id() == s || r.name().as_deref() == Some(s))
            .with_context(|| format!("no joined room matching {s:?}")),
        None if joined.len() == 1 => Ok(joined.into_iter().next().unwrap()),
        None => anyhow::bail!("`room` required: {} rooms joined", joined.len()),
    }
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
        let registry = Registry::new(personas);
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
        let env = build_send_envelope("writer_agent", &req).unwrap();
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
        let env = build_send_envelope("writer_agent", &req).unwrap();
        assert_eq!(env.from, "writer_agent");
    }

    #[test]
    fn build_send_envelope_requires_to_and_body() {
        assert!(build_send_envelope("writer_agent", &json!({"body": "hi"})).is_err());
        assert!(build_send_envelope("writer_agent", &json!({"to": "research_agent"})).is_err());
    }

    #[test]
    fn build_send_envelope_rejects_unknown_type() {
        let req = json!({"to": "research_agent", "body": "hi", "type": "smoke_signal"});
        assert!(build_send_envelope("writer_agent", &req).is_err());
    }

    #[test]
    fn build_send_envelope_defaults_type_to_chat() {
        let env = build_send_envelope(
            "writer_agent",
            &json!({"to": "research_agent", "body": "hi"}),
        )
        .unwrap();
        assert_eq!(env.kind, "chat");
    }

    #[test]
    fn build_send_envelope_rejects_malformed_task_id() {
        let req = json!({"to": "research_agent", "body": "hi", "task_id": "not-valid!"});
        assert!(build_send_envelope("writer_agent", &req).is_err());
    }

    #[test]
    fn build_send_envelope_accepts_well_formed_task_id() {
        let req = json!({"to": "research_agent", "body": "hi", "task_id": "source_check_1"});
        let env = build_send_envelope("writer_agent", &req).unwrap();
        assert_eq!(env.task_id.as_deref(), Some("source_check_1"));
    }

    // ---- unsupported-version surfacing dedup (§7.2, §9) --------------------

    #[tokio::test]
    async fn unsupported_surface_dedups_per_sender_and_version() {
        let registry = Registry::new(vec![]);
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
}
