//! provision-host — one-shot, fleet-side host onboarding via Matrix
//! admin-room automation (issue #94).
//!
//! The homeserver's registration posture is settled and out of scope here:
//! `allow_registration` stays `false` (a sealed homeserver, `docs/research/
//! 2026-07-26-homeserver.md`), and the zero-downtime way to mint a bot
//! account against a *live* server is to run `users create_user <name>
//! <password>` **as a message in the Matrix admin room** — the server
//! executes it in-process, no stop/start (see that doc's "Zero-downtime
//! alternative (admin room)"). Registration tokens and a new `safehoused
//! provision` CLI subcommand were considered and explicitly rejected (see
//! issue #94's "Revision 2026-09-14" — no new homeserver surface, no new
//! arg-parsing dependency in the daemon itself).
//!
//! This binary is the admin-room half of onboarding only: it logs in as the
//! already-provisioned `@safehouse-admin` server-admin bot (credentials read
//! from the environment at runtime, never baked in — same convention as
//! `spikes/create-claims-room`), posts the create-user (or deactivate)
//! command into the admin room, and does a best-effort wait for a reply
//! before printing the minted credential. It does **not** invite the new bot
//! into the fleet room — that is a *different* actor (an already-onboarded
//! host's own daemon, over its unix socket, which is the identity that is
//! actually a member of the fleet room) and is the second step `scripts/
//! provision-host.sh` performs via `safehouse-mcp invite` (README "Running
//! it" > step 4, "Onboarding a new fleet host into an existing room").
//!
//! Two modes, selected by the first CLI argument:
//!
//! - `create` (onboarding): mints `safehoused-<host>` (or an explicit
//!   `PROVISION_NEW_USERNAME` override) with a freshly generated password
//!   unless `PROVISION_NEW_PASSWORD` is set, via `users create_user`.
//! - `deactivate` (decommission, #94 acceptance criterion "teardown path"):
//!   runs `users deactivate` for the host's bot account. Tuwunel mirrors
//!   Synapse's admin-API deactivate semantics, which force the account to
//!   leave every room it was joined to as part of deactivation — so this
//!   also retires the fleet-room membership without a separate kick/leave
//!   op (verify with `!admin users list` / the room's member list if your
//!   server's behavior differs; this tool does not assume otherwise).
//!
//! Best-effort, not a hard gate: `safehoused` keeps its zero-hard-dependency
//! posture (a host that fails onboarding still runs, just without
//! coordination — loom ADR-0014), and this tool mirrors that stance for
//! itself. A missing/ambiguous ack from the admin room is reported as a
//! warning, not a failure — the command was still sent, and the operator (or
//! calling script) can verify with `!admin users list` before trusting the
//! printed credential.
//!
//! Deliberately NOT a persistent session (matches `create-claims-room`): a
//! fresh login every run, matrix-sdk's default in-memory store, nothing
//! written to disk.
//!
//! Run via `scripts/provision-host.sh` / `scripts/deprovision-host.sh`, not
//! directly — see those scripts and the README's operator-setup section for
//! the full onboarding/decommission flow.

use std::{env, fmt::Write as _, fs, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use matrix_sdk::{
    config::SyncSettings,
    event_handler::Ctx,
    ruma::{OwnedRoomAliasId, OwnedRoomId},
    Client, Room,
};
use serde_json::json;
use tokio::sync::mpsc;

fn required(var: &str) -> Result<String> {
    env::var(var).with_context(|| format!("{var} must be set"))
}

fn optional(var: &str, default: &str) -> String {
    env::var(var).unwrap_or_else(|_| default.to_owned())
}

/// Default bot username for a fleet host, absent an explicit
/// `PROVISION_NEW_USERNAME` override — matches the naming already in use on
/// the 2AM fleet (`safehoused-studio`, `docs/research/
/// 2026-07-26-homeserver.md`).
fn derive_username(host: &str) -> String {
    format!("safehoused-{host}")
}

/// A fresh 32-byte password, hex-encoded. Reads `/dev/urandom` directly
/// (present on every fleet OS this project targets — macOS/Linux, see
/// `scripts/install.sh`'s own OS support matrix) rather than pulling in a
/// `rand` crate dependency for one call site.
fn generate_password() -> Result<String> {
    // `fs::read` reads to EOF, which /dev/urandom never reaches — open it
    // and take a bounded `read_exact` instead.
    let mut file =
        fs::File::open("/dev/urandom").context("opening /dev/urandom to generate a password")?;
    generate_password_from(&mut file)
}

fn generate_password_from(source: &mut impl std::io::Read) -> Result<String> {
    let mut buf = [0u8; 32];
    source
        .read_exact(&mut buf)
        .context("reading randomness for generated password")?;
    let mut hex = String::with_capacity(64);
    for byte in buf {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(hex)
}

/// Substitutes `{user}`/`{password}` in the configured admin-room command
/// template. Pure and unit-testable — no Matrix I/O.
fn render_command(template: &str, user: &str, password: &str) -> String {
    template
        .replace("{user}", user)
        .replace("{password}", password)
}

/// Best-effort success signal for a reply observed in the admin room after
/// the create-user command was sent: does the reply mention the new
/// account's username? Tuwunel's exact admin-room reply wording is not
/// pinned by this tool (it varies by server/version, and #94's own curation
/// found the underscore form `users create_user` verified live while the
/// issue text itself uses a hyphenated `create-user` — see
/// `PROVISION_CREATE_CMD_TEMPLATE`'s doc comment below), so this is
/// deliberately loose: a substring match, not a full parse. Absence of a
/// match is a warning, never a hard failure (see module doc "Best-effort").
fn ack_mentions_user(reply_body: &str, username: &str) -> bool {
    reply_body.contains(username)
}

/// Resolves `spec` — a room id (`!...:server`) or a canonical alias
/// (`#...:server`) — to a [`Room`] the logged-in client is already joined
/// to. Unlike `safehoused`'s own `rpc.rs::resolve_room`, this tool only ever
/// targets one room (the admin room) so there is no name-based fuzzy match:
/// an id or alias is required.
async fn resolve_room(client: &Client, spec: &str) -> Result<Room> {
    let room_id: OwnedRoomId = if spec.starts_with('#') {
        let alias: OwnedRoomAliasId = spec
            .try_into()
            .with_context(|| format!("invalid room alias {spec:?}"))?;
        client
            .resolve_room_alias(&alias)
            .await
            .with_context(|| format!("resolving room alias {spec:?}"))?
            .room_id
    } else {
        spec.try_into().with_context(|| {
            format!("invalid room id {spec:?} (expected `!id:server` or `#alias:server`)")
        })?
    };
    client.get_room(&room_id).with_context(|| {
        format!(
            "not joined to room {room_id} (resolved from {spec:?}) — is the admin account \
             actually a member of the admin room?"
        )
    })
}

/// Shared state for the reply-capture event handler below: which room to
/// watch, and where to forward what it sees.
struct AckWatch {
    target_room: OwnedRoomId,
    tx: mpsc::UnboundedSender<(String, String)>,
}

/// Forwards every non-own message seen in the target room to the ack
/// channel. Named function (not a closure) to match this codebase's
/// established `add_event_handler` convention (see `safehoused/src/
/// main.rs`'s `on_invite`/`on_message`) rather than fighting the
/// `EventHandler` trait's bounds with a capturing closure.
async fn on_message(
    event: matrix_sdk::ruma::events::room::message::OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    Ctx(watch): Ctx<Arc<AckWatch>>,
) {
    if room.room_id() != watch.target_room {
        return;
    }
    if Some(event.sender.as_ref()) == client.user_id() {
        return; // our own echoed command, not a reply
    }
    let _ = watch
        .tx
        .send((event.sender.to_string(), event.content.body().to_owned()));
}

/// Polls forward via repeated `sync_once` calls (matrix-sdk reuses the
/// previous sync token by default — see `SyncSettings::default()`'s
/// `SyncToken::ReusePrevious` — so each call continues where the last left
/// off) until either a reply arrives on `rx` or `deadline` elapses.
async fn wait_for_ack(
    client: &Client,
    rx: &mut mpsc::UnboundedReceiver<(String, String)>,
    deadline: Duration,
) -> Option<(String, String)> {
    let start = tokio::time::Instant::now();
    loop {
        if let Ok(reply) = rx.try_recv() {
            return Some(reply);
        }
        if start.elapsed() >= deadline {
            return None;
        }
        let remaining = deadline.saturating_sub(start.elapsed());
        let step = remaining
            .min(Duration::from_secs(2))
            .max(Duration::from_millis(100));
        // A sync error here (e.g. a transient network blip) is not fatal to
        // the wait — just fall through and retry on the next iteration,
        // bounded by the same overall deadline.
        let _ = client
            .sync_once(SyncSettings::default().timeout(step))
            .await;
        if let Ok(reply) = rx.try_recv() {
            return Some(reply);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mode = env::args().nth(1).context(
        "usage: provision-host <create|deactivate> (run via scripts/provision-host.sh \
                   / scripts/deprovision-host.sh, not directly)",
    )?;
    anyhow::ensure!(
        mode == "create" || mode == "deactivate",
        "unknown mode {mode:?} — expected \"create\" or \"deactivate\""
    );
    let is_create = mode == "create";

    let homeserver = required("PROVISION_HOMESERVER")?;
    let admin_username = required("PROVISION_ADMIN_USERNAME")?;
    let admin_password = required("PROVISION_ADMIN_PASSWORD")?;
    let admin_room_spec = required("PROVISION_ADMIN_ROOM")?;
    let host = required("PROVISION_HOST")?;
    let new_username =
        env::var("PROVISION_NEW_USERNAME").unwrap_or_else(|_| derive_username(&host));

    // Only `create` needs a password: it is minted (or read from an
    // explicit override) here and printed for delivery to the new host.
    // `deactivate` never touches a password — the account is being retired,
    // not logged into.
    let new_password = if is_create {
        match env::var("PROVISION_NEW_PASSWORD") {
            Ok(p) => p,
            Err(_) => generate_password().context("generating a password for the new account")?,
        }
    } else {
        String::new()
    };

    // Verified live 2026-07-28 against a tuwunel admin room (`docs/research/
    // 2026-07-26-homeserver.md`) with the underscore form. Overridable
    // because tuwunel's admin-command syntax has moved before and #94's own
    // curated text uses a hyphenated `create-user` — if your server expects
    // that form, set this explicitly rather than editing the binary.
    let default_template = if is_create {
        "users create_user {user} {password}"
    } else {
        "users deactivate {user}"
    };
    let cmd_template = optional("PROVISION_CREATE_CMD_TEMPLATE", default_template);
    let ack_timeout_secs: u64 = optional("PROVISION_ACK_TIMEOUT_SECS", "20")
        .parse()
        .context("PROVISION_ACK_TIMEOUT_SECS must be a non-negative integer")?;

    let command = render_command(&cmd_template, &new_username, &new_password);

    let client = Client::builder()
        .homeserver_url(&homeserver)
        .build()
        .await
        .context("building Matrix client")?;

    client
        .matrix_auth()
        .login_username(&admin_username, &admin_password)
        .initial_device_display_name("provision-host (one-shot)")
        .await
        .context(
            "login failed — check PROVISION_ADMIN_USERNAME/PROVISION_ADMIN_PASSWORD/\
             PROVISION_HOMESERVER",
        )?;
    println!("provision-host: logged in as {admin_username}");

    // Populate the room list before we can resolve the admin room by id or
    // alias — a fresh login has no local room state yet.
    client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;

    let room = resolve_room(&client, &admin_room_spec).await?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    client.add_event_handler_context(Arc::new(AckWatch {
        target_room: room.room_id().to_owned(),
        tx,
    }));
    client.add_event_handler(on_message);

    println!(
        "provision-host: sending to {}: {:?}",
        room.room_id(),
        if is_create {
            render_command(&cmd_template, &new_username, "<redacted>")
        } else {
            command.clone()
        }
    );
    room.send_raw(
        "m.room.message",
        json!({"msgtype": "m.text", "body": command}),
    )
    .await
    .context("sending admin command")?;

    println!(
        "provision-host: waiting up to {ack_timeout_secs}s for a reply in the admin room \
         (best-effort — see module docs; absence of a reply is not treated as failure)"
    );
    match wait_for_ack(&client, &mut rx, Duration::from_secs(ack_timeout_secs)).await {
        Some((sender, body)) => {
            println!("provision-host: reply from {sender}: {body}");
            if ack_mentions_user(&body, &new_username) {
                println!("provision-host: reply mentions {new_username:?} — treating as success");
            } else {
                println!(
                    "provision-host: warning: reply does not mention {new_username:?} — \
                     verify manually (e.g. `!admin users list`) before trusting the credential \
                     below"
                );
            }
        }
        None => {
            println!(
                "provision-host: warning: no reply observed within {ack_timeout_secs}s — the \
                 command was sent, but the ack could not be confirmed. Verify manually before \
                 trusting the credential below."
            );
        }
    }

    println!();
    println!("==================================================================");
    if is_create {
        println!("HOMESERVER={homeserver}");
        println!("USERNAME={new_username}");
        println!("PASSWORD={new_password}");
        println!("==================================================================");
        println!(
            "Deliver USERNAME/PASSWORD to the new host out of band — never write them to a \
             committed file. The new host consumes these as its normal `username`/`password` \
             config fields (see safehoused/example-config.toml); scripts/install.sh wires them \
             in automatically when SAFEHOUSE_ADMIN_* env is present (see README)."
        );
    } else {
        println!("HOMESERVER={homeserver}");
        println!("USERNAME={new_username}");
        println!("STATUS=deactivated");
        println!("==================================================================");
        println!(
            "{new_username} has been sent a deactivate command. Tuwunel's admin API mirrors \
             Synapse's deactivate semantics, which force-leave every room the account was \
             joined to — verify with the fleet room's member list if you need to confirm this \
             account is gone from it."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_command_substitutes_both_placeholders() {
        let cmd = render_command("users create_user {user} {password}", "bot-a", "hunter2");
        assert_eq!(cmd, "users create_user bot-a hunter2");
    }

    #[test]
    fn render_command_leaves_unmatched_text_alone() {
        let cmd = render_command("users deactivate {user}", "bot-a", "unused");
        assert_eq!(cmd, "users deactivate bot-a");
    }

    #[test]
    fn ack_mentions_user_matches_substring() {
        assert!(ack_mentions_user(
            "Created user with user_id: @bot-a:example.com and password: hunter2",
            "bot-a"
        ));
    }

    #[test]
    fn ack_mentions_user_rejects_unrelated_reply() {
        assert!(!ack_mentions_user("pong", "bot-a"));
    }

    /// Guards against a `{user}`/`{password}` order swap or dropped
    /// placeholder silently changing which value lands where — the
    /// generated password ending up in the username slot (or vice versa)
    /// would mint the wrong account with no error.
    #[test]
    fn render_command_does_not_swap_user_and_password() {
        let cmd = render_command("users create_user {user} {password}", "USER", "PASSWORD");
        let user_idx = cmd.find("USER").unwrap();
        let password_idx = cmd.find("PASSWORD").unwrap();
        assert!(user_idx < password_idx);
    }

    #[test]
    fn derive_username_prefixes_host() {
        assert_eq!(derive_username("studio"), "safehoused-studio");
    }

    #[test]
    fn generate_password_from_is_64_lowercase_hex_chars() {
        // 32 fixed bytes (not /dev/urandom) so this test is deterministic —
        // it checks encoding shape, not entropy.
        let mut source: &[u8] = &[0xABu8; 32];
        let password = generate_password_from(&mut source).unwrap();
        assert_eq!(password.len(), 64);
        assert!(password
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(password, "ab".repeat(32));
    }

    #[test]
    fn generate_password_from_errors_on_short_read() {
        let mut source: &[u8] = &[0x01u8; 4]; // fewer than the 32 bytes required
        assert!(generate_password_from(&mut source).is_err());
    }
}
