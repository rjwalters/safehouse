//! One-shot, client-side creator for the fleet's UNENCRYPTED claims room
//! (issue #101).
//!
//! `safehoused`'s own `create_room` RPC op unconditionally enables
//! `m.room.encryption` for every non-space room (`rpc.rs`'s `create_room`
//! handler, per D6: "every meaningful message goes through the encrypted
//! room"). That default is deliberate and stays unchanged — see
//! `docs/decisions.md` D6's amendment note. The peer-claim room is a
//! narrow, documented carve-out: claim payloads (issue numbers, hostnames,
//! TTLs) are coordination metadata, not secrets, and E2EE contributes only
//! a failure class here — an undecryptable room after a crypto-store loss
//! black-holes fleet-wide delivery with `advertised>0, received=0` and no
//! diagnostic signal as to why.
//!
//! This binary bypasses the daemon's RPC entirely rather than widening its
//! general-purpose surface with an encryption opt-out (see the issue #101
//! curator comment for the full two-option tradeoff). It logs in fresh
//! with an existing bot account (no persistent session — this is a
//! one-shot admin operation, not a daemon), creates a room with NO
//! `m.room.encryption` state, invites every fleet bot listed, and prints
//! the room ID for the operator to paste into each host's config
//! (`LOOM_SAFEHOUSE_ROOM_CLAIMS` / `rooms.claims`) — an explicit id, no
//! alias-resolution magic, matching the issue's requirement.
//!
//! Throwaway by design, same provenance convention as `spikes/qj-coldstart`
//! — not daemon code. Run via `scripts/create-claims-room.sh`, not
//! directly.

use std::env;

use anyhow::{bail, Context, Result};
use matrix_sdk::{
    ruma::{api::client::room::create_room::v3::Request as CreateRoomRequest, OwnedUserId},
    Client,
};

fn required(var: &str) -> Result<String> {
    env::var(var).with_context(|| format!("{var} must be set"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let homeserver = required("CLAIMS_HOMESERVER")?;
    let username = required("CLAIMS_USERNAME")?;
    let password = required("CLAIMS_PASSWORD")?;
    let room_name = env::var("CLAIMS_ROOM_NAME").unwrap_or_else(|_| "safehouse-claims".to_owned());
    let invite_raw = env::var("CLAIMS_INVITE").unwrap_or_default();

    let invites: Vec<OwnedUserId> = invite_raw
        .split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            OwnedUserId::try_from(s)
                .with_context(|| format!("invalid Matrix user id in CLAIMS_INVITE: {s:?}"))
        })
        .collect::<Result<_>>()?;
    if invites.is_empty() {
        eprintln!(
            "create-claims-room: warning: CLAIMS_INVITE is empty — the room will be created \
             with no invitees. Invite fleet bots afterwards from an already-onboarded host's \
             socket (`{{\"op\": \"invite\", \"room\": ..., \"user\": ...}}`, see README) or \
             re-run with CLAIMS_INVITE set."
        );
    }

    // Deliberately NOT a persistent (sqlite) session — this is a one-shot
    // admin operation, not a daemon. Every run is a fresh login; matrix-sdk
    // defaults to an in-memory store when none is configured, so nothing is
    // written to disk and nothing needs cleaning up afterwards.
    let client = Client::builder()
        .homeserver_url(&homeserver)
        .build()
        .await
        .context("building Matrix client")?;

    client
        .matrix_auth()
        .login_username(&username, &password)
        .initial_device_display_name("create-claims-room (one-shot)")
        .await
        .context("login failed — check CLAIMS_USERNAME/CLAIMS_PASSWORD/CLAIMS_HOMESERVER")?;
    println!("create-claims-room: logged in as {username}");

    let mut request = CreateRoomRequest::new();
    request.name = Some(room_name.clone());
    request.invite = invites.clone();
    // No `initial_state` for `m.room.encryption`, and — unlike `rpc.rs`'s
    // `create_room` op — no call to `room.enable_encryption()` below. That
    // absence is the entire point of this binary.
    let room = client
        .create_room(request)
        .await
        .context("creating claims room")?;
    let room_id = room.room_id().to_owned();
    println!("create-claims-room: created room {room_id} (name {room_name:?})");
    for invite in &invites {
        println!("create-claims-room: invited {invite}");
    }

    // Defensive verification, straight from the homeserver (no sync loop
    // needed — `latest_encryption_state` issues its own `GET
    // /state/m.room.encryption` request). This can only fail if the
    // homeserver forces `m.room.encryption` on room creation (not the case
    // for the tuwunel deployment this project targets, D12) — treat that as
    // a hard error rather than silently handing back an encrypted room from
    // a script whose whole purpose is to avoid D6's default.
    let state = room
        .latest_encryption_state()
        .await
        .context("verifying encryption state of the newly-created room")?;
    if state.is_encrypted() {
        bail!(
            "room {room_id} came back encrypted — refusing to hand back an encrypted room \
             from a script whose whole purpose is an unencrypted claims room (D6 carve-out). \
             Check homeserver-side room-creation defaults (e.g. a server-enforced \
             m.room.encryption preset)."
        );
    }
    println!("create-claims-room: verified {room_id} is NOT encrypted");

    println!();
    println!("==================================================================");
    println!("ROOM_ID={room_id}");
    println!("==================================================================");
    println!(
        "Paste this room ID explicitly into every fleet host's claims-room config \
         (LOOM_SAFEHOUSE_ROOM_CLAIMS / rooms.claims) — no alias resolution. See README \
         \"Claims room (unencrypted, D6 carve-out)\" for the full onboarding flow."
    );

    Ok(())
}
