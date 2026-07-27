# Contributing to safehouse

Thanks for considering it. Ground rules are few but firm.

## License and sign-off (DCO)

safehouse is **Apache-2.0**, inbound = outbound. Every commit must carry a
`Signed-off-by` line certifying the [Developer Certificate of Origin](https://developercertificate.org/):

```
git commit -s
```

That's the whole process — no CLA, no paperwork. Unsigned commits can't be merged (see
`docs/decisions.md` D11).

## Non-negotiable invariants

These are load-bearing — legally as well as technically (`docs/decisions.md` D8):

- **The agent socket is AF_UNIX only. Never add a TCP listener.**
- **No in-process plugin ABI.** Agents are separate processes speaking the documented envelope
  protocol (`docs/protocol/envelope-v1.md`); that arm's-length boundary is what lets third-party
  agents carry any license.
- **Never copy code from `baibot` (AGPL-3.0) or `mxlink` (LGPL-3.0).** Reading them for patterns is
  fine; copying is not.
- **Never hand-roll cryptography.** All crypto stays in vodozemac via matrix-rust-sdk.

## Before you touch the daemon's boot path

Read the "things that will bite you" list in `docs/next-agent.md` first — `EncryptionSettings`
defaults, the mandatory recovery passphrase, the store/session consistency invariant, and the
shutdown backup flush all have live-verified failure modes behind them.

## Practical notes

- `cargo build` at the workspace root; the daemon is `-p safehoused`, the MCP shim `-p safehouse-mcp`.
- Docs are part of the change: decisions go in `docs/decisions.md`, protocol changes bump or extend
  `docs/protocol/envelope-v1.md` per its versioning rules.
