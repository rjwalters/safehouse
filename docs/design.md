# safehouse — design

Status: **design phase**, no code. This captures the architecture we converged on and the reasoning,
so the decisions survive with their provenance.

## 1. Goal

Let AI coding agents (Claude Code / MCP-capable, scoped per git repo) and their humans coordinate in
**small shared rooms** (hard cap: **≤20 participants, ever** — no large or public rooms), instead of
a human relaying handoffs by copy-paste. FOSS, self-hosted, end-to-end-encryptable, reusable across
many project-systems.

## 2. Threat model

**In scope:** a compromised homeserver, a compromised network/wire, a curious server operator. The
server must see **ciphertext only**.

**Explicitly out of scope:** one agent process on a host attacking another agent on the *same* host.
On a host you own, co-located agents already share memory and filesystem — the machine is a single
trust domain. We do not try to isolate agents from each other cryptographically.

Consequence: **the machine is the unit of trust.** One cryptographic identity per host is the honest
design, not a shortcut.

Accepted limitation: E2E hides message **content** from the homeserver, not **metadata** — the server
still learns who is in which room, when, and roughly how much. Acceptable now; mitigated later by
self-hosting the server too.

## 3. Principles

- **Async / store-and-forward.** No persistent socket is assumed. This matches how human chat apps
  already work and how request/response agents already run.
- **The room is the single source of truth.** *Every* meaningful message goes through the encrypted
  room, including messages between two agents on the same host. Do **not** add a local agent-to-agent
  shortcut: the server round-trip is what fans the message out to the human's phone. The room is the
  audit log and the observability surface.
- **Agents are not cryptographic endpoints.** They hold no keys; a per-host daemon does.
- **Don't reinvent.** Use vodozemac for crypto, an existing homeserver for transport. Build only the
  agent-native layer.

## 4. Components

### 4.1 The daemon (`safehoused`) — one per host

The heart of the system and the reusable IP.

- Owns **one Matrix device/identity per host**, on its **own dedicated Matrix account**
  (`@safehoused:host`) — never a second device on the human's account (**D9**). It **cross-signs
  itself** at first login; no human verification step is required (**D10**).
- Holds the **E2E crypto store** (vodozemac via matrix-rust-sdk ≥ 0.18.0). Persistent, crash-safe,
  encrypted at rest.
- Runs the **sync loop**; performs **all** encrypt/decrypt.
- **Serializes the ratchet** — a single daemon is a single writer, so concurrent sends from multiple
  local agents are trivially ordered (a strict improvement over many bot-devices fighting over keys).
- Exposes a **local unix-socket RPC** to agents (perms-gated, never a network port):
  - `send(room, envelope)` — agent hands the daemon a plaintext message; daemon encrypts + posts.
  - `check(persona)` / `read(room)` — agent pulls its per-persona mailbox (durable, cursor-tracked)
    or room history **on its own cadence**; the daemon never pushes or spawns (**D16**, **D17**). A
    live-connected socket may additionally receive low-latency delivery, but that is an optimization
    over the same pull-based mailbox, not a separate wake path.

The socket is **AF_UNIX only, permission-gated, never a TCP listener**, and there is **no in-process
plugin ABI**. That is a security property *and* — since it is what keeps third-party agents legally
separate works — a licensing invariant (**D8**). Do not add a `--listen` flag.

#### 4.1.1 Identity and key lifecycle (verified 2026-07-26, `research/2026-07-26-headless-login.md`)

Fully headless. The **only** human action in the daemon's whole lifecycle is creating the bot account
and handing over a password.

**First run.** Log in with password → matrix-rust-sdk's initialization task uploads device keys,
creates and uploads the cross-signing identity, and **uploads a self-signature over the daemon's own
device**. No user-interactive auth challenge, because the Matrix spec (MSC3967, stable since v1.11)
exempts a user's *first-ever* cross-signing upload. Then enable secret storage + key backup with the
configured recovery passphrase.

That self-signature is exactly the bar Element's "exclude insecure devices" enforces — **owner-signed,
not interactively-verified**. This is why D9 matters: the exemption only applies to an account with no
existing master key, so the daemon must be its own user.

**Every later run.** Restore the persisted session against the *same* store passphrase and database
directory. Bootstrap correctly no-ops. **Invariant:** if the session blob is missing but the SQLite
store exists, the store is undecryptable — purge and cold-start. A half-written state here is the most
likely way to brick the daemon.

**Disaster recovery (crypto store lost, account intact).** Delete the store, cold-start. Bootstrap
no-ops, then `recover(passphrase)` pulls the private self-signing key back from secret storage,
**self-signs the replacement device**, re-enables backup, and pulls every room key down. Still no
human. This is why the recovery passphrase is **required config, not optional** — without it a
replacement device is permanently unsigned and invisible.

**Non-negotiable settings** (SDK defaults are all wrong for us):
`auto_enable_cross_signing: true`, `auto_enable_backups: true`,
`backup_download_strategy: OneShot`, `recovery_reset_allowed: false` (reset orphans every room key
already in backup), and `matrix-sdk ≥ 0.18.0` (CVE-2026-45056 is a to-device sender-binding gap fixed
in 0.16.1 — directly our threat model).

#### 4.1.2 Public completion feed egress (#28–#31)

**Off by default.** With no `egress` block in the daemon config, this subsystem does not exist at
runtime — zero behavior change from a daemon that predates it.

When explicitly configured, it is a narrow, fail-safe mirror of one envelope type (`completion`,
`completion-v1` `meta` — `protocol/envelope-v1.md` §4a) out of specific, opted-in rooms to a public
sink, so e.g. a status page can show "agent X finished task Y" without exposing the room's actual
content:

- **Explicit per-room opt-in + type allowlist.** Only rooms listed in `egress.rooms`, and only
  well-formed `completion` envelopes, are ever eligible (`safehoused/src/egress.rs::is_allowlisted`).
  Everything else — every other envelope type, every other room — is never even considered.
- **Mandatory redaction.** A non-empty `egress.deny_patterns` list is required whenever `egress.rooms`
  is non-empty; every deny pattern is a literal substring stripped from every string in the payload
  before it is queued. The daemon refuses to boot on a room opted in with no deny patterns
  (`validate_egress_config`) — leak-everything-by-omission is not a state this feature can reach.
- **Delay buffer with retraction.** A completion sits in a durable sqlite-backed buffer for
  `egress.delay_seconds` before publishing. A native Matrix edit (`m.replace`) or redaction of the
  source event inside that window suppresses the pending row entirely — the same "undo" a human
  already has in Element, reused rather than inventing a bespoke retract envelope.
- **Sink (#31): strictly outbound HTTP, or a local file.** The published sink is configured via
  `egress.sink_url` (an outbound `POST` — e.g. a Workers/Pages endpoint or an R2-backed feed) and/or
  `egress.sink_path` (a local JSON-lines file, the original #30 mechanism, kept for backward
  compatibility); `sink_url` wins if both are set. **The sink only ever originates outbound
  connections — it never binds a listening socket, on any address, ever** (D8). This is the same
  invariant as the agent socket (§4.1): safehoused has exactly one place it accepts connections
  (the AF_UNIX agent socket) and the public feed is not it.
- **Bounded, poll-driven retry — never an unbounded queue.** The existing 1s delay-buffer poll loop
  also drives retry: a transient failure (network error or `5xx`) reschedules the row with
  exponential backoff and an `attempts` counter; after a small fixed number of attempts the row is
  marked `failed` (inspectable, not silently dropped) rather than retried forever. A `4xx` is treated
  as a config/schema problem to surface to the operator, not a transient fault — it is marked
  `failed` immediately, without consuming retry attempts, on the reasoning that hammering a
  provably-broken request will never succeed.
- **At-least-once delivery.** A crash between a successful sink write and marking the row published
  re-emits that row on the next flush after restart. The published body
  (`{"room_id", "event_id", "payload"}`) carries `(room_id, event_id)` as a natural dedup key — a
  receiver on the other end of `sink_url` **must** tolerate the same pair arriving more than once.
- A sink failure (of any kind) never blocks or crashes the sync loop — the room remains the single
  source of truth (§3); the public feed is a lossy, best-effort mirror of it, not something the
  daemon's core loop depends on.

### 4.2 Agents — ephemeral, behind the daemon

- Spawn and die freely (per-task). **Never touch keys, never verify, never hit "unable to decrypt."**
- Talk **plaintext** to the local daemon over the unix socket (inside the trust boundary).
- Identified by an **envelope field** (`from: writer-agent`, `to: research-agent | @human | room`),
  not by a Matrix identity. The daemon multiplexes many agent personas over its one device.

### 4.3 Human client

- **Element** (or any Matrix client), on phone/desktop, verified device with **key backup on**.
- Gets the **glass-box view**: sees all agent coordination live, because everything went through the
  room.
- Is a **remote control**, not just a mirror: @-mention an agent from your phone → the message lands
  in the target persona's mailbox on the target host, and that agent picks it up on its own next
  check-in (**D16**) — the daemon delivers/queues, it does not spawn or push-wake the agent (see §6).
- **Optional, cosmetic:** a one-time user-to-user verification of `@safehoused:host` from Element
  gets a green check on the daemon's *user*. Operator confidence only — it is not required for
  anything to work (D10). If we ever implement the daemon side of this, auto-confirming a SAS must be
  gated behind an explicitly operator-opened window, or we'd auto-accept a MITM'd verification.

### 4.4 Homeserver

- **tuwunel ≥ v1.8.2** (lightweight Rust), **federation off**, single box. Chosen over continuwuity
  on current machine-verified E2E conformance — see `decisions.md` D12.
- Stores the encrypted log (durability + history + recovery for free).
- **Federation must be off before first boot** (both servers warn that changing it later breaks
  things), `allow_registration = false`, and the cache modifiers clamped explicitly — defaults scale
  with core count and will happily eat 384 MB of block cache on a 4-core box. Budget **2–4 GB**, not
  the 64–256 MB originally assumed.
- **Avoid OIDC.** It breaks bot-token minting on continuwuity and forces a browser-visited approval
  URL for cross-signing re-upload on tuwunel. Plain password auth for the daemon.

## 5. Message flow (one path for everyone)

Dispatch is driven by the **room event stream (from sync)**, not by local send calls. This yields a
single code path for same-host, cross-host, and human traffic:

```
agent A ──send()──▶ daemon(A host) ──encrypt──▶ room ──▶ homeserver
                                                   │
                          ┌────────────────────────┼───────────────────────┐
                          ▼                         ▼                        ▼
                   daemon(A host) sync       daemon(B host) sync        phone (Element)
                   sees event,               sees event,                shows it
                   `to: B`? B not local →     `to: B`? B local →
                   ignore (+ don't loop       decrypt, file into
                    back to sender A)          B's mailbox (§6)
```

The only special case is a trivial "don't deliver A's own message back to A" filter. The phone is a
first-class recipient automatically — no dual-delivery consistency to reason about. Filing into B's
mailbox is not a wake or a spawn — B reads it whenever it next checks in (**D16**, **D17**).

## 6. Wake (pull, not push — D16, D17)

**Waking an agent is not safehouse's job.** The daemon is a pure always-on substrate: it holds room
state (the source of truth, §3/D6), files every inbound envelope into the recipient persona's durable
**mailbox** (D17), and accepts sends. It never spawns, launches, or push-notifies an agent process.
*When* an agent runs and checks its mailbox is entirely the agent's own choice — exactly like a human
checking their phone on their own cadence: idle while heads-down on a task, frequent while awaiting a
reply. Scheduling that cadence belongs to whatever runs the agent (a supervisor like loom, cron, a
long-lived process, or a human at a REPL), not to the message layer. See `decisions.md` **D16** for
the full rationale and **D17** for the mailbox primitive this section assumes.

Concretely:

- The daemon's sync loop decrypts every inbound event and, per §5, files it into the mailbox of each
  locally-hosted recipient persona (or all local personas, for a broadcast `to: "*"`).
- Agents pull from their mailbox on their own schedule — via the `check` MCP tool / socket RPC (§4.1),
  which returns unread envelopes and advances a per-persona read cursor. Because the mailbox is
  durable and rebuildable from the room (D17), an agent that was offline for hours still gets exactly
  what it missed on its next check-in; nothing is lost by not being "awake" when a message arrived.
- The envelope's `wake` field and the "suggested `wake`?" classification in `envelope-v1.md` §4 are
  **advisory metadata only** — a hint for an *external* waker (see below), never an instruction the
  daemon itself acts on.

**External wakers are opt-in, not required.** At ≤20-participant scale a fully pull-based agent (one
that checks its mailbox each time it happens to run) already works. For agents that want lower
latency, an optional external layer may use the advisory `wake` hint to decide when to actually invoke
the agent — this is a **v0-vs-v1** split. v0 needs nothing beyond the mailbox above. For v1, **Claude
Code Channels** is one such optional waker: Claude Code can run a channel as an MCP stdio subprocess
and receive pushed notifications that prompt it to check its mailbox. The plan is
`safehoused-channel`, a thin **keyless** shim that dials the daemon's socket and translates — the
daemon stays the only device, only crypto store, and only enforcement point for `from`. Channels is
still a research preview whose protocol contract may change, so it stays off the v0 critical path.
See `research/2026-07-26-mcp-and-channels.md`.

## 7. Why this stack (research-backed, 2026-07-26)

The 2026 Matrix research surfaced a central architectural trade:

- **Encrypted appservices** (MSC3202/MSC4203) give the ideal "wake an idle agent over HTTP, with
  E2E" — but at the time we chose, they were **Synapse-only**, and MSC3202 was an unmerged,
  being-superseded proposal.

  *Updated 2026-07-26: tuwunel v1.8.2 shipped MSC3202 and MSC4203 on 2026-07-17, so "Synapse-only" is
  no longer true. The conclusion is unaffected, and in fact better-supported — round-2 research found
  that **there is no Rust appservice SDK at all** (`matrix-sdk-appservice` is a 2022 name
  placeholder) and that **tuwunel's appservice device path is broken** (issue #327). The path stays
  closed to us in practice. See `decisions.md` D5's amendment.*
- **Persistent client-SDK bot** = natural E2E on any server, but "must stay online."

**safehouse's daemon-per-host model dissolves this trade.** We already committed to an always-on
per-host daemon, so "must stay online" is a cost we pay by design — which frees us from encrypted
appservices and from Synapse, letting us run the lightweight Rust server. The daemon is a persistent
**client-SDK** bot (matrix-rust-sdk), not an appservice.

It also dissolves the historically-hard **ephemeral-agent key lifecycle** problem: since ephemeral
agents are *not* Matrix devices at all, there are no per-agent keys to provision, verify, or recover.
The daemon is the only device; it accumulates room keys once and stays online.

## 8. Non-goals

- Large/public rooms, big-community moderation, federation at scale.
- Per-agent cryptographic isolation (see threat model).
- A bespoke chat protocol or homeserver.
