//! Unix-socket RPC — the only door agents have into the room.
//!
//! JSON lines, one object per line. First request must be
//! `{"op":"hello","persona":"..."}`; the daemon gates the persona against the
//! configured allowlist and stamps it into every outbound envelope (§6 — the
//! socket connection, not a message field, is the identity). After hello, the
//! daemon also pushes inbound room events to the connection as
//! `{"event":"message", ...}` lines (no `id`).

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use matrix_sdk::{
    room::MessagesOptions,
    ruma::api::client::room::create_room::v3::Request as CreateRoomRequest,
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
        })
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
            let to = req.get("to").and_then(Value::as_str).context("send requires `to`")?;
            let body = req.get("body").and_then(Value::as_str).context("send requires `body`")?;
            let kind = req.get("type").and_then(Value::as_str).unwrap_or("chat");
            anyhow::ensure!(
                envelope::KNOWN_TYPES.contains(&kind),
                "unknown type {kind:?} (v1 types: {:?})",
                envelope::KNOWN_TYPES
            );
            let task_id = req.get("task_id").and_then(Value::as_str).map(str::to_owned);
            if let Some(t) = &task_id {
                anyhow::ensure!(
                    t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                    "task_id must be [A-Za-z0-9_]"
                );
            }
            let room = resolve_room(client, req.get("room").and_then(Value::as_str))?;
            let env = Envelope {
                v: 1,
                from: persona.to_owned(), // stamped here; never taken from the request
                to: to.to_owned(),
                kind: kind.to_owned(),
                task_id,
                body: body.to_owned(),
            };
            let content = envelope::to_event_content(&env);
            let response = room
                .send_raw("m.room.message", content)
                .await
                .context("sending to room")?;
            Ok(json!({"ok": true, "event_id": response.response.event_id, "room_id": room.room_id()}))
        }
        "create_room" => {
            let name = req.get("name").and_then(Value::as_str).context("create_room requires `name`")?;
            let mut request = CreateRoomRequest::new();
            request.name = Some(name.to_owned());
            if let Some(invites) = req.get("invite").and_then(Value::as_array) {
                for user in invites {
                    let user = user.as_str().context("invite entries must be user id strings")?;
                    request.invite.push(user.try_into().context("invalid user id in invite")?);
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
            let limit = req.get("limit").and_then(Value::as_u64).unwrap_or(20).min(100);
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
                let sender = parsed.get("sender").and_then(Value::as_str).unwrap_or("").to_owned();
                let content = parsed.get("content").cloned().unwrap_or(Value::Null);
                let env = envelope::from_event_json(&content, &sender, &registry.personas);
                messages.push(json!({
                    "event_id": parsed.get("event_id"),
                    "sender": sender,
                    "own": sender == own,
                    "ts": parsed.get("origin_server_ts"),
                    "envelope": env,
                }));
            }
            Ok(json!({"ok": true, "room_id": room.room_id(), "messages": messages}))
        }
        other => anyhow::bail!("unknown op {other:?}"),
    }
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
