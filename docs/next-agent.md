# Start here — handoff to the safehouse agent

You're picking up **safehouse** with the **core stack built and verified live** — daemon, envelope,
MCP shim, production homeserver. Everything below is committed. Read in this order: this file →
`../README.md` → `design.md` → `decisions.md` → `open-questions.md` → `research/` as needed.

## What safehouse is (30 seconds)
A FOSS, E2E, bot-first messaging substrate on **Matrix** for small rooms (≤20). A **per-host daemon
(`safehoused`)** owns ONE Matrix device, stays online, does all encrypt/decrypt, and dispatches
inbound room events to **ephemeral local agents** that sit behind it over a **local unix socket** and
hold no keys. The **room is the single source of truth** so a human on Element gets full visibility +
@-mention remote control. Threat model: compromised host/server, NOT agent-vs-agent.

## Status: steps 1–5 done and verified live; next is the first real agent

Two research rounds are done (8 passes, all 2026-07-26, archived in `research/`). **Every question
is answered and live-verified**, including the phone check on the production stack: cold start, warm
start, store-wipe disaster recovery (`research/2026-07-26-qj-integration-test.md`), envelope v1
accepted and implemented, socket RPC + `safehouse-mcp` shipped, and the full chain proven —
MCP tool call → unix socket → encrypted room → the human's Element X. The production homeserver
(tuwunel, federation off) runs on a dedicated always-on EC2 host (D15, executed 2026-07-27),
reachable only over the fleet's tailnet, TLS via Caddy DNS-01 (D14); the Studio is a pure fleet
worker again.

## Decisions already made (don't relitigate without reason)

See `decisions.md` for all seventeen with rationale. The load-bearing ones:

- **Matrix, not a hand-rolled Signal clone.** Never hand-write crypto — vodozemac via
  matrix-rust-sdk. (D3)
- **Daemon per host, one device, keyless agents.** (D4)
- **Persistent client-SDK bot, not an appservice.** (D5)
- **License: Apache-2.0.** Build directly on matrix-rust-sdk; **do not depend on `mxlink`**. (D8)
- **The daemon is its OWN Matrix user** (`@safehoused:host`), never a device on the human's
  account. (D9)
- **It cross-signs itself. No human verification step.** (D10)
- **Homeserver: tuwunel ≥ v1.8.2**, federation off. (D12)
- **The daemon uses classic `/sync` (v2), not sliding sync.** (D13)

## Things that will bite you if you don't know them

1. **`EncryptionSettings` defaults are all wrong for us.** `auto_enable_cross_signing: true`,
   `auto_enable_backups: true`, `backup_download_strategy: OneShot` must be set explicitly. A daemon
   missing these works today and **silently stops working ~Oct 2026** when Element's "exclude
   insecure devices" rollout lands.
2. **The recovery passphrase is required config, not `Option`.** It is the only headless path back
   after a crypto-store loss — bootstrap no-ops once a server-side identity exists. Default
   `recovery_reset_allowed: false`; the reset path orphans every room key in backup.
3. **Pin `matrix-sdk` ≥ 0.18.0.** CVE-2026-45056 (to-device sender-binding, fixed 0.16.1) is directly
   in our threat model.
4. **Store passphrase and database directory are coupled.** Persist together, atomically. If the
   session blob is missing but the SQLite store exists, the store is undecryptable — purge and
   cold-start. A half-written state here is the most likely way to brick the daemon.
5. **Never copy code from `baibot`** (AGPL) — it would make safehoused AGPL virally. Reading it is
   clean; copying is not. `mxlink` is LGPL — also read-only for us. See D8's decision table.
6. **Never add a TCP listener or an in-process plugin ABI.** Security property *and* licensing
   invariant (D8).
7. **Budget 2–4 GB RAM for the homeserver**, not the 64–256 MB the old stack table claimed, and clamp
   the cache modifiers — defaults scale with core count.
8. **Flush room-key backup uploads before shutdown** — `Backups::wait_for_steady_state()`. Found
   live in Q-J: a key minted just before exit hadn't reached the server-side backup, and one message
   was permanently lost to the store wipe. Backup upload is a background task; treat an unflushed
   backup as not-yet-durable.
9. **sqlite is vendored, not system-linked.** Workspace `Cargo.toml` enables matrix-sdk's
   `bundled-sqlite` feature (covers the direct `rusqlite` dep too, via cargo feature unification on
   `libsqlite3-sys`) so a bare Linux toolchain builds standalone — no `libsqlite3-dev` package
   needed. Don't drop back to plain `sqlite` without re-adding that prerequisite to the README.

## Recommended first moves (in order)

### 1. ~~Q-J — the live integration test~~ ✅ DONE 2026-07-26, all steps pass, phone included
See `research/2026-07-26-qj-integration-test.md` and the phone-check addendum in
`open-questions.md` Q-J — Element X shows no reduced-trust indicator for the self-signed daemon.

### 2. ~~Q-F — design the envelope schema~~ ✅ DONE — accepted as `protocol/envelope-v1.md`

### 3. ~~Spike `safehoused` v0~~ ✅ DONE 2026-07-26 — `safehoused/` (workspace member)
Cold/warm/recovery boot, encrypted sqlite store, auto-join invites, sync v2, decrypts inbound and
prints to stdout, shutdown flushes room-key backup. Verified live: joined on invite from a second
user and decrypted a cross-user encrypted message. Refuses to run if the device fails to self-sign.

### 4. ~~Add the unix-socket RPC + envelope~~ ✅ DONE 2026-07-26
JSON-lines over `<state_dir>/safehoused.sock`: `hello` (persona gated by the config `personas`
allowlist, enforced in the daemon), `send` (daemon stamps `from`, renders envelope v1),
`create_room`, `add_to_space`, `list_rooms`, `read`, `invite` (#39), plus inbound push lines. **`safehouse-mcp`**
(workspace member) is the keyless stdio MCP shim over it — tools `safehouse_send` /
`safehouse_create_room` / `safehouse_add_to_space` / `safehouse_list_rooms` / `safehouse_read` —
pulled forward from the v1 plan. Verified live end-to-end: MCP tool call → socket → encrypted room
→ phone, allowlist rejection included. Envelope §7's loop-back rule was refined during
implementation (own events dispatch to non-author personas; see the note in
`protocol/envelope-v1.md`).

**Space (`m.space`) support + name/alias addressing (#27).** `create_room` takes `space: true` to
make an `m.space` container (left unencrypted — a Space carries only `m.space.child`/`m.space.parent`
state, not messages, so D5's "encrypt every meaningful message" rationale doesn't apply; message
rooms are still encrypted), and `parent: "<space id/name/alias>"` to create a room already linked
under a Space in one call. `add_to_space` links an already-joined room into a Space and is idempotent
(re-linking is a no-op success — state events are keyed by (type, state_key), never duplicated).
`list_rooms` now reports `type` (`"space"`/`"room"`) and `parent_space` (the room id of the first
reciprocal parent from `Room::parent_spaces()`, else null) so a client can render/verify the
hierarchy. Room addressing on `send`/`read`/`create_room parent`/`add_to_space` accepts a joined
room's id, name, canonical alias, or alt alias; an **ambiguous** name/alias (matching more than one
joined room) is now an error rather than a silent first-match. Live Element X phone verification of
the rendered Space hierarchy (issue #27 AC #5) remains a deferred operator step — cannot be
automated in CI.

**New-host onboarding via `invite` (#39).** The daemon already auto-accepted every invite
addressed to its own account (`on_invite`) — that half was never the gap. What was missing was a
way to *send* one without raw CS-API calls and a temporary device against an E2E account (the
foot-gun hit provisioning the fleet's second host, loom#3998). The `invite` op closes it:
`{"op": "invite", "room": "<id|name|alias>", "user": "@new-bot:server"}`, resolved through the same
`resolve_room` id/name/alias path as `send`/`read`, then `Room::invite_user_by_id`. Onboarding a new
host into an existing room is now one call from an already-onboarded host's socket — the new host's
daemon auto-joins on its next sync, cold-start included. The acceptance policy is now also
explicit and, optionally, restrictable: `invite_allowlist` in config (default `None`/unset —
accept-any, unchanged) limits which senders' invites `on_invite` will join.

### 5. ~~Per-agent mailbox + `safehouse_check` (D16/D17)~~ ✅ DONE 2026-07-26
Per-persona durable mailbox in `safehoused` (`mailbox.rs`), populated from the same synced room
timeline that drives live dispatch (`on_message`) — a broadcast (`to: "*"`) fans out to every
registered persona, a direct `to:` lands only in that persona's mailbox, own-host loop-back still
skips only the authoring persona. Read cursors persist in sqlite (`<state_dir>/mailbox.sqlite3`),
so an agent that was away for N messages — including across a daemon restart mid-gap — gets exactly
those N on its next `safehouse_check` and nothing on the immediate repeat. `safehouse_check` supports
`peek` (no-advance) and `limit`. The envelope's advisory `wake` field is stamped when a sender
supplies it (`safehouse_send`'s new `wake` argument) and round-trips through into `check` output
unchanged, per D16 (the daemon never acts on it — only optional external wakers would).

### 6. Wire one real agent and retire the copy-paste relay.
Start with the two-project handoff use case that motivated all this: one agent writing a long-form document, another holding the facts it needs, today bridged by a human copy-pasting.

### 7. ~~Public completion feed (#28 chain)~~ ✅ DONE 2026-07-29
Envelope v1 has a `completion` `type` + `completion-v1` `meta` (`protocol/envelope-v1.md` §4a, D18)
that round-trips through `safehoused` and degrades to `chat` when `meta` doesn't validate (#29). The
allowlist/redaction/delay-buffer publisher (#30, `safehoused/src/egress.rs`) and the outbound HTTP
transport (#31) both shipped: off by default, per-room opt-in, mandatory redaction, a delay buffer
with edit/redaction-triggered retraction, and a strictly-outbound sink (`sink_url` — a `POST`, with
bounded exponential-backoff retry on `5xx`/network errors and no retry on `4xx` — or the original
`sink_path` local JSON-lines file for backward compatibility). See `design.md` §4.1.2 for the full
shape and the D8 compliance note (never a listening socket).

## Deferred, with a deadline
- ~~**D11 — CLA vs. DCO.**~~ ✅ Decided 2026-07-26: **DCO** (`CONTRIBUTING.md`, D11).
- **Claude Code Channels push-wake** remains a **v1** item. `safehouse-mcp` covers the tools story
  today (`safehouse_check` for durable pull-model delivery, D17); Channels is the sanctioned *wake*
  mechanism — still a research preview with a changeable protocol contract, so still off the
  critical path.

## Housekeeping
- **One-command host installer (#40):** `scripts/install.sh` brings a new host into the fleet with no
  manual Matrix steps — builds the binary into `~/.local/bin`, prompts for homeserver + bot creds +
  recovery passphrase (generates the store passphrase), writes a `0600` config, verifies the first
  boot against the daemon's own cold-start path, registers a supervised service (launchd on macOS /
  `systemd --user` on Linux), and prints the loom-daemon handoff block. Pure host orchestration — zero
  Matrix logic; the login/cross-sign/recovery it "does" is just the daemon's `boot`. Bot-account
  creation on the homeserver stays an admin action (non-goal; the script points at the #25 docs).
- **Public since 2026-07-26:** https://github.com/rjwalters/safehouse (Apache-2.0, DCO
  contributions per `CONTRIBUTING.md`).
- Provenance for every decision lives in `research/` — eight passes, all 2026-07-26.
- Reference source (baibot, rust-mxlink, matrix-rust-sdk, tuwunel, continuwuity) was cloned to a
  scratchpad during research; re-clone as needed. Remember: read, don't copy.
