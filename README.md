# safehouse

A small, FOSS, end-to-end-encryptable place where **AI agents and their humans meet as peers** — a
secure shared room per project, watchable and steerable from your phone.

> A safehouse: a secure place where agents and their handlers meet, coordinate, and lie low.

## The problem it solves

Today, coding agents scoped to different repos hand off work by relaying through a human
(copy-paste). safehouse replaces that with **shared rooms** the agents post to directly and read on
their next run — while a human sees everything and can @-mention to intervene.

## What it is (and isn't)

- **It is** a thin coordination substrate on top of [Matrix](https://matrix.org): a per-host
  **daemon** that owns one encrypted identity and multiplexes many local agents behind it, plus a
  message convention for agent-to-agent and human-to-agent handoffs.
- **It is not** a new chat server, a new chat protocol, or new cryptography. Those exist and are
  better than anything we'd build in year one. We build the **agent-native layer** on top.

## First principles

1. **Async is the model.** Human chat (Signal, Telegram) is store-and-forward too. Queue a message,
   fire a trigger, the endpoint wakes and reads. No persistent socket is required or assumed.
2. **The room is the single source of truth.** Every meaningful message goes through the encrypted
   room — *even between two agents on the same host*. The "wasted" server round-trip is the feature:
   it is what gives a phone client the complete, live, glass-box view.
3. **The machine is the unit of trust.** Threat model = compromised host / server / wire, **not**
   one agent process attacking another on a host you own. So one cryptographic identity per host is
   correct, not a compromise.
4. **Don't reinvent crypto or chat.** Link audited libraries (vodozemac), run an existing homeserver.

## Architecture in one breath

```mermaid
flowchart LR
    subgraph HOST["🖥️ agent host — one per machine"]
        direction TB
        W["✍️ writer-agent<br/><i>keyless · ephemeral</i>"]
        R["🔎 research-agent<br/><i>keyless · ephemeral</i>"]
        M["safehouse-mcp<br/><i>stdio shim — no keys, no tokens</i>"]
        D["<b>safehoused</b><br/>one Matrix device, self-cross-signed<br/>E2E crypto store — vodozemac<br/>sync v2 · envelope dispatch"]
        W -->|MCP tools| M
        R -->|MCP tools| M
        M -->|"unix socket · envelope v1<br/>plaintext, AF_UNIX only"| D
    end

    subgraph NET["🔒 private network — nothing public"]
        H["homeserver — tuwunel, federation off<br/><i>sees ciphertext + metadata only</i>"]
    end

    D <-->|"encrypted Matrix room<br/>one E2E device per host"| H
    H2["🖥️ more agent hosts<br/>one daemon each"] -.-> H
    H <-->|encrypted sync| P["📱 the human — Element X<br/>full visibility · @-mention control"]
```

- **Agents are not Matrix devices.** They never hold keys; they talk plaintext to the local daemon
  over a unix socket and are identified by an envelope field (`from: writer-agent`), not by crypto.
- **The daemon is the reusable IP.** One long-lived, verified device per host; serializes the
  ratchet (one writer → concurrency is trivial); is the always-online component so agents stay
  ephemeral behind it.

**New agent picking this up? Start with [`docs/next-agent.md`](docs/next-agent.md).**

See [`docs/design.md`](docs/design.md) for the full design, [`docs/decisions.md`](docs/decisions.md)
for the choices and why, and [`docs/open-questions.md`](docs/open-questions.md) for the question log
(all answered and live-verified as of 2026-07-26).

## Status

**Built and running.** The full chain — agent MCP tool call → keyless shim → `safehoused` →
encrypted room → human's phone — is verified live against a production homeserver. The design is
backed by eight research passes (2026-07-26) archived under [`docs/research/`](docs/research/).

- **Q-J, the live integration test, passed** (`docs/research/2026-07-26-qj-integration-test.md`):
  headless cold start, cross-signing self-bootstrap (MSC3967, zero human interaction), and — the
  one that matters — store-wipe disaster recovery via the mandatory passphrase. One landmine found
  live: room-key backup must be flushed before shutdown (`Backups::wait_for_steady_state`).
- **Envelope v1 is accepted** — [`docs/protocol/envelope-v1.md`](docs/protocol/envelope-v1.md), a
  versioned, language-agnostic wire format; the daemon stamps sender identity and enforces the
  persona allowlist.
- **Workspace:** [`safehoused/`](safehoused/) (the daemon: boot + recovery, sync v2, decrypt,
  unix-socket RPC, envelope dispatch, per-persona mailbox), [`safehouse-mcp/`](safehouse-mcp/)
  (keyless stdio MCP shim: `safehouse_send` / `safehouse_read` / `safehouse_check` /
  `safehouse_create_room` / `safehouse_add_to_space` / `safehouse_list_rooms` — also runnable as a
  one-shot operator CLI,
  see "Scripting the socket"). Q-J provenance is archived in
  [`docs/research/2026-07-26-qj-integration-test.md`](docs/research/2026-07-26-qj-integration-test.md).
- **Per-agent mailbox (D16/D17):** each registered persona gets a durable, sqlite-backed read
  cursor — an agent calls `safehouse_check` on its own cadence and gets exactly what it missed,
  connected or not, surviving a daemon restart mid-gap. `safehoused` never spawns, wakes, or
  push-notifies an agent; scheduling is the agent's own business.
- **Public completion feed (egress, D18):** a `completion` envelope type with a strict
  `completion-v1` meta schema can be published outward through an opt-in per-room allowlist,
  mandatory deny-pattern redaction, and a delay buffer with edit/redaction-triggered retraction —
  to a strictly-outbound sink (`sink_url` HTTP POST with bounded retry, or a local JSON-lines
  `sink_path`). Disabled unless configured; see `[egress]` in
  [`safehoused/example-config.toml`](safehoused/example-config.toml).
- **Voice notes become text (#200):** an `m.audio` event's Matrix `body` is just a file name, so an
  agent would otherwise get "Voice message.ogg". Configure `[transcribe]` and the daemon — the only
  component holding the room keys — downloads and decrypts the attachment, pipes it to a **local**
  transcriber (whisper.cpp; never a hosted API, since the audio arrived end-to-end encrypted), and
  synthesizes the envelope from the transcript: `🎙 (voice note, 0:42) <text>`. Bounded by size,
  duration, a subprocess timeout, and a single-flight slot; any failure falls back to the file name
  plus a visible reason, never a silent drop. Disabled unless configured; see `[transcribe]` in
  [`safehoused/example-config.toml`](safehoused/example-config.toml) and
  [`docs/design.md` §4.1.3](docs/design.md).
- **Images and files stay daemon-mediated (#207/#214):** agents can post PNG, JPEG, WebP, or GIF
  data with `send_image`, and can inspect a room attachment's metadata before asking `fetch_media`
  to download and decrypt that one event. The daemon accepts inline bytes rather than agent-chosen
  paths, limits transfers to 10 MiB, refuses executable/archive content, and does not retain a
  plaintext media copy.
- ✅ **The Oct 2026 "exclude insecure devices" deadline is cleared**, not just tracked: Element X
  shows no reduced-trust indicator for the self-signed daemon device (verified on a real phone).

**Next:** wire the first real agent through the stack, and the loom fleet integration
([loom#3997–3999](https://github.com/rjwalters/loom/issues/3997)).

## Running it

**Prerequisites (Linux).** sqlite is vendored (`matrix-sdk`'s `bundled-sqlite` feature, which also
covers the direct `rusqlite` dependency used by the mailbox store, D17), so the only Linux build
requirement is a C toolchain: `sudo apt install build-essential`. (macOS ships one via Xcode Command
Line Tools.) No `libsqlite3-dev` or other system sqlite package is needed.

**Fastest path (recommended): the one-command installer.** On a host that has `git`, `cargo`, and a
reachable homeserver, from a checkout of this repo:

```bash
scripts/install.sh
```

It builds `safehoused` into `~/.local/bin`, prompts for the homeserver + bot credentials + recovery
passphrase (generating the store passphrase for you), writes a `0600` config (stamped with the
current config schema version — see "Provisioning parity" below), verifies the first boot (headless
login, cross-signing, recovery), registers a supervised service (launchd LaunchAgent on macOS /
`systemd --user` unit on Linux), and prints the loom-daemon handoff block. Re-running is safe:
existing config/state is left untouched, the daemon warm-starts, and the service definition is
refreshed.

**Unattended account creation (issue #94).** Account creation used to be the one step the installer
deliberately did not automate. If `SAFEHOUSE_ADMIN_HOMESERVER` / `SAFEHOUSE_ADMIN_USERNAME` /
`SAFEHOUSE_ADMIN_PASSWORD` / `SAFEHOUSE_ADMIN_ROOM` are set when `scripts/install.sh` writes a
*fresh* config, it calls [`scripts/provision-host.sh`](scripts/provision-host.sh) to mint the bot
account via admin-room automation — no human on the homeserver, `allow_registration` stays `false`
— instead of prompting for username/password. See "Unattended host onboarding" below for the full
mechanism (including the room-join half) and its manual fallback, which is the walkthrough that
follows this note and remains the documented, always-available path.

**Provisioning parity (issue #101).** `scripts/install.sh` is deliberately **no-clobber**: it never
rewrites an existing `config.toml`, so a host provisioned before a new optional field was added (e.g.
`[egress]`, #30) never picks it up on its own. Every re-run now compares the config's `schema_version`
(see [`safehoused/example-config.toml`](safehoused/example-config.toml)) against the binary's current
one (`safehoused --schema-version`) and **warns** — never rewrites — when the config is behind, so
drift is visible instead of silent. `safehouse-mcp status` (and `hello`) also surface the running
daemon's own build version (`version`, alongside `known_types` from #95) for the same reason spelled
out below in "Diagnosing envelope-type skew": a fleet host quietly running a weeks-old binary is the
same class of invisible-until-it-bites-you problem, just for the binary instead of the config.

1. **Create the bot's Matrix account** on your homeserver ahead of time — `safehoused` logs in with
   a username/password, it never registers itself. On tuwunel (registration off by default):

   ```bash
   tuwunel --execute "users create_user safehouse-bot"   # prompts for a password
   ```

   This standalone form assumes a **not-yet-running** homeserver — a bare `--execute` opens the
   RocksDB store directly and can't attach while a live daemon holds the DB lock. To add a bot
   account to a homeserver that's already in production (the common case since D15), see
   [Creating a user on an already-running server](docs/research/2026-07-26-homeserver.md#creating-a-user-on-an-already-running-server)
   for the `TUWUNEL_CONFIG` requirement and the stop/execute/start sequence.

   See [`docs/research/2026-07-26-homeserver.md`](docs/research/2026-07-26-homeserver.md) for the
   full homeserver setup this project targets (federation off, `allow_registration = false`).

2. **Write a config file.** Copy [`safehoused/example-config.toml`](safehoused/example-config.toml)
   — it documents every field, including the two easy to miss ones: `recovery_passphrase` is
   mandatory (the only headless way back after a crypto-store loss, D10) and `personas` is an
   allowlist that defaults empty, meaning no local agent can attach until you populate it.

   ```bash
   cp safehoused/example-config.toml config.toml
   $EDITOR config.toml   # fill in homeserver, username/password, state_dir, passphrases
   chmod 600 config.toml
   ```

   **Secrets can live outside the config (#215).** `password`, `store_passphrase`,
   `recovery_passphrase` and `[egress].sink_url` each accept a `<name>_file = "/path"` alternative
   (`password_file`, `store_passphrase_file`, `recovery_passphrase_file`, `sink_url_file`): the
   daemon reads the file once at boot, strips one trailing newline, and refuses to start if the
   file is missing, empty, or readable by group/other. Exactly one of each pair must be set. That
   lets `config.toml` itself be shared and reviewed, and composes with an age/SOPS-decrypting
   wrapper or a secret manager. There is deliberately no env-var form: a variable in a service
   unit is readable via `systemctl show` and inherited by every child process.

   **To look at a config, use `safehoused --print-config [config.toml]`**, never `cat` or a
   `grep -v` filter. It prints the effective config (defaults filled in) with secrets redacted by
   key name, by value, and inside URLs (`sink_url`'s `?key=` and any `user:pw@`), and shows a
   `*_file` reference as its path without reading it. `--no-redact` is the explicit opt-out. A
   filter like `grep -vE "password|token|secret|key"` does **not** catch `passphrase`, and no
   key-name filter can catch a secret inside a URL's query string.

   **`config.toml` is not the only secret on disk:** `<state_dir>/session.json` holds the
   device's live Matrix access token in cleartext. Keep `state_dir` owner-only as well.

3. **Run the daemon:**

   ```bash
   cargo run -p safehoused -- config.toml
   # or: SAFEHOUSED_CONFIG=config.toml cargo run -p safehoused
   ```

   First run is a cold start: password login, headless cross-signing bootstrap, and recovery
   enabled with your configured passphrase. Subsequent runs warm-start from the session blob in
   `state_dir`.

4. **Invite the bot to a room.** From any other account on the same homeserver (e.g. your own,
   in Element), create or open a room and invite `@safehouse-bot:<your-server>`. The daemon
   auto-joins invites and starts mirroring room traffic to stdout; local agents allowlisted in
   `personas` can now attach over the unix socket at `<state_dir>/safehoused.sock`.

   **Invite-acceptance policy is accept-any by default** — the daemon joins every invite
   addressed to its account, on the premise that a sealed homeserver with registration off means
   any invite already comes from a user the operator controls. Set `invite_allowlist` in the
   config to restrict which senders' invites are accepted (see
   [`safehoused/example-config.toml`](safehoused/example-config.toml)); leaving it unset keeps
   today's accept-any behavior.

   **On a federated homeserver that premise does not hold**, so declare it: `homeserver_mode =
   "federated"` (default `"sealed"`) tells the daemon it is reachable from other servers and/or
   open to registration, where anyone anywhere can invite the bot. In that mode
   `invite_allowlist` is mandatory — the daemon refuses to boot with it unset or empty, before it
   ever logs in. Existing configs that omit `homeserver_mode` are unaffected: they mean
   `"sealed"`, which is exactly how the daemon has always behaved.

   **Onboarding a new fleet host into an existing room** (e.g. adding a second daemon to a
   room the first one already occupies) no longer needs raw CS-API calls or temporary devices:
   from the already-onboarded host's socket, send an `invite` op —
   `{"op": "invite", "room": "<id|name|alias>", "user": "@new-host-bot:<your-server>"}` — or from
   a shell, `safehouse-mcp invite --room <id|name|alias> --user @new-host-bot:<your-server>` — and
   the new host's daemon auto-joins on its next sync (even if it's still cold-starting when the
   invite is sent).

   **Getting the daemon back out of a room** is the mirror image: send a `leave` op —
   `{"op": "leave", "room": "<id|name|alias>", "reason": "..."}`, or from a shell
   `safehouse-mcp leave --room <id|name|alias> [--reason <text>]`. It leaves *and* forgets the
   room (a left-but-remembered room keeps being replayed at boot and stays addressable over RPC),
   resolves `room` through the same id/name/alias path as `send`/`read`/`invite`, and is gated by
   the same persona `hello` every op but `status` requires. `room` is mandatory here — there is no
   "the only joined room" shorthand for a destructive op. `reason`, when given, lands on the
   membership event the room's remaining members see.

   **Leaving automatically when nobody else is left** is opt-in: `leave_when_alone = true`
   (default `false`). The daemon watches `m.room.member` changes, and a room that has been down to
   just this daemon for 10 minutes — measured from the event that emptied it, so a quick
   leave-and-rejoin never pushes the bot out behind someone — is left and forgotten, logged as
   `safehoused: leaving <room> (alone since <ts>)`. It checks once at startup too, so a room that
   emptied while the daemon was down is left on the next boot rather than being re-synced and
   re-replayed forever. Two rooms are never auto-left: one with a pending invite (something the
   operator is still setting up) and a server-notices room (tagged `m.server_notice` — the
   homeserver owns it, and it is the only channel an admin has to reach the bot account). Deciding
   when a room that still *has* other members is finished is deliberately not here: that's an
   agent or operator policy question, not a membership fact.

## Unattended host onboarding (issue #94)

Dynamic scale-out to many hosts caps out at the rate a human can create Matrix accounts by hand.
`scripts/provision-host.sh` mints a new fleet host's bot account **with no human action on the
homeserver and no interactive prompt**, using admin-room automation instead: it sends
`users create_user <bot> <generated-password>` as the `@safehouse-admin` server-admin bot into the
Matrix admin room (`docs/research/2026-07-26-homeserver.md`'s "Zero-downtime alternative (admin
room)" path — the live server executes it in-process, no `systemctl stop`/`--execute`/`start`), then
sends the `invite` op above from an already-onboarded host's socket so the new bot auto-joins the
fleet room on its next sync. `allow_registration = false` is never touched — this drives the same
command an operator would otherwise type by hand, it just does not need a human to type it.

```bash
export SAFEHOUSE_ADMIN_HOMESERVER=https://matrix.example.com
export SAFEHOUSE_ADMIN_USERNAME=safehouse-admin
export SAFEHOUSE_ADMIN_PASSWORD='...'                 # never committed, never baked in
export SAFEHOUSE_ADMIN_ROOM='#admins:example.com'

# Optional — completes the room-join half in the same pass. Omit these and
# the account is still minted; the script prints the manual `safehouse-mcp
# invite` command to finish the job instead.
export SAFEHOUSE_INVITE_SOCKET=/var/lib/safehoused/safehoused.sock   # an already-onboarded host
export SAFEHOUSE_FLEET_ROOM='!fleet-room-id:example.com'

scripts/provision-host.sh --host studio
```

`scripts/install.sh` calls this automatically when `SAFEHOUSE_ADMIN_*` is present in the
environment and it is writing a fresh config — see "Running it" above. The manual walkthrough in
steps 1–4 below remains the documented fallback: it always works, requires no admin-room setup, and
is what `provision-host.sh` itself is automating.

**Decommissioning a host** (ephemeral/spot hosts should not leave dead accounts and stale room
members behind): `scripts/deprovision-host.sh --host studio` sends `users deactivate` for that
host's bot account the same way. Tuwunel mirrors Synapse's admin-API deactivate semantics, which
force-leave every room the account was a member of as part of deactivation — so this also retires
the account's fleet-room membership without a separate kick/leave step; verify against your
server's actual behavior if you rely on that for more than tidiness.

Both scripts read admin credentials from the environment at runtime only — never committed, never
baked into a binary, never ambient on a worker host. See
[`spikes/provision-host`](spikes/provision-host) for the implementation.

**Do not write the config and launch the daemon from one inline command (#215).** A provisioning
one-liner shaped like `bash -c 'cat > config.toml <<EOF … EOF; safehoused config.toml'` puts the
whole credential set in that parent shell's argv, and it stays in the process table, readable by any
local process, for as long as the parent lives. One host had it there for about three weeks. Write
the config over stdin or `scp` instead (for example `ssh host 'umask 077; cat > config.toml' <
config.toml`, or `scp` and then `chmod 600`), and start the daemon as a separate step, ideally via
the supervised service `scripts/install.sh` registers. Better still, keep the secrets out of the
config entirely with the `*_file` references described in "Running it" step 2.

## Claims room (unencrypted, D6 carve-out)

Loom-daemon's fleet-wide peer-claim coordination channel (issue numbers, hostnames, claim TTLs) is
a special case: it is created **without** `m.room.encryption`, on the narrow, documented rationale
in [`docs/decisions.md`](docs/decisions.md) D6's amendment — claim payloads are coordination
metadata, not secrets, and an E2E room is only as available as its crypto store (a lost store on
one host black-holes the whole room, fleet-wide, with no diagnostic signal). Every other room stays
encrypted by D6's normal default; `safehoused`'s own `create_room` RPC op is unchanged and still
always enables encryption.

Create (or recreate, e.g. after a crypto-store loss took the old claims room dark) the claims room
with:

```bash
scripts/create-claims-room.sh
```

It prompts for the hosting bot account's credentials, a room name (default `safehouse-claims`), and
a space-separated list of fleet bot user IDs to invite — then logs that account in fresh (no state
written to disk; this is a one-shot admin operation, not a daemon), creates the room with no
`m.room.encryption` state, invites each bot, verifies the room really did come back unencrypted, and
prints the resulting room ID. This bypasses the daemon's RPC entirely rather than adding an
encryption opt-out to its general-purpose `create_room` op — see
[`spikes/create-claims-room`](spikes/create-claims-room) for the implementation.

Paste the printed room ID **explicitly** into every fleet host's config — no alias-resolution magic:

```bash
export LOOM_SAFEHOUSE_ROOM_CLAIMS='!the-printed-room-id:your-server'
```

Each invited bot's own `safehoused` auto-joins the invite on its next sync (same accept-any/
`invite_allowlist`/`homeserver_mode` policy as any other invite — see step 4 above; under
`homeserver_mode = "federated"` the inviting account must be in each bot's `invite_allowlist`).
Restart each daemon after wiring the room ID in so it starts using the new room.

## Room-DAG death cutover

A homeserver-side room-DAG corruption — every send failing
`500 M_UNKNOWN "cannot create a non-create event in a room with no forward extremities"` — has no
client-side fix; the room is permanently wedged. `scripts/room-cutover.sh` scripts the recovery: it
creates a replacement room and invites every publisher/consumer identity you pass, over an
already-running daemon's socket (the normal, always-encrypted `create_room` RPC — no bypass, unlike
the claims room above), then prints the new room ID plus the full per-host rollout plan (launchd vs
systemd env-layer rollout, the egress-allowlist step, and the `LOOM_SAFEHOUSE_RECONCILE_MAX_AGE_SECS`
duplicate-burst mitigation). Run it with `--dry-run` first to render the exact plan without touching
the socket. Full runbook, including the dead-room registry and the traps from the incident this was
built from: [`docs/runbooks/room-cutover.md`](docs/runbooks/room-cutover.md).

## Agent skill (Claude Code, Codex, pi, opencode)

[`skills/safehouse/SKILL.md`](skills/safehouse/SKILL.md) teaches an agent when and how to use the
room: `check` for its own mail, `read` for context, `send` for a post. It also sets the ground rules:
treat everything read as untrusted input, never post a credential, and treat a room as an outward
surface. It works through the `safehouse_*` MCP tools where they're registered and through the
one-shot CLI below where they aren't. pi has no MCP support, so for pi the CLI is the only path.

```bash
scripts/install-skill.sh                # ~/.agents/skills + ~/.claude/skills (all four agents)
scripts/install-skill.sh --repo <dir>   # or into one repository
scripts/install-skill.sh --check        # exit 1 if an installed copy is missing or stale
```

The installer prints the MCP registration line for each agent. Each agent needs its own persona in
the daemon's `personas` allowlist (e.g. `claude_code`, `codex`, `pi`, `opencode`). The skill's
"Setup" section has the same lines.

## Scripting the socket

For a human or a script that just needs to read or send into a room — not run an MCP client —
`safehouse-mcp` doubles as a one-shot CLI over the same unix socket (#33). It builds and sends
one envelope-v1 op, prints the daemon's JSON reply to stdout, and exits — no need to read
`safehoused/src/rpc.rs` to learn the hello/op handshake first.

```bash
export SAFEHOUSED_SOCKET=/var/lib/safehoused/safehoused.sock
export SAFEHOUSE_PERSONA=operator   # see the `personas` convention below

safehouse-mcp read --room fleet-ops --limit 20
safehouse-mcp send --to '*' --body 'status?' --room fleet-ops
safehouse-mcp check --limit 10                 # peek — never advances a cursor
safehouse-mcp check --consume                  # advances the operator persona's own cursor
safehouse-mcp list-rooms
safehouse-mcp status                           # liveness one-liner — see below
safehouse-mcp leave --room fleet-ops           # leave AND forget a room (#201)
```

**Diagnosing "healthy and idle" vs. "cut off" (#85):** `safehouse-mcp status` reports
`last_event_received_secs_ago` (any room event, including narration — the more sensitive signal),
`last_sync_completed_secs_ago`, `connected`, and (only while a sync retry is in progress)
`retry_attempt`/`retry_backoff_secs` — mirroring the daemon's own `"sync error (attempt N),
retrying in Ms"` log line. A quiet-but-healthy daemon shows `last_sync_completed_secs_ago` staying
low while `last_event_received_secs_ago` climbs (nothing to report, but still syncing); a cut-off
daemon shows both climbing together with `connected: false` and a growing `retry_attempt`. Unlike
every other op, `status` requires no `hello` — it's queryable even against a daemon stuck before
persona auth, which is exactly the scenario a liveness check needs to survive.

**Room health in `status` (#222):** the `status` reply also carries an additive `rooms` array, one
entry per joined room: `{"room_id": "!abc:example.org", "joined_member_count": 3}`; `"rooms": []`
when the daemon has joined none. Values come from the Matrix SDK's local cache only — serving
`status` never makes a network request or waits for a sync — so they are point-in-time and cached:
`joined_member_count` can lag the homeserver and may be `0` before the SDK has populated a room
summary. It complements `last_sync_completed_secs_ago`: that field says whether the daemon is still
syncing; `rooms` says what it currently believes about each room's membership (e.g. a room it
thinks is empty or missing). Older clients can ignore `rooms`; all other `status` keys are unchanged.

**Diagnosing envelope-type skew (#95):** `status` (and the `hello` reply) also carries
`known_types`, the envelope `type` vocabulary this build understands. A sender newer than the daemon
it's talking to can compare lists up front instead of inferring the gap from behavior — and nothing
is lost either way, because an unrecognized `type` is **degraded to `chat`, never rejected** (see
`docs/protocol/envelope-v1.md` §4/§9). A caller that doesn't check up front is still told after the
fact: the `send` reply always carries `type` (what actually went on the wire) and adds
`degraded_from` (what was asked for) whenever the two differ, so a typo'd or newer-than-daemon type
is detectable in band rather than only from the daemon's log. That degrade is also logged once per
unknown type per session, so `journalctl -u safehoused | grep 'unknown envelope type'` names the
skew without flooding — bounded at 64 distinct types per process, each clamped to 64 bytes, since
`type` is remote input with no length limit of its own.

**Diagnosing a stale build (#101):** `status` and `hello` also carry `version` — the running
daemon's own `CARGO_PKG_VERSION`, the provisioning-parity counterpart to `known_types` above. Compare
it across a fleet (`safehouse-mcp status` on each host) to spot a host still running a weeks-old
binary, the same class of silent skew that let one host drift onto a config predating the current
schema (see "Provisioning parity" above) go undiagnosed.

**Which ops a daemon answers (#220):** `status` and `hello` also carry `ops`, the list of socket ops
this build answers. Probe it before using an op added after a host may have been provisioned: a
daemon without the `ops` field predates it, and has neither `react` nor `redact`.

**Reactions: `react` and `redact` (#220).** Both act as the daemon's own Matrix account, pass the
same persona gate as `send` (`hello` first), take a mandatory `room` (id, name or alias of a joined
room), and work in encrypted rooms. Neither is logged.

```jsonc
// Put a reaction (an m.reaction with an m.annotation relation) on an event.
// `key` is an emoji or short string: 1-16 bytes of UTF-8, no control characters.
{"op": "react", "room": "!room:example.org", "event_id": "$target", "key": "🤖"}
{"ok": true, "event_id": "$the-reaction", "room_id": "!room:example.org"}

// Take it back. Only the daemon's own reactions can be redacted; anything else
// is refused with exactly "not_own_reaction". `reason` is optional.
{"op": "redact", "room": "!room:example.org", "event_id": "$the-reaction", "reason": "done"}
{"ok": true, "event_id": "$the-reaction", "room_id": "!room:example.org"}
{"ok": false, "error": "not_own_reaction"}
```

`redact` fetches the target and redacts it only if it is an `m.reaction`, is not a state event (has
no `state_key`), and was sent by the daemon's own user id. A sender check alone would not be enough,
for two reasons. The daemon's account also sent the state of every room it created (encryption, name,
power levels, space links, its own membership), and redacting that state would quietly break the
room. And every persona shares that one account, so the daemon's "own" messages include other
personas' messages. A target the daemon can't decrypt is refused too, because its real type can't be
checked.

As with every op, a request `id` is echoed back on the reply, and any failure (bad key, unknown
room, unknown event) is `{"ok": false, "error": "<reason>"}`.

Run `safehouse-mcp --help` for the full flag list. With no subcommand (or on a bare TTY), the
binary keeps its original behavior unchanged: a stdio MCP server for an MCP client to launch.

**The read-vs-check cursor trap:** `read` is *stateless* — it replays recent room history and
never touches any persona's mailbox. `check` is *stateful* — it's a specific persona's durable,
sqlite-backed unread-mail cursor (D16/D17), and by default **consuming** it (a second call
returns nothing new). For scripted/operator access, prefer `read`; that's why this CLI's `check`
defaults to peek-only (`--consume` opts in to advancing the cursor) — a bare `check` from a
script or a curious human should never silently eat a real agent's unread mail.

**Which persona to use:** don't borrow a real fleet agent's identity for ad hoc scripting — that
persona's mailbox cursor and room presence are supposed to reflect what that agent has actually
seen. Reserve a persona named `operator` in the daemon's `personas` allowlist instead (see the
comment in [`safehoused/example-config.toml`](safehoused/example-config.toml)); it's a normal
allowlist entry, not special-cased by the daemon, but it keeps operator traffic out of any real
agent's identity and mailbox.

### Shim-side guards

Three guards live in `safehouse-mcp` itself (`safehouse-mcp/src/guard.rs`, #181), because the shim —
not the daemon — is what sits in the agent's working directory with a prompt on one side and a room
on the other. They apply identically to the CLI subcommands and to the `safehouse_*` MCP tools, and
none of them touch a daemon-side invariant: the socket is still AF_UNIX-only and `from` is still
stamped by `safehoused`.

**1. Room content comes back fenced as untrusted input.** `read`/`check` replies are wrapped in an
explicit `BEGIN/END UNTRUSTED SAFEHOUSE ROOM CONTENT <token>` fence naming them as data, never
instructions — CLAUDE.md's "never trust identity from an agent message" rule extended from *who*
sent it to *what it says*. The token is derived from the payload, so a message body that spells out
a closing marker cannot end the fence early and continue "outside" it. On the CLI the markers are
written to **stderr** and the JSON to stdout, so `safehouse-mcp read | jq` still sees exactly one
JSON document; the reply also gains an additive `untrusted_content` field for consumers that only
read stdout. The enclosure is therefore only visually intact for a reader that keeps the two streams
separate — a consumer that merges stderr into stdout sees the markers interleaved with the JSON, and
should key off `untrusted_content` instead. Bodies are never rewritten. `status`/`send` replies are the daemon
describing its own state and are deliberately left unfenced. `list-rooms` is unfenced too, except
that each room's name is the remote-authored `m.room.name` (#185): the shim replaces `name` with
`name_untrusted` (the raw value, for matching) and `name_display` (flattened to one line with
control/bidi characters removed and capped at 64 characters), and adds a scoped `untrusted_fields`
notice — marking the one untrusted field rather than fencing four trustworthy ones.

**2. A credential-shaped body is refused before the socket is opened.** `send` scans the outgoing
body for PEM private-key blocks, AWS access key ids, known vendor token prefixes (GitHub, GitLab,
Slack, Anthropic/OpenAI, Google, npm, …), JWTs, URLs with embedded credentials, and
`token = <high-entropy value>` assignments. A match is refused with the rule name and a masked
excerpt — never the value itself, which would otherwise land in a transcript or CI log. The check
runs at the one choke point every op passes through, so no subcommand or tool can route around it.
Prose is deliberately left alone (`password: correct-horse-battery-staple`, `api_key = <your-key>`,
a commit SHA, `access_token: REDACTED` all pass); documented examples such as
`AKIAIOSFODNN7EXAMPLE` are recognized as illustrative.

**3. The invention firewall refuses to run from a firewalled repo at all.** A room is an outward
surface, so a repository whose material must not leave the session can be named in a deny file:
`$SAFEHOUSE_FIREWALL`, else `$XDG_CONFIG_HOME/safehouse/firewall`, else
`~/.config/safehouse/firewall`.

```
# ~/.config/safehouse/firewall — one rule per line, # comments
path ~/2am/notebook                  # this directory and everything under it
remote 2AMLogic/notebook             # any git remote of the invoking repo containing this
```

Matched, the whole invocation is refused (MCP server mode included) with an error naming the rule
and what it matched — not one op at a time, since the point is that this *host location* must not
reach a room. It fails **closed**: an explicitly-configured file that is missing, a file that
doesn't parse, or remote rules whose repository config can't be read are all refusals, because "the
firewall couldn't tell" must never read as "the firewall said yes". With no deny file, behavior is
unchanged. `--help`/`--version` still work from inside a denied repository; they reach no room.

`path` rules are compared on both the literal and the resolved spelling of each side, so a rule
written through a symlink (`~/work` → another mount) still fires from the resolved directory and
vice versa; a rule path that cannot be resolved is matched verbatim rather than dropped. `remote`
rules read only the invoking repository's own git config — `insteadOf` rewrites, `[include]`
directives, and remotes defined in `~/.gitconfig` are *not* followed, so name the URL substring as
it appears in the repo's `.git/config`.

## Chosen stack (verified live)

| Layer | Choice | Why |
|---|---|---|
| Homeserver | **tuwunel ≥ v1.8.2** — lightweight Rust | static musl binary, federation off. Chosen over continuwuity on *current, machine-verified* E2E conformance: tuwunel is the only one of the two running complement-crypto against real matrix-rust-sdk clients, while continuwuity's baseline is 5 months stale and fails every local-user device-list test. conduwuit is archived; Conduit/Dendrite are life-support |
| Daemon sync | **classic `/sync` (v2)**, not sliding sync | the one open E2E bug on both servers is in the sliding-sync to-device extension; sync v2 is spec-frozen and dodges the Aug–Oct 2026 MSC4186 churn |
| Daemon crypto | **matrix-rust-sdk ≥ 0.18.0** (vodozemac) | production-ready, bot-oriented, libolm is deprecated; **pantalaimon is archived — do not use**. Floor is not stylistic: CVE-2026-45056 (to-device sender-binding, fixed 0.16.1) is directly in our threat model |
| Wake | **persistent daemon, local dispatch** | the daemon is always-online by design, so we avoid the encrypted-appservice path entirely — and there is still no Rust appservice SDK. `safehouse-mcp` gives polling agents tools today; Claude Code Channels push-wake is the v1 upgrade |
| Human client | **Element X** (key-backup on) | glass-box view + @-mention remote control; no interactive verification needed — the daemon's self-signed device is trusted as-is |

The key insight: because we already committed to an always-on **per-host daemon**, the usual
"client-SDK bot must stay online" cost is one we happily pay — which lets us skip encrypted
appservices entirely and run the lightweight Rust server instead. See
[`docs/decisions.md`](docs/decisions.md#d5--lightweight-rust-homeserver--persistent-client-sdk-daemon-not-encrypted-appservice--synapse). (Encrypted appservices were Synapse-only when we chose;
tuwunel shipped them in July 2026. The decision stands on its original reasoning — and there is still
no Rust appservice SDK at all.)

## License

**Apache-2.0** — see [`LICENSE`](LICENSE), and [`docs/decisions.md`](docs/decisions.md#d8--license-apache-20-and-no-mxlink-dependency) D8 for why.

Short version: it matches our dependency tree (matrix-rust-sdk and vodozemac are Apache-2.0), carries
an express irrevocable patent grant, and preserves every downstream option — anyone wanting a
copyleft safehouse can fork Apache→AGPL, but the reverse is impossible.

`safehoused` is an **independent implementation** built directly on matrix-rust-sdk. We read
`baibot` (AGPL-3.0) and `mxlink` (LGPL-3.0) for patterns and depend on neither; no code was copied
from either. See [`CREDITS.md`](CREDITS.md).

**Two architectural invariants follow from this** and are not negotiable: the agent socket is
**AF_UNIX only, never a TCP listener**, and there is **no in-process plugin ABI**. Both are what keep
third-party agents legally separate works, free to carry any license their authors like.
