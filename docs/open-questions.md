# safehouse — open questions

Things to resolve before (or early in) coding.

**Round 2 complete (2026-07-26).** Q-G, Q-H and Q-I are answered; provenance in `research/`. The
headline: the biggest technical risk (Q-G) is **retired**, and Q-I found a **factual error in our own
docs** that inverted the licensing answer. What remains is one live integration test and one design
decision (Q-F).

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

### Q-D. Daemon language / SDK — ✅ **ANSWERED**
**Rust, directly on matrix-rust-sdk ≥ 0.18.0.** Settled by D8 (no wrapper crate) and confirmed by
Q-G, which read the SDK source and verified the full headless path exists at that version. matrix-nio
(Python) remains ruled out for the crypto-holding daemon — non-core-team and libolm-era. Agents
remain any-language; they only speak the socket protocol.

---

## 🔴 OPEN — resolve before / early in coding

### Q-J. Live integration test *(NEW — the honest gap left by Q-G)*
Everything in Q-G is **source- and spec-reading. None of it has been run.** Before building daemon
code around it, do the one-evening test:

1. Stand up the homeserver locally, federation off.
2. Register `@safehoused:host`.
3. Run a ~60-line binary through the cold-start sequence.
4. Confirm the device reports **cross-signed** and that a human's Element sees its messages.
5. **Wipe the crypto store**, cold-start again, and confirm passphrase recovery self-signs the new
   device and pulls room keys back.

Step 5 is the one that matters most — it's the disaster-recovery path, and it's the reason the
recovery passphrase is mandatory config. Passing this converts Q-G from "solved on paper" to solved.

Also unverified: whether MSC4268 historic-key-bundles-on-invite works on our homeserver (matters only
if agents need pre-join history).

### Q-F. Envelope schema *(the main thing we get to invent)*
Define: `from`, `to` (agent | @human | room-broadcast), `type` (chat | task | handoff | ack),
`task_id`/threading, and how it renders for a *human* reading the room in Element — legible, not JSON
soup. Consider borrowing A2A's Task-object lifecycle for the `task` type while keeping chat readable.

Two constraints landed from round 2:
- **Field names must be `[A-Za-z0-9_]`-safe.** Claude Code Channels silently drops `meta` keys
  containing hyphens — so **`task_id`, not `task-id`** (this file previously said `task-id`). Free to
  fix now; annoying after the v1 shim exists.
- **Gate on sender identity, never room identity.** From the Channels docs, which independently
  reached our design: *"gating on the room would let anyone in an allowlisted group inject messages
  into the session."* The `from:` field is the right primitive, and it must be enforced **in the
  daemon**, not in any shim.
- Per D8, the envelope is also a **licensing-relevant boundary**: keep it a documented, versioned,
  language-agnostic wire format, and never leak internal Rust types across it.

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
