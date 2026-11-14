# safehouse roadmap

safehouse is a FOSS, end-to-end-encrypted, bot-first messaging substrate on Matrix: a per-host
daemon (`safehoused`) holds the keys and keyless local agents reach it over a unix socket.

Status as of 2026-11-13, `main` @ `e5d6f8d`.

This file is the hand-written direction. [`WORK_PLAN.md`](../WORK_PLAN.md) is a generated snapshot
of tracker labels and is currently stale (it still lists #178 and #181, both closed), because the
docs PR that refreshes it, #223, is one of the held PRs below. Decisions are in
[`decisions.md`](decisions.md) (D1 to D19); the handoff is [`next-agent.md`](next-agent.md).

## Where it stands

Done and on `main`:

- **Core stack, verified live** (steps 1 to 5 of `next-agent.md`): cold, warm and recovery boot,
  self cross-signing, envelope v1 (`protocol/envelope-v1.md`), the unix-socket RPC, the keyless
  `safehouse-mcp` shim, and the per-persona durable mailbox. Evidence:
  `research/2026-07-26-qj-integration-test.md`.
- **Sixteen socket ops** advertised in `OPS` (`safehoused/src/rpc.rs`): `hello`, `status`, `send`,
  `send_image`, `fetch_media`, `react`, `redact`, `reactions`, `create_room`, `invite`, `leave`,
  `add_to_space`, `list_rooms`, `members`, `read`, `check`. The newest are `members` (#250, PR #251)
  and `reactions` plus an `undecryptable` count on `read` (#252, PR #253).
- **Completion feed egress** (`safehoused/src/egress.rs`, D18): off by default, per-room opt-in,
  mandatory redaction, outbound only.
- **Voice notes to text** (`safehoused/src/transcribe.rs`, #200), with stored transcripts served by
  `read` (#231, PR #232). Local subprocess only.
- **Redaction notifications** through `check` mailboxes (#229, PR #230), and `thread_root` on `read`
  rows (#224, PR #226).
- **Config secrets by file reference** and a redacting `--print-config` (#215, PR #216).
- **Sealed or federated mode in config**: `homeserver_mode` with a mandatory `invite_allowlist` when
  federated (#198). Built, not yet run against a federated server (#195).
- **Dependencies current**: `matrix-sdk` 0.19 (#242, PR #245); TLS, transport, ctutils and Tokio
  advisories patched (#240, #247).
- **Operations**: one-command host installer (`scripts/install.sh`), provision and deprovision
  scripts, a scripted room cutover with a runbook (`runbooks/room-cutover.md`), and a portable agent
  skill with its installer (`skills/safehouse/`, `scripts/install-skill.sh`).
- **CI** (`.github/workflows/ci.yml`, `dco.yml`): rustfmt, clippy with `-D warnings`, build, test,
  the room-cutover dry run, the skill installer test, and a sign-off check on every commit.

## The constraint on everything below

D11 requires `Signed-off-by` on every commit, and its 2026-10-23 amendment says an agent cannot
attest: an agent-authored PR lands only through a replacement commit that the maintainer authors
and signs. So agent work here can be built and reviewed, but it does not merge on its own. All five
open PRs are in that state. #193 holds the open question of whether anything mechanical may change
this; until it is answered, each merged change costs one maintainer action.

## Now

Can start today. None has an issue yet; drafts exist for each.

1. **Advisory check for the locked dependency graph.** Three advisory rounds were found and fixed by
   hand (#240, #242, #247) and CI has no advisory job. Done when a script runs a RustSec advisory
   check against `Cargo.lock`, a CI job calls it without gating unrelated PRs, and the README says
   how to run it locally. Blocked by: nothing.
2. **Reconcile the handoff docs with `main`.** `CLAUDE.md` says "decisions D1–D17" and
   `next-agent.md` says "all seventeen" while `decisions.md` ends at D19; `next-agent.md` and the
   README stack table give the pin as `matrix-sdk` ≥ 0.18.0 while `Cargo.toml` requires 0.19;
   `open-questions.md` still lists the Oct 2026 device-exclusion deadline as upcoming while the
   README records it as cleared. Done when those statements agree with `Cargo.toml`,
   `decisions.md` and the README. Blocked by: nothing.
3. **Close the "track it" on matrix-rust-sdk#5018.** `open-questions.md` Q-B asks for that upstream
   issue to be tracked through the Oct 2026 window and records no outcome. Done when Q-B states the
   upstream issue's current state with a link, and says whether the recovery-from-backup path in
   `safehoused` is affected at `matrix-sdk` 0.19. Blocked by: nothing.

## Next

Unblocked once a named item lands.

- **Socket-op reference table in the README, with a test that it lists every op in `OPS`** (#255).
  Built in PR #257. Done when the table and the `readme_lists_every_advertised_op` test are on
  `main`. Waits on: a signed replacement commit, then code review (not yet done, CI was red on
  sign-off only).
- **Mark `members[].display_name` as untrusted in the shim** (#254). Built in PR #256, reviewed with
  no code findings. Done when `safehouse-mcp members` output carries the same marking `list-rooms`
  gives room names. Waits on: a signed replacement commit.
- **Fail provisioning when the admin room sends no ack** (#235). The `!admin` prefix half landed in
  PR #236; the remainder is that `scripts/provision-host.sh` and `scripts/deprovision-host.sh` exit
  non-zero when no ack arrives. Done when a missing ack is an error, with a test. Waits on: #235
  being narrowed to that remainder, and PR #237 (which overlaps #236) being replaced or reconciled.
- **Let validated `data-mx-color` on `<span>`/`<font>` through outbound Markdown** (#227). Built in
  PR #228, CI green. Done when a coloured span survives into `formatted_body` and anything else is
  still escaped. Waits on: a signed replacement commit.
- **Refresh `WORK_LOG.md` and `WORK_PLAN.md`** (PR #223). Waits on: a signed commit on that PR.

## Later

Direction, each with the question to answer first.

- **Move voice-note transcription off the sync loop.** Today `on_message` awaits it inline, so one
  note can stall every room for up to about three times `timeout_seconds` (documented in
  `transcribe.rs`). Question: how is per-room message order into the mailbox kept if the transcript
  arrives after later events?
- **Access-token auth instead of a password** (Ask 1b on #215). Question: can the headless
  cross-signing bootstrap (D10), which uses `AuthData::Password` at cold start, work from a token? A
  spike against a disposable homeserver comes first.
- **CLI and MCP coverage of the newer ops.** `safehouse-mcp` has subcommands for `read`, `send`,
  `check`, `list-rooms`, `status`, `invite`, `leave` and `members`; `react`, `redact`, `reactions`,
  `send_image` and `fetch_media` are reachable only over the socket. Question: which of these should
  the shim expose, and with which guards?
- **Push wake through Claude Code Channels** (a v1 item in `next-agent.md`, D16). Question: has the
  Channels protocol contract stabilised enough to build an optional waker on?
- **Wire the first real agent and retire the copy-paste relay** (step 6 of `next-agent.md`; the
  README's "Next"). Question: which two-agent handoff is the acceptance case, and what observable
  result closes it?
- **A first tagged release.** Both crates are `0.1.0` and the repo has no tags. Question: what does
  a release promise about envelope v1 and the socket ops?

## Blocked and waiting

- **Maintainer decision, #193:** may a repo-local hook add `Signed-off-by` to agent commits, or does
  the D11 amendment stand as written? Every open PR waits on this or on a signed replacement.
- **Live verification that needs a person:** voice-note transcription end to end with measured
  whisper.cpp latency (#204), and the four-runtime skill check (#184). Both are held for the
  maintainer; an agent cannot run either.
- **Federated homeserver run** (#195): needs a bot account on a federated server, provided by the
  maintainer.

The tracker has no unbuilt `loom:issue` today: #254, #235 and #227 each already have a PR. New work
comes from the three Now items once they are filed.

## Not planned

- A TCP listener or an in-process plugin ABI (D8).
- An appservice or Synapse-based design (D5); sliding sync in the daemon (D13); hand-written
  cryptography (D3); code copied from `baibot` or `mxlink` (D8).
- Hosted transcription: audio stays on the host (`design.md` §4.1.3).
- A bot exemption or blanket attestation in the sign-off check (D11).
