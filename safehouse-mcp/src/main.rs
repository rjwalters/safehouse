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
    io::{BufRead, BufReader, IsTerminal, Write},
    os::unix::net::UnixStream,
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

fn main() -> Result<()> {
    // Answer --help/--version before anything else so they work regardless of
    // whether stdin is a TTY, a pipe, or /dev/null (e.g. in CI).
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage(&mut std::io::stdout());
                return Ok(());
            }
            "--version" | "-V" => {
                println!("safehouse-mcp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => {}
        }
    }

    let stdin = std::io::stdin();
    // A stdio MCP server expects JSON-RPC frames from a client on stdin. Run
    // directly from an interactive shell with no redirection, stdin is the
    // controlling TTY and the read loop would block forever waiting for a frame
    // a human will never type — silently wedging any `cmd && next` pipeline.
    // Print a usage hint and exit non-zero instead of hanging.
    if stdin.is_terminal() {
        let mut stderr = std::io::stderr();
        print_usage(&mut stderr);
        std::process::exit(1);
    }

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

/// One-line usage note for humans who ran the binary directly instead of via an
/// MCP client. Written to stderr on the TTY-hang guard, stdout for `--help`.
fn print_usage(out: &mut impl Write) {
    let _ = writeln!(
        out,
        "safehouse-mcp {} — stdio MCP server, meant to be launched by an MCP client.",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(
        out,
        "Set SAFEHOUSED_SOCKET / SAFEHOUSE_PERSONA and connect via JSON-RPC MCP framing on stdin."
    );
}

fn handle_tool_call(msg: &Value) -> Result<Value> {
    let name = msg
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = msg
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or(json!({}));
    let op = match name {
        "safehouse_send" => {
            let mut op = json!({"op": "send"});
            copy_fields(
                &args,
                &mut op,
                &["to", "body", "type", "task_id", "room", "wake"],
            );
            op
        }
        "safehouse_create_room" => {
            let mut op = json!({"op": "create_room"});
            copy_fields(&args, &mut op, &["name", "invite", "space", "parent"]);
            op
        }
        "safehouse_add_to_space" => {
            let mut op = json!({"op": "add_to_space"});
            copy_fields(&args, &mut op, &["space", "room"]);
            op
        }
        "safehouse_list_rooms" => json!({"op": "list_rooms"}),
        "safehouse_read" => {
            let mut op = json!({"op": "read"});
            copy_fields(&args, &mut op, &["room", "limit"]);
            op
        }
        "safehouse_check" => {
            let mut op = json!({"op": "check"});
            copy_fields(&args, &mut op, &["peek", "limit"]);
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
    let stream = UnixStream::connect(&socket).with_context(|| {
        format!("connecting to safehoused at {socket} — is the daemon running?")
    })?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let hello = json!({"id": 0, "op": "hello", "persona": persona});
    writeln!(writer, "{hello}")?;
    let reply = read_reply(&mut reader)?;
    if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        bail!(
            "daemon rejected persona {persona:?}: {}",
            reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        );
    }

    op.as_object_mut()
        .context("op must be an object")?
        .insert("id".into(), json!(1));
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
                to.as_object_mut()
                    .unwrap()
                    .insert((*field).to_owned(), v.clone());
            }
        }
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "safehouse_send",
            "description": "Send a message into a safehouse room. The daemon stamps your persona as the sender; `to` is a persona (e.g. research_agent), a Matrix user id, or \"*\" for room broadcast. Types: chat (conversational), task (unit of work, use task_id), handoff (transfer of responsibility), ack (completion).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "Target persona, Matrix user id, or \"*\""},
                    "body": {"type": "string", "description": "Message content (plain text)"},
                    "type": {"type": "string", "enum": ["chat", "task", "handoff", "ack"], "description": "Message type (default chat)"},
                    "task_id": {"type": "string", "description": "Stable task identifier, [A-Za-z0-9_]"},
                    "room": {"type": "string", "description": "Room id, name, or alias; optional when only one room is joined. An ambiguous name/alias (matching more than one joined room) is an error, never a guess"},
                    "wake": {"type": "boolean", "description": "Advisory hint only — the daemon never acts on it. For optional external wakers deciding whether to nudge the recipient."}
                },
                "required": ["to", "body"]
            }
        },
        {
            "name": "safehouse_create_room",
            "description": "Create a new safehouse room, optionally inviting Matrix users (e.g. the human's account). Message rooms are encrypted; pass space=true to create a Matrix Space (m.space) container instead — Spaces hold rooms, not messages, and are left unencrypted. Pass parent (a Space's id/name/alias) to create the new room already linked under that Space.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Room name"},
                    "invite": {"type": "array", "items": {"type": "string"}, "description": "Matrix user ids to invite"},
                    "space": {"type": "boolean", "description": "Create a Matrix Space (m.space) container instead of a message room (default false)"},
                    "parent": {"type": "string", "description": "Existing Space (id, name, or alias) to create this room under — sets the m.space.child/m.space.parent relationship"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "safehouse_add_to_space",
            "description": "Link an already-joined room into a Space (m.space), setting the reciprocal m.space.child/m.space.parent state. Idempotent: linking an already-linked room is a no-op success, never an error or a duplicate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "space": {"type": "string", "description": "The Space (id, name, or alias) to add the room to"},
                    "room": {"type": "string", "description": "The room (id, name, or alias) to link under the Space"}
                },
                "required": ["space", "room"]
            }
        },
        {
            "name": "safehouse_list_rooms",
            "description": "List joined safehouse rooms with their ids, names, and encryption state. Each entry also carries `type` (\"space\" for an m.space container, else \"room\") and `parent_space` (the room id of its confirmed parent Space, or null) so clients can render/verify the Space hierarchy.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "safehouse_read",
            "description": "Read recent messages from a safehouse room, newest last. Each message carries its envelope (from/to/type/task_id/body); human messages get a synthesized envelope.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "room": {"type": "string", "description": "Room id, name, or alias; optional when only one room is joined. An ambiguous name/alias (matching more than one joined room) is an error, never a guess"},
                    "limit": {"type": "integer", "description": "Max messages (default 20, cap 100)"}
                }
            }
        },
        {
            "name": "safehouse_check",
            "description": "Check your mailbox: unread envelopes addressed to you (`to: <your persona>` or broadcasts), oldest first, since you last checked. Call this on your own cadence, like checking your phone — no agent needs to stay connected to receive. By default this advances your read cursor so a repeat call returns nothing new; pass peek=true to look without consuming. Survives daemon restarts: anything you missed while the daemon (or you) were down is still here.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "peek": {"type": "boolean", "description": "If true, don't advance the read cursor — a repeated peek returns the same unread set (default false)"},
                    "limit": {"type": "integer", "description": "Max envelopes to return (oldest unread first); unset returns everything unread"}
                }
            }
        }
    ])
}
