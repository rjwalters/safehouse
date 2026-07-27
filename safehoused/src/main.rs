//! safehoused v0 — the boot path.
//!
//! One Matrix device per host. Cold/warm start, persistent encrypted crypto
//! store, headless cross-signing + recovery (verified live in Q-J, see
//! docs/research/2026-07-26-qj-integration-test.md), auto-join invites,
//! sync v2 loop (D13), decrypt inbound room messages and print them to
//! stdout. No agents, no unix socket yet — that's the next step.

use std::{env, fs, path::PathBuf, process::ExitCode};

use anyhow::{bail, Context, Result};
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{recovery::RecoveryState, BackupDownloadStrategy, EncryptionSettings},
    ruma::events::room::{
        encrypted::OriginalSyncRoomEncryptedEvent, member::StrippedRoomMemberEvent,
        message::OriginalSyncRoomMessageEvent,
    },
    Client, Room, RoomState,
};
use serde::Deserialize;

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

    client.add_event_handler(on_invite);
    client.add_event_handler(on_message);
    client.add_event_handler(on_undecryptable);

    println!("safehoused: entering sync loop (sync v2); ctrl-c to stop");
    let sync = client.sync(SyncSettings::default());
    tokio::select! {
        result = sync => result.context("sync loop exited")?,
        _ = tokio::signal::ctrl_c() => println!("safehoused: shutdown requested"),
    }

    // A room key minted moments ago may not have reached the server-side
    // backup yet; exiting without flushing makes it unrecoverable after a
    // store loss. Found live in Q-J.
    client.encryption().backups().wait_for_steady_state().await?;
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
        let session = client.matrix_auth().session().context("no session after login")?;
        fs::create_dir_all(&config.state_dir)?;
        let tmp = session_path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(&session)?)?;
        fs::rename(&tmp, &session_path)?;
        println!("safehoused: cold start, new device");
    }

    client.encryption().wait_for_e2ee_initialization_tasks().await;
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
            recovery.enable().with_passphrase(&config.recovery_passphrase).await?;
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
    let Some(own_user) = client.user_id() else { return };
    if event.state_key != own_user || room.state() != RoomState::Invited {
        return;
    }
    println!("safehoused: invited to {} by {}, joining", room.room_id(), event.sender);
    if let Err(err) = room.join().await {
        eprintln!("safehoused: joining {} failed: {err:#}", room.room_id());
    }
}

async fn on_message(event: OriginalSyncRoomMessageEvent, room: Room, client: Client) {
    if Some(event.sender.as_ref()) == client.user_id() {
        return; // the don't-loop-back filter — the only special case (envelope §7)
    }
    println!(
        "[{}] {}: {}",
        room.name().unwrap_or_else(|| room.room_id().to_string()),
        event.sender,
        event.content.body()
    );
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
