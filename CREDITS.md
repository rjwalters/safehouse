# Credits

safehouse is an independent implementation. This file records the prior work we **studied** while
designing it, and for which problem. It is deliberately separate from `NOTICE`: under Apache-2.0
§4(d), anything in `NOTICE` must be carried by every downstream fork forever, so `NOTICE` stays
minimal and the thanks live here.

## Statement of independent implementation

safehouse's persistent-E2E daemon design was informed by reading the projects below. **No code was
copied from any of them.** `safehoused` is written directly against
[matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk) (Apache-2.0) and
[vodozemac](https://github.com/matrix-org/vodozemac) (Apache-2.0).

What we took from these projects is *methods of operation* — which SDK functions to call, in what
order, and which failure modes to handle. Under 17 U.S.C. §102(b) that is not protected by
copyright. What we did not take is their expression: no function bodies, no error strings, no
comments, no code structure.

This statement is written **now, before any dispute exists**, precisely because a contemporaneous
record of independent implementation is worth far more than the same claim made later.

## What we studied, and for what

| Project | License | What we learned from it |
|---|---|---|
| [**baibot**](https://github.com/etkecc/baibot) (etke.cc) | AGPL-3.0-or-later | That a production E2E Matrix bot ships with **no interactive verification code at all** — strong evidence the daemon needs no human verification step. Also the shape of its recovery-passphrase / reset-allowed config pair. |
| [**mxlink**](https://github.com/etkecc/rust-mxlink) (etke.cc) | LGPL-3.0 | The headless login → bootstrap → recover sequence; the three-way recovery error match; the session-file/database consistency invariant; the transient-vs-permanent `whoami` backoff distinction. **We deliberately did not take this as a dependency** — see `docs/decisions.md` D8. |
| [**pantalaimon**](https://github.com/matrix-org/pantalaimon) | Apache-2.0 (archived 2026-04-08) | The canonical "daemon owns the device and crypto store, clients hold no keys" proxy shape. |
| **Hermes Agent** "proxy mode" (Nous Research) | — | The decrypt-and-dispatch pattern: gateway decrypts inbound, forwards plaintext to a keyless local agent. |
| [**matrix-rust-sdk**](https://github.com/matrix-org/matrix-rust-sdk) | Apache-2.0 | Our actual dependency. Its `examples/` are Apache-2.0 and are the one source we may copy from directly. |

Thanks in particular to **Slavi Pantaleev / etke.cc**, whose baibot and mxlink are the clearest
public demonstrations that headless persistent-E2E Matrix bots are practical at all. safehouse
exists in a much better-lit room because that work is public.

## Note for contributors

If you are implementing against these references: read them, close the tab, and write from notes.
Do not paste-then-edit. See `docs/decisions.md` D8 for the decision rule.
