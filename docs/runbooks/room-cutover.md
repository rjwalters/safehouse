# Runbook: room-DAG death cutover

**When to use this:** a narration room's homeserver-side event DAG has wedged —
every send (from any client) fails the same way:

```
500 M_UNKNOWN "cannot create a non-create event in a room with no forward extremities"
```

No client-side fix exists for this: the corruption is in tuwunel's own
forward-extremities tracking for that room, not in anything a Matrix client
can repair. It has already happened twice (see the registry below). The
response is to cut the fleet over to a brand-new replacement room — scripted
by [`scripts/room-cutover.sh`](../../scripts/room-cutover.sh) so the third
time is minutes of operator time, not days.

Root-cause forensics for *why* the homeserver corrupts a room's DAG (server
logs, DB state, an upstream tuwunel report) is tracked separately — see
issue #173. This runbook is the mitigation path, independent of whether or
when the root cause is ever fixed upstream.

## Dead-room registry — never reuse or alias-resolve these

| Room | ID | Died | Error signature |
|------|----|------|------------------|
| v1 | `!FZ8oYmGnszTpYdXUGz` | 2026-08-17 | `500 M_UNKNOWN "cannot create a non-create event in a room with no forward extremities"` |
| v2 | `!MQP2aSTA5uDu7czxYZ` | 2026-09-04 | `500 M_UNKNOWN "cannot create a non-create event in a room with no forward extremities"` |

**Rule: every consumer addresses rooms by ID only, never by name or alias.**
Both dead rooms keep the same display name (`loom-fleet`) as every room that
replaced them, and a dead room can never be left cleanly (there is nothing to
leave — the DAG itself is what's broken) — so name/alias resolution can
never again distinguish a live room from a dead one on this homeserver. Add
each newly-dead room to this table (ID, death date, exact error text) as soon
as it's confirmed dead, before doing anything else.

## Running the cutover

```bash
export SAFEHOUSED_SOCKET=/path/to/safehoused.sock   # an already-onboarded host's daemon
export SAFEHOUSE_PERSONA=operator                    # a persona allowlisted on that daemon

scripts/room-cutover.sh \
  --room-name loom-fleet-v4 \
  --invite "@safehouse-bot:example.com @safehoused-worker1:example.com @safehoused-worker2:example.com"
```

This creates the replacement room (always encrypted — the same `create_room`
op every other room on the fleet uses) and invites every identity you passed
in one call, then prints the new room ID plus the full rollout plan below.
Every invited bot's own `safehoused` auto-joins on its next sync.

**Live runs require confirmation.** Before calling `create_room` for real, the
script prints the exact socket/persona/room name it is about to act against
and requires you to type the room name back — this is irreversible (Matrix
has no room-deletion RPC) and an already-exported `SAFEHOUSED_SOCKET` pointing
at production is easy to miss in an otherwise-normal dev shell. Pass `--yes`
to skip the prompt for scripted/non-interactive use; without a tty on stdin
and without `--yes`, the script refuses to proceed rather than run
unattended against a live socket.

**Try it dry first.** `scripts/room-cutover.sh --dry-run` (with or without
`--room-name`/`--invite`) renders the exact JSON-RPC frame that would be sent
and the full rollout plan, without opening the daemon socket at all — this is
also what CI exercises on every change to the script. Before using it in
anger against the real fleet, the one live-fire step that is *not* a CI
gate — because it needs a reachable daemon socket — is a single scratch-room
dry run: create one throwaway room against your own daemon
(`scripts/room-cutover.sh --room-name loom-cutover-scratch-test --invite ""`
against a socket you control, not the fleet's), confirm it appears in
`safehouse-mcp list-rooms`, and discard it — there is no scripted "undo" for
`create_room` (Matrix has no room-deletion RPC; the room just sits unused
until someone leaves it by hand in Element).

If `create_room` fails, nothing was created — a malformed Matrix user ID in
`--invite` aborts the whole call before the room is made (safehoused
validates every invite entry up front), so just fix the list and re-run.

## Rollout — per-host env layer

Every host addresses the room by ID only (`LOOM_SAFEHOUSE_ROOM`, env layer
beats config). The layer differs by supervisor:

- **launchd (macOS):** a plain `restart` does not re-read the plist. Re-render
  it first (`loom-daemon-start.sh`, or hand-edit `EnvironmentVariables`), then
  reload with `loom-daemon restart --reload-supervisor` (if the build has it)
  or fall back to `launchctl bootout` + `launchctl bootstrap` by hand.
  **No drain needed first** — a bootout does not kill in-flight sweeps
  (they reparent to `launchd`).
- **systemd (Linux):** drop-in file + `daemon-reload`, then
  `loom-daemon restart --drain` — **always drained here**, since a systemd
  stop job reaps the whole cgroup including in-flight sweeps, unlike launchd.

See `scripts/room-cutover.sh`'s own printed output for the exact commands —
it renders them with the real new room ID substituted in, so there is no
copy-paste transcription step.

## Re-enable order

1. Create the room + invite every identity. Verify with `safehouse-mcp
   list-rooms` on at least one host that each invited bot auto-joined.
2. Roll out `LOOM_SAFEHOUSE_ROOM` + `LOOM_SAFEHOUSE_RECONCILE_MAX_AGE_SECS`
   (see below) on every **consumer** host first — nothing is lost by a
   consumer being ready before any publisher sends into the new room.
3. Update the `[egress]` `rooms` allowlist and restart `safehoused` on every
   **publisher**.
4. Roll out `LOOM_SAFEHOUSE_ROOM` on every **publisher** host last, one at a
   time — watch each daemon's log for its first successful publish into the
   new room before moving to the next host.
5. Confirm end-to-end: a real `completion` (not just a `chat`) lands in the
   new room and reaches every downstream consumer.

## Duplicate-burst mitigation

A publisher that was dark while the old room was wedged re-narrates its
whole in-window reconciliation backlog the moment it reconnects into the new
room. The lookback window defaults to **7 days** (`604800` seconds) — far
more than a typical outage, which is exactly what makes a multi-day outage
like #169's produce a large duplicate burst on reconnect.

**Set `LOOM_SAFEHOUSE_RECONCILE_MAX_AGE_SECS=3600` (1 hour) on every
publisher before re-enabling it.** This is the recommended value — bounding
the window to something on the order of the outage-detection latency, not
the outage length itself, is what keeps the burst small regardless of how
long the room was actually dead. (A more conservative `14400`, 4 hours, is
also acceptable — the goal is "well below the 7-day default," not this exact
number.) Combine with dedup-state union across peers if any single publisher
still produces a burst larger than expected after the window is bounded.

## Traps (from #169's incident — read before you hit them)

1. **`loom-daemon restart --reload-supervisor` bootstraps the
   already-installed plist — it does not re-render it.** A brand-new env var
   (like this cutover's new `LOOM_SAFEHOUSE_ROOM` value) only takes effect if
   the plist was re-rendered with it first. Skip the re-render and the
   reload "succeeds" while the daemon quietly keeps running against the old
   plist's env — i.e. still pointed at the dead room. Always verify after any
   launchd reload:
   ```bash
   launchctl print gui/$(id -u)/com.rjwalters.loom-daemon | grep LOOM_SAFEHOUSE_ROOM
   ```
2. **Older daemon builds predate `--reload-supervisor` entirely** (seen at
   0.18.121 during #169). For those hosts the manual
   `launchctl bootout` + `launchctl bootstrap` sequence is not a fallback —
   it is the only path.
3. **Daemon logs rotate on restart, and the post-restart claim-reconciliation
   burst can trip the forge rate-limit breaker for up to 15 minutes.** A wave
   of failures in that window right after a restart is expected backoff, not
   a new incident — don't start a second investigation on top of the
   cutover. Capture any log evidence you still need *before* restarting,
   since it won't be there after.

## See also

- [`scripts/room-cutover.sh`](../../scripts/room-cutover.sh) — the script
  this runbook documents.
- [`scripts/create-claims-room.sh`](../../scripts/create-claims-room.sh) — the
  sibling one-shot room-provisioning script for the unencrypted claims room
  (different carve-out, different mechanism — see its own header comment).
- [`docs/protocol/envelope-v1.md`](../protocol/envelope-v1.md) — the wire
  format; the daemon-socket RPC (`create_room`, `invite`, …) is documented in
  `safehoused/src/rpc.rs` and README "Scripting the socket".
- Issue #172 (this runbook + script) and issue #169 (the v2 incident this was
  built from) and issue #173 (server-side forensics, tracked separately).
