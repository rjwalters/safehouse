# Research 8 — Q-J: live integration test (2026-07-26)

First **live** run of the headless cold-start / recovery sequence. Everything before this was
source- and spec-reading (research 4); this pass ran the real stack: **tuwunel v1.8.2** (Docker,
aarch64, federation off, config from research 7) + **matrix-sdk 0.18.0** via the throwaway binary
in `spikes/qj-coldstart/`.

## Verdict: **Q-G is now solved in practice, not just on paper.** One new landmine found.

| Step | Result |
|---|---|
| Cold start: password login, MSC3967 cross-signing bootstrap, self-sign | ✅ `master/self_signing/user_signing = true`, `cross_signed_by_owner = true`, zero human interaction |
| Recovery enable with passphrase (secret storage + backup) | ✅ `backup: Enabled`, `recovery: Enabled` |
| Create encrypted room, invite human, send encrypted message | ✅ |
| Warm start: restore session from blob, all state intact | ✅ history decrypts, same device |
| **Wipe crypto store + session → cold start → `recover(passphrase)`** | ✅ **replacement device self-signed; all pre-wipe messages decrypted via OneShot backup download** |

The disaster-recovery path — the reason the recovery passphrase is mandatory config — works exactly
as research 4 predicted: bootstrap no-ops (identity exists server-side), `recover()` imports the
private cross-signing keys from secret storage, `own_device.verify()` self-signs the new device,
and `BackupDownloadStrategy::OneShot` pulls every room key back.

## 🆕 Landmine found live: flush backup uploads before exit

First disaster-recovery attempt **failed** (1 of 3 messages permanently undecryptable). Cause: a
message sent moments before process exit rides a fresh outbound megolm session whose key had **not
yet been uploaded to the server-side backup** — backup upload is a background task. Wipe the store
at that moment and the key is gone forever; no recovery passphrase brings it back.

Fix, verified live: call `Backups::wait_for_steady_state()` after sending / before shutdown. **The
daemon's shutdown path MUST flush room-key backup uploads** (and a long-running daemon should treat
an unflushed backup as not-yet-durable state). This joins the "things that will bite you" list.

One event in the first throwaway room (`!QSjli72JcjB2ogGKcU`) is unrecoverable as a result; the room
was abandoned (left + forgotten by both users) and the ladder re-run in a fresh room, all green.

## Environment notes (reproducibility)

- tuwunel v1.8.2 image loaded from the GitHub release tarball
  (`…aarch64-v8-linux-gnu-tuwunel-docker.tar.gz`) — GHCR pull was denied. Runs as container
  `safehouse-tuwunel` on `127.0.0.1:8008`, data + config + user passwords in the session scratchpad
  (`tuwunel/` subdir). Server reports spec v1.19.
- Users created via one-shot `--execute "users create_user …"` runs (server not running
  concurrently — RocksDB single-process). `users list-users` confirms both. First user auto-granted
  admin. `--execute "server shutdown"` makes one-shot admin runs clean.
- `EncryptionSettings` non-defaults confirmed live: with them set, first-ever cross-signing upload
  passes with no UIA (MSC3967) on tuwunel exactly as its route-handler source promised.
- The Q-B store/session consistency invariant is enforced in the spike (store present + session
  blob missing → refuse to start, tell operator to wipe).

## Still open (needs Robb's phone) — the last sliver of Q-J

Log in to Element as `@robb:safehouse.local`, accept the invite to **safehouse-test**, and confirm:
1. bot messages sent *after* robb's device exists decrypt (re-run the spike warm to send one);
2. what shield Element renders for the self-cross-signed, never-interactively-verified bot
   (research 4 predicted: cosmetic at worst);
3. Element's own key-backup enrollment for the human is smooth against tuwunel (Q-C).

Pre-join history is *expected* to be undecryptable for robb (messages were encrypted before his
device existed) unless MSC4268 bundles land — still unverified, still only matters if agents need
pre-join history.

The server is bound to loopback; for the phone, restart the container publishing on the LAN
(`-p 0.0.0.0:8008:8008`) and point Element (classic, not X — X wants https) at
`http://<mac-lan-ip>:8008`.
