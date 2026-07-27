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

## Status: steps 1–4 done and verified live; next is the first real agent

Two research rounds are done (8 passes, all 2026-07-26, archived in `research/`). **Every question
is answered and live-verified**, including the phone check on the production stack: cold start, warm
start, store-wipe disaster recovery (`research/2026-07-26-qj-integration-test.md`), envelope v1
accepted and implemented, socket RPC + `safehouse-mcp` shipped, and the full chain proven —
MCP tool call → unix socket → encrypted room → the human's Element X. The production homeserver
(tuwunel, federation off) runs on an always-on host, reachable only over the fleet's tailnet, TLS
via Caddy DNS-01 (D14).

## Decisions already made (don't relitigate without reason)

See `decisions.md` for all fourteen with rationale. The load-bearing ones:

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
`create_room`, `list_rooms`, `read`, plus inbound push lines. **`safehouse-mcp`** (workspace
member) is the keyless stdio MCP shim over it — tools `safehouse_send` / `safehouse_create_room` /
`safehouse_list_rooms` / `safehouse_read` — pulled forward from the v1 plan. Verified live
end-to-end: MCP tool call → socket → encrypted room → phone, allowlist rejection included.
Envelope §7's loop-back rule was refined during implementation (own events dispatch to non-author
personas; see the note in `protocol/envelope-v1.md`).

### 5. Wire one real agent and retire the copy-paste relay.
Start with the two-project handoff use case that motivated all this: one agent writing a long-form document, another holding the facts it needs, today bridged by a human copy-pasting.

## Deferred, with a deadline
- ~~**D11 — CLA vs. DCO.**~~ ✅ Decided 2026-07-26: **DCO** (`CONTRIBUTING.md`, D11).
- **Claude Code Channels push-wake** remains a **v1** item. `safehouse-mcp` covers the tools story
  today (polling via `safehouse_read`); Channels is the sanctioned *wake* mechanism — still a
  research preview with a changeable protocol contract, so still off the critical path.

## Housekeeping
- **Public since 2026-07-26:** https://github.com/rjwalters/safehouse (Apache-2.0, DCO
  contributions per `CONTRIBUTING.md`).
- Provenance for every decision lives in `research/` — eight passes, all 2026-07-26.
- Reference source (baibot, rust-mxlink, matrix-rust-sdk, tuwunel, continuwuity) was cloned to a
  scratchpad during research; re-clone as needed. Remember: read, don't copy.
