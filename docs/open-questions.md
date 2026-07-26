# safehouse — open questions

Things to resolve before (or early in) coding. Flagged by the 2026-07-26 research as *not* covered
by verified claims — treat as unanswered, not answered-negatively.

## Q-A. Prior art — ✅ ANSWERED (see research/2026-07-26-prior-art.md)
**Nobody built the "one device, many keyless personas, local dispatch" daemon → BUILD FRESH** on
matrix-rust-sdk. **CRIB `baibot` + `mxlink`** (etke.cc, Rust, active, AGPL-3.0) for the persistent-E2E
skeleton; rebuild pantalaimon's proxy shape (archived) + Hermes's decrypt-and-dispatch pattern. No
red-flag "stop, it exists" project. Remaining sub-gaps promoted to Q-G/Q-H/Q-I below.

## Q-G. MSC4190 / headless cross-signing bootstrap (BIGGEST TECHNICAL RISK)
Unverified: does matrix-rust-sdk support creating a device + bootstrapping cross-signing **without**
user-interactive auth (dehydrated devices / MSC4190), and do baibot/mxlink already implement that
headless-login + key-backup path? This is the daemon's login story — resolve early, it gates
everything. Read baibot/mxlink source directly.

## Q-H. Matrix MCP server (complement or partial substitute?)
Does a maintained MCP-server-for-Matrix exist that a Claude Code agent could use as a tool — and if
so, who holds the E2E keys? Could complement the daemon (agents reach Matrix via MCP through the local
daemon) or partially substitute for it. Unanswered by research.

## Q-I. License — AGPL-3.0 on baibot/mxlink
baibot and mxlink are AGPL-3.0. Depending on `mxlink` as a crate (vs. reading it for patterns) makes
`safehoused` AGPL — likely fine (safehouse is FOSS anyway) but **decide the license consciously**, and
mind the daemon↔agent process boundary (agents talk to the daemon over a socket, so they're separate
programs, not AGPL-linked — worth confirming).

## Q-B. Ephemeral-agent history visibility
The daemon (one device) accumulates Megolm room keys and stays online, so it decrypts fine. But
confirm: when the daemon relays room history to a freshly-spawned agent, does the agent get the
history it should see without gaps? (This is a daemon-internal concern now, not a Matrix key problem —
but verify no UISI/"unable to decrypt" edge cases for the daemon itself after restarts.)

## Q-C. Human phone client history + key backup
The human's new devices need **key backup / cross-signing** to read history from before they joined.
Confirm the Element flow is as smooth as assumed, and that `matrix-bot-sdk`-class "room key is
missing" bugs (seen in bridges) don't bite the daemon. Enable server-side key backup from day one.

## Q-D. Daemon language / SDK
matrix-rust-sdk (vodozemac) is the verified production-ready, bot-oriented choice. matrix-nio (Python)
is non-core-team and libolm-era — **avoid for the crypto-holding daemon.** Decide: daemon in Rust on
matrix-rust-sdk directly, or a thin Python/other wrapper over its bindings? (Agents can be any
language; they only speak the local unix-socket protocol.)

## Q-E. Wake-without-Synapse reliability (only if the daemon ever sleeps)
Not needed while the daemon is always-on. But if we ever want the daemon itself to sleep, is
Sygnal / UnifiedPush push-wake of a client-SDK bot reliable and low-footprint at ≤20 users, or does
it effectively force staying online anyway?

## Q-F. Envelope schema
Define the message envelope: `from`, `to` (agent | @human | room-broadcast), `type` (chat | task |
handoff | ack), `task-id`/threading, and how it renders for a *human* reading the room in Element
(should be legible, not JSON soup). Consider borrowing A2A's Task-object lifecycle for the `task`
type while keeping chat human-readable.

## Churn watch
The lightweight-homeserver landscape and appservice-E2E MSCs are moving fast (conduwuit died 2025;
continuwuity/tuwunel are young, ~weekly releases; MSC3202 being superseded by MSC4326). Because
safehouse depends on the **client-SDK** path, not encrypted appservices, we are insulated from the
churniest part — but re-check homeserver status before committing ops.
