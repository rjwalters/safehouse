# safehouse

FOSS, E2E, bot-first messaging substrate on Matrix. **Start with `docs/next-agent.md`** — it is the
canonical handoff and points everywhere else (design, decisions D1–D17, open questions, protocol).

- Rust workspace: `safehoused/` (the daemon — key custody, sync v2, unix-socket RPC),
  `safehouse-mcp/` (keyless stdio MCP shim), `spikes/` (throwaway provenance binaries).
  `cargo build` / `cargo build -p safehoused`.
- The wire format is `docs/protocol/envelope-v1.md`; the daemon stamps `from` and enforces the
  persona allowlist — never trust identity from an agent message.
- Hard invariants (D8, licensing-load-bearing): agent socket is **AF_UNIX only — never TCP**; no
  in-process plugin ABI; never copy code from `baibot` (AGPL) or `mxlink` (LGPL) — reading is fine.
- `EncryptionSettings` are all explicitly non-default and the recovery passphrase is mandatory
  config; see "things that will bite you" in `docs/next-agent.md` before touching boot code.

<!-- BEGIN REPO-SKILLS -->
This repository has [Repo Skills](https://github.com/rjwalters/repo) v0.4.3 installed —
general repository hygiene and environment commands invoked as `/repo:<command>`. Run
`/repo:help` for the command list, or see `.claude/skills/repo/SKILL.md` for the full
guide. Hygiene commands apply safe, reversible fixes by default and report each
change; run with `--ask` to review first, and `--prune` to allow irreversible
removals. Managed by `install.sh` — edit outside the markers only.
<!-- END REPO-SKILLS -->

<!-- BEGIN LOOM ORCHESTRATION -->
This repository uses [Loom](https://github.com/rjwalters/loom) for AI-powered development orchestration — see the Loom repository for the full guide (roles, labels, worktrees, configuration). When installed, Loom also writes a locally-substituted copy of that guide to `.loom/CLAUDE.md`.
<!-- END LOOM ORCHESTRATION -->