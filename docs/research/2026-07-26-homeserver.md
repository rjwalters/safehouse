# Research 7 — Churn watch: which homeserver? (2026-07-26)

Recheck of the lightweight-Rust-homeserver landscape, since the stack table's "continuwuity (or
tuwunel)" was provisional and this space churns hard.

## Verdict: **Tuwunel, pinned ≥ v1.8.2** (2026-07-17). Runner-up: continuwuity v26.6.2.

This **reverses** the provisional pick. The reversal happened mid-research: an initial read favoured
continuwuity on project-health grounds (15 authors vs 2, larger deployment base, better security
process) — sound reasoning, but incomplete. Both projects publish **machine-generated Complement test
results in-repo**, and reading those directly flipped the answer.

**Switch to continuwuity if** tuwunel's two-person team stops shipping, or once continuwuity
refreshes its Complement baseline and the E2E numbers come back clean.

## The evidence that flipped it

| | **Tuwunel** | **Continuwuity** |
|---|---|---|
| Baseline last updated | **2026-07-23** (current) | **2026-02-24** — *5 months stale*, predates the v26.6.x sync rewrite |
| Complement top-level | **169 pass / 35 fail** | 113 pass / 89 fail |
| `TestUploadKey` | **11/11 pass** | 8 pass / **3 fail** |
| `TestKeyChangesLocal` | **2/2 pass** | **2/2 FAIL** |
| `TestToDevice` | **3/3 pass** | 2 pass / **3 FAIL** |
| `TestDeviceListUpdates` | 8 pass / 3 fail (all `leave` cases) | **11/11 FAIL** |
| complement-crypto (E2EE interop, driven by **matrix-rust-sdk**) | **50 pass / 5 skip** | **suite not run at all** |

`TestDeviceListUpdates` is the one that matters most, and the raw count understates it —
continuwuity fails **every local-user case** (`when_local_user_joins_a_room`, `…leaves…`,
`…rejoins…`, `when_joining_a_room_with_a_local_user`). **safehouse runs federation off, so every user
is a local user.** Unreliable `device_lists.changed` means clients don't re-fetch device keys when a
device appears — a textbook undecryptable-message generator, and exactly the failure mode flagged as
the one thing that would actually break safehouse.

Tuwunel's complement-crypto passes include our exact dependency list: `TestCanBackupKeys`,
`TestBackupWrongRecoveryKeyFails`, `TestFallbackKeyIsUsedIfOneTimeKeysRunOut`,
`TestToDeviceMessagesArentLostWhenKeysQueryFails`, `TestClientRetriesSendToDevice`,
`TestFailedKeysClaimRetries`, and all five `RoomKeyIsCycled*`.

**A fresh bug found in continuwuity source** (no issue filed anywhere): `src/api/client/sync/v5.rs`
hardcodes `device_unused_fallback_key_types: None` on the **sliding-sync** path. Continuwuity added
fallback keys in v26.6.0, but Element X uses sliding sync exclusively and is therefore never told to
upload or replace a fallback key — the fix doesn't reach the client that needs it most. Tuwunel
populates the field properly.

**Honest caveats:**
- Continuwuity's baseline is stale and predates a large sync rewrite, so **some failures are likely
  already fixed — we can't tell which.** That absence of current evidence *is* the finding: with
  tuwunel you can audit E2E conformance today; with continuwuity you cannot.
- Tuwunel's complement-crypto **skips 5 subtests**, and Go marks a parent "pass" when all subtests
  skip. The skips include `TestVerificationSAS/{rust_hs1}` and
  `TestUnprocessedToDeviceMessagesArentLostOnRestart/rust`. **Do not read "0 failures" as "SAS
  verification works"** — that's exactly the untested area, and where the known open bug lives.

## ⚠️ One of our premises is now stale

`decisions.md` D5 and `design.md` §7 say encrypted appservices (MSC3202/MSC4203) are **Synapse-only**.
**Tuwunel v1.8.2 shipped both on 2026-07-17.** This does **not** change the plan — D5's actual load
-bearing reason was that our always-on daemon already pays the "stay online" cost, making the
client-SDK path strictly better, and Q-G independently found there is *no Rust appservice SDK at all*
(`matrix-sdk-appservice` is a 2022 name placeholder) plus tuwunel's appservice device path is broken
(issue #327). But the *reasoning* should be stated accurately.

## The single most valuable mitigation

**Have the daemon use classic `/sync` (sync v2), not sliding sync.**

The one open E2E bug shared by both servers lives in the **sliding-sync to-device extension**:
tuwunel#292 / continuwuity#1476, *"Verification timeout with Element X"* — same reporter, near
-identical text, *"works fine with FluffyChat or Synapse."* FluffyChat is matrix-dart-sdk; Element X
is matrix-rust-sdk. Maintainer root cause: *"the v5 to-device extension was using only the
sliding-sync `pos` and ignoring the per-extension `since` token (MSC3885) that matrix-rust-sdk
persists separately."* A fix landed before v1.6.1 but the reporter retested and the bug **persists**.

Sync v2 is spec-frozen and sidesteps both this bug **and** the MSC4186 churn expected Aug–Oct 2026.
Reserve sliding sync for Element X on the phone, where the worst case is a hang cleared by
restarting the app. Severity note: both reports say *"after restarting Element X, both users seem
properly verified"* — this is a **verification-UX hang that self-heals, not key loss**. It affects
both servers equally, so it is not a differentiator; it is just a reason to prefer sync v2.

## Project landscape

| Project | Version + date | Activity | License | Risk |
|---|---|---|---|---|
| **Tuwunel** ✅ | v1.8.2, 2026-07-17 | 100 commits/8d, **2 authors = 92%** | Apache-2.0 | **Med** — bus factor, governance |
| Continuwuity | v26.6.2, 2026-07-12 | 50 commits/13d, 15 authors | Apache-2.0 | **Med** — unverifiable E2E |
| conduwuit | v0.5.0-rc4, 2025-04-09 | **ARCHIVED 2026-05-29** | Apache-2.0 | **DO NOT USE** |
| Conduit | v0.10.12, 2026-02-12 | 1 author, life support | Apache-2.0 | High |
| Dendrite | v0.15.2, **2025-08-15** | 2 commits/3mo | AGPL-3.0 | **High — maintenance mode** |
| Synapse | v1.157.1, 2026-07-22 | Very active | **AGPLv3 + Element commercial** | Low tech / med governance |

**conduwuit is confirmed dead** — archived 2026-05-29; its README states *"Tuwunel is the ONLY
official successor to conduwuit"* (pointedly not endorsing continuwuity).

**Canonical repos — both moved.** Continuwuity: `forgejo.ellis.link/continuwuation/continuwuity`
(self-hosted Forgejo; GitHub/Codeberg are mirrors). Tuwunel: `github.com/matrix-construct/tuwunel`.

**Governance risk is real and is the main argument against tuwunel:** bus factor 2, no published
security advisories, a documented interpersonal conflict between the projects (continuwuity #849,
primary source but one side), and per the TWIM 2026-07-24 federation census **tuwunel doesn't make
the top four** while continuwuity holds 8.4% / 1,669 servers. Fewer deployments = less field
shakeout.

A widely-repeated claim that tuwunel's maintainer was *"banned by the Matrix Foundation for abusing
protocol exploits"* **could not be verified against any primary source. Treat as rumor.**

The call is reversible: same config key names, same RocksDB lineage, binary-swappable.

## Deployment

Tuwunel ships **fully static musl binaries** (x86_64 + aarch64), an apt repo, `.deb`/`.rpm`, a NixOS
module, and an OS-less container with a built-in healthcheck. Continuwuity **cannot** cross-build
static binaries and needs jemalloc + io_uring on the host plus **glibc ≥ 2.41** (unusable on anything
older than 2025-01-30). For a box you want to forget about, static musl is the right answer.

**RAM — our "64–256 MB" target was wrong.** Cache defaults scale with core count:
`default_db_cache_capacity_mb() = 128.0 + parallelism_scaled_f64(64.0)` — on a 4-core box that's
**384 MB of block cache alone**, and both servers share the formula. Best available true-working-set
datapoint is **~680 MB** (tuwunel#294, after `!admin debug trim-memory` on a 72-thread host at ~10
users; high RSS is `MADV_FREE`, not a leak). **Plan 2–4 GB and clamp the cache modifiers explicitly.**
Federation-off removes the dominant memory driver (no remote state resolution), so we should sit at
the low end. There is **no published measured footprint for either server at ~20 users.**

**`.well-known`:** `/matrix/server` is *federation* delegation — **not needed** (continuwuity actively
403s it with federation off). `/matrix/client` is client discovery, needed only if `server_name`
differs from the URL clients hit. Set `server_name = chat.example.com`, serve on that host, and you
need neither — but Element X strongly prefers well-known discovery, so serve the `client` file anyway
if the phone is in scope. **Port 8448 is federation-only — keep it closed.** TLS: neither does ACME;
put Caddy in front. Lighttpd is unsupported (mangles `X-Matrix`).

## Config

```toml
[global]
server_name   = "chat.example.com"
database_path = "/var/lib/tuwunel"
address       = ["127.0.0.1"]
port          = 8008

allow_federation       = false     # set BEFORE first boot
federate_created_rooms = false     # rooms permanently non-federating, even if re-enabled
federate_admin_room    = false
trusted_servers        = []        # default is ["matrix.org"]

allow_registration = false         # create users explicitly instead
allow_encryption   = true          # default true — do NOT touch

db_cache_capacity_mb        = 64   # do NOT let nproc pick these
db_write_buffer_capacity_mb = 16
cache_capacity_modifier     = 0.25
```

**Federation-off gotchas:** set it before first boot (both configs warn about breakage if changed
after the fact). Tuwunel never registers the federation routes; continuwuity replaces them with 403
handlers — both sound. `federate_created_rooms = false` is **tuwunel-only** and permanently pins
rooms non-federating. `allow_registration` defaults **true on continuwuity, false on tuwunel**.
Continuwuity phones home by default (`allow_announcements_check = true`); no tuwunel equivalent.

**First two users** (tuwunel has no auto-generated first-run token). These forms assume the
homeserver is **not yet running** — a bare `--execute` invocation opens the RocksDB store directly,
so it must be the only process touching it:

```bash
tuwunel --execute "users create_user alice"          # the human
tuwunel --execute "users make_user_admin alice"
tuwunel --execute "users create_user safehouse-bot"  # the daemon
```

Then log the bot in once via `/login` with `m.login.password` to mint a long-lived token.
`allow_registration` stays `false` throughout — the box is sealed at two users from the start.

### Creating a user on an already-running server

Since D15 the production homeserver runs
`tuwunel` under systemd on a dedicated EC2 host, so adding a bot account (e.g. a second host's
`safehoused-studio`) means creating a user against a *live* server — now the normal case. The
standalone `--execute` form above does not work as-is; the sequence below is **verified live
2026-07-28** provisioning `safehoused-studio` against the production homeserver:

1. **A bare `--execute` needs the config** the systemd unit otherwise supplies. Without it:

   ```text
   Error: … missing field `server_name`
   ```

   `server_name` (and friends) come from the config file; point `--execute` at it explicitly:

   ```bash
   export TUWUNEL_CONFIG=/etc/tuwunel/tuwunel.toml   # same config the systemd unit uses
   ```

2. **`--execute` cannot attach while the service holds the DB lock.** With the config set but the
   service still running, opening the store fails because the running daemon owns
   `/var/lib/tuwunel/LOCK`:

   ```text
   I/O error: While lock file: /var/lib/tuwunel/LOCK: Resource temporarily unavailable
   ```

   RocksDB is single-writer; `--execute` cannot share the store with the live server.

3. **Verified working sequence** (stop → execute → shutdown → start, ~15s downtime; clients
   reconnect automatically):

   ```bash
   sudo systemctl stop tuwunel
   sudo TUWUNEL_CONFIG=/etc/tuwunel/tuwunel.toml tuwunel \
     --execute 'users create_user safehoused-studio <password>' \
     --execute 'server shutdown'
   sudo systemctl start tuwunel
   ```

   The trailing `--execute 'server shutdown'` lets the transient process exit cleanly and release
   the lock before `systemctl start` brings the service back up.

**Zero-downtime alternative (admin room).** If the Matrix admin room has been provisioned on that
homeserver, run the same `users create_user <name> <password>` command *in the admin room* instead
— the live server executes it in-process, no stop/start and no downtime. This is conditional on the
admin room existing; the stop/execute/start sequence above is the fallback when it hasn't been set
up.

## Known risks for an E2E bot daemon

1. **Sliding-sync to-device `since`-token bug** — open on both. → **use sync v2.**
2. **Continuwuity's 11/11 local `TestDeviceListUpdates` failures** (stale baseline; may be fixed,
   unverifiable). Primary reason for the switch.
3. **Continuwuity never reports fallback-key state over sliding sync.**
4. **Tuwunel v1.7.0 erases existing one-time keys** on first read-write open. One-way. Deploy ≥1.7.0
   fresh and you never hit it.
5. **Tuwunel v1.8.0** does a one-time timestamp-index rebuild on first boot (slow start, large DBs).
6. **Tuwunel's sliding sync was rewritten three times**; ≤v1.4.4 had known message loss. **Pin ≥1.7.x**
   (we pin ≥1.8.2).
7. **`server_name` is immutable** — changing it requires a data wipe. Choose carefully.
8. **Never run `!admin users logout`** against the bot — docs warn it *"may result in data loss for
   the user, such as encryption keys."*
9. **MSC4186 churn Aug–Oct 2026** — pin the matrix-rust-sdk version.
10. **Avoid OIDC on either server.** Continuwuity #2044 (open 2026-07-25): with delegated OIDC,
    `/login` 404s and there is **no supported way to mint a bot access token**. Q-G separately found
    tuwunel's OIDC path forces a browser-visited approval URL for cross-signing re-upload.

Both trackers were swept for E2E bugs: continuwuity 132 open issues → exactly 1 tagged E2EE;
tuwunel 50 open → exactly 1. It's the same bug (#292 / #1476). Neither has any open issue on
`/room_keys`, to-device loss, or cross-signing, and the route handlers were read in source on both.
