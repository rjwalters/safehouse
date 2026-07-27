//! safehouse-mcp — a keyless stdio MCP server over safehoused's unix socket.
//!
//! Holds no Matrix keys, no tokens, no crypto: every tool call opens the
//! daemon socket, identifies as $SAFEHOUSE_PERSONA (gated by the daemon's
//! allowlist), performs one op, and closes. The daemon stamps `from`; this
//! shim cannot impersonate anyone (envelope-v1 §6).
//!
//! Deliberately dependency-light: hand-rolled JSON-RPC 2.0 over stdio, so the
//! whole agent-facing surface stays a documented, language-agnostic protocol
//! (D8) rather than an SDK contract.

use std::{
    env,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

fn main() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        // Notifications (no id) get no response.
        let Some(id) = id else { continue };
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": msg
                    .pointer("/params/protocolVersion")
                    .cloned()
                    .unwrap_or_else(|| json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "safehouse-mcp", "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_definitions()})),
            "tools/call" => handle_tool_call(&msg),
            other => Err(anyhow::anyhow!("method not found: {other}")),
        };
        let reply = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(err) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("{err:#}")},
            }),
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle_tool_call(msg: &Value) -> Result<Value> {
    let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or("");
    let args = msg.pointer("/params/arguments").cloned().unwrap_or(json!({}));
    let op = match name {
        "safehouse_send" => {
            let mut op = json!({"op": "send"});
            copy_fields(&args, &mut op, &["to", "body", "type", "task_id", "room"]);
            op
        }
        "safehouse_create_room" => {
            let mut op = json!({"op": "create_room"});
            copy_fields(&args, &mut op, &["name", "invite"]);
            op
        }
        "safehouse_list_rooms" => json!({"op": "list_rooms"}),
        "safehouse_read" => {
            let mut op = json!({"op": "read"});
            copy_fields(&args, &mut op, &["room", "limit"]);
            op
        }
        other => bail!("unknown tool: {other}"),
    };
    match daemon_call(op) {
        Ok(reply) => Ok(json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&reply)?}],
            "isError": !reply.get("ok").and_then(Value::as_bool).unwrap_or(false),
        })),
        Err(err) => Ok(json!({
            "content": [{"type": "text", "text": format!("{err:#}")}],
            "isError": true,
        })),
    }
}

/// One connection per call: hello, op, first non-push reply, close. Push
/// lines (inbound room events, no "id") are skipped — polling agents use
/// safehouse_read instead.
fn daemon_call(mut op: Value) -> Result<Value> {
    let socket = env::var("SAFEHOUSED_SOCKET").context("SAFEHOUSED_SOCKET must be set")?;
    let persona = env::var("SAFEHOUSE_PERSONA").context("SAFEHOUSE_PERSONA must be set")?;
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to safehoused at {socket} — is the daemon running?"))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let hello = json!({"id": 0, "op": "hello", "persona": persona});
    writeln!(writer, "{hello}")?;
    let reply = read_reply(&mut reader)?;
    if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        bail!("daemon rejected persona {persona:?}: {}",
            reply.get("error").and_then(Value::as_str).unwrap_or("unknown error"));
    }

    op.as_object_mut().context("op must be an object")?.insert("id".into(), json!(1));
    writeln!(writer, "{op}")?;
    read_reply(&mut reader)
}

fn read_reply(reader: &mut impl BufRead) -> Result<Value> {
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("daemon closed the connection");
        }
        let value: Value = serde_json::from_str(&line).context("bad reply from daemon")?;
        if value.get("event").is_none() {
            return Ok(value);
        }
    }
}

fn copy_fields(from: &Value, to: &mut Value, fields: &[&str]) {
    for field in fields {
        if let Some(v) = from.get(field) {
            if !v.is_null() {
                to.as_object_mut().unwrap().insert((*field).to_owned(), v.clone());
            }
        }
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "safehouse_send",
            "description": "Send a message into a safehouse room. The daemon stamps your persona as the sender; `to` is a persona (e.g. family_tree_agent), a Matrix user id, or \"*\" for room broadcast. Types: chat (conversational), task (unit of work, use task_id), handoff (transfer of responsibility), ack (completion).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "Target persona, Matrix user id, or \"*\""},
                    "body": {"type": "string", "description": "Message content (plain text)"},
                    "type": {"type": "string", "enum": ["chat", "task", "handoff", "ack"], "description": "Message type (default chat)"},
                    "task_id": {"type": "string", "description": "Stable task identifier, [A-Za-z0-9_]"},
                    "room": {"type": "string", "description": "Room id or name; optional when only one room is joined"}
                },
                "required": ["to", "body"]
            }
        },
        {
            "name": "safehouse_create_room",
            "description": "Create a new encrypted safehouse room, optionally inviting Matrix users (e.g. the human's account).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Room name"},
                    "invite": {"type": "array", "items": {"type": "string"}, "description": "Matrix user ids to invite"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "safehouse_list_rooms",
            "description": "List joined safehouse rooms with their ids, names, and encryption state.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "safehouse_read",
            "description": "Read recent messages from a safehouse room, newest last. Each message carries its envelope (from/to/type/task_id/body); human messages get a synthesized envelope.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "room": {"type": "string", "description": "Room id or name; optional when only one room is joined"},
                    "limit": {"type": "integer", "description": "Max messages (default 20, cap 100)"}
                }
            }
        }
    ])
}
