# safehouse — open questions

Things to resolve before (or early in) coding.

**Round 2 complete (2026-07-26).** Q-G, Q-H and Q-I are answered; provenance in `research/`. The
headline: the biggest technical risk (Q-G) is **retired**, and Q-I found a **factual error in our own
docs** that inverted the licensing answer. Q-F (envelope schema) is answered by
`protocol/envelope-v1.md`, and **Q-J ran live on 2026-07-26 and passed** — nothing blocks building
`safehoused` v0.

---

## ✅ ANSWERED

### Q-A. Prior art — ANSWERED (`research/2026-07-26-prior-art.md`)
Nobody built the "one device, many keyless personas, local dispatch" daemon → **BUILD FRESH** on
matrix-rust-sdk. Note the file carries a correction banner: mxlink is **LGPL-3.0** at
`etkecc/rust-mxlink`, not AGPL at `etkecc/mxlink`.

### Q-G. Headless login + cross-signing — ✅ **SOLVED** (`research/2026-07-26-headless-login.md`)
**This was the biggest technical risk. It is retired.** The daemon can create a device, upload keys,
bootstrap a full cross-signing identity, **self-sign its own device**, and enable key backup with
**zero human interaction**.

The question was aimed at the wrong MSC: **MSC4190 is appservice-only** and irrelevant; dehydrated
devices are a red herring. The mechanism is **MSC3967** (stable since Matrix v1.11, June 2024) —
a user's *first-ever* cross-signing upload is exempt from user-interactive auth. Confirmed in both
candidate homeservers' route handlers.

Consequences recorded as **D9** (daemon is its own Matrix user) and **D10** (headless self-bootstrap,
no human verification step — dropped from the design).

**Remaining gap → Q-J below: none of it has been run against a live server.**

### Q-H. Matrix MCP server — ✅ **COMPLEMENT, not v0** (`research/2026-07-26-mcp-and-channels.md`)
~30 Matrix MCP repos exist; all either ignore encryption or hold their own device+keys. **None
proxies to a keyless client** — that category is empty, and it's the one safehouse would fill.
Not a substitute: adopting any means one-device-per-agent, reintroducing the key lifecycle problem
`design.md` §7 dissolves.

Not ignorable either: **Claude Code Channels** is the sanctioned mechanism for the "daemon wakes the
agent" step §6 assumes, and it *is* an MCP server (stdio subprocess). **v1** plan is
`safehoused-channel`, a ~200-line keyless stdio shim over the unix socket. Channels is still a
research preview with a changeable protocol contract — **keep it off the v0 critical path.**

### Q-I. License — ✅ **Apache-2.0** (`research/2026-07-26-licensing.md`, decision **D8**)
Our docs recorded mxlink as AGPL; it is **LGPL-3.0**. That inverted the answer: LGPLv3 §4 would have
let us link it and stay permissive anyway, and baibot (the actual AGPL project) is an *application*
with no linkable code. **The work delta between Apache-2.0 and AGPL was zero.**

Decision: **Apache-2.0, no `mxlink` dependency**, write the ~400 lines ourselves — it's the daemon's
boot and key-custody path, the most security-critical code in the project. Decision rule and the
architectural invariants that flow from it (no TCP listener, no plugin ABI) are in **D8**.

### Q-F. Envelope schema — ✅ **ANSWERED** (`protocol/envelope-v1.md`, accepted 2026-07-26)
An ordinary `m.room.message` (`msgtype: m.text`) carrying a namespaced `org.safehouse.envelope` key —
not a custom event type, so Element renders it legibly. Fields: `v`, `from` (daemon-stamped, never
agent-supplied), `to`, `type` (`chat`|`task`|`handoff`|`ack`), `task_id`, `body`, plus native
`m.thread` relations. Human messages get an envelope synthesized by the daemon (`@persona` token,
thread reply, or no-wake broadcast). Both round-2 constraints honored: `[A-Za-z0-9_]` field names and
sender-identity gating enforced in the daemon.

### Q-D. Daemon language / SDK — ✅ **ANSWERED**
**Rust, directly on matrix-rust-sdk ≥ 0.18.0.** Settled by D8 (no wrapper crate) and confirmed by
Q-G, which read the SDK source and verified the full headless path exists at that version. matrix-nio
(Python) remains ruled out for the crypto-holding daemon — non-core-team and libolm-era. Agents
remain any-language; they only speak the socket protocol.

---

### Q-J. Live integration test — ✅ **PASSED** (`research/2026-07-26-qj-integration-test.md`)
Ran live against tuwunel v1.8.2 + matrix-sdk 0.18.0 (`spikes/qj-coldstart/`). Headless cold start,
cross-signing self-bootstrap (MSC3967), warm start, and — the one that matters — **store-wipe
disaster recovery** all pass: `recover(passphrase)` self-signs the replacement device and OneShot
pulls every room key back. Q-G is now solved in practice, not just on paper.

**New landmine found live:** a room key minted just before process exit may not have reached the
server-side backup — the first recovery attempt lost a message permanently. The daemon's shutdown
path MUST call `Backups::wait_for_steady_state()` (verified as the fix).

**Phone check done 2026-07-26, production stack** (Element X on Android → tailnet →
the production homeserver): the human's encrypted message was decrypted and printed by
`safehoused` end-to-end, and the bot's post-join messages decrypt on the phone. Pre-join history
shows as undecryptable on the phone as expected — MSC4268 historic-key-bundles remains the open
item there (only matters if pre-join history should be readable). Shield check (observed by Robb,
2026-07-26): **Element X shows no reduced-trust indicator at all** for the self-signed,
never-interactively-verified bot — the headless bootstrap is treated as first-class, better than
the "gray shield at worst" prediction. (Cosmetic note: tuwunel appends a 💕 display-name suffix to
new users by default — disabled via `new_user_displayname_suffix = ""`.)

## 🔴 OPEN — resolve before / early in coding

### Q-E. Wake-without-Synapse reliability *(only if the daemon ever sleeps)*
Not needed while the daemon is always-on. If we ever want it to sleep: is Sygnal / UnifiedPush
push-wake of a client-SDK bot reliable and low-footprint at ≤20 users, or does it force staying
online anyway?

---

## 🟡 PARTIALLY ANSWERED — verify during Q-J

### Q-B. Ephemeral-agent history visibility
Largely dissolved by Q-G: the daemon accumulates Megolm keys, stays online, and — with
`BackupDownloadStrategy::OneShot` plus a recovery passphrase — recovers all room keys after a store
loss. Two landmines to watch during Q-J:
- **matrix-rust-sdk#5018**, "Megolm session retrieved from backup incorrectly marked as insecure" —
  an open blocker on Element's exclusion rollout that would hit a daemon restoring history from
  backup, right in the Oct 2026 window. **Track it.**
- The session-file/database consistency invariant: if the session blob is missing but the SQLite
  store exists, the store is undecryptable — purge and cold-start. A half-written state here is the
  most likely way to brick the daemon.

### Q-C. Human phone client history + key backup
Enable server-side key backup from day one (D10 makes `auto_enable_backups` mandatory anyway).
Still worth confirming hands-on during Q-J that Element's own verification + backup flow is as smooth
as assumed against our chosen homeserver.

---

## Churn watch

**tuwunel#525 (ours, filed 2026-07-26):** feature request for MSC4108 QR-code login on top of
tuwunel's native OIDC server (#342). If it lands, phones onboard by QR instead of typed passwords —
but only adoptable if password `/login` keeps coexisting, since the daemon's headless path (D10)
depends on it. Until then: manual sign-in on phones; Element X QR login is impossible on tuwunel.
The lightweight-homeserver landscape moves fast (conduwuit died 2025; continuwuity/tuwunel are young,
~weekly releases; MSC3202 superseded by MSC4326). Because safehouse rides the **client-SDK** path, not
encrypted appservices, we're insulated from the churniest part — and Q-G added two independent
reasons that was the right call: tuwunel's appservice path is **broken** (issue #327, unresolved since
2026-02-22) and **no Rust appservice SDK exists at all** (`matrix-sdk-appservice` on crates.io is a
2022 name placeholder).

**Recheck done → `research/2026-07-26-homeserver.md`, decision D12.** It **reversed** the provisional
pick: **tuwunel ≥ v1.8.2**, not continuwuity. Continuwuity's committed Complement baseline is 5 months
stale and fails *every local-user device-list test* — and with federation off, every user is a local
user. Tuwunel is also the only one of the two running complement-crypto against real matrix-rust-sdk
clients. Two premises we had recorded turned out stale: encrypted appservices are no longer
Synapse-only (tuwunel shipped MSC3202/4203 in July 2026 — D5 still stands, on better reasoning), and
the "64–256 MB RAM" target was wrong (**budget 2–4 GB**, clamp cache modifiers). Also produced **D13:
the daemon uses sync v2, not sliding sync.**

**Deadline to keep in view:** Element's "exclude insecure devices" rollout lands ~**Oct 2026**. A
daemon without cross-signing bootstrap silently stops working then. That's why D10 is v0, not v1.
