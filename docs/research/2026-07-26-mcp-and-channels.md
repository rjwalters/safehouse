# Research 5 — Q-H: Matrix MCP servers, and the Channels discovery (2026-07-26)

Question: does a maintained MCP server for Matrix exist that a Claude Code agent could use as a tool
— and **who holds the E2E keys** in that design?

## Verdict: COMPLEMENT — build the daemon as designed, add MCP in v1

## The design space is empty (again)

~30 Matrix MCP repos found. Key custody sorts into exactly two buckets:

- **(a) ignores encryption entirely** — thin REST wrappers over the client-server API
- **(b) holds its own Matrix device + keys** — one device *per agent tool instance*

**(c) MCP server proxying to a keyless client: exists nowhere.** That is the gap safehouse fills —
the same "nobody built this" result as the Q-A prior-art scan, from a different direction.

Every E2E-capable option is one-device-per-agent, which reintroduces exactly the per-agent key
lifecycle problem `design.md` §7 dissolves. One repo goes further and imports the *human's*
cross-signing private keys via their recovery key, making the MCP server a full-trust device of the
human. Adopting any of these means inheriting a key-custody model we explicitly rejected.

**Maturity signal — this is not a mature field:**
- The official MCP registry contains **zero** Matrix chat servers (confirmed twice, independently).
- Anthropic's `claude-plugins-official` marketplace has **zero** Matrix entries — the channel
  plugins are telegram, discord, imessage, fakechat.
- Top repo is 48★ and 12 months stale. PyPI packages show 155 and 97 downloads/month.
- A dozen-plus repos were **created and last pushed on the same date.**

## The find: Claude Code Channels

Not-IGNORE, because **Channels** (Claude Code v2.1.80+, 2026-03-20) is the sanctioned mechanism for
the "daemon wakes the agent" step `design.md` §6 assumes — and it *is* an MCP server. Claude Code
spawns it as a **stdio subprocess**; it declares `capabilities.experimental['claude/channel']` and
pushes `notifications/claude/channel` with `{content, meta}`. An optional
`claude/channel/permission` method relays tool-approval prompts — meaning **the human could approve
an agent's `Bash` call from Element on their phone**, a sharper version of §4.3's remote control.

**Sequencing: v0 unchanged.** Channels is a research preview whose own docs say the "flag syntax and
protocol contract may change." Keep it off the critical path.

**v1: `safehoused-channel`** — a ~200-line stdio shim holding **zero keys and zero credentials**.
Claude Code spawns it; it dials `/run/safehoused.sock`, subscribes as a named persona, converts each
decrypted event into a channel notification, and exposes one `send` tool. The daemon remains the only
device, only crypto store, only ratchet writer, and only `from`-allowlist enforcement point. This
shim would be the first instance of the empty category (c).

## Free insurance to take in v0

- **Envelope field names must be `[A-Za-z0-9_]`-safe.** Channels silently drops `meta` keys
  containing hyphens. `open-questions.md` Q-F proposed `task-id` — use **`task_id`**. Free now,
  annoying later.
- Ensure one socket connection can both stream inbound events for a persona *and* accept sends.

## Independent validation of the envelope design

The Channels docs arrive at our design and sharpen it: *"Gate on the sender's identity, not the chat
or room identity… gating on the room would let anyone in an allowlisted group inject messages into
the session."* Our `from:` field is the right primitive — and it must be enforced **in the daemon**,
not in the shim.

## Operational gotchas for the v1 shim (hard-won by others)

- The launch flag must be `plugin:<name>@<marketplace>`, **never** `server:<name>` — Claude Code's
  matcher rejects the `server:` form for plugin-loaded MCP servers, and inbound events are then
  **silently dropped**.
- `"channelsEnabled": true` is additionally required on Team/Enterprise and wherever a managed-
  settings policy file is present.
- No Matrix plugin is on Anthropic's allowlist, so users would need
  `--dangerously-load-development-channels` (full-screen warning) until that changes.
- Events arrive only while a session is open; queued events are delivered together on the next turn.

## Spillover findings

**Q-G corroboration:** two unrelated projects independently converged on the same headless
cross-signing solution, and **neither used MSC4190 or dehydrated devices** — both bootstrap by
importing cross-signing private keys from secret storage via the recovery key. Same mechanism the
Q-G pass found in `import_secrets()`. Three independent reads, one answer. One documented a trap
worth having: a *deviceless* token succeeds at `/keys/signatures/upload` but fails `/keys/upload`,
leaving "a cross-signing signature naming an ed25519 the server never received, attached to a device
record that doesn't exist — from peers' perspective: an unsigned ghost."

Counterpoint confirming the risk was real: the cleanest matrix-rust-sdk MCP implementation found
simply punts — *"does not perform interactive device verification or cross-signing, so other users
may see this device as unverified."*

**Q-A addition:** [`RadekalOne/hearth`](https://github.com/RadekalOne/hearth) (active) is Conduit +
Element + shared memory + Matrix MCP + @-mention agent wake — nearly safehouse's product surface.
**Not a threat:** it gives each agent its **own Matrix identity** (the opposite of our thesis) and its
MCP server has no Matrix SDK and no crypto at all, unencrypted-only. Useful as a UX reference.

## Confidence

**CONFIRMED:** every E2E classification above (repos/deps/docs read directly); empty official
registry and empty Anthropic plugin list (each confirmed by two independent methods); the full
Channels protocol contract; the `plugin:` flag gotcha.

**UNCERTAIN:** E2E posture of ~6 same-day toy repos (inferred from size/age); two directory sites
403'd bot fetches. Neither can move the verdict — the registry and Anthropic allowlist are the
load-bearing checks and both are confirmed empty of Matrix.
