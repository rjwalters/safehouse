//! safehoused v0 — the boot path.
//!
//! One Matrix device per host. Cold/warm start, persistent encrypted crypto
//! store, headless cross-signing + recovery (verified live in Q-J, see
//! docs/research/2026-07-26-qj-integration-test.md), auto-join invites,
//! sync v2 loop (D13), decrypt inbound room messages and print them to
//! stdout. No agents, no unix socket yet — that's the next step.

mod egress;
mod envelope;
mod mailbox;
mod rpc;

use std::{env, fs, path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{bail, Context, Result};
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{recovery::RecoveryState, BackupDownloadStrategy, EncryptionSettings},
    event_handler::{Ctx, RawEvent},
    room::MessagesOptions,
    ruma::events::room::{
        encrypted::OriginalSyncRoomEncryptedEvent, member::StrippedRoomMemberEvent,
        message::OriginalSyncRoomMessageEvent, redaction::OriginalSyncRoomRedactionEvent,
    },
    Client, Room, RoomState,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    egress::{Egress, EgressConfig},
    mailbox::Mailbox,
    rpc::Registry,
};

/// The egress subsystem is optional; when absent from config the daemon runs
/// exactly as before. This is the event-handler context type carrying that
/// optionality through to `on_message`/`on_redaction`.
type EgressHandle = Option<Arc<Egress>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    homeserver: String,
    username: String,
    password: String,
    state_dir: PathBuf,
    store_passphrase: String,
    /// The only headless path back after a crypto-store loss. Mandatory, not
    /// Option — a daemon without it cannot survive its own disk (D10).
    recovery_passphrase: String,
    /// The reset path orphans every room key in backup. Off unless explicitly
    /// enabled by the operator.
    #[serde(default)]
    recovery_reset_allowed: bool,
    /// Personas allowed to attach over the unix socket (§6). Empty = no
    /// agents can connect; the daemon still mirrors the room to stdout.
    #[serde(default)]
    personas: Vec<String>,
    /// Unix socket path; defaults to `<state_dir>/safehoused.sock`.
    #[serde(default)]
    socket_path: Option<PathBuf>,
    /// Per-persona mailbox store (D17); defaults to
    /// `<state_dir>/mailbox.sqlite3`. Holds only delivery bookkeeping (which
    /// envelopes each persona has consumed) — rebuildable from the room, not
    /// a second source of truth (D6).
    #[serde(default)]
    mailbox_path: Option<PathBuf>,
    /// Invite-acceptance policy (issue #39). `None` (the default) preserves
    /// the daemon's existing accept-any behavior — see `on_invite`'s doc
    /// comment for the rationale. When set, only invites whose sender is a
    /// full Matrix user id (`@user:server`) in this list are joined; every
    /// other invite is declined (logged, not joined).
    #[serde(default)]
    invite_allowlist: Option<Vec<String>>,
    /// Optional public-feed egress (#30). Absent = the egress subsystem is
    /// entirely disabled and the daemon behaves identically to before. When
    /// present with a non-empty `rooms` allowlist, `deny_patterns` MUST also be
    /// non-empty or the daemon refuses to boot (fail-safe, per #28).
    #[serde(default)]
    egress: Option<EgressConfig>,
}

fn load_config() -> Result<Config> {
    let path = env::args()
        .nth(1)
        .or_else(|| env::var("SAFEHOUSED_CONFIG").ok())
        .context("usage: safehoused <config.toml> (or set SAFEHOUSED_CONFIG)")?;
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
    toml::from_str(&raw).with_context(|| format!("parsing {path}"))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("safehoused: fatal: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let config = load_config()?;
    let client = boot(&config).await?;

    for persona in &config.personas {
        anyhow::ensure!(
            envelope::valid_persona(persona),
            "invalid persona {persona:?} in config"
        );
    }
    let mailbox_path = config
        .mailbox_path
        .clone()
        .unwrap_or_else(|| config.state_dir.join("mailbox.sqlite3"));
    let mailbox = Mailbox::open(&mailbox_path)
        .with_context(|| format!("opening mailbox store {}", mailbox_path.display()))?;
    let registry = Registry::new(config.personas.clone(), mailbox);
    let socket_path = config
        .socket_path
        .clone()
        .unwrap_or_else(|| config.state_dir.join("safehoused.sock"));

    // Optional public-feed egress (#30). Built (and its boot guard enforced)
    // only when configured; otherwise `None` threads through every handler as a
    // no-op, so the daemon runs exactly as it did before this feature existed.
    let egress: EgressHandle = match config.egress.clone() {
        Some(cfg) => {
            let db_path = config.state_dir.join("egress.sqlite3");
            let egress = Egress::open(cfg, &db_path).context("initializing egress")?;
            println!(
                "safehoused: egress enabled (delay buffer at {})",
                db_path.display()
            );
            Some(egress)
        }
        None => None,
    };

    client.add_event_handler(on_invite);
    client.add_event_handler(on_message);
    client.add_event_handler(on_redaction);
    client.add_event_handler(on_undecryptable);
    client.add_event_handler_context(registry.clone());
    client.add_event_handler_context(Arc::new(config.invite_allowlist.clone()));
    client.add_event_handler_context(egress.clone());

    // The background flush task: it polls the durable delay buffer and writes
    // due, un-retracted rows to the sink. Only spawned when egress is on.
    if let Some(egress) = egress.clone() {
        tokio::spawn(egress.run());
    }

    // Rebuild §2/§5.2 thread bookkeeping from durable room history before any
    // live event can be routed. `ThreadState` is in-memory (D6: rebuildable
    // from the room, never persisted), so after a restart it starts empty;
    // without this replay, an un-tokened human thread reply (§5.2) that
    // predates the restart resolves no target and silently falls through to a
    // no-wake §5.3 broadcast until some later event re-establishes the thread.
    replay_thread_history(&client, &registry).await;

    let rpc = tokio::spawn(rpc::serve(client.clone(), registry, socket_path.clone()));

    println!("safehoused: entering sync loop (sync v2); ctrl-c to stop");
    let sync = client.sync(SyncSettings::default());
    // SIGTERM is the routine supervised-stop signal (systemd `stop`, launchctl
    // `bootout`, bare `kill`), so it MUST reach the same clean-shutdown path as
    // ctrl-c below — otherwise the default termination action skips both the
    // socket cleanup and the load-bearing backup flush, leaving any just-minted
    // room key unrecoverable after a store loss (found live in Q-J). The daemon
    // is unix-only by invariant D8, so no cfg(unix) guard is needed. Register
    // before the select! because installation can fail.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        result = sync => result.context("sync loop exited")?,
        result = rpc => result.context("rpc task panicked")?.context("rpc server exited")?,
        _ = tokio::signal::ctrl_c() => println!("safehoused: shutdown requested"),
        _ = sigterm.recv() => println!("safehoused: shutdown requested (SIGTERM)"),
    }
    let _ = fs::remove_file(&socket_path);

    // A room key minted moments ago may not have reached the server-side
    // backup yet; exiting without flushing makes it unrecoverable after a
    // store loss. Found live in Q-J.
    client
        .encryption()
        .backups()
        .wait_for_steady_state()
        .await?;
    println!("safehoused: room-key backup flushed; bye");
    Ok(())
}

/// How far back to walk each room's history on boot when rebuilding thread
/// bookkeeping (§2/§5.2). Only currently-open threads matter, so this is a
/// bound rather than full-history replay — a restart re-establishes routing
/// for recently-active threads without paginating the entire room.
const THREAD_REPLAY_MAX_EVENTS: usize = 500;

/// Rebuild [`rpc::ThreadState`] from recent history for every joined room,
/// before the live sync loop can route anything. Best-effort per room: a
/// pagination failure is logged and skipped rather than aborting boot, since
/// degraded thread routing (the pre-replay status quo) is strictly better than
/// refusing to start. See [`replay_thread_events`] for the per-event logic.
async fn replay_thread_history(client: &Client, registry: &Registry) {
    for room in client.joined_rooms() {
        match collect_recent_events(&room, THREAD_REPLAY_MAX_EVENTS).await {
            Ok(events) => {
                if !events.is_empty() {
                    replay_thread_events(&registry.threads, &registry.personas, &events).await;
                    println!(
                        "safehoused: replayed {} event(s) of {} history to rebuild thread state",
                        events.len(),
                        room.room_id()
                    );
                }
            }
            Err(err) => eprintln!(
                "safehoused: thread-state replay for {} failed \
                 (in-thread routing may be degraded until the next live event): {err:#}",
                room.room_id()
            ),
        }
    }
}

/// Walk `room` backward from the live edge, collecting up to `max` decrypted
/// events as parsed JSON in **chronological** (oldest-first) order — the same
/// order the live sync path observes them, which [`rpc::ThreadState::observe`]
/// depends on (it records the *first* root seen for a `task_id` and the
/// *latest* agent addressed in a thread). Uses the same backward pagination as
/// the `read` RPC op, continuing via the returned `end` token until `max` is
/// reached or history is exhausted.
async fn collect_recent_events(room: &Room, max: usize) -> Result<Vec<Value>> {
    let mut newest_first: Vec<Value> = Vec::new();
    let mut from: Option<String> = None;
    while newest_first.len() < max {
        let mut options = MessagesOptions::backward();
        options.from = from.clone();
        let remaining = max - newest_first.len();
        options.limit = (remaining.min(100) as u32).into();
        let batch = room.messages(options).await?;
        if batch.chunk.is_empty() {
            break;
        }
        for event in &batch.chunk {
            if let Ok(parsed) = serde_json::from_str::<Value>(event.raw().json().get()) {
                newest_first.push(parsed);
            }
        }
        match batch.end {
            Some(token) => from = Some(token),
            None => break, // reached the start of accessible history
        }
    }
    newest_first.reverse();
    Ok(newest_first)
}

/// Replay a chronological slice of room-event JSON through
/// [`rpc::ThreadState::observe`], reconstructing the §2/§5.2 index exactly as
/// `on_message` does for live events: resolve the thread root, resolve the
/// currently-addressed agent from the state rebuilt so far, interpret the
/// envelope (skipping unsupported versions, as the live path drops them), then
/// observe. Non-`m.room.message` events are ignored. Factored out of the boot
/// path so it can be unit-tested without a live homeserver.
async fn replay_thread_events(threads: &rpc::ThreadState, personas: &[String], events: &[Value]) {
    for parsed in events {
        if parsed.get("type").and_then(Value::as_str) != Some("m.room.message") {
            continue;
        }
        let Some(event_id) = parsed.get("event_id").and_then(Value::as_str) else {
            continue;
        };
        let content = parsed.get("content").cloned().unwrap_or(Value::Null);
        let sender = parsed.get("sender").and_then(Value::as_str).unwrap_or("");
        let thread_root =
            envelope::thread_root_from_content(&content).unwrap_or_else(|| event_id.to_owned());
        let thread_agent = threads.target_for_thread(&thread_root).await;
        let env =
            match envelope::from_event_json(&content, sender, personas, thread_agent.as_deref()) {
                envelope::Inbound::Envelope(env, _unknown) => env,
                // §7.2: never let an unsupported-version envelope into the index —
                // the live path drops it, so replay must too.
                envelope::Inbound::UnsupportedVersion(_) => continue,
            };
        threads.observe(&thread_root, event_id, &env).await;
    }
}

/// Cold/warm/recovery boot. The sequence Q-J verified live: build client on an
/// encrypted sqlite store, login or restore, wait for the E2EE init tasks
/// (cross-signing bootstrap via MSC3967), then reconcile recovery state with
/// the mandatory passphrase.
async fn boot(config: &Config) -> Result<Client> {
    let store_dir = config.state_dir.join("store");
    let session_path = config.state_dir.join("session.json");

    // Store and session blob are one unit. A store without its session is
    // undecryptable; refuse to guess (open-questions Q-B).
    if store_dir.exists() && !session_path.exists() {
        bail!(
            "store exists but session.json is missing — the store is undecryptable; \
             wipe {} and cold-start (recovery passphrase will restore keys)",
            config.state_dir.display()
        );
    }

    let client = Client::builder()
        .homeserver_url(&config.homeserver)
        .sqlite_store(&store_dir, Some(&config.store_passphrase))
        // All three default to off; a daemon missing them silently breaks when
        // Element's insecure-device exclusion lands (~Oct 2026).
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: true,
            auto_enable_backups: true,
            backup_download_strategy: BackupDownloadStrategy::OneShot,
        })
        .build()
        .await?;

    if session_path.exists() {
        let session: MatrixSession = serde_json::from_str(&fs::read_to_string(&session_path)?)?;
        client.restore_session(session).await?;
        println!("safehoused: warm start");
    } else {
        // Password login, not token restore: it supplies AuthData::Password to
        // the cross-signing bootstrap, so we don't depend solely on MSC3967.
        client
            .matrix_auth()
            .login_username(&config.username, &config.password)
            .initial_device_display_name("safehoused")
            .await?;
        let session = client
            .matrix_auth()
            .session()
            .context("no session after login")?;
        fs::create_dir_all(&config.state_dir)?;
        let tmp = session_path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(&session)?)?;
        fs::rename(&tmp, &session_path)?;
        println!("safehoused: cold start, new device");
    }

    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;
    client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;

    let recovery = client.encryption().recovery();
    match recovery.state() {
        RecoveryState::Enabled => {}
        RecoveryState::Disabled => {
            // First run ever for this account: mint secret storage guarded by
            // the passphrase. The minted key is intentionally not printed —
            // the passphrase is the operator's handle.
            recovery
                .enable()
                .with_passphrase(&config.recovery_passphrase)
                .await?;
            println!("safehoused: recovery enabled with configured passphrase");
        }
        state @ (RecoveryState::Incomplete | RecoveryState::Unknown) => {
            println!("safehoused: recovery state {state:?}, recovering with passphrase");
            recovery
                .recover(&config.recovery_passphrase)
                .await
                .context(
                    "recovery failed — wrong passphrase? \
                     (recovery_reset_allowed is the destructive way out and orphans \
                     every backed-up room key)",
                )?;
        }
    }
    if config.recovery_reset_allowed {
        println!("safehoused: warning: recovery_reset_allowed=true (destructive path armed)");
    }

    let device = client
        .encryption()
        .get_own_device()
        .await?
        .context("own device not found")?;
    if !device.is_cross_signed_by_owner() {
        bail!(
            "device {} failed to self-sign — refusing to run insecure \
             (would be excluded by Element ~Oct 2026)",
            device.device_id()
        );
    }
    println!(
        "safehoused: {} device {} cross-signed; backup {:?}",
        client.user_id().context("no user id")?,
        device.device_id(),
        client.encryption().backups().state()
    );
    Ok(client)
}

/// The room is invite-only from our side: the sealed homeserver has no open
/// registration, so by default any invite comes from a user the operator
/// controls and is accepted. When `invite_allowlist` (issue #39) is
/// configured, only invites from a sender in that list are joined — an
/// explicit opt-in tightening of this default, not a change to it.
async fn on_invite(
    event: StrippedRoomMemberEvent,
    room: Room,
    client: Client,
    Ctx(invite_allowlist): Ctx<Arc<Option<Vec<String>>>>,
) {
    let Some(own_user) = client.user_id() else {
        return;
    };
    if event.state_key != own_user || room.state() != RoomState::Invited {
        return;
    }
    if let Some(allowlist) = invite_allowlist.as_ref() {
        if !allowlist.iter().any(|u| u == event.sender.as_str()) {
            println!(
                "safehoused: declining invite to {} from {} (not in invite_allowlist)",
                room.room_id(),
                event.sender
            );
            return;
        }
    }
    println!(
        "safehoused: invited to {} by {}, joining",
        room.room_id(),
        event.sender
    );
    if let Err(err) = room.join().await {
        eprintln!("safehoused: joining {} failed: {err:#}", room.room_id());
    }
}

async fn on_message(
    event: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    raw: RawEvent,
    Ctx(registry): Ctx<Arc<Registry>>,
    Ctx(egress): Ctx<EgressHandle>,
) {
    let own_event = Some(event.sender.as_ref()) == client.user_id();
    let content: Value = serde_json::from_str(raw.get())
        .ok()
        .and_then(|v: Value| v.get("content").cloned())
        .unwrap_or(Value::Null);

    if !own_event {
        println!(
            "[{}] {}: {}",
            room.name().unwrap_or_else(|| room.room_id().to_string()),
            event.sender,
            event.content.body()
        );
    }

    // §5.2: resolve the thread this event belongs to (its own id, unless it
    // explicitly relates to an earlier root via `m.thread`) and, from prior
    // observations, which persona is currently addressed there — used only
    // for human-message synthesis below, ignored for events that already
    // carry an envelope.
    let event_id = event.event_id.to_string();
    let thread_root =
        envelope::thread_root_from_content(&content).unwrap_or_else(|| event_id.clone());
    let thread_agent = registry.threads.target_for_thread(&thread_root).await;

    // §7.2/§9: gate on envelope version. An unsupported `v` is never dispatched
    // or guess-parsed — it is logged and surfaced to the human once per sender.
    let (env, unknown_persona) = match envelope::from_event_json(
        &content,
        event.sender.as_str(),
        &registry.personas,
        thread_agent.as_deref(),
    ) {
        envelope::Inbound::Envelope(env, unknown_persona) => (env, unknown_persona),
        envelope::Inbound::UnsupportedVersion(v) => {
            eprintln!(
                "safehoused: ignoring unsupported envelope version {v} from {} in {} (event {})",
                event.sender,
                room.room_id(),
                event.event_id
            );
            if registry
                .mark_unsupported_surfaced(event.sender.as_str(), v)
                .await
            {
                surface_unsupported_version(&room, &client, event.sender.as_str(), v).await;
            }
            return;
        }
    };

    // Record this event's contribution to thread state (§2 task_id -> root,
    // §5.2 root -> current agent) before dispatching, so a same-tick reply
    // (e.g. this daemon's own subsequent `send`) already sees it.
    registry
        .threads
        .observe(&thread_root, &event_id, &env)
        .await;

    let push = json!({
        "event": "message",
        "room_id": room.room_id(),
        "room_name": room.name(),
        "sender": event.sender,
        "event_id": event.event_id,
        "envelope": env,
    });
    // §7 refined: own events still dispatch to local agents, skipping only the
    // authoring persona — that's how same-host agent-to-agent traffic flows
    // while staying loop-free.
    registry
        .dispatch(&push.to_string(), own_event, &env.from)
        .await;

    // D16/D17: the durable mailbox write. Unlike the live push above, this
    // always runs — receipt must not depend on any agent being connected.
    // Resolution (direct address / broadcast / not-ours-to-keep) happens
    // inside `mailbox_deliver`, per envelope-v1 §7.
    if let Err(err) = registry
        .mailbox_deliver(
            own_event,
            room.room_id().as_str(),
            event.event_id.as_str(),
            event.sender.as_str(),
            &env,
        )
        .await
    {
        eprintln!(
            "safehoused: mailbox delivery failed for event {} in {}: {err:#}",
            event.event_id,
            room.room_id()
        );
    }

    // #30: public-feed egress. Only runs when configured; the allowlist +
    // is_allowlisted gate lives inside `consider`. A native edit (`m.replace`)
    // of a source event inside its delay window suppresses the pending
    // completion — the same "undo" a human has in Element — so an edit is
    // routed to `retract` rather than treated as a fresh completion to queue.
    if let Some(egress) = egress.as_ref() {
        let room_id = room.room_id().as_str().to_owned();
        if let Some(target) = egress::edit_target(&content) {
            if let Err(err) = egress.retract(&room_id, &target).await {
                eprintln!(
                    "safehoused: egress retract (edit of {target}) in {room_id} failed: {err:#}"
                );
            }
        } else if let Err(err) = egress.consider(&room_id, &event_id, &env).await {
            eprintln!(
                "safehoused: egress consider for event {} in {room_id} failed: {err:#}",
                event.event_id
            );
        }
    }

    // §5.1: a human addressed an unknown persona — post a visible ack rather
    // than let the message silently fall back to a no-wake broadcast. The ack
    // itself carries a real envelope, so the next sync round-trips through
    // the early-return branch of `from_event_json` above and never re-enters
    // this path.
    if let Some(token) = unknown_persona {
        let ack = envelope::unknown_persona_ack(&env.from, &token, &registry.personas);
        let content = envelope::to_event_content(&ack, None);
        if let Err(err) = room.send_raw("m.room.message", content).await {
            eprintln!(
                "safehoused: failed to post unknown-persona ack for @{token} in {}: {err:#}",
                room.room_id()
            );
        }
    }
}

/// #30: a native Matrix redaction of a source event suppresses any completion
/// still sitting in the egress delay buffer for that event — reusing the human's
/// existing "delete message" as the retract signal rather than a bespoke
/// envelope type. No-op when egress is off, or when the redaction targets an
/// event with no pending row (an unrelated redaction, or one arriving after the
/// completion already published). The `redacts` target is read from the raw
/// event JSON so this stays correct across room versions (v11 moved `redacts`
/// into `content`).
async fn on_redaction(
    _event: OriginalSyncRoomRedactionEvent,
    room: Room,
    raw: RawEvent,
    Ctx(egress): Ctx<EgressHandle>,
) {
    let Some(egress) = egress.as_ref() else {
        return;
    };
    let room_id = room.room_id().as_str().to_owned();
    if !egress.is_egress_room(&room_id) {
        return;
    }
    let parsed: Value = match serde_json::from_str(raw.get()) {
        Ok(v) => v,
        Err(_) => return,
    };
    // `redacts` is top-level pre-v11 and under `content` in v11+ — accept either.
    let target = parsed.get("redacts").and_then(Value::as_str).or_else(|| {
        parsed
            .get("content")
            .and_then(|c| c.get("redacts"))
            .and_then(Value::as_str)
    });
    if let Some(target) = target {
        if let Err(err) = egress.retract(&room_id, target).await {
            eprintln!(
                "safehoused: egress retract (redaction of {target}) in {room_id} failed: {err:#}"
            );
        }
    }
}

/// Post a human-legible notice to the room that a message used an envelope
/// version this daemon can't speak (§7.2). The notice is itself a valid v1
/// envelope stamped from the daemon's own Matrix user, so Element renders it in
/// the timeline the human is already reading. Send failures are logged, never
/// fatal — surfacing is best-effort.
async fn surface_unsupported_version(room: &Room, client: &Client, sender: &str, v: u64) {
    let from = client
        .user_id()
        .map(|u| u.to_string())
        .unwrap_or_else(|| "safehoused".to_owned());
    let env = envelope::Envelope {
        v: envelope::SUPPORTED_VERSION,
        from,
        to: "*".to_owned(),
        kind: "ack".to_owned(),
        task_id: None,
        body: format!(
            "Ignored a message from {sender} using unsupported safehouse envelope version {v} \
             (this daemon speaks v{}). It was not delivered to any agent.",
            envelope::SUPPORTED_VERSION
        ),
        wake: None,
        meta: None,
    };
    let content = envelope::to_event_content(&env, None);
    if let Err(err) = room.send_raw("m.room.message", content).await {
        eprintln!("safehoused: failed to surface unsupported-version notice for {sender}: {err:#}");
    }
}

/// An event that reaches this handler stayed encrypted after decryption was
/// attempted. Surface it — silence is how key bugs go unnoticed.
async fn on_undecryptable(event: OriginalSyncRoomEncryptedEvent, room: Room) {
    eprintln!(
        "safehoused: UNDECRYPTABLE event {} from {} in {}",
        event.event_id,
        event.sender,
        room.room_id()
    );
}

/// Config parsing — in particular the #30 `egress` block. These lock in the
/// fail-safe defaults without a live homeserver: egress is opt-in (absent =
/// `None`, zero behavior change) and its boot guard rejects an allowlisted room
/// with no deny patterns.
#[cfg(test)]
mod config_tests {
    use super::*;

    /// A minimal otherwise-valid config, with `extra` appended for the egress
    /// block (or empty for the no-egress regression case).
    fn config_toml(extra: &str) -> String {
        format!(
            "homeserver = \"https://hs.example\"\n\
             username = \"safehoused\"\n\
             password = \"pw\"\n\
             state_dir = \"/tmp/safehoused\"\n\
             store_passphrase = \"sp\"\n\
             recovery_passphrase = \"rp\"\n\
             {extra}"
        )
    }

    #[test]
    fn config_without_egress_leaves_it_disabled() {
        // Regression: a config with no `egress` block parses and the subsystem
        // is off — the daemon must behave identically to before #30.
        let config: Config = toml::from_str(&config_toml("")).unwrap();
        assert!(config.egress.is_none());
    }

    #[test]
    fn config_parses_a_full_egress_block() {
        let config: Config = toml::from_str(&config_toml(
            "[egress]\n\
             rooms = [\"!feed:example\"]\n\
             deny_patterns = [\"secret\"]\n\
             delay_seconds = 30\n\
             sink_path = \"/tmp/feed.jsonl\"\n",
        ))
        .unwrap();
        let egress = config.egress.expect("egress block present");
        assert_eq!(egress.rooms, vec!["!feed:example".to_owned()]);
        assert_eq!(egress.deny_patterns, vec!["secret".to_owned()]);
        assert_eq!(egress.delay_seconds, 30);
        // The boot guard accepts a room paired with a deny pattern.
        assert!(egress::validate_egress_config(&egress).is_ok());
    }

    #[test]
    fn egress_rooms_without_deny_patterns_is_a_boot_error() {
        // The daemon must refuse to boot (via `Egress::open` -> the guard) when
        // a room is opted in but no deny patterns are configured.
        let config: Config = toml::from_str(&config_toml(
            "[egress]\n\
             rooms = [\"!feed:example\"]\n\
             sink_path = \"/tmp/feed.jsonl\"\n",
        ))
        .unwrap();
        let egress = config.egress.expect("egress block present");
        assert!(egress::validate_egress_config(&egress).is_err());
    }

    #[test]
    fn egress_block_parses_sink_url_in_place_of_sink_path() {
        // #31: an existing `sink_path`-only config keeps parsing unchanged
        // (tested above); a new config can instead configure `sink_url` for
        // the outbound HTTP sink, with no `sink_path` at all.
        let config: Config = toml::from_str(&config_toml(
            "[egress]\n\
             rooms = [\"!feed:example\"]\n\
             deny_patterns = [\"secret\"]\n\
             delay_seconds = 30\n\
             sink_url = \"https://feed.example.com/ingest\"\n",
        ))
        .unwrap();
        let egress = config.egress.expect("egress block present");
        assert_eq!(
            egress.sink_url.as_deref(),
            Some("https://feed.example.com/ingest")
        );
        assert!(egress.sink_path.is_none());
        assert!(egress::validate_egress_config(&egress).is_ok());
    }

    #[test]
    fn egress_block_without_any_sink_is_a_boot_error() {
        // #31: an egress block that configures neither sink_path nor sink_url
        // has no publish target — refuse to boot rather than silently no-op.
        let config: Config = toml::from_str(&config_toml(
            "[egress]\n\
             rooms = [\"!feed:example\"]\n\
             deny_patterns = [\"secret\"]\n",
        ))
        .unwrap();
        let egress = config.egress.expect("egress block present");
        assert!(egress::validate_egress_config(&egress).is_err());
    }

    #[test]
    fn unknown_config_key_is_rejected() {
        // `deny_unknown_fields` still holds with the new optional field present.
        assert!(toml::from_str::<Config>(&config_toml("bogus_key = true\n")).is_err());
    }
}

/// Boot-time thread-state replay (§2/§5.2). These exercise
/// [`replay_thread_events`] directly — the pure per-event replay logic — which
/// is what makes the fix testable without a live homeserver (the paginating
/// [`collect_recent_events`] wrapper does the network I/O and is covered in
/// production, not here).
#[cfg(test)]
mod replay_tests {
    use serde_json::{json, Value};

    use super::*;

    fn personas() -> Vec<String> {
        vec!["writer_agent".to_owned(), "research_agent".to_owned()]
    }

    /// A room event carrying an explicit agent envelope of `type: task`.
    /// `root: None` means the event *is* a thread root (no `m.relates_to`);
    /// `Some(root)` threads it under an earlier root via `m.thread`.
    fn agent_event(event_id: &str, root: Option<&str>, to: &str, task_id: Option<&str>) -> Value {
        let mut content = json!({
            "msgtype": "m.text",
            "body": "rendered text agents never read",
            envelope::ENVELOPE_KEY: {
                "v": 1,
                "from": "writer_agent",
                "to": to,
                "type": "task",
                "task_id": task_id,
                "body": "go",
            },
        });
        if let Some(root) = root {
            content["m.relates_to"] = envelope::thread_relation(root, root);
        }
        json!({
            "type": "m.room.message",
            "event_id": event_id,
            "sender": "@safehoused:x",
            "content": content,
        })
    }

    /// A human message with no envelope, threaded under `root` — the §5.2
    /// case: no `@token`, routing depends entirely on rebuilt thread state.
    fn human_thread_reply(event_id: &str, root: &str, body: &str) -> Value {
        json!({
            "type": "m.room.message",
            "event_id": event_id,
            "sender": "@robb:x",
            "content": {
                "msgtype": "m.text",
                "body": body,
                "m.relates_to": envelope::thread_relation(root, root),
            },
        })
    }

    #[tokio::test]
    async fn replay_rebuilds_task_root_and_thread_target() {
        // A single agent task event establishes the whole index for its
        // thread: the task's root, the latest event, and who's addressed.
        let threads = rpc::ThreadState::default();
        let events = vec![agent_event("$root", None, "research_agent", Some("t"))];
        replay_thread_events(&threads, &personas(), &events).await;

        assert_eq!(threads.root_for_task("t").await.as_deref(), Some("$root"));
        assert_eq!(
            threads.target_for_thread("$root").await.as_deref(),
            Some("research_agent")
        );
        assert_eq!(
            threads.latest_in_thread("$root").await.as_deref(),
            Some("$root")
        );
    }

    #[tokio::test]
    async fn replay_after_restart_restores_untokened_reply_routing() {
        // The issue's exact scenario: a task thread is established, then the
        // daemon restarts. Replaying history must restore `thread_target` so
        // an un-tokened §5.2 reply resolves an agent instead of falling
        // through to a no-wake §5.3 broadcast. Also verifies chronological
        // replay: the target follows the *latest* agent addressed, the task
        // root is never overwritten, and a later human reply advances only
        // `latest_in_thread`.
        let threads = rpc::ThreadState::default();
        let events = vec![
            agent_event("$root", None, "research_agent", Some("t")),
            agent_event("$reply1", Some("$root"), "writer_agent", Some("t")),
            human_thread_reply("$reply2", "$root", "and one more thing"),
        ];
        replay_thread_events(&threads, &personas(), &events).await;

        // Was empty before replay (the bug) — now resolves the last agent
        // addressed, so the un-tokened reply above routed to it, not "*".
        assert_eq!(
            threads.target_for_thread("$root").await.as_deref(),
            Some("writer_agent")
        );
        assert_eq!(
            threads.latest_in_thread("$root").await.as_deref(),
            Some("$reply2")
        );
        assert_eq!(
            threads.root_for_task("t").await.as_deref(),
            Some("$root"),
            "the first root seen for a task_id must never be overwritten"
        );
    }

    #[tokio::test]
    async fn replay_skips_unsupported_versions_and_non_messages() {
        let threads = rpc::ThreadState::default();
        let unsupported = json!({
            "type": "m.room.message",
            "event_id": "$bad",
            "sender": "@evil:remote",
            "content": {
                "body": "rendered",
                envelope::ENVELOPE_KEY: {
                    "v": 2,
                    "from": "writer_agent",
                    "to": "research_agent",
                    "type": "task",
                    "task_id": "t",
                    "body": "go",
                },
            },
        });
        let reaction = json!({
            "type": "m.reaction",
            "event_id": "$react",
            "sender": "@robb:x",
            "content": { "m.relates_to": { "rel_type": "m.annotation", "event_id": "$x" } },
        });
        replay_thread_events(&threads, &personas(), &[unsupported, reaction]).await;

        // Neither event contributed to the index.
        assert!(threads.root_for_task("t").await.is_none());
        assert!(threads.target_for_thread("$bad").await.is_none());
    }

    #[tokio::test]
    async fn replay_ignores_events_missing_an_event_id() {
        // Defensive: a malformed history entry with no `event_id` must be
        // skipped, not panic or misindex.
        let threads = rpc::ThreadState::default();
        let malformed = json!({
            "type": "m.room.message",
            "sender": "@robb:x",
            "content": { "body": "no event id here" },
        });
        replay_thread_events(&threads, &personas(), &[malformed]).await;
        assert!(threads.target_for_thread("$anything").await.is_none());
    }
}
