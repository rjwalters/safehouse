# Research 6 — Q-I: licensing (2026-07-26)

Engineering-level analysis, **not legal advice.** Question: what license must safehouse carry, and
does the daemon↔agent socket boundary keep third-party agents free?

## Headline: our docs were wrong, and the error inverted the answer

`open-questions.md` Q-I and `research/2026-07-26-prior-art.md` both recorded baibot **and** mxlink as
AGPL-3.0. Verified against the actual LICENSE files:

| Project | Actual SPDX | Note |
|---|---|---|
| **mxlink** | **LGPL-3.0** | And it is at **`etkecc/rust-mxlink`** — `etkecc/mxlink` is a **404**. LGPL for all 22 published versions; no AGPL history. |
| **baibot** | AGPL-3.0-or-later | Correct as recorded. But it is an **application**, not a library. |
| matrix-rust-sdk | Apache-2.0 | All crates @ 0.18.0 |
| vodozemac | Apache-2.0 | 0.10.0 |
| ruma (transitive) | MIT | |

Neither etke.cc project offers dual-licensing or a commercial exception, and mxlink carries **no
linking exception**.

## Why this inverts the question

The Q-I premise was "depending on mxlink makes safehoused AGPL." **False, twice over:**

1. **LGPLv3 §4** permits conveying a Combined Work "under terms of your choice, … if you also"
   give notice (§4a), ship GPLv3 + LGPLv3 texts (§4b), and satisfy §4(d). Apache/MIT don't restrict
   modification or reverse-engineering of the library portions, so the §4 chapeau is satisfied.
   Depending on mxlink would **not** have forced copyleft.
2. **baibot is an application.** You don't link an app. There was never any AGPL code we'd take.

**So the work delta between Apache-2.0 and AGPL is zero.** They are not on the code-reuse axis at
all. AGPL would have unlocked no reuse while costing adopters.

## Decision: Apache-2.0, no mxlink dependency → `decisions.md` D8

Reasoning recorded in D8. Summary: matches our dependency tree; express **irrevocable** patent grant
(Apache §3 — MIT has none, and `MIT OR Apache-2.0` lets downstream drop the grant); and compatibility
runs the right way (Apache→AGPL fork is possible, AGPL→permissive is not). AGPL's leverage is against
SaaS re-hosting, a threat a per-host daemon doesn't face.

Note GPLv3/AGPLv3 §11 *does* have a patent grant — this isn't "Apache has one, AGPL doesn't." The
differences are that Apache's is explicitly irrevocable and perpetual in its own text, its
termination trigger is a well-understood defensive-litigation clause, and it's the one corporate
legal review already has a playbook for.

## Reading vs. copying

Copyright protects **expression**, not ideas, procedures, or methods of operation (17 U.S.C.
§102(b)). Reading AGPL source to learn *which SDK methods to call in what order* transfers no
copyright. That is exactly what the Q-G pass extracted, and it was deliberately written up as prose
call-sequences with **no reproduced function bodies**.

Practical hazards, in descending order of realism:
1. **Unconscious literal copying** — distinctive error strings, comment text, unusual identifier
   names, and most commonly *identical function decomposition and ordering*. This is the real risk,
   not deliberate theft.
2. **Copying the arbitrary bits.** Matrix-protocol-shaped things (endpoint names, event types, JSON
   fields) aren't etke.cc's expression — that's the spec. Their expression is in the arbitrary
   choices: a particular retry schedule, a particular cache-key scheme.
3. Formal clean-room (one person reads and specs, another implements) is real practice but overkill
   for a solo FOSS project at this scale.

**Cheap mitigations that work:** read in one session, write from notes later rather than with source
in a split pane; never paste-then-edit; rename everything; keep `CREDITS.md` as a contemporaneous
record of what was studied for which problem.

## AGPL §13 — moot now, recorded for completeness

Had we gone AGPL: §13 only bites "**if you modify the Program**," and the FSF's gloss is that a
program "expressly designed to accept user requests and send responses over a network" qualifies.
safehoused runs a sync loop and acts on room messages a human types — that's the paradigm case, and
`#AGPLv3ServerAsUser` explicitly refuses the client/server framing. The "it's my own host" argument
works practically (the remote-user set is {you}), not legally. Where it would bite is downstream: a
company forking safehoused for 200 employees would owe them the fork.

A **unix socket is not "a computer network"** — kernel IPC on one machine, no network stack, no
remoteness — and an agent process is a program, not a "user." That one was never close.

## The daemon↔agent boundary — the part that still matters under Apache-2.0

Recorded because it constrains the architecture regardless of our license: it is what lets third
parties write agents under any license they like.

FSF `#MereAggregation` tests **both** prongs: *"a proper criterion depends both on the mechanism of
communication (exec, pipes, rpc, function calls within a shared address space) and the semantics of
the communication (what kinds of information are interchanged). … pipes, sockets and command-line
arguments are communication mechanisms normally used between two separate programs. … But if the
semantics of the communication are intimate enough, exchanging complex internal data structures,
that too could be a basis to consider the two parts as combined."*

safehouse passes both prongs: sockets are named explicitly as the separate-programs mechanism, and a
documented, versioned, language-agnostic envelope is close to the definitional opposite of
"intimate … complex internal data structures."

**Four things that keep it that way — treat as architectural invariants:**
1. **Spec the protocol in a standalone versioned document.** A published spec is evidence of
   arm's-length interface design.
2. **Ship a client library in a language that isn't Rust.** Nothing kills a "combined work" theory
   faster than a working Python agent that links none of our code.
3. **Never leak safehouse-internal Rust types across the boundary** — no serde dumps of internal
   structs, no enums tracking our internal state machine. Envelope fields are protocol concepts.
4. **No plugin ABI, ever.** No `dlopen`, no `cdylib` agent interface, no "load agents in-process for
   speed." That single optimization would collapse the whole analysis.

Caveat: the FSF FAQ is the FSF's interpretation — persuasive, widely followed, and *not binding law*
("a legal question, which ultimately judges will decide").

## 🚩 Where a real lawyer is needed

1. **CLA vs. DCO, before contributor #2** — see D11. Once outside work merges under bare
   inbound=outbound Apache-2.0, relicensing needs every contributor's consent. Pair with a
   code-provenance audit (confirm nothing from baibot ever landed) before any commercial deal.
2. **Only if the mxlink decision is ever reversed *and* we ship binaries** — LGPLv3 §4(d)(0)'s
   "relink with a modified version" has no settled meaning for statically-linked Rust, and §4(d)(1)'s
   shared-library escape hatch is unavailable. Publishing full source is the consensus answer;
   "probably correct on an unsettled question" is the sentence that should precede a call to counsel.

## Sources

Licenses read directly from each repo. [Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0.txt) ·
[LGPL-3.0](https://www.gnu.org/licenses/lgpl-3.0.en.html) ·
[AGPL-3.0](https://www.gnu.org/licenses/agpl-3.0.en.html) ·
FSF GPL FAQ [#MereAggregation](https://www.gnu.org/licenses/gpl-faq.html#MereAggregation) ·
[#GPLPlugins](https://www.gnu.org/licenses/gpl-faq.html#GPLPlugins) ·
[#AGPLv3InteractingRemotely](https://www.gnu.org/licenses/gpl-faq.html#AGPLv3InteractingRemotely) ·
[#AGPLv3ServerAsUser](https://www.gnu.org/licenses/gpl-faq.html#AGPLv3ServerAsUser) ·
[FSF license list](https://www.gnu.org/licenses/license-list.html) · [SPDX](https://spdx.org/licenses/)
