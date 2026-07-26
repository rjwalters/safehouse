# Research 1 — Buy vs build an agent messaging system (2026-07-26)

Deep-research pass (5 phases, ~105 agents, adversarial verification). Question: build a real-time
multi-human + multi-agent messaging system on Cloudflare with MCP backing, or adopt Slack / Discord /
Matrix / etc.?

## Verdict
**Don't build it for a few repo-scoped agents + one human. Buy / assemble.** And the "real-time
sockets" premise is the wrong target — coding agents are request/response and don't hold a socket
between runs, so a durable async **inbox + wake trigger** buys nearly all the value of a socket layer.

## Key verified findings
- **The wake-up problem dominates.** Agents only receive while running; the production primitive for
  reaching a not-running agent is a durable async inbox, not a held socket. Even shipped products work
  this way: **Claude-in-Slack** reads a *bounded lookback* (20 channel / 50 thread / 100 forwarded
  msgs) on @-mention, not live state.
  [support.claude.com](https://support.claude.com/en/articles/12461605-use-claude-in-slack)
- **Native wake mechanisms already exist.** Claude Code Routines (cron / HTTP `/fire` / GitHub
  triggers) [code.claude.com/docs/en/routines]; Managed Agents scheduled deployments (minute-level
  cron + on-demand run endpoint) [platform.claude.com/docs/en/managed-agents]. Latency floor: Routines
  ~1-hour min cadence; Managed Agents ~15% cron jitter.
- **Chat is the wrong abstraction for agent-to-agent.** MCP is vertical (agent→tools); **A2A**
  (Agent2Agent) is horizontal (agent→agent), modeling a handoff as a **Task object** with a lifecycle
  over HTTP/JSON-RPC/SSE and **webhook push for disconnected clients**. Complementary, not competing.
  [a2a-protocol.org](https://a2a-protocol.org/latest/specification/)
- **Cloudflare has the primitives** (McpAgent on Durable Objects, hibernatable WebSockets, Agents SDK)
  — but remote-MCP transport is **Streamable HTTP/SSE, not WebSocket**, so even a custom build gives
  agents no persistent socket over MCP. Building is only justified as a shared multi-tenant product.

## Honest gaps
No verified cost / build-effort numbers (budget-dropped). Per-platform comparison was Slack-heavy;
Discord/Matrix/Zulip claims largely unverified this pass. A2A "de facto standard" was the one
non-unanimous finding (2-1).

## How it steered safehouse
Confirmed **async + inbox + wake** as the core. Two later user constraints — **FOSS** and **E2E** —
then eliminated Slack/Discord and pointed at Matrix, motivating Research 2.
