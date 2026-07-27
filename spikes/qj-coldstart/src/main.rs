//! Q-J: live integration test for the headless cold-start / recovery sequence.
//!
//! Runs the sequence from design.md §4.1.1 against a local tuwunel and reports
//! what the research (research/2026-07-26-headless-login.md) predicted on paper:
//!   cold start  → login, bootstrap cross-signing (MSC3967), self-sign, enable
//!                 backup + recovery passphrase, send an encrypted message.
//!   warm start  → restore session, everything already in place.
//!   recovery    → after the crypto store is wiped: fresh login, bootstrap
//!                 no-ops, recover(passphrase) self-signs the replacement
//!                 device and OneShot pulls room keys back — proven by
//!                 decrypting the message sent before the wipe.
//!
//! Throwaway by design; kept in-repo as Q-J provenance. Not daemon code.

use std::{env, fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use matrix_sdk::{
    Client, Room,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    deserialized_responses::TimelineEventKind,
    encryption::{BackupDownloadStrategy, EncryptionSettings, recovery::RecoveryState},
    room::MessagesOptions,
    ruma::{
        OwnedUserId, api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::room::message::RoomMessageEventContent,
    },
};

const ROOM_NAME: &str = "safehouse-test";

fn required(var: &str) -> Result<String> {
    env::var(var).with_context(|| format!("{var} must be set"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let homeserver = env::var("QJ_HOMESERVER").unwrap_or_else(|_| "http://127.0.0.1:8008".into());
    let user = env::var("QJ_USER").unwrap_or_else(|_| "safehouse-bot".into());
    let password = required("QJ_PASSWORD")?;
    let store_pass = required("QJ_STORE_PASS")?;
    // Mandatory, not Option — the only headless path back after a store loss (D10 / handoff §2).
    let recovery_pass = required("QJ_RECOVERY_PASS")?;
    let invite: OwnedUserId = required("QJ_INVITE")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("QJ_INVITE is not a valid user id"))?;
    let state_dir = PathBuf::from(env::var("QJ_STATE_DIR").unwrap_or_else(|_| "qj-state".into()));

    let store_dir = state_dir.join("store");
    let session_path = state_dir.join("session.json");

    // The consistency invariant from open-questions Q-B: a store without its session
    // blob is undecryptable. Refuse to guess; the operator wipes and cold-starts.
    if store_dir.exists() && !session_path.exists() {
        bail!(
            "store exists but session.json is missing — store is undecryptable; \
             wipe {} and cold-start",
            state_dir.display()
        );
    }

    let client = Client::builder()
        .homeserver_url(&homeserver)
        .sqlite_store(&store_dir, Some(&store_pass))
        // Every one of these defaults to off; a daemon missing them silently breaks
        // when Element's insecure-device exclusion lands (~Oct 2026).
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: true,
            auto_enable_backups: true,
            backup_download_strategy: BackupDownloadStrategy::OneShot,
        })
        .build()
        .await?;

    let run_kind;
    if session_path.exists() {
        let session: MatrixSession = serde_json::from_str(&fs::read_to_string(&session_path)?)?;
        client.restore_session(session).await?;
        run_kind = "warm";
    } else {
        client
            .matrix_auth()
            .login_username(&user, &password)
            .initial_device_display_name("safehoused-qj")
            .await?;
        let session = client
            .matrix_auth()
            .session()
            .context("no session after login")?;
        fs::create_dir_all(&state_dir)?;
        // Session blob and store must land together — write-then-rename.
        let tmp = session_path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string(&session)?)?;
        fs::rename(&tmp, &session_path)?;
        run_kind = "cold";
    }
    let device_id = client.device_id().context("no device id")?.to_owned();
    println!("== {run_kind} start; device {device_id}");

    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;
    client.sync_once(SyncSettings::default()).await?;

    let recovery = client.encryption().recovery();
    let mut run_kind = run_kind;
    match recovery.state() {
        RecoveryState::Enabled => println!("recovery: already enabled"),
        RecoveryState::Disabled => {
            let key = recovery.enable().with_passphrase(&recovery_pass).await?;
            println!("recovery: enabled with passphrase (recovery key also minted: {key})");
        }
        state @ (RecoveryState::Incomplete | RecoveryState::Unknown) => {
            println!("recovery: state {state:?} — recovering with passphrase");
            recovery.recover(&recovery_pass).await?;
            println!("recovery: secrets imported from secret storage");
            run_kind = "recovered";
        }
    }

    let enc = client.encryption();
    let cs = enc
        .cross_signing_status()
        .await
        .context("no cross-signing status")?;
    let device = enc
        .get_own_device()
        .await?
        .context("own device not found")?;
    println!(
        "cross-signing: master={} self_signing={} user_signing={}",
        cs.has_master, cs.has_self_signing, cs.has_user_signing
    );
    println!(
        "device {}: cross_signed_by_owner={} verified={}",
        device.device_id(),
        device.is_cross_signed_by_owner(),
        device.is_verified()
    );
    println!(
        "backup: {:?} | recovery: {:?}",
        enc.backups().state(),
        recovery.state()
    );

    let room = match client
        .joined_rooms()
        .into_iter()
        .find(|r| r.name().as_deref() == Some(ROOM_NAME))
    {
        Some(room) => room,
        None => {
            let mut req = CreateRoomRequest::new();
            req.name = Some(ROOM_NAME.to_owned());
            req.invite = vec![invite.clone()];
            let room = client.create_room(req).await?;
            room.enable_encryption().await?;
            println!("room: created {} and invited {invite}", room.room_id());
            room
        }
    };
    println!(
        "room {} encrypted={}",
        room.room_id(),
        room.latest_encryption_state().await?.is_encrypted()
    );
    if room.get_member(&invite).await?.is_none() {
        room.invite_user_by_id(&invite).await?;
        println!("room: invited {invite}");
    }

    // On warm/recovered runs, prove the room keys are present: history must decrypt.
    if run_kind != "cold" {
        report_history_decryption(&client, &room).await?;
    }

    let body = format!("qj-coldstart[{run_kind}]: hello from device {device_id}");
    let resp = room.send(RoomMessageEventContent::text_plain(body)).await?;
    println!("sent encrypted message: {}", resp.response.event_id);

    // Flush the outbound megolm session to the server-side backup before exiting.
    // Without this, a message sent just before shutdown is unrecoverable after a
    // store loss — found live in Q-J, must carry into the daemon's shutdown path.
    enc.backups().wait_for_steady_state().await?;
    println!("backup: room keys flushed to server");

    client.sync_once(SyncSettings::default()).await?;
    println!("== {run_kind} start sequence complete");
    Ok(())
}

/// Fetch recent history and try to decrypt every `m.room.encrypted` event,
/// retrying while the OneShot backup download runs in the background.
async fn report_history_decryption(client: &Client, room: &Room) -> Result<()> {
    for attempt in 1..=15 {
        let batch = room.messages(MessagesOptions::backward()).await?;
        let (mut ok, mut utd) = (0, 0);
        for event in &batch.chunk {
            match &event.kind {
                TimelineEventKind::Decrypted(_) => ok += 1,
                TimelineEventKind::UnableToDecrypt { .. } => utd += 1,
                TimelineEventKind::PlainText { .. } => {}
            }
        }
        if utd == 0 {
            println!("history: {ok} encrypted event(s), all decrypted");
            return Ok(());
        }
        println!(
            "history: attempt {attempt}: {ok} decrypted, {utd} undecryptable — waiting for backup download"
        );
        client.sync_once(SyncSettings::default()).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    bail!("history still undecryptable after backup-download wait — recovery path FAILED");
}
