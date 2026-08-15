# Work Log

Chronological record of merged PRs and closed issues, maintained by the Guide
triage agent. Newest first.

### 2026-08-15

- **Issue #123** (closed): Auditor: guard false-positive on read-only python3 heredoc (worktree-write-confinement)
- **Issue #122** (closed): Auditor: guard false-positive on mktemp -d + heredoc write (worktree-write-confinement-unresolved-var)
- **Issue #120** (closed): Guard false positive: worktree-write-confinement blocks read-only python heredocs
- **Issue #119** (closed): Guard false positive: worktree-write-confinement-unresolved-var blocks mktemp+rm -rf idiom
- **Issue #117** (closed): Guard false-positive: read-only python3 heredoc denied as catastrophic worktree-write-confinement
- **Issue #116** (closed): Guard false-positive: mktemp -d smoke-test tmpdirs denied as worktree-write-confinement-unresolved-var
- **Issue #114** (closed): Guard-decision review: worktree-write-confinement-unresolved-var on mktemp-based scratch dirs — confirm keep-flagged
- **Issue #113** (closed): Guard: worktree-write-confinement misfires on quoted heredocs containing Python comparison operators (>)
- **Issue #109** (closed): Recurring DCO sign-off failures: commit.signoff knob unset + Guide's --signoff regressed
- **PR #110**: fix: set commit.signoff knob, restore Guide --signoff, wire regression test
- **Issue #105** (closed): Deduplicate the test-only tempdir() helper in mailbox.rs and egress.rs
- **PR #106**: refactor: dedupe test-only tempdir() helper into shared test_support module
- **PR #103**: feat: add config schema-version drift check and daemon version in RPC status

### 2026-08-10

- **Issue #95** (closed): loom emits a "digest" envelope type that no safehoused version knows — and unknown types are rejected rather than degraded to chat
- **PR #96**: feat(envelope): accept `digest` and degrade unknown types to chat

### 2026-08-07

- **Issue #91** (closed): PreToolUse wiring bypasses the guard-destructive.sh dispatcher, contradicting its own design doc
- **PR #92**: fix: wire PreToolUse Bash guard through the Loom dispatcher

### 2026-08-06

- **Issue #85** (closed): safehoused cannot distinguish 'room is quiet' from 'cut off' — expose last_event_received_at over the RPC socket
- **PR #87**: feat: expose sync liveness/staleness over the RPC socket
- **Issue #82** (closed): Guide role docs-maintenance commits missing --signoff again (regressed by fa2751d)
- **PR #83**: fix(loom): re-apply --signoff to Guide's docs-maintenance commit
- **Issue #79** (closed): safehoused wedges silently on a hung Matrix sync — 11h pulse outage, launchd cannot detect it
- **PR #78**: fix: block Guide docs-maintenance commits with excluded WORK_LOG entries
- **Issue #76** (closed): Guide docs-maintenance PR #75 regressed the #72 self-referential WORK_LOG fix
- **PR #74**: docs: Guide document maintenance update
- **Issue #72** (closed): Guide's docs-maintenance PR creates a self-referential churn loop with no fixed point
- **PR #73**: fix(loom): sign off Guide role's docs-maintenance commit
- **Issue #71** (closed): Guide role docs-maintenance commits fail the DCO sign-off check
- **PR #70**: docs: Guide document maintenance update
- **PR #69**: docs: Guide document maintenance update
- **PR #67**: docs: Guide document maintenance update

### 2026-08-05

- **PR #66**: docs: Guide document maintenance update
- **PR #65**: docs: Guide document maintenance update
- **PR #64**: docs: Guide document maintenance update
- **PR #63**: docs: update WORK_LOG and WORK_PLAN
- **PR #62**: docs: Guide document maintenance update

### 2026-07-31

- **PR #61**: fix(safehoused): bound per-persona mailbox growth with GC and ephemeral skip
- **Issue #60** (closed): mailbox grows without bound when personas have no consumer — 208k rows on studio, 92% expendable claim heartbeats

### 2026-07-29

- **PR #59**: fix(safehoused): room-store consistency — boot reconciliation (#57) + read-your-writes create_room (#58)
- **PR #56**: fix: make egress/mailbox test tempdir helpers collision-proof
- **PR #54**: fix(safehoused): retry sync loop on transient network failures (#52)
- **PR #51**: feat: add outbound HTTP sink with bounded retry for egress feed
- **PR #50**: build: vendor sqlite via matrix-sdk's bundled-sqlite feature
- **PR #49**: fix(install): stop --help at first non-comment line
- **PR #48**: feat(egress): allowlist + redaction + delay-buffer publisher with local sink
- **PR #47**: feat(rpc): add invite op for new-host room onboarding
- **PR #46**: fix(safehoused): flush room-key backup on SIGTERM, not just SIGINT
- **PR #45**: feat(safehouse-mcp): add one-shot operator CLI subcommands
- **PR #42**: installer: one-command safehoused host setup (#40)
- **PR #41**: feat(envelope): completion type + completion-v1 meta schema (#29)
- **PR #35**: feat(rpc): m.space support + name/alias room addressing (#27)
- **PR #34**: fix(safehouse-mcp): print usage on TTY instead of hanging silently
- **PR #32**: docs: creating a bot user on a live tuwunel (config env, DB lock, stop/execute/start)
- **Issue #58** (closed): resolve_room can't see a room the daemon itself just created until the next sync
- **Issue #57** (closed): list_rooms keeps reporting a room the bot left+forgot out-of-band
- **Issue #55** (closed): flaky test: egress::tests::buffer_is_durable_across_reopen fails under parallel execution (SQLITE_READONLY_DBMOVED)
- **Issue #53** (closed): operator: supervise the laptop daemon (launchd) instead of manual nohup
- **Issue #52** (closed): safehoused: sync loop exits fatally on transient network timeout
- **Issue #44** (closed): installer --help prints stray script code after the header comment
- **Issue #43** (closed): Daemon skips room-key backup flush on SIGTERM — supervised service stops are unclean
- **Issue #40** (closed): installer: one-command safehoused setup on a new host (build, guided bot login, supervised service, loom handoff)
- **Issue #39** (closed): New-host onboarding: room membership requires raw CS-API invite/join — daemon or CLI should bootstrap a new host into existing rooms
- **Issue #38** (closed): docs/build: Linux build deps undocumented — fresh Ubuntu 24.04 fails at link with 'unable to find library -lsqlite3'
- **Issue #37** (closed): operator: bootstrap loom-tokens pool + register loom MCP server for daemon-dispatch sweeps
- **Issue #36** (closed): operator: verify Space hierarchy renders in Element X with E2E intact (deferred AC of #27)
- **Issue #33** (closed): Operator/script room access: a read-only CLI (or dedicated persona) instead of hand-rolled socket clients
- **Issue #31** (closed): Wire outbound HTTP transport for the public completion feed
- **Issue #30** (closed): Implement egress publisher core: allowlist, redaction, delay buffer (local sink)
- **Issue #29** (closed): Design: completion-v1 public feed schema + derivation from envelope v1
- **Issue #28** (closed): Public egress feed: curated stream of agent completion events
- **Issue #27** (closed): Space (m.space) support + name-based room addressing for the multi-room fleet layout
- **Issue #26** (closed): safehouse-mcp: hangs silently when run without an MCP client — print usage on TTY
- **Issue #25** (closed): docs: creating a bot user on a live tuwunel (config env, DB lock, stop/execute/start sequence)
- **Issue #23** (closed): Backup story for the EC2 homeserver data dir (/var/lib/tuwunel)

### 2026-07-27

- **PR #21**: fix: rebuild thread-routing state from room history on boot
- **PR #20**: docs: reframe wake/spawn prose as pull-model per D16/D17
- **PR #16**: feat: add per-persona mailbox with sqlite-backed read cursor (D16/D17)
- **PR #15**: feat: thread outbound task/handoff chains and route thread replies
- **PR #12**: test: add envelope unit tests and no-network socket protocol tests
- **PR #11**: Gate inbound dispatch on unsupported envelope version
- **PR #10**: ci: add build/fmt/clippy/test workflow + DCO sign-off check
- **PR #9**: fix: post visible ack when a human addresses an unknown persona
- **PR #8**: docs: add commented example daemon config and a Running It README section
- **Issue #19** (closed): Harden Studio services to survive reboot without a GUI login (colima + LaunchDaemons) — interim before D15/#14
- **Issue #18** (closed): Reconcile design.md §6 and envelope §4/§7 'wake/spawn' language with D16/D17 (pull-not-push)
- **Issue #17** (closed): ThreadState is not rebuilt from room history after a daemon restart — §5.2 routing silently degrades
- **Issue #14** (closed): Migrate homeserver off the Studio to a dedicated always-on cloud host (D15)
- **Issue #13** (closed): Evaluate codecast (codecast-sh/codecast): competitor, complement, or ideas to borrow?
- **Issue #7** (closed): Per-agent mailbox + MCP check tools (D16/D17): pull-model delivery, not spawn/wake
- **Issue #6** (closed): Envelope §7.2: gate on unsupported envelope version
- **Issue #5** (closed): Envelope §5.1: visible ack when a human addresses an unknown persona
- **Issue #4** (closed): Envelope §2/§5.2: m.thread relations on send + thread-reply routing
- **Issue #3** (closed): Commit safehoused example config + a 'running it' README section
- **Issue #2** (closed): Tests: envelope unit tests + socket protocol integration test
- **Issue #1** (closed): CI: build, fmt, clippy, test on every PR + DCO check
