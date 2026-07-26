# Research 2 — Matrix as the foundation for bot-first E2E messaging (2026-07-26)

Deep-research pass (5 phases, 102/102 agents, 24/25 claims confirmed). Question: is Matrix the right
base for a bot-first, E2E, self-hosted, ≤20-participant, reusable agent+human messaging substrate?

## Verdict
**Yes, Matrix is a defensible foundation, and 2026 made it materially easier.** The historically hard
headless-agent blockers moved. There is one central architectural trade — which **safehouse's
daemon-per-host model sidesteps** (see [`../decisions.md#d5`](../decisions.md)).

## Key verified findings (all 3-0 unless noted)

### Homeserver
- **conduwuit is obsolete.** Two live Rust successors, both "stable" per matrix.org:
  **continuwuity** (community continuation) and **tuwunel** (former maintainer's official successor).
  Run on 64–256 MB RAM, single binary + SQLite.
  [continuwuity](https://codeberg.org/continuwuity/continuwuity) ·
  [tuwunel](https://github.com/matrix-construct/tuwunel) ·
  [matrix.org/ecosystem/servers](https://matrix.org/ecosystem/servers/)
- For ≤20 users, federation off, the choice is binary: **lightweight Rust** (footprint) **or Synapse**
  (only if you need encrypted appservices). Conduit + Dendrite are beta / maintenance-only — not
  credible here.

### Crypto
- **Settled and favorable.** libolm deprecated (Aug 2024); all core SDKs now use **vodozemac**;
  **matrix-rust-sdk is production-ready and bot-oriented.**
  [matrix.org/blog/2024/08/libolm-deprecation](https://matrix.org/blog/2024/08/libolm-deprecation/)

### Headless identity / keys
- **MSC4190** (appservices manage their own E2EE devices + reset cross-signing **without**
  user-interactive auth) landed in **stable Matrix v1.17, Dec 2025** — substantially solving the
  historically hard part. [MSC4190](https://github.com/matrix-org/matrix-spec-proposals/pull/4190)

### The wake × E2E trade (most important)
- **Encrypted appservices** (MSC3202/MSC4203) genuinely work in production (Hookshot, mautrix) — the
  ideal HTTP "wake an idle agent with E2E" — **but only against Synapse**; MSC3202 itself is still an
  open, unmerged, being-superseded (MSC4326) proposal.
  [Hookshot encryption](https://matrix-org.github.io/matrix-hookshot/latest/advanced/encryption.html)
- **The lightweight/Rust path does NOT get the appservice shortcut**: matrix-rust-sdk's appservice
  crate has no E2E (blocked on MSC3202); lightweight servers don't implement it; **pantalaimon (the
  classic E2E-proxy workaround) was archived April 2026.** On a Rust server you use a **persistent
  client-SDK bot** for E2E. [rust-sdk#228](https://github.com/matrix-org/matrix-rust-sdk/issues/228)

## Honest gaps (see [`../open-questions.md`](../open-questions.md))
Corpus was thin on **prior art** (no verified evidence on baibot / matrix-chatgpt-bot / maubot / any
A2A+Matrix work — treat as unanswered) and on a **bot-SDK E2E head-to-head** (nio/bot-sdk/maubot pain
not directly evidenced beyond "rust-sdk is the production-ready one"). Ephemeral per-run Megolm-key
receipt and lightweight-server push-wake reliability also unverified.

## How it steered safehouse
Locked the stack: **continuwuity/tuwunel + matrix-rust-sdk (vodozemac) + persistent per-host
daemon**, explicitly *not* encrypted appservices / Synapse / pantalaimon. The daemon's always-online
design turns the research's central "must stay online" downside into a non-issue.
