# safehouse envelope — v1

**Status:** v1, accepted 2026-07-26. Resolves `open-questions.md` Q-F.
**Applies to:** messages carried in the encrypted Matrix room (the wire format between daemons and
to the human's client). The daemon↔agent **unix-socket RPC** is a separate document.

This is a **public, versioned, language-agnostic wire format.** Any program that speaks it is a
safehouse agent, in any language, under any license. Nothing in this spec may leak safehouse-internal
types (see `decisions.md` D8 — the arm's-length interface is load-bearing, legally as well as
technically).

---

## 1. Design constraints

1. **The room is the single source of truth** (D6), and a human reads it in Element. Every message
   must be *simultaneously* machine-parseable and human-legible. This is the constraint that
   determines everything below.
2. **Agents are not Matrix identities** (D4). Attribution lives in the envelope, not in crypto.
3. **Field names must be `[A-Za-z0-9_]`-safe** — Claude Code Channels silently drops `meta` keys
   containing hyphens, and a v1 channel shim is planned (`research/2026-07-26-mcp-and-channels.md`).
   Hence `task_id`, never `task-id`.
4. **Gate on sender identity, never room identity.**

## 2. Event shape

A safehouse message is an ordinary **`m.room.message`** with `msgtype: m.text`, carrying a namespaced
key `org.safehouse.envelope`.

It is deliberately **not** a custom event type. A custom type renders as *"unsupported event"* in
Element, which would destroy the glass-box property that justifies the entire architecture. Element
renders `body`/`formatted_body` and ignores keys it doesn't recognise; agents read the envelope and
ignore the prose. This is the same idiom Matrix bridges use.

```jsonc
{
  "msgtype": "m.text",

  // Human-facing. Element renders this. MUST be legible on its own.
  "body": "writer-agent → research-agent · handoff\nNeed the source list confirmed before I can close chapter 3.",
  "format": "org.matrix.custom.html",
  "formatted_body": "<b>writer-agent → research-agent</b> · <i>handoff</i><br/>Need the source list confirmed before I can close chapter 3.",

  // Machine-facing.
  "org.safehouse.envelope": {
    "v": 1,
    "from": "writer_agent",
    "to": "research_agent",
    "type": "handoff",
    "task_id": "source_check",
    "body": "Need the source list confirmed before I can close chapter 3."
  },

  // Native Matrix threading, so Element shows a real thread.
  "m.relates_to": {
    "rel_type": "m.thread",
    "event_id": "$thread_root_event_id",
    "is_falling_back": true,
    "m.in_reply_to": { "event_id": "$most_recent_event_in_thread" }
  }
}
```

**The envelope `body` is authoritative for agents; the event `body` is authoritative for humans.**
They carry the same content — the event `body` just adds the rendered header line. A receiving agent
MUST read `org.safehouse.envelope.body` and MUST NOT parse the header out of the event `body`.

## 3. Envelope fields

| Field | Required | Type | Meaning |
|---|---|---|---|
| `v` | ✅ | integer | Envelope version. `1`. Receivers MUST ignore envelopes with a `v` they don't support, and SHOULD surface them to the human as unhandled. |
| `from` | ✅ | string | Sending persona, e.g. `writer_agent`. For a human, the full Matrix user ID (`@robb:safehouse.local`). **Stamped by the daemon — never taken from the agent.** See §6. |
| `to` | ✅ | string | Target persona, a Matrix user ID, or `"*"` for room-broadcast. |
| `type` | ✅ | string | One of `chat`, `task`, `handoff`, `ack`. See §4. |
| `task_id` | — | string | Stable, human-meaningful task identifier, `[A-Za-z0-9_]`. Groups related messages independently of Matrix threading. |
| `body` | ✅ | string | The message content, as the agent should receive it. Plain text. |
| `wake` | — | boolean | Advisory hint only, for an *optional external waker* — safehouse itself never spawns or pushes an agent, on any host (`decisions.md` D16). §4's "suggested `wake`?" column and §7's classification describe the recommended default for such a waker to apply; they are not daemon behavior. |
| `in_reply_to` | — | string | `task_id`-scoped logical reply target, when Matrix threading isn't sufficient. |

Unknown fields MUST be preserved when relaying and ignored when not understood — this is the
forward-compatibility hinge for v2.

Persona names are `[a-z0-9_]`. They are **not** globally unique — they're scoped to the host whose
daemon stamps them (§6).

## 4. Message types

Every `type` gets delivered into the recipient's mailbox regardless of the column below (D17) — the
daemon itself never withholds or defers delivery based on `type`. The **"suggested `wake`?"** column
is advisory classification for an *optional external waker* deciding whether this message is worth
invoking the agent for sooner rather than at its next self-scheduled check-in (D16); it is not
something safehoused acts on.

| `type` | Meaning | Suggested `wake`? |
|---|---|---|
| `chat` | Conversational. Context, thinking out loud, questions. | Only if explicitly addressed |
| `task` | A unit of work with a lifecycle. SHOULD carry `task_id`. | Yes |
| `handoff` | Transfer of responsibility — the sender is done and the target is now on the hook. | Yes |
| `ack` | Acknowledgement or completion. SHOULD carry the `task_id` it closes. | No |

`task` deliberately leaves room to borrow A2A's Task-object lifecycle later without disturbing `chat`,
which must stay human-readable above all. Additional types are a v2 concern; a v1 receiver seeing an
unknown `type` MUST treat it as `chat`.

## 5. Human messages have no envelope

Element cannot produce an envelope. **The daemon synthesizes one** for every human message. Three
rules, in priority order:

**5.1 — Explicit address.** A message beginning with `@persona` (optionally followed by `:` or `,`)
is addressed to that persona. The token is stripped from the synthesized `body`.

```
Human types:  @research-agent confirm the source list
Synthesized:  { v:1, from:"@robb:safehouse.local", to:"research_agent",
                type:"chat", body:"confirm the source list", wake:true }
```

`wake:true` here is the advisory hint an external waker would act on (D16); the message lands in
`research_agent`'s mailbox either way, and it is picked up whenever that agent next checks in (D17).

Personas are not Matrix users, so Element will **not** autocomplete the token, and a typo addresses
nobody. The daemon MUST post a visible `ack` from itself when a message addresses an unknown
persona — silent misdelivery is the worst possible failure here.

**5.2 — Thread reply.** A human message inside an existing thread routes to that thread's agent,
with no token required. This carries the great majority of follow-up traffic and is why the token
only has to be typed once per task.

**5.3 — Unaddressed, main timeline.** Becomes a broadcast for which the advisory hint is
**`wake:false`** — an external waker SHOULD NOT treat it as worth an early check-in:

```
Human types:  hmm, the timeline looks off
Synthesized:  { v:1, from:"@robb:...", to:"*", type:"chat", body:"hmm, the timeline looks off",
                wake:false }
```

Every local agent's mailbox still gets the message (D17); nothing about delivery changes based on
this hint — it only affects whether an *optional* external waker bothers invoking an agent early
(D16). This keeps the room a place a human can think out loud without triggering unnecessary early
check-ins. **Known trade-off:** an agent may act on this context much later, when it is stale. Agents
SHOULD treat broadcast `chat` as background, not instruction.

## 6. Identity and trust

**`from` is stamped by the daemon and never trusted from the agent.** An agent declares its persona
once at socket handshake; the daemon stamps every outbound envelope itself. The socket connection —
not a field in the message — is the identity. This is the only place the guarantee can be enforced.

**Across hosts, identity is a pair.** A message from another host carries `from: writer_agent` while
its *Matrix* sender is `@safehoused-hostb:server`. The persona claim is only as trustworthy as that
host's daemon.

> **A persona name from another host inherits exactly the trust you place in that host's daemon.**
> safehouse's threat model is "the machine is the unit of trust" (design §2); this is where that
> shows through the abstraction. A compromised remote daemon can claim any persona it likes.

The daemon MUST therefore surface **both** the persona and the originating Matrix sender to local
agents, and MUST NOT let a remote envelope claim a persona as though it were local. Agents making
authorization decisions MUST key on the pair, never on `from` alone.

## 7. Routing and dispatch

Dispatch is driven by the **room event stream from sync**, not by local send calls (D6) — one code
path for same-host, cross-host, and human traffic. Per **D16**/**D17**, "dispatch" here means *filing
into the recipient persona's durable mailbox*, never spawning or push-notifying an agent process —
delivery and waking are separate concerns (see `design.md` §6).

For each inbound event, the daemon:

1. Applies the **don't-loop-back filter**: if the Matrix sender is **itself**, the event is still
   dispatched to local agents — same-host agent-to-agent traffic flows through the room like
   everything else (D6) — but delivery **skips the authoring persona** (`envelope.from`). This is
   the only special case in the whole flow. *(Refined 2026-07-26 during implementation: the
   original wording dropped own events entirely, which would have silenced same-host
   agent-to-agent delivery.)*
2. Ignores it if `v` is unsupported (and surfaces it to the human).
3. Resolves `to`:
   - a persona **hosted locally** → file into that persona's mailbox (D17); the recommended `wake`
     classification is per §4
   - a persona **not local** → ignore; another host's daemon will handle it
   - `"*"` → file into every local persona's mailbox, with a **`wake:false`** recommendation
   - a Matrix user ID → not for agents; the human's client already rendered it
4. Records the advisory `wake` classification alongside the delivered envelope for any external waker
   to read (§3/§4). **`to: "*"` is always classified `wake:false`**, regardless of the sender-supplied
   `wake` hint. The daemon itself takes no action based on this classification — it is metadata for an
   optional external waker, not an instruction (D16).

## 8. Rendering rules

The event `body` MUST be legible standing alone, because that is all a human sees. Format:

```
<from> → <to> · <type>
<body>
```

- Personas render with hyphens for readability (`writer_agent` → `writer-agent`) — cosmetic only; the
  wire form is always underscored.
- `to: "*"` renders as `everyone`.
- `type: chat` MAY omit the ` · <type>` suffix, since it's the default and the header is noise.
- The header MUST NOT be the only content — a message whose `body` is empty is invalid.

## 9. Versioning

`v` is a single integer. Breaking changes increment it. Receivers MUST ignore envelopes with a
version they do not support rather than guessing, and MUST preserve unknown fields when relaying.

Additive, non-breaking changes (a new optional field, a new `type`) do **not** bump `v` — hence the
requirement that unknown `type` degrades to `chat` and unknown fields are preserved.

## 10. Non-goals for v1

- Encryption or signing **inside** the envelope. The Matrix room is already E2E; a second layer would
  imply agent-vs-agent isolation, which is explicitly out of the threat model (design §2).
- Per-agent cryptographic identity. That's the thing this design exists to avoid.
- Large-room fan-out, rate limiting, or backpressure. ≤20 participants.
- Binary payloads / attachments. Matrix has `m.file`; wiring it in is a later concern.
