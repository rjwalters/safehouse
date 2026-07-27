//! safehoused v0 — the boot path.
//!
//! One Matrix device per host. Cold/warm start, persistent encrypted crypto
//! store, headless cross-signing + recovery (verified live in Q-J, see
//! docs/research/2026-07-26-qj-integration-test.md), auto-join invites,
//! sync v2 loop (D13), decrypt inbound room messages and print them to
//! stdout. No agents, no unix socket yet — that's the next step.

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
    ruma::events::room::{
        encrypted::OriginalSyncRoomEncryptedEvent, member::StrippedRoomMemberEvent,
        message::OriginalSyncRoomMessageEvent,
    },
    Client, Room, RoomState,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{mailbox::Mailbox, rpc::Registry};

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

    client.add_event_handler(on_invite);
    client.add_event_handler(on_message);
    client.add_event_handler(on_undecryptable);
    client.add_event_handler_context(registry.clone());

    let rpc = tokio::spawn(rpc::serve(client.clone(), registry, socket_path.clone()));

    println!("safehoused: entering sync loop (sync v2); ctrl-c to stop");
    let sync = client.sync(SyncSettings::default());
    tokio::select! {
        result = sync => result.context("sync loop exited")?,
        result = rpc => result.context("rpc task panicked")?.context("rpc server exited")?,
        _ = tokio::signal::ctrl_c() => println!("safehoused: shutdown requested"),
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
/// registration, so any invite comes from a user the operator controls.
async fn on_invite(event: StrippedRoomMemberEvent, room: Room, client: Client) {
    let Some(own_user) = client.user_id() else {
        return;
    };
    if event.state_key != own_user || room.state() != RoomState::Invited {
        return;
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
