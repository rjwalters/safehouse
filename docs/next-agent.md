# Start here — handoff to the safehouse agent

You're picking up **safehouse** with **research complete and no code written**. Everything below is
committed. Read in this order: this file → `../README.md` → `design.md` → `decisions.md` →
`open-questions.md` → `research/` as needed.

## What safehouse is (30 seconds)
A FOSS, E2E, bot-first messaging substrate on **Matrix** for small rooms (≤20). A **per-host daemon
(`safehoused`)** owns ONE Matrix device, stays online, does all encrypt/decrypt, and dispatches
inbound room events to **ephemeral local agents** that sit behind it over a **local unix socket** and
hold no keys. The **room is the single source of truth** so a human on Element gets full visibility +
@-mention remote control. Threat model: compromised host/server, NOT agent-vs-agent.

## Status: ready to build — Q-J passed live, Q-F accepted

Two research rounds are done (8 passes, all 2026-07-26, archived in `research/`). **The biggest
technical risk — headless login + cross-signing — is retired, and as of 2026-07-26 it is verified
live** (`research/2026-07-26-qj-integration-test.md`, spike in `spikes/qj-coldstart/`): cold start,
warm start, and store-wipe disaster recovery all pass against tuwunel v1.8.2 + matrix-sdk 0.18.0.
The envelope schema is accepted (`protocol/envelope-v1.md`). Nothing blocks `safehoused` v0 except
the phone-side Element check (needs Robb).

## Decisions already made (don't relitigate without reason)

See `decisions.md` for all thirteen with rationale. The load-bearing ones:

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

### 1. ~~Q-J — the live integration test~~ ✅ DONE 2026-07-26, all steps pass
See `research/2026-07-26-qj-integration-test.md`. Only the phone-side check remains: Element as
`@robb:safehouse.local`, accept the safehouse-test invite, confirm decryption + the bot's shield.

### 2. ~~Q-F — design the envelope schema~~ ✅ DONE — accepted as `protocol/envelope-v1.md`

### 3. Spike `safehoused` v0.
Cold/warm start, persistent encrypted crypto store, join the room, sync v2, decrypt inbound, print to
stdout. No agents yet. ~400 lines for the boot path — write it yourself, don't reach for mxlink.

### 4. Add the unix-socket RPC + envelope.
`send(room, envelope)` and inbound dispatch. Enforce the `from` allowlist **in the daemon**.

### 5. Wire one real agent and retire the copy-paste relay.
Start with the nitas-mama or family-tree handoff use case that motivated all this.

## Deferred, with a deadline
- **D11 — CLA vs. DCO.** Must be decided **before merging any outside contribution**; after that,
  relicensing needs every contributor's consent.
- **Claude Code Channels / MCP** is a **v1** item (`safehoused-channel`, a keyless stdio shim). It's
  the sanctioned wake mechanism and worth building — but it's a research preview with a changeable
  protocol contract, so keep it off the v0 critical path.

## Housekeeping
- Repo is local only. Public repo `rjwalters/safehouse` to be created when Robb says go
  (`gh repo create rjwalters/safehouse --public --source . --push`).
- Provenance for every decision lives in `research/` — seven passes, all 2026-07-26.
- Reference source (baibot, rust-mxlink, matrix-rust-sdk, tuwunel, continuwuity) was cloned to a
  scratchpad during research; re-clone as needed. Remember: read, don't copy.
