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

- Owns **one Matrix device/identity per host**, verified/cross-signed **once** (by a human, from
  their phone/Element).
- Holds the **E2E crypto store** (vodozemac via matrix-rust-sdk). Persistent, crash-safe.
- Runs the **sync loop**; performs **all** encrypt/decrypt.
- **Serializes the ratchet** — a single daemon is a single writer, so concurrent sends from multiple
  local agents are trivially ordered (a strict improvement over many bot-devices fighting over keys).
- Exposes a **local unix-socket RPC** to agents (perms-gated, never a network port):
  - `send(room, envelope)` — agent hands the daemon a plaintext message; daemon encrypts + posts.
  - `subscribe(room)` / dispatch — daemon delivers/wakes the agent on relevant inbound events.

### 4.2 Agents — ephemeral, behind the daemon

- Spawn and die freely (per-task). **Never touch keys, never verify, never hit "unable to decrypt."**
- Talk **plaintext** to the local daemon over the unix socket (inside the trust boundary).
- Identified by an **envelope field** (`from: book-agent`, `to: family-tree-agent | @human | room`),
  not by a Matrix identity. The daemon multiplexes many agent personas over its one device.

### 4.3 Human client

- **Element** (or any Matrix client), on phone/desktop, verified device with **key backup on**.
- Gets the **glass-box view**: sees all agent coordination live, because everything went through the
  room.
- Is a **remote control**, not just a mirror: @-mention an agent from your phone → the target host's
  daemon wakes that agent with your directive.

### 4.4 Homeserver

- **Lightweight Rust** (continuwuity / tuwunel), **federation off**, single box.
- Stores the encrypted log (durability + history + recovery for free).

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
                   ignore (+ don't loop       decrypt, dispatch/wake
                    back to sender A)          agent B
```

The only special case is a trivial "don't deliver A's own message back to A" filter. The phone is a
first-class recipient automatically — no dual-delivery consistency to reason about.

## 6. Wake

Because the daemon is **always online by design**, "wake" is just: *daemon sees a room event →
dispatch to / spawn the target local agent.* No appservice, no push notifications needed at this
scale. (If a host's daemon ever needs to sleep, Sygnal/UnifiedPush push-wake of the daemon is the
fallback — see open questions.)

## 7. Why this stack (research-backed, 2026-07-26)

The 2026 Matrix research surfaced a central architectural trade:

- **Encrypted appservices** (MSC3202/MSC4203) give the ideal "wake an idle agent over HTTP, with
  E2E" — but they are **Synapse-only** (lightweight Rust servers and the matrix-rust-sdk appservice
  crate do not support them; MSC3202 is still an unmerged, being-superseded proposal).
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
