# Work Log

Chronological record of merged PRs and closed issues, maintained by the Guide
triage agent. Newest first.

### 2026-10-08

- **PR #221**: safehoused: react and redact (own events) ops
- **Issue #220** (closed): safehoused: react and redact (own events) ops

### 2026-10-03

- **PR #217**: docs: new-thread voice notes cannot address a persona (#206)
- **Issue #206** (closed): envelope §5.1: a transcribed voice note can never explicitly address a persona (the 🎙 prefix defeats the leading-@token match)
- **Issue #192** (closed): Guard friction: worktree-write-confinement/rm-scope-unresolved-var deny a function-scoped mktemp+rm/redirect idiom (read_ci_checks, PR #102, 2026-08-18..20)

### 2026-10-02

- **PR #216**: safehoused: *_file credential references and redacting --print-config (#215)
- **Issue #215** (closed): config.toml holds four cleartext secrets with no credential-reference or token-auth option, and no redaction-safe way to read it

### 2026-10-01

- **PR #214**: feat(safehoused): describe attachments; fetch_media serves one on request
- **PR #213**: Sanitize Markdown link/image URL schemes in envelope rendering
- **PR #207**: feat(safehoused): send_image op — post an image as m.image
- **PR #211**: docs(config): trim with ffmpeg -t in the whisper wrapper example, not whisper-cli -d
- **PR #199**: feat(safehoused): render envelope bodies' Markdown into formatted_body, escaped
- **PR #210**: docs(safehoused): state transcription's real resource bounds
- **PR #208**: feat(safehoused): leave a room when alone; add a leave socket op (#201)
- **PR #203**: feat(safehoused): transcribe m.audio voice notes locally before envelope synthesis
- **PR #202**: feat: refuse to boot a federated homeserver without an invite allowlist
- **PR #197**: feat: surface Matrix addressing metadata on check; send thread_root (#194)
- **PR #196**: feat(safehoused): make the unknown-persona ack switchable (#195)
- **Issue #212** (closed): markdown_html: filter link and image URLs (javascript:, remote <img>) instead of relying on clients
- **Issue #209** (closed): transcribe: whisper.cpp -d pads short audio — the documented '-d {max_millis}' makes every note cost max_seconds
- **Issue #205** (closed): safehoused: transcription's resource bounds are weaker than documented (sync-loop stall, post-hoc size cap)
- **Issue #201** (closed): Leave a room when the daemon is its only member; add a leave socket op
- **Issue #200** (closed): Transcribe voice notes (m.audio) to text for agents, locally (whisper.cpp)
- **Issue #198** (closed): Add `homeserver_mode` config (sealed/federated): mandatory invite_allowlist + configurable persona-ack
- **Issue #194** (closed): Surface Matrix addressing metadata on mailbox entries (mentions, pills, reply-to, thread root)

### 2026-09-30

- **PR #182**: skills: portable safehouse agent skill + installer (#181)

### 2026-09-29

- **PR #191**: fix: bound safehouse_check replies by size, not just row count (#190)
- **PR #189**: fix: cap safehouse_check backlog by default, signal more_available
- **Issue #190** (closed): safehouse_check default cap is count-based, not size-based — 200 envelopes may still exceed a bounded client's tool-output budget
- **Issue #188** (closed): safehouse_check with no --limit returns unbounded backlog that exceeds MCP client token limits

### 2026-09-27

- **PR #186**: fix(shim): mark and flatten remote-authored room name in list_rooms
- **Issue #185** (closed): list-rooms reply is unfenced, but m.room.name in it is remote-authored untrusted text

### 2026-09-26

- **PR #183**: feat(shim): fence untrusted room content, refuse secret bodies, add invention firewall

### 2026-09-18

- **Issue #176** (closed): Clean up leftover test room loom-room-cutover-scripttest-DELETE-ME on production homeserver
- **PR #175**: feat: script the room-DAG-death cutover (room-cutover.sh)
- **Issue #172** (closed): Room-DAG death is now a recurring failure shape: capture server-side forensics for upstream, and script the room cutover so the third wedge is minutes not days

### 2026-09-15

- **Issue #94** (closed): Host onboarding needs a human on the homeserver, which caps the fleet at the rate an operator can mint Matrix accounts — blocks dynamic scale-out to hundreds of hosts
- **Issue #112** (closed): Guard decision: 'worktree-write-confinement' denies a read-only python heredoc (no writes at all)
- **Issue #24** (closed): Retire the Studio rollback backup once the EC2 homeserver is proven stable
- **Issue #108** (closed): Guard decision: 'worktree-write-confinement-unresolved-var' denies rm -rf on a mktemp-scoped tempdir
- **PR #177**: feat: unattended host onboarding via Matrix admin-room automation (#94)
- **PR #174**: chore(repo-skills): update installed Repo Skills to 0.11.12
- **Issue #159** (closed): verdict-staleness-guard.sh errors on every run: gh pr view has no 'merged' JSON field

### 2026-09-11

- **Issue #101** (closed): Unencrypted claims room + client-side provisioning parity — E2EE black-holed the fleet's peer-claim channel

### 2026-09-09

- **Issue #169** (closed): Narration room v2 rejects all sends: 500 M_UNKNOWN 'no forward extremities' — peer-coordination liveness unknowable

### 2026-09-06

- **PR #171**: fix: normalize operator-premise state before hashing (CLOSED/MERGED churn)
- **Issue #170** (closed): dep-recheck-fingerprint.sh operator-premise: CLOSED vs MERGED state string causes spurious hash churn

### 2026-09-02

- **PR #167**: fix(check-promotion-landed): parse tolerance-window timestamps portably on BSD/macOS date
- **Issue #166** (closed): check-promotion-landed.sh's tolerance-window date math silently degrades on macOS/BSD date
- **PR #165**: fix: check-promotion-landed.sh timeline comparison is direction-insensitive within a tolerance window
- **Issue #164** (closed): check-promotion-landed.sh: timeline comparison is backwards, misdiagnoses legitimately-parked promotions as MISMATCH

### 2026-08-23

- **PR #160**: fix: replace invalid gh --json field 'merged' with 'mergedAt'
- **Issue #158** (closed): verdict-staleness-guard.sh: 'merged' is not a valid gh pr view JSON field, breaks entire Stale-Verdict Sweep
- **Issue #157** (closed): verdict-staleness-guard.sh fails on every invocation: invalid gh --json field 'merged'

### 2026-08-20

- **PR #156**: egress: gate the public completion feed on repo visibility (fail closed on an absent tag)
- **Issue #155** (closed): egress: gate the public completion feed on repo visibility (fail closed on an absent tag)

### 2026-08-17

- **PR #154**: fix: add ThrottleInterval to macOS LaunchAgent plist
- **Issue #153** (closed): macOS LaunchAgent has no KeepAlive — a clean exit on a transient sqlite failure left safehoused down 24h (parity with systemd Restart=always)
- **PR #151**: refactor: deduplicate completion_meta() test helper into test_support
- **Issue #149** (closed): Deduplicate identical completion_meta() test helper (egress.rs + rpc.rs)

### 2026-08-16

- **PR #147**: refactor: consolidate exponential-backoff formula into shared backoff module
- **Issue #146** (closed): Consolidate duplicated exponential-backoff formula (egress.rs + main.rs)
- **Issue #144** (closed): Guard false positive: worktree-write-confinement denies a read-only python3 heredoc with no file writes
- **Issue #143** (closed): Guard false positive: worktree-write-confinement-unresolved-var denies mktemp-scoped scratch dirs
- **PR #142**: refactor: consolidate test-only Envelope builders into test_support::envelope
- **Issue #140** (closed): Deduplicate five test-only Envelope-builder helpers scattered across rpc.rs, egress.rs, envelope.rs, mailbox.rs
- **Issue #138** (closed): Guard false-positive: worktree-write-confinement-unresolved-var blocks the standard mktemp-scoped-cleanup smoke-test idiom
- **Issue #137** (closed): Guard false-positive: worktree-write-confinement denies read/no-op Python heredocs

### 2026-08-15

- **Issue #134** (closed): Guard false-positive: worktree-write-confinement denies python3 heredocs that write nothing
- **Issue #133** (closed): Guard false-positive: worktree-write-confinement-unresolved-var denies mktemp -d smoke-test idiom
- **PR #132**: docs: drop stale spikes/ workspace reference from CLAUDE.md
- **Issue #129** (closed): Fix stale spikes/ reference in CLAUDE.md after #127 removed it
- **Issue #131** (closed): Guard friction: worktree-write-confinement-unresolved-var repeatedly blocks mktemp-based scratch config files for safehoused smoke tests
- **Issue #130** (closed): Guard false positive: write-confinement misparses Python comparison operators in heredocs as shell redirects
- **PR #127**: chore: remove spikes/qj-coldstart, drop it from workspace members
- **Issue #125** (closed): Remove spikes/qj-coldstart: throwaway spike whose validation work is done and archived
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
