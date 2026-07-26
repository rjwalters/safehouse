# safehouse — decisions (ADR-lite)

Short log of the choices and their rationale. Newest context at the bottom.

## D1 — Buy the substrate, build the agent layer
**Decision:** don't build a chat server or crypto; build a thin agent-native layer on an existing
E2E messaging substrate.
**Why:** a first research pass concluded that for a few repo-scoped agents + a human, a purpose-built
"Slack for agents" is over-engineering. Existing platforms + wake mechanisms already cover it. The
differentiated, reusable value is agent identity/keys/inbox/wake, not chat or crypto.

## D2 — Async, not real-time sockets
**Decision:** treat messaging as store-and-forward; no assumption that agents hold a socket open.
**Why:** coding agents are request/response and don't hold sockets between runs. Human chat apps are
also store-and-forward. The real problem is *waking* an idle endpoint, not transport.

## D3 — Matrix as the substrate (over rolling our own from Signal)
**Decision:** build on Matrix rather than cribbing Signal to hand-build a bot-friendly messenger.
**Why:** Matrix is FOSS, E2E (vodozemac), federated-optional, has production bot SDKs, and — as of
2026 — has moved the historical headless-agent blockers (MSC4190 in stable v1.17). Signal is
gold-standard crypto but phone-number-bound, centralized, and hostile to bots. Rolling our own means
reimplementing device/group-key management Matrix already solved. **Never hand-write the ratchet —
link vodozemac.**

## D4 — One key per host + a per-host daemon (`safehoused`)
**Decision:** each host runs one long-lived daemon owning a single Matrix device; ephemeral agents
sit behind it over a local unix socket and hold no keys.
**Why:** collapses the ephemeral-agent key-lifecycle problem to nothing (agents aren't devices),
serializes the ratchet (single writer), and matches the threat model (machine = unit of trust).
Attribution moves to the message envelope (`from: book-agent`), not the crypto identity.

## D5 — Lightweight Rust homeserver + persistent client-SDK daemon (NOT encrypted appservice / Synapse)
**Decision:** run continuwuity (or tuwunel) with federation off; the daemon is a persistent
matrix-rust-sdk **client**, not an appservice.
**Why:** research showed the clean "wake over HTTP with E2E" appservice path is **Synapse-only**
(MSC3202/4203), while lightweight Rust servers force a persistent client-SDK bot for E2E. But our
always-on daemon (D4) already pays the "stay online" cost, so the client-SDK path is strictly better
for us: tiny footprint, no Synapse, no dependency on an unmerged/being-superseded appservice MSC.
**Corollary:** pantalaimon (the old E2E-proxy we'd have cribbed) was archived April 2026 — build the
daemon directly on matrix-rust-sdk.

## D6 — The room is the single source of truth
**Decision:** every meaningful message goes through the encrypted room, including same-host
agent-to-agent; dispatch is driven by the room event stream, not local sends.
**Why:** gives the human phone client a complete live view + remote control for free, one code path,
no dual-delivery consistency problems. At ≤20 participants the round-trip cost is negligible.

## D7 — Name: safehouse
Daemon binary: `safehoused`. Chosen for "a secure place where agents and their handlers meet."
