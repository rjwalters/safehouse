//! safehouse-mcp — a keyless stdio MCP server over safehoused's unix socket,
//! doubling as a one-shot operator CLI over the same socket path (#33).
//!
//! Holds no Matrix keys, no tokens, no crypto: every tool call (or CLI
//! subcommand) opens the daemon socket, identifies as $SAFEHOUSE_PERSONA
//! (gated by the daemon's allowlist), performs one op, and closes. The
//! daemon stamps `from`; this shim cannot impersonate anyone (envelope-v1
//! §6).
//!
//! Deliberately dependency-light: hand-rolled JSON-RPC 2.0 over stdio (for
//! MCP) and hand-rolled argv parsing (for the CLI subcommands below), so the
//! whole agent-facing surface stays a documented, language-agnostic protocol
//! (D8) rather than an SDK contract.
//!
//! ## Operator CLI (see also README "Scripting the socket")
//!
//! `safehouse-mcp read|send|check|list-rooms|status` runs one op against the
//! daemon and prints the JSON reply — no MCP client, no hand-rolled
//! envelope-v1 socket client required. `check` defaults to **peek** (never
//! advances a persona's mailbox cursor); pass `--consume` to advance it
//! explicitly. `status` (#85) is the liveness/staleness one-liner —
//! `last_event_received`/`last_sync_completed`/connection/retry state — for
//! telling "healthy and idle" apart from "cut off" without reading logs.
//! Running with no arguments (or an argument this dispatcher doesn't
//! recognize as a subcommand) preserves the original stdio-MCP-server
//! behavior exactly.

use std::{
    env,
    io::{BufRead, BufReader, IsTerminal, Write},
    os::unix::net::UnixStream,
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Message `type` values the daemon accepts (mirrors
/// `safehoused::envelope::KNOWN_TYPES`; duplicated rather than depending on
/// the `safehoused` crate to keep this shim dependency-light, per its module
/// doc comment).
const KNOWN_MESSAGE_TYPES: [&str; 4] = ["chat", "task", "handoff", "ack"];

fn main() -> Result<()> {
    let raw_args: Vec<String> = env::args().skip(1).collect();

    // Answer --help/--version before anything else so they work regardless of
    // whether stdin is a TTY, a pipe, or /dev/null (e.g. in CI).
    for arg in &raw_args {
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

    // One-shot operator CLI: `safehouse-mcp <subcommand> [flags...]`. Only
    // dispatches for a recognized subcommand name in first position, so a
    // bare invocation (no args) falls through unchanged to the stdio MCP
    // server below — see AC "no subcommand preserves existing behavior".
    if let Some(sub) = raw_args.first() {
        if let Some(op) = build_cli_op(sub, &raw_args[1..])? {
            return run_cli(sub, op);
        }
        if !sub.starts_with('-') {
            eprintln!("safehouse-mcp: unknown subcommand {sub:?}");
            print_usage(&mut std::io::stderr());
            std::process::exit(2);
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

/// Usage note for humans who ran the binary directly. Written to stderr on
/// the TTY-hang guard / unknown-subcommand error, stdout for `--help`.
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
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Or run a one-shot operator command against the same socket (SAFEHOUSED_SOCKET /"
    );
    let _ = writeln!(
        out,
        "SAFEHOUSE_PERSONA still required — see README \"Scripting the socket\"):"
    );
    let _ = writeln!(
        out,
        "  safehouse-mcp read [--room <id|name|alias>] [--limit N]"
    );
    let _ = writeln!(
        out,
        "  safehouse-mcp send --to <persona|*> --body <text> [--type chat|task|handoff|ack]"
    );
    let _ = writeln!(
        out,
        "                      [--task-id ID] [--room <id|name|alias>] [--wake]"
    );
    let _ = writeln!(
        out,
        "  safehouse-mcp check [--limit N] [--consume]   # defaults to peek: never advances the cursor"
    );
    let _ = writeln!(out, "  safehouse-mcp list-rooms");
    let _ = writeln!(
        out,
        "  safehouse-mcp status   # last_event_received/last_sync_completed/retry state — a one-line liveness check"
    );
}

/// Builds the daemon op JSON for a CLI subcommand. Returns `Ok(None)` when
/// `sub` isn't a recognized subcommand name, signaling the caller to fall
/// through to stdio-MCP-server mode instead. Pure and socket-free — the
/// unit tests below exercise this directly, no daemon required.
fn build_cli_op(sub: &str, args: &[String]) -> Result<Option<Value>> {
    let op = match sub {
        "read" => build_read_op(args)?,
        "send" => build_send_op(args)?,
        "check" => build_check_op(args)?,
        "list-rooms" => build_list_rooms_op(args)?,
        "status" => build_status_op(args)?,
        _ => return Ok(None),
    };
    Ok(Some(op))
}

fn build_read_op(args: &[String]) -> Result<Value> {
    let mut op = json!({"op": "read"});
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--room" => {
                let v = flag_value(args, &mut i, "--room")?;
                op["room"] = json!(v);
            }
            "--limit" => {
                let v = flag_u64(args, &mut i, "--limit")?;
                op["limit"] = json!(v);
            }
            other => bail!("read: unknown argument {other:?}"),
        }
    }
    Ok(op)
}

fn build_send_op(args: &[String]) -> Result<Value> {
    let mut op = json!({"op": "send"});
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                let v = flag_value(args, &mut i, "--to")?;
                op["to"] = json!(v);
            }
            "--body" => {
                let v = flag_value(args, &mut i, "--body")?;
                op["body"] = json!(v);
            }
            "--type" => {
                let v = flag_value(args, &mut i, "--type")?;
                anyhow::ensure!(
                    KNOWN_MESSAGE_TYPES.contains(&v.as_str()),
                    "send: --type must be one of {KNOWN_MESSAGE_TYPES:?}, got {v:?}"
                );
                op["type"] = json!(v);
            }
            "--task-id" => {
                let v = flag_value(args, &mut i, "--task-id")?;
                op["task_id"] = json!(v);
            }
            "--room" => {
                let v = flag_value(args, &mut i, "--room")?;
                op["room"] = json!(v);
            }
            "--wake" => {
                op["wake"] = json!(true);
                i += 1;
            }
            other => bail!("send: unknown argument {other:?}"),
        }
    }
    anyhow::ensure!(op.get("to").is_some(), "send: --to is required");
    anyhow::ensure!(op.get("body").is_some(), "send: --body is required");
    Ok(op)
}

/// `check` inverts the MCP tool's default: run bare, this is always a
/// stateless peek (no cursor advance). Advancing the persona's mailbox
/// cursor requires the explicit `--consume` flag. This is the read-vs-check
/// trap the issue calls out — the CLI makes the safe choice the default.
fn build_check_op(args: &[String]) -> Result<Value> {
    let mut consume = false;
    let mut limit: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--consume" => {
                consume = true;
                i += 1;
            }
            "--peek" => {
                // Already the default; accepted so `--peek` is always valid
                // to write explicitly at the call site.
                i += 1;
            }
            "--limit" => {
                limit = Some(flag_u64(args, &mut i, "--limit")?);
            }
            other => bail!("check: unknown argument {other:?}"),
        }
    }
    let mut op = json!({"op": "check", "peek": !consume});
    if let Some(limit) = limit {
        op["limit"] = json!(limit);
    }
    Ok(op)
}

fn build_list_rooms_op(args: &[String]) -> Result<Value> {
    if let Some(other) = args.first() {
        bail!("list-rooms: unknown argument {other:?}");
    }
    Ok(json!({"op": "list_rooms"}))
}

/// #85 — the daemon's liveness/staleness one-liner: `last_event_received`
/// vs. `last_sync_completed` (are they diverging?), connection state, and any
/// in-progress retry/backoff attempt, so diagnosing "is this cut off" doesn't
/// require reading a multi-MB log and running `lsof`. Takes no flags, same
/// shape as `list-rooms`.
fn build_status_op(args: &[String]) -> Result<Value> {
    if let Some(other) = args.first() {
        bail!("status: unknown argument {other:?}");
    }
    Ok(json!({"op": "status"}))
}

/// Reads the value for `flag` at `args[*i]`, advancing `*i` past both.
fn flag_value(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    let value = args
        .get(*i + 1)
        .with_context(|| format!("{flag} requires a value"))?
        .clone();
    *i += 2;
    Ok(value)
}

fn flag_u64(args: &[String], i: &mut usize, flag: &str) -> Result<u64> {
    let raw = flag_value(args, i, flag)?;
    raw.parse()
        .with_context(|| format!("{flag} must be a non-negative integer, got {raw:?}"))
}

/// Runs one CLI subcommand: hello, op, print the JSON reply, exit non-zero
/// if the daemon reported `ok: false` (so scripts can check `$?` without
/// parsing JSON). Connection/protocol failures propagate as `Err` so `main`
/// reports them the same way it reports any other startup error.
fn run_cli(sub: &str, op: Value) -> Result<()> {
    let reply = daemon_call(op).with_context(|| format!("safehouse-mcp {sub}"))?;
    println!("{}", serde_json::to_string_pretty(&reply)?);
    if reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(())
    } else {
        std::process::exit(1);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unrecognized_subcommand_falls_through() {
        // `Ok(None)` is the signal main() uses to fall back to stdio-MCP-server
        // mode — a bare invocation (empty argv) must never be routed to the
        // CLI dispatcher at all, and any non-subcommand first word must not
        // be mistaken for one.
        assert!(build_cli_op("not-a-subcommand", &[]).unwrap().is_none());
        assert!(build_cli_op("--help", &[]).unwrap().is_none());
    }

    #[test]
    fn read_defaults_to_no_room_or_limit() {
        let op = build_read_op(&[]).unwrap();
        assert_eq!(op, json!({"op": "read"}));
    }

    #[test]
    fn read_parses_room_and_limit() {
        let op = build_read_op(&args(&["--room", "fleet-ops", "--limit", "5"])).unwrap();
        assert_eq!(op, json!({"op": "read", "room": "fleet-ops", "limit": 5}));
    }

    #[test]
    fn read_rejects_unknown_flag() {
        assert!(build_read_op(&args(&["--bogus"])).is_err());
    }

    #[test]
    fn read_rejects_non_numeric_limit() {
        assert!(build_read_op(&args(&["--limit", "not-a-number"])).is_err());
    }

    #[test]
    fn send_requires_to_and_body() {
        assert!(build_send_op(&[]).is_err());
        assert!(build_send_op(&args(&["--to", "research_agent"])).is_err());
        assert!(build_send_op(&args(&["--body", "hi"])).is_err());
    }

    #[test]
    fn send_builds_full_op() {
        let op = build_send_op(&args(&[
            "--to",
            "research_agent",
            "--body",
            "status?",
            "--type",
            "task",
            "--task-id",
            "abc123",
            "--room",
            "fleet-ops",
            "--wake",
        ]))
        .unwrap();
        assert_eq!(
            op,
            json!({
                "op": "send",
                "to": "research_agent",
                "body": "status?",
                "type": "task",
                "task_id": "abc123",
                "room": "fleet-ops",
                "wake": true,
            })
        );
    }

    #[test]
    fn send_rejects_unknown_message_type() {
        let err = build_send_op(&args(&[
            "--to",
            "x",
            "--body",
            "hi",
            "--type",
            "not-a-real-type",
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("--type"));
    }

    #[test]
    fn check_defaults_to_peek_true() {
        // The read-vs-check trap the issue names: a bare `check` must never
        // advance a persona's mailbox cursor.
        let op = build_check_op(&[]).unwrap();
        assert_eq!(op, json!({"op": "check", "peek": true}));
    }

    #[test]
    fn check_consume_flips_peek_false() {
        let op = build_check_op(&args(&["--consume"])).unwrap();
        assert_eq!(op, json!({"op": "check", "peek": false}));
    }

    #[test]
    fn check_explicit_peek_flag_stays_peek_true() {
        let op = build_check_op(&args(&["--peek"])).unwrap();
        assert_eq!(op, json!({"op": "check", "peek": true}));
    }

    #[test]
    fn check_parses_limit() {
        let op = build_check_op(&args(&["--limit", "10"])).unwrap();
        assert_eq!(op, json!({"op": "check", "peek": true, "limit": 10}));
    }

    #[test]
    fn list_rooms_takes_no_arguments() {
        assert_eq!(
            build_list_rooms_op(&[]).unwrap(),
            json!({"op": "list_rooms"})
        );
        assert!(build_list_rooms_op(&args(&["--room", "x"])).is_err());
    }

    // ---- `status` subcommand (#85) -----------------------------------------

    #[test]
    fn status_takes_no_arguments() {
        assert_eq!(build_status_op(&[]).unwrap(), json!({"op": "status"}));
        assert!(build_status_op(&args(&["--bogus"])).is_err());
    }

    #[test]
    fn build_cli_op_dispatches_by_subcommand() {
        assert_eq!(
            build_cli_op("list-rooms", &[]).unwrap().unwrap(),
            json!({"op": "list_rooms"})
        );
        assert_eq!(
            build_cli_op("read", &args(&["--room", "x"]))
                .unwrap()
                .unwrap(),
            json!({"op": "read", "room": "x"})
        );
        assert_eq!(
            build_cli_op("status", &[]).unwrap().unwrap(),
            json!({"op": "status"})
        );
    }
}
