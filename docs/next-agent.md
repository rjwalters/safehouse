# Start here — handoff to the safehouse agent

You're picking up **safehouse** at the end of its design phase. Everything below is committed; nothing
is coded yet. Read in this order: this file → `../README.md` → `design.md` → `decisions.md` →
`research/2026-07-26-prior-art.md` → `open-questions.md`.

## What safehouse is (30 seconds)
A FOSS, E2E, bot-first messaging substrate on **Matrix** for small rooms (≤20). A **per-host daemon
(`safehoused`)** owns ONE Matrix device, stays online, does all encrypt/decrypt, and dispatches
inbound room events to **ephemeral local agents** that sit behind it over a **local unix socket** and
hold no keys. The **room is the single source of truth** so a human on Element gets full visibility +
@-mention remote control. Threat model: compromised host/server, NOT agent-vs-agent.

## Decisions already made (don't relitigate without reason)
- Build on Matrix, not a hand-rolled Signal clone. Never hand-write crypto — use **vodozemac**.
- **Daemon per host, one device, keyless agents.** (`decisions.md` D4.)
- **Lightweight Rust homeserver** (continuwuity/tuwunel), federation off; daemon is a **persistent
  client-SDK bot**, NOT an encrypted appservice — that path is Synapse-only and we don't need it
  because the daemon is always-on anyway. (`decisions.md` D5.)
- pantalaimon is **archived (2026-04-08)** — rebuild its shape, don't adopt it.

## Prior-art verdict (research/2026-07-26-prior-art.md)
**BUILD FRESH** — nobody built the "one device, many keyless personas, local dispatch" daemon.
- **DEPEND-ON:** `matrix-rust-sdk` (`matrix-sdk` + standalone `matrix-sdk-crypto`).
- **CRIB:** `baibot` + `mxlink` (etke.cc, Rust, active, **AGPL-3.0**) — closest living persistent-E2E
  skeleton. Also crib the *pattern* of Hermes Agent proxy mode (decrypt → forward plaintext to keyless
  agent) and pantalaimon (daemon owns crypto).

## Recommended first moves (in order)
1. **Resolve Q-G first (biggest risk):** read `baibot`/`mxlink` source to confirm the **headless login
   + cross-signing bootstrap without user-interactive auth** (MSC4190 / dehydrated devices) works in
   matrix-rust-sdk today. This gates the whole daemon. (`open-questions.md` Q-G.)
2. **Stand up the substrate locally:** continuwuity (or tuwunel), federation off, one test room; log in
   Element on your phone as the human; verify it.
3. **Spike `safehoused` v0** on matrix-rust-sdk (cribbing mxlink): headless login as one device,
   persistent encrypted crypto store, join the room, decrypt inbound, print to stdout. No agents yet.
4. **Add the unix-socket RPC + envelope** (`open-questions.md` Q-F): `send(room, envelope)` and inbound
   dispatch. Envelope carries `from`/`to`/`type` and must render legibly for a human in Element.
5. **Wire one real agent** behind the daemon (start with the nitas-mama or family-tree handoff use
   case that motivated all this) and retire the copy-paste relay.

## Decide consciously before writing much code
- **License** (Q-I): cribbing AGPL `mxlink` as a dependency makes `safehoused` AGPL. Probably fine;
  confirm and pick the repo license.
- **Daemon language:** Rust on matrix-rust-sdk directly is the verified-safe path. matrix-nio (Python)
  is libolm-era — avoid for the crypto-holding daemon. Agents can be any language (socket protocol).

## Housekeeping
- Repo is local only. Public repo `rjwalters/safehouse` to be created when Robb says go
  (`gh repo create rjwalters/safehouse --public --source . --push`).
- Provenance for every decision lives in `research/` (three deep-research passes, 2026-07-26).
