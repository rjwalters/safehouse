# Research 4 — Q-G: headless login + cross-signing bootstrap (2026-07-26)

Source-and-spec reading pass against live repos (baibot, rust-mxlink, matrix-rust-sdk, tuwunel,
continuwuity all cloned and read at HEAD). Question: can `safehoused` become a fully functional E2E
device with **no human at a prompt**?

## Verdict: SOLVED — one operational precondition, not a technical blocker

A plain matrix-rust-sdk client bot can, with zero human interaction, create a device, upload device
keys, create and upload a full cross-signing identity, **self-sign its own device**, create a
server-side key backup, and create secret storage. The precondition is D9: the daemon must be its
**own Matrix user** with no pre-existing cross-signing identity on first run.

## The question was aimed at the wrong MSC

| MSC | Status | Relevance |
|---|---|---|
| **MSC4190** | Stable in Matrix **v1.17** (2025-12-18) | ❌ **Appservice-only.** A normal client-SDK bot gets nothing from it. This was the red herring in the original Q-G framing. |
| **MSC3967** | Stable in Matrix **v1.11** (June 2024) | ✅ **This is the mechanism.** Spec: UIA "MUST be performed for regular clients, **except** … there is no existing master signing key uploaded to the homeserver." |
| Dehydrated devices | — | ❌ Red herring. Solves the *opposite* problem (users who frequently delete their device). Not exposed on the high-level `Client` at all. |

**Confirmed in both candidate homeservers' actual route handlers:** tuwunel 1.8.2
(`src/api/client/keys/upload_signing_keys.rs`) literally logs `"Skipping UIA as per MSC3967: user had
no existing keys"`; continuwuity 26.6.2 (`src/api/client/keys.rs`, `uiaa_needed_to_upload_keys()`)
has the equivalent branch. Both also short-circuit an identical re-upload as an idempotent no-op.

## Why the daemon needs cross-signing (three distinct capabilities, don't conflate)

- **(a) Send/receive encrypted messages at all: NO, not strictly.** SDK defaults are permissive —
  `CollectStrategy::AllDevices` and `TrustRequirement::Untrusted`.
- **(b) Not be excluded by the human's client: YES, with a deadline.** Element's "exclude insecure
  devices" rollout (MSC4153) means unverified devices can't send and their messages aren't shown.
  Element's blog (2025-11-19) puts the transition at **~Oct 2026**. Crucially, "insecure" means
  *not signed by its own owner's identity* — **not** "not interactively verified by you." Self-
  bootstrap clears the bar.
- **(c) Read history / restore room keys:** needs secret storage + key backup, a separate mechanism
  from cross-signing.

## How it works in matrix-rust-sdk 0.18.0

`Encryption::spawn_initialization_task()` fires from every auth path and runs, in order:
`update_verification_state()` → `bootstrap_cross_signing_if_needed()` (gated on
`auto_enable_cross_signing`) → `backups().setup_and_resume()` → `recovery().setup()`. The bootstrap's
signatures request **signs the daemon's own device with its own self-signing key**.

**Important asymmetry:** password login (`login_username`) auto-supplies `AuthData::Password` into
that task, so bootstrap works even on a server *without* MSC3967. `restore_session()` passes `None`,
so the access-token path depends entirely on server-side MSC3967. **Prefer password login** — it's
belt-and-braces.

**The single most important function found:** `SecretStore::import_secrets()`, which
`Recovery::recover()` calls. It imports the private cross-signing keys from secret storage, then —
if a self-signing key is present — fetches `get_own_device()` and calls **`own_device.verify()`**,
self-signing the device, then re-enables backups. This is the complete answer to "how does a
*replacement* device get cross-signed with no human?"

## Disaster recovery is fully headless

Crypto store lost, account intact: delete the store and session file, cold start. Bootstrap no-ops
(identity exists server-side) but does not fail; `recover(passphrase)` then pulls the private
self-signing key from secret storage, self-signs the new device, re-enables backup, and with
`BackupDownloadStrategy::OneShot` pulls every room key back down. **This is why the recovery
passphrase is mandatory config, not optional.**

## Landmines

1. **CVE-2026-45056** (2026-06-04, CVSS 4.9) — `matrix-sdk-crypto` missing sender user-ID validation
   when decrypting Olm to-device messages carrying `sender_device_keys`; a malicious homeserver
   operator can forge to-device attribution. Affects **0.12.0–0.16.0, fixed 0.16.1+**. Directly our
   threat model. Never pin below 0.16.1; use 0.18.0.
2. **CVE-2026-45057** (`matrix-sdk-ui` < 0.16.1) — an unencrypted `m.replace` edit could override an
   encrypted original. If we ever dispatch edits to agents, don't treat an unencrypted edit as
   authentic for an encrypted original.
3. **All `EncryptionSettings` default to off** — a `..Default::default()` that drops one produces a
   daemon that works today and gets excluded in October.
4. **Store passphrase ↔ database directory are coupled.** Either without the other = daemon can't
   start. Persist together, atomically.
5. **`recovery.reset_key()` is destructive** — orphans every room key in backup. Gate behind a flag,
   default off.
6. **`Recovery` naming trap:** the SDK's "recovery key" means the *secret storage* key, not the
   spec's backup recovery key. Don't mix `Recovery` with direct `SecretStorage`/`Backups` calls.
7. **matrix-rust-sdk#5018** — "Megolm session retrieved from backup incorrectly marked as insecure,"
   listed as a blocker on Element's exclusion rollout. Would hit a daemon that restores history from
   backup, precisely in the Oct 2026 window. **Track this.**
8. **0.18.0 API churn:** `RumaApiError` is now an alias for `UiaaResponse`; `ClientApi`→`MatrixError`,
   `Uiaa`→`AuthResponse` (PR #6574). Pre-0.18 blog snippets won't compile.
9. **libolm:** no trap on a greenfield build. 0.18.0 is vodozemac throughout; libolm appears only in
   legacy import paths we never touch.
10. **tuwunel's appservice path is broken** (issue #327, unresolved since 2026-02-22) and **there is
    no Rust appservice SDK at all** — `matrix-sdk-appservice` on crates.io is a `0.0.1-reserved`
    name placeholder from 2022. D5's rejection of the appservice path is now doubly justified.

## Version basis (all verified 2026-07-26)

| Thing | Version | Date |
|---|---|---|
| matrix-sdk | **0.18.0** | 2026-06-02 |
| baibot | v1.25.0 (+36 commits) | HEAD 2026-07-26 |
| mxlink (`etkecc/rust-mxlink`) | 1.15.0 | 2026-06-02 |
| tuwunel | 1.8.2 | 2026-07-25 |
| continuwuity | 26.6.2 | 2026-07-25 |
| Matrix spec | v1.19 | — |

## Honest gap

**None of this was run against a live homeserver.** The handlers say the right thing; the spec says
the right thing; the SDK source says the right thing. A one-evening integration test — stand up the
server, run a ~60-line binary through the sequence, confirm the device reports cross-signed, then
wipe the store and confirm passphrase recovery — is the honest confirmation. **Do it before writing
daemon code around this.**

Also unverified: whether MSC4268 historic-room-key-bundles-on-invite works against
tuwunel/continuwuity (matters only if agents need pre-join history), and exactly what shield Element
renders for a self-cross-signed bot user you haven't verified (cosmetic).
