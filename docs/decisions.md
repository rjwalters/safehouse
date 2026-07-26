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
**Decision:** run a lightweight Rust homeserver with federation off; the daemon is a persistent
matrix-rust-sdk **client**, not an appservice. *(The specific server was provisional here and is now
settled in **D12: tuwunel ≥ v1.8.2**.)*
**Why:** research showed the clean "wake over HTTP with E2E" appservice path is **Synapse-only**
(MSC3202/4203), while lightweight Rust servers force a persistent client-SDK bot for E2E. But our
always-on daemon (D4) already pays the "stay online" cost, so the client-SDK path is strictly better
for us: tiny footprint, no Synapse, no dependency on an unmerged/being-superseded appservice MSC.
**Corollary:** pantalaimon (the old E2E-proxy we'd have cribbed) was archived April 2026 — build the
daemon directly on matrix-rust-sdk.

**⚠️ Amended 2026-07-26 — the "Synapse-only" premise is now stale, but the decision stands.**
**Tuwunel v1.8.2 shipped MSC3202 and MSC4203 on 2026-07-17**, so encrypted appservices are no longer
Synapse-only. The decision is unchanged because its *load-bearing* reason was never exclusivity — it
was that our always-on daemon (D4) already pays the "stay online" cost, making the client-SDK path
strictly better for us. Round-2 research added two independent reinforcements: **there is no Rust
appservice SDK at all** (`matrix-sdk-appservice` on crates.io is a `0.0.1-reserved` name placeholder
from 2022, and no such crate exists in the matrix-rust-sdk workspace), and **tuwunel's appservice
device path is broken** (issue #327, unresolved since 2026-02-22). So the appservice path remains
closed to us in practice — just not for the reason originally written down.

## D6 — The room is the single source of truth
**Decision:** every meaningful message goes through the encrypted room, including same-host
agent-to-agent; dispatch is driven by the room event stream, not local sends.
**Why:** gives the human phone client a complete live view + remote control for free, one code path,
no dual-delivery consistency problems. At ≤20 participants the round-trip cost is negligible.

## D7 — Name: safehouse
Daemon binary: `safehoused`. Chosen for "a secure place where agents and their handlers meet."

---

*The decisions below were made 2026-07-26 after a second research round (Q-G/Q-H/Q-I). Provenance:
`research/2026-07-26-headless-login.md`, `-mcp-and-channels.md`, `-licensing.md`.*

## D8 — License: Apache-2.0, and no `mxlink` dependency
**Decision:** the repo is **Apache-2.0**. `safehoused` is written directly against matrix-rust-sdk;
we do **not** take a dependency on `mxlink`, and we copy no code from `baibot` or `mxlink`.

**Why:** the premise that forced this question was wrong. Our docs recorded both baibot and mxlink as
AGPL-3.0; in fact **mxlink is LGPL-3.0** (and lives at `etkecc/rust-mxlink` — the URL we had was a
404) while only **baibot** is AGPL. That inverts the analysis two ways:

- LGPLv3 §4 lets you convey a combined work "under terms of your choice," so depending on mxlink
  would *not* have forced copyleft. The library was available under either license choice.
- baibot is an **application**, not a library. There was never any AGPL code we'd link. So choosing
  AGPL would have unlocked **zero** code reuse while costing adopters.

The work delta between Apache-2.0 and AGPL is therefore zero — they are not on the reuse axis.
Given that, Apache-2.0 wins on: matching our actual dependency tree (matrix-rust-sdk and vodozemac
are Apache-2.0, ruma is MIT); an express irrevocable patent grant (MIT has none); and one-way
compatibility — anyone wanting a copyleft safehouse can fork Apache→AGPL, but AGPL→permissive is
impossible without every contributor's consent. Apache-2.0 preserves every downstream option.

We also declined AGPL because its leverage is against **SaaS re-hosting**, and safehoused is a
per-host daemon on the user's own machine (D4). There is no SaaS moat to defend; we'd pay the full
adoption cost against a threat our architecture doesn't have.

**Why no mxlink dependency**, separately from the license: it is a thin convenience layer over
`matrix-sdk`, and what we'd get from it is ~400 lines of fully-specified code (client builder,
cold/warm start, session blob at rest, consistency guard, error taxonomy). Against that, LGPL §4(b)
requires shipping GPLv3 + LGPLv3 texts in every release artifact including Docker images; §4(d)(0)'s
"relink with a modified library" obligation has **no settled meaning for statically-linked Rust** and
no shared-library escape hatch exists; and corporate scanners flag any LGPL node in a Rust tree.
Decisive engineering reason: this is the daemon's **boot and key-custody path** — the most
security-critical code in the project and the most likely to brick the daemon if subtly wrong. We
want to understand it line by line. We keep mxlink's hard-won *invariants* (purge guard,
transient-vs-permanent backoff, three-way recovery error match); those are ideas, not expression.

**Decision rule while coding:**

| Action | Consequence |
|---|---|
| `cargo add mxlink` | LGPLv3 §4 combined work. Legal, but taxes every release and every adopter. **Don't.** |
| Copy code from **mxlink** | That code stays LGPL-3.0. **Don't.** |
| Copy code from **baibot** | **safehoused becomes AGPL-3.0, virally, across the whole binary.** **Never.** |
| Read either, take notes, write your own | Clean — ideas and methods of operation aren't copyrightable. **This is the path.** |
| Copy from matrix-rust-sdk `examples/` | Apache-2.0. Fine. |

**Corollary — now a licensing invariant, not just a security one:** `design.md` §4.1's "never a
network port" must stay literally true, and there must never be an in-process plugin ABI for agents.
The unix-socket + serialized-envelope boundary is what keeps third-party agents legally separate
works (FSF tests both the *mechanism* and the *semantics* of communication). A `--listen` TCP flag or
a `dlopen` agent interface would collapse that analysis.

## D9 — `safehoused` is its own Matrix user, never a device on the human's account
**Decision:** the daemon registers and owns a dedicated account (`@safehoused:host`). It is **not** a
second device on the human's Matrix account.

**Why:** this is what makes the entire headless story work. The daemon self-bootstraps its
cross-signing identity, which the spec only permits without user-interactive auth when **no master
key exists yet** for that user (MSC3967). A fresh bot account satisfies that; the human's account
does not. If safehoused were a device on the human's account: the identity already exists, so
bootstrap silently no-ops and the device stays unsigned, and the only headless fix would be handing
the daemon the human's **personal** secret-storage passphrase — which would let it decrypt the
human's entire history and impersonate them. Separate bot user is both simpler and dramatically
better for blast radius, which matters for a design whose whole premise is that agents hold no keys.

## D10 — Headless self-bootstrap; no human verification step
**Decision:** the daemon cross-signs **itself** at first login. We drop the "human verifies the
daemon's device once from Element" step from the design (`design.md` §4.1/§4.3 as originally
written). The only human action is creating the bot account and handing over a password.

**Why:** MSC3967 (stable since Matrix v1.11, June 2024 — **not** MSC4190, which is appservice-only
and was a red herring) exempts a user's first-ever cross-signing upload from user-interactive auth.
matrix-rust-sdk does this automatically on login when `auto_enable_cross_signing` is set, and the
bootstrap uploads a self-signature over the daemon's own device. That self-signature is exactly the
bar MSC4153 / Element's "exclude insecure devices" cares about — **owner-signed, not
interactively-verified**. Confirmed in both candidate homeservers' route handlers. Corroborated
empirically: baibot ships zero verification code, and two unrelated projects independently reached
the same secret-storage-based self-signing approach.

**Consequences that are now v0-mandatory, not v1 niceties:**
- `EncryptionSettings` defaults are **all off**. `auto_enable_cross_signing` and
  `auto_enable_backups` must be set explicitly, or the daemon works today and gets excluded when
  Element's rollout lands (~Oct 2026).
- The **recovery passphrase is required config, not `Option`**. It is the *only* headless path back
  after a crypto-store loss: `bootstrap_cross_signing_if_needed` no-ops once a server-side identity
  exists, so without the passphrase a replacement device is permanently unsigned.
- `recovery_reset_allowed` defaults to **false**. The reset fallback orphans every room key already
  in backup, so a passphrase typo would silently destroy history.
- Pin `matrix-sdk` **≥ 0.18.0**. CVE-2026-45056 (fixed 0.16.1) is a sender-binding gap in to-device
  attribution — directly our threat model, since the daemon decrypts to-device traffic and dispatches
  it to agents that act on it.

Optional and cosmetic: a one-time human user-to-user verification from Element gets a green check on
`@safehoused:host`. Operator confidence only. If we ever implement the daemon side, auto-`confirm()`
must be gated behind an explicitly operator-opened window — otherwise we'd auto-accept a MITM'd SAS.

## D11 — DEFERRED: CLA vs. DCO
**Status:** consciously deferred, with a deadline.
**Deadline:** must be resolved **before merging any outside contribution.** Once outside work is
merged under bare inbound=outbound Apache-2.0, relicensing becomes impossible without tracking down
every contributor. DCO is simpler and community-friendly but forecloses relicensing; a CLA preserves
dual-licensing/commercial options but deters casual contributors. Zero cost while Robb is sole
author; the only real risk is forgetting when the first PR arrives.

## D12 — Homeserver: **tuwunel**, pinned ≥ v1.8.2 (reverses the provisional pick)
**Decision:** run **tuwunel** ≥ v1.8.2, federation off. Runner-up continuwuity v26.6.2.
**Switch trigger:** if tuwunel's two-person team stops shipping, or once continuwuity refreshes its
Complement baseline with clean E2E numbers.

**Why:** the stack table said "continuwuity (or tuwunel)" provisionally. On project health
continuwuity looks better — 15 authors vs 2, 8.4% of the federation census vs tuwunel not making the
top four. But both projects commit **machine-generated Complement results in-repo**, and those decide
it on the axis we actually care about:

- Continuwuity's baseline is **5 months stale** (2026-02-24) and fails **every local-user
  `TestDeviceListUpdates` case**, plus `TestKeyChangesLocal` 2/2 and 3 `TestToDevice` subtests. We run
  **federation off, so every user is a local user.** Unreliable `device_lists.changed` is a textbook
  undecryptable-message generator — the exact failure mode we said would break safehouse.
- Tuwunel's baseline is **current** (2026-07-23) and it is the **only one of the two that runs
  complement-crypto**, the E2EE interop suite driven by real matrix-rust-sdk clients — 50 pass,
  including `TestCanBackupKeys`, `TestFallbackKeyIsUsedIfOneTimeKeysRunOut`, and
  `TestToDeviceMessagesArentLostWhenKeysQueryFails`.

Caveat kept honest: continuwuity's stale baseline predates a large sync rewrite, so some of those
failures are probably already fixed — **we can't tell which, and that unverifiability is itself the
finding.** Tuwunel also ships static musl binaries (continuwuity can't cross-build static, and needs
glibc ≥ 2.41), which matters for a box you want to forget about.

Accepted risk: tuwunel's bus factor is **2**, it publishes no security advisories, and it has ~3×
fewer deployments. Reversible — same config keys, same RocksDB lineage, binary-swappable.

**Correction to the stack table:** the "64–256 MB RAM" figure was wrong. Cache defaults scale with
core count (`128 MB + parallelism_scaled(64 MB)` — 384 MB of block cache on a 4-core box). Best
true-working-set datapoint is ~680 MB. **Plan 2–4 GB and clamp the cache modifiers explicitly.**

## D13 — The daemon uses classic `/sync` (sync v2), not sliding sync
**Decision:** `safehoused` syncs with **sync v2**. Sliding sync is for Element X on the human's phone
only.

**Why:** the one open E2E bug shared by both candidate homeservers lives in the **sliding-sync
to-device extension** (tuwunel #292 / continuwuity #1476 — same reporter, *"works fine with
FluffyChat or Synapse"*; FluffyChat is matrix-dart-sdk, Element X is matrix-rust-sdk). Root cause per
the maintainer: the v5 to-device extension used only the sliding-sync `pos` and ignored the
per-extension `since` token that matrix-rust-sdk persists separately. A fix landed but the reporter
retested and **the bug persists.**

Sync v2 is spec-frozen. Choosing it sidesteps that bug *and* the MSC4186 churn expected Aug–Oct 2026.
The blast radius is asymmetric and that's the point: on the phone, the worst case is a verification
hang cleared by restarting the app; in the daemon, to-device loss means missing room keys.

