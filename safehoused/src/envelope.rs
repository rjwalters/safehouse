//! Envelope v1 — wire format per docs/protocol/envelope-v1.md.
//!
//! The daemon is the only writer of `from` (§6) and the only place rendering
//! happens (§8). Agents never see Matrix event JSON; they see envelopes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const ENVELOPE_KEY: &str = "org.safehouse.envelope";
pub const KNOWN_TYPES: [&str; 4] = ["chat", "task", "handoff", "ack"];

/// The daemon's own persona for system-generated messages, e.g. §5.1's
/// unknown-persona ack. Distinct from any configured agent persona — the
/// daemon is speaking as itself, not on behalf of an agent.
pub const DAEMON_PERSONA: &str = "safehouse";

/// The single envelope version this daemon understands (§9). Receivers MUST
/// ignore envelopes with any other `v` rather than guessing (§7.2).
pub const SUPPORTED_VERSION: u32 = 1;

/// The result of interpreting an inbound room event (§7 dispatch).
#[derive(Clone, Debug)]
pub enum Inbound {
    /// A supported-version envelope, or one synthesized for a human message
    /// (§5), plus — when a human's leading `@token` failed to resolve to any
    /// locally-known persona — the raw token as typed (`Some("typo-agent")`).
    /// Messages that already carried an envelope (agent traffic) never
    /// produce a token. Safe to dispatch to local agents.
    Envelope(Envelope, Option<String>),
    /// The event carried an `org.safehouse.envelope` whose `v` this daemon does
    /// not support (§7.2, §9). The receiver MUST NOT dispatch or guess-parse
    /// it; the caller logs it and surfaces it to the human instead. Carries the
    /// raw version seen, purely for the surfaced notice.
    UnsupportedVersion(u64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub body: String,
}

pub fn valid_persona(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// §8: `<from> → <to> · <type>` over the body. Personas render hyphenated;
/// the wire form stays underscored. `chat` omits the type suffix.
pub fn render(env: &Envelope) -> (String, String) {
    let display = |s: &str| {
        if s == "*" {
            "everyone".to_owned()
        } else if s.starts_with('@') {
            s.to_owned()
        } else {
            s.replace('_', "-")
        }
    };
    let (from, to) = (display(&env.from), display(&env.to));
    let suffix = if env.kind == "chat" {
        String::new()
    } else {
        format!(" · {}", env.kind)
    };
    let plain = format!("{from} → {to}{suffix}\n{}", env.body);
    let html_suffix = if env.kind == "chat" {
        String::new()
    } else {
        format!(" · <i>{}</i>", html_escape(&env.kind))
    };
    let html = format!(
        "<b>{} → {}</b>{}<br/>{}",
        html_escape(&from),
        html_escape(&to),
        html_suffix,
        html_escape(&env.body)
    );
    (plain, html)
}

/// Full `m.room.message` content carrying the envelope. `relates_to`, when
/// present, is native Matrix `m.thread` threading (§2) — see
/// [`thread_relation`]. Callers that never thread (daemon-generated notices)
/// pass `None`.
pub fn to_event_content(env: &Envelope, relates_to: Option<Value>) -> Value {
    let (plain, html) = render(env);
    let mut content = json!({
        "msgtype": "m.text",
        "body": plain,
        "format": "org.matrix.custom.html",
        "formatted_body": html,
        ENVELOPE_KEY: env,
    });
    if let Some(relates_to) = relates_to {
        content["m.relates_to"] = relates_to;
    }
    content
}

/// If `content` carries an `m.relates_to` with `rel_type: m.thread`, return
/// the referenced thread-root event id. `None` means this event either isn't
/// part of a thread, or — per §2 — is itself the first message of one (its
/// own event id becomes the root once observed).
pub fn thread_root_from_content(content: &Value) -> Option<String> {
    let relates_to = content.get("m.relates_to")?;
    if relates_to.get("rel_type").and_then(Value::as_str) != Some("m.thread") {
        return None;
    }
    relates_to
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Build the `m.relates_to` value for §2's native `m.thread` relation:
/// `root_event_id` is the task's thread root, `latest_event_id` is the most
/// recent event in that thread (used for the `is_falling_back` rich-reply
/// fallback so non-threading clients still see a sane reply chain).
pub fn thread_relation(root_event_id: &str, latest_event_id: &str) -> Value {
    json!({
        "rel_type": "m.thread",
        "event_id": root_event_id,
        "is_falling_back": true,
        "m.in_reply_to": { "event_id": latest_event_id },
    })
}

/// Extract an envelope from decrypted event JSON, or synthesize one for a
/// human message (§5). `personas` is the local allowlist used to resolve a
/// leading `@persona` token (§5.1).
///
/// Version is gated **explicitly** (§7.2, §9): when the event carries an
/// envelope whose `v` is not [`SUPPORTED_VERSION`], this returns
/// [`Inbound::UnsupportedVersion`] rather than guess-parsing it or falling back
/// to human-message synthesis. Guessing would let a future `v: 2` envelope be
/// dispatched as though the daemon user were a human speaker.
///
/// The [`Inbound::Envelope`] variant carries, alongside the resolved envelope,
/// the raw `@token` as typed when a human's leading `@token` failed to
/// resolve to any locally-known persona (`Some("typo-agent")`). Messages that
/// already carried an envelope (agent traffic) never produce a token — the
/// caller uses it to post the visible ack §5.1 requires; silent misdelivery
/// is explicitly disallowed there.
///
/// `thread_agent` is the persona the caller has already resolved (from its
/// own thread-tracking state) as "the agent for the thread this event
/// belongs to" — `None` when the event isn't part of any tracked thread.
/// Only consulted for human-message synthesis (§5.2); ignored for events that
/// already carry an envelope.
pub fn from_event_json(
    content: &Value,
    sender: &str,
    personas: &[String],
    thread_agent: Option<&str>,
) -> Inbound {
    if let Some(raw) = content.get(ENVELOPE_KEY) {
        // Read `v` before attempting to interpret the rest. An envelope with a
        // present-but-unsupported integer `v` is never guess-parsed and never
        // treated as a human message — the envelope key can only come from
        // another daemon, never from Element (§5).
        if let Some(v) = raw.get("v").and_then(Value::as_u64) {
            if v != u64::from(SUPPORTED_VERSION) {
                return Inbound::UnsupportedVersion(v);
            }
        }
        // Supported version (or a malformed envelope with no integer `v`):
        // best-effort parse. A well-formed v1 envelope returns here; anything
        // else falls through to human synthesis, preserving prior behavior.
        if let Ok(env) = serde_json::from_value::<Envelope>(raw.clone()) {
            return Inbound::Envelope(env, None);
        }
    }
    let body = content
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (env, unknown_persona) = synthesize_for_human(sender, body, personas, thread_agent);
    Inbound::Envelope(env, unknown_persona)
}

/// §5 synthesis, in priority order: §5.1 explicit `@token` address, §5.2
/// thread reply (routes to `thread_agent` with no token needed), §5.3
/// unaddressed broadcast.
fn synthesize_for_human(
    sender: &str,
    body: &str,
    personas: &[String],
    thread_agent: Option<&str>,
) -> (Envelope, Option<String>) {
    if let Some(rest) = body.strip_prefix('@') {
        let token: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ':' && *c != ',')
            .collect();
        if !token.is_empty() {
            let normalized = token.replace('-', "_");
            if personas.iter().any(|p| p == &normalized) {
                let stripped = rest[token.len()..]
                    .trim_start_matches([':', ','])
                    .trim()
                    .to_owned();
                return (
                    Envelope {
                        v: 1,
                        from: sender.to_owned(),
                        to: normalized,
                        kind: "chat".to_owned(),
                        task_id: None,
                        body: stripped,
                    },
                    None,
                );
            }
            // §5.1: "a typo addresses nobody" — falls through to the §5.3
            // broadcast below, but the caller must be told so it can post
            // the visible ack the spec mandates instead of misdelivering
            // silently.
            return (broadcast(sender, body), Some(token));
        }
    }
    // §5.2 — no explicit token, but the message is inside a thread the
    // caller has already resolved to an agent: route there, no token
    // required. This is the majority of follow-up traffic — the whole point
    // is the token only has to be typed once per task.
    if let Some(agent) = thread_agent {
        return (
            Envelope {
                v: 1,
                from: sender.to_owned(),
                to: agent.to_owned(),
                kind: "chat".to_owned(),
                task_id: None,
                body: body.to_owned(),
            },
            None,
        );
    }
    // §5.3 — unaddressed human message: broadcast, wakes no one.
    (broadcast(sender, body), None)
}

fn broadcast(sender: &str, body: &str) -> Envelope {
    Envelope {
        v: 1,
        from: sender.to_owned(),
        to: "*".to_owned(),
        kind: "chat".to_owned(),
        task_id: None,
        body: body.to_owned(),
    }
}

/// §5.1: the visible ack the daemon posts when a human's leading `@token`
/// didn't resolve to any locally-known persona — naming the typo'd token and
/// listing the known personas, so the misdirected message is never silent.
pub fn unknown_persona_ack(to: &str, token: &str, personas: &[String]) -> Envelope {
    let known = if personas.is_empty() {
        "(none configured)".to_owned()
    } else {
        personas
            .iter()
            .map(|p| p.replace('_', "-"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Envelope {
        v: 1,
        from: DAEMON_PERSONA.to_owned(),
        to: to.to_owned(),
        kind: "ack".to_owned(),
        task_id: None,
        body: format!("Unknown persona \"@{token}\" — known personas: {known}"),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn personas() -> Vec<String> {
        vec!["research_agent".to_owned(), "writer_agent".to_owned()]
    }

    fn envelope_content(v: u64) -> Value {
        json!({
            "msgtype": "m.text",
            "body": "writer-agent → research-agent · handoff\nconfirm sources",
            ENVELOPE_KEY: {
                "v": v,
                "from": "writer_agent",
                "to": "research_agent",
                "type": "handoff",
                "body": "confirm sources",
            }
        })
    }

    #[test]
    fn supported_version_parses_to_envelope() {
        let content = envelope_content(u64::from(SUPPORTED_VERSION));
        match from_event_json(&content, "@robb:safehouse.local", &personas(), None) {
            Inbound::Envelope(env, unknown) => {
                assert_eq!(env.v, SUPPORTED_VERSION);
                assert_eq!(env.from, "writer_agent");
                assert_eq!(env.to, "research_agent");
                assert_eq!(env.kind, "handoff");
                assert_eq!(env.body, "confirm sources");
                assert!(unknown.is_none());
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn future_version_is_not_dispatched() {
        // Regression for #6: a well-formed-but-future envelope must NOT be
        // guess-parsed or synthesized as a human message — it is gated on `v`.
        let content = envelope_content(2);
        match from_event_json(&content, "@robb:safehouse.local", &personas(), None) {
            Inbound::UnsupportedVersion(v) => assert_eq!(v, 2),
            other => panic!("expected UnsupportedVersion(2), got {other:?}"),
        }
    }

    #[test]
    fn large_future_version_preserved_without_truncation() {
        let big = u64::from(u32::MAX) + 5;
        let content = envelope_content(big);
        match from_event_json(&content, "@robb:safehouse.local", &personas(), None) {
            Inbound::UnsupportedVersion(v) => assert_eq!(v, big),
            other => panic!("expected UnsupportedVersion({big}), got {other:?}"),
        }
    }

    #[test]
    fn addressed_human_message_synthesizes_envelope() {
        let content =
            json!({ "msgtype": "m.text", "body": "@research-agent confirm the source list" });
        match from_event_json(&content, "@robb:safehouse.local", &personas(), None) {
            Inbound::Envelope(env, unknown) => {
                assert_eq!(env.v, SUPPORTED_VERSION);
                assert_eq!(env.from, "@robb:safehouse.local");
                assert_eq!(env.to, "research_agent");
                assert_eq!(env.kind, "chat");
                assert_eq!(env.body, "confirm the source list");
                assert!(unknown.is_none());
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn unaddressed_human_message_broadcasts() {
        let content = json!({ "msgtype": "m.text", "body": "hmm, the timeline looks off" });
        match from_event_json(&content, "@robb:safehouse.local", &personas(), None) {
            Inbound::Envelope(env, unknown) => {
                assert_eq!(env.to, "*");
                assert_eq!(env.kind, "chat");
                assert_eq!(env.body, "hmm, the timeline looks off");
                assert!(unknown.is_none());
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    // ---- valid_persona -------------------------------------------------

    #[test]
    fn valid_persona_accepts_lowercase_digits_underscore() {
        assert!(valid_persona("writer_agent"));
        assert!(valid_persona("agent1"));
        assert!(valid_persona("a"));
        assert!(valid_persona("a_b_c_123"));
    }

    #[test]
    fn valid_persona_rejects_empty() {
        assert!(!valid_persona(""));
    }

    #[test]
    fn valid_persona_rejects_too_long() {
        let ok = "a".repeat(64);
        let too_long = "a".repeat(65);
        assert!(valid_persona(&ok));
        assert!(!valid_persona(&too_long));
    }

    #[test]
    fn valid_persona_rejects_uppercase() {
        assert!(!valid_persona("Writer_Agent"));
    }

    #[test]
    fn valid_persona_rejects_hyphen() {
        // Hyphens are only a rendering convenience (§8); the wire form is
        // always underscored, so a hyphenated persona is invalid.
        assert!(!valid_persona("writer-agent"));
    }

    #[test]
    fn valid_persona_rejects_whitespace_and_symbols() {
        assert!(!valid_persona("writer agent"));
        assert!(!valid_persona("writer.agent"));
        assert!(!valid_persona("@writer_agent"));
    }

    // ---- render ----------------------------------------------------------

    fn env(from: &str, to: &str, kind: &str, body: &str) -> Envelope {
        Envelope {
            v: 1,
            from: from.to_owned(),
            to: to.to_owned(),
            kind: kind.to_owned(),
            task_id: None,
            body: body.to_owned(),
        }
    }

    #[test]
    fn render_chat_omits_type_suffix() {
        let (plain, html) = render(&env("writer_agent", "research_agent", "chat", "hi"));
        assert_eq!(plain, "writer-agent → research-agent\nhi");
        assert_eq!(html, "<b>writer-agent → research-agent</b><br/>hi");
    }

    #[test]
    fn render_non_chat_includes_type_suffix() {
        let (plain, html) = render(&env("writer_agent", "research_agent", "handoff", "go"));
        assert_eq!(plain, "writer-agent → research-agent · handoff\ngo");
        assert_eq!(
            html,
            "<b>writer-agent → research-agent</b> · <i>handoff</i><br/>go"
        );
    }

    #[test]
    fn render_hyphenates_underscored_personas() {
        let (plain, _) = render(&env("writer_agent", "research_agent", "task", "x"));
        assert!(plain.starts_with("writer-agent → research-agent"));
    }

    #[test]
    fn render_broadcast_target_as_everyone() {
        let (plain, html) = render(&env("writer_agent", "*", "chat", "hi all"));
        assert!(plain.starts_with("writer-agent → everyone\n"));
        assert!(html.starts_with("<b>writer-agent → everyone</b><br/>"));
    }

    #[test]
    fn render_passes_matrix_user_ids_through_unhyphenated() {
        // A human's `from`/`to` is a full Matrix user id and must not be
        // mangled by the persona hyphenation rule.
        let (plain, _) = render(&env("@robb:safehouse.local", "writer_agent", "chat", "hi"));
        assert!(plain.starts_with("@robb:safehouse.local → writer-agent"));
    }

    #[test]
    fn render_html_escapes_body_and_type() {
        let (_, html) = render(&env(
            "writer_agent",
            "research_agent",
            "<task>",
            "a & b <tag>",
        ));
        assert!(html.contains("&lt;task&gt;"));
        assert!(html.contains("a &amp; b &lt;tag&gt;"));
        assert!(!html.contains("<tag>"));
    }

    #[test]
    fn render_plain_body_not_html_escaped() {
        let (plain, _) = render(&env(
            "writer_agent",
            "research_agent",
            "chat",
            "a & b <tag>",
        ));
        // The event `body` (plain) is not HTML — no escaping expected there.
        assert!(plain.ends_with("a & b <tag>"));
    }

    // ---- from_event_json — envelope passthrough --------------------------

    #[test]
    fn from_event_json_passes_through_explicit_envelope() {
        let content = json!({
            "msgtype": "m.text",
            "body": "irrelevant — agents read the envelope, not this",
            ENVELOPE_KEY: {
                "v": 1,
                "from": "writer_agent",
                "to": "research_agent",
                "type": "handoff",
                "task_id": "source_check",
                "body": "Need the source list confirmed."
            }
        });
        match from_event_json(&content, "@robb:safehouse.local", &[], None) {
            Inbound::Envelope(env, unknown) => {
                assert_eq!(env.v, 1);
                assert_eq!(env.from, "writer_agent");
                assert_eq!(env.to, "research_agent");
                assert_eq!(env.kind, "handoff");
                assert_eq!(env.task_id.as_deref(), Some("source_check"));
                assert_eq!(env.body, "Need the source list confirmed.");
                assert!(unknown.is_none());
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn from_event_json_malformed_envelope_falls_back_to_synthesis() {
        // Missing required fields (`to`, `type`, `body`) — not a valid
        // Envelope, so this must fall back to human-message synthesis rather
        // than panicking or silently dropping the message.
        let content = json!({
            "body": "hmm, the timeline looks off",
            ENVELOPE_KEY: { "v": 1, "from": "someone" }
        });
        match from_event_json(&content, "@robb:safehouse.local", &[], None) {
            Inbound::Envelope(env, unknown) => {
                assert_eq!(env.from, "@robb:safehouse.local");
                assert_eq!(env.to, "*");
                assert_eq!(env.kind, "chat");
                assert_eq!(env.body, "hmm, the timeline looks off");
                assert!(unknown.is_none());
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn from_event_json_no_body_field_defaults_to_empty() {
        let content = json!({});
        match from_event_json(&content, "@robb:safehouse.local", &[], None) {
            Inbound::Envelope(env, _unknown) => {
                assert_eq!(env.body, "");
                assert_eq!(env.to, "*");
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    // ---- from_event_json / synthesize_for_human — human synthesis
    // (§5.1 / §5.3) -----------------------------------------------------

    /// §5.1 — a known persona addresses correctly and strips the token,
    /// unaffected by the unknown-persona handling added alongside it.
    #[test]
    fn known_persona_address_unchanged() {
        let (env, unknown) = synthesize_for_human(
            "@robb:safehouse.local",
            "@research-agent confirm the source list",
            &personas(),
            None,
        );
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.body, "confirm the source list");
        assert_eq!(env.kind, "chat");
        assert_eq!(env.from, "@robb:safehouse.local");
        assert!(unknown.is_none());
    }

    /// §5.1 — `:`/`,` separators after the token are still consumed, and the
    /// token still resolves normally.
    #[test]
    fn known_persona_address_with_colon_unchanged() {
        let (env, unknown) = synthesize_for_human(
            "@robb:safehouse.local",
            "@research-agent: hello",
            &personas(),
            None,
        );
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.body, "hello");
        assert!(unknown.is_none());
    }

    /// §5.1 — the issue's own example: `@typo-agent ...` must no longer
    /// silently become a no-wake broadcast without a trace. It still
    /// broadcasts (nobody local to deliver it to), but the caller now learns
    /// the token so it can post a visible ack.
    #[test]
    fn unknown_persona_flagged_for_ack() {
        let (env, unknown) = synthesize_for_human(
            "@robb:safehouse.local",
            "@typo-agent do the thing",
            &personas(),
            None,
        );
        assert_eq!(env.to, "*");
        assert_eq!(env.kind, "chat");
        // Broadcast body is left intact (matches pre-existing §5.3 behavior).
        assert_eq!(env.body, "@typo-agent do the thing");
        assert_eq!(unknown.as_deref(), Some("typo-agent"));
    }

    /// §5.3 — an unaddressed broadcast message must remain unaffected: no
    /// unknown-persona token, still a no-wake broadcast.
    #[test]
    fn unaddressed_broadcast_unaffected() {
        let (env, unknown) = synthesize_for_human(
            "@robb:safehouse.local",
            "hmm, the timeline looks off",
            &personas(),
            None,
        );
        assert_eq!(env.to, "*");
        assert_eq!(env.from, "@robb:safehouse.local");
        assert_eq!(env.body, "hmm, the timeline looks off");
        assert_eq!(env.task_id, None);
        assert!(unknown.is_none());
    }

    /// Edge case: a bare `@` (or `@` followed by whitespace) has no token to
    /// judge as unknown — treat it as ordinary unaddressed text, not a typo'd
    /// address, so the daemon doesn't ack on stray `@` usage.
    #[test]
    fn bare_at_sign_is_not_an_unknown_address() {
        let (env, unknown) =
            synthesize_for_human("@robb:safehouse.local", "@ hello", &personas(), None);
        assert_eq!(env.to, "*");
        assert_eq!(env.body, "@ hello");
        assert!(unknown.is_none());
    }

    /// Messages that already carry an envelope (agent-to-agent traffic) skip
    /// synthesis entirely and never produce an unknown-persona token.
    #[test]
    fn existing_envelope_never_flags_unknown_persona() {
        let content = json!({
            "body": "irrelevant rendered text",
            ENVELOPE_KEY: {
                "v": 1,
                "from": "writer_agent",
                "to": "does_not_exist",
                "type": "chat",
                "body": "hi"
            }
        });
        match from_event_json(&content, "@safehoused-hosta:server", &personas(), None) {
            Inbound::Envelope(env, unknown) => {
                assert_eq!(env.from, "writer_agent");
                assert_eq!(env.to, "does_not_exist");
                assert!(unknown.is_none());
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    /// The ack the daemon posts names the typo'd token and lists known
    /// personas hyphenated for readability (§8), addressed back at the human
    /// who sent the misdirected message, from the daemon's own persona.
    #[test]
    fn unknown_persona_ack_names_token_and_lists_personas() {
        let ack = unknown_persona_ack("@robb:safehouse.local", "typo-agent", &personas());
        assert_eq!(ack.from, DAEMON_PERSONA);
        assert_eq!(ack.to, "@robb:safehouse.local");
        assert_eq!(ack.kind, "ack");
        assert!(ack.body.contains("@typo-agent"));
        assert!(ack.body.contains("research-agent"));
        assert!(ack.body.contains("writer-agent"));
    }

    /// With no personas configured, the ack still renders something legible
    /// instead of an empty list.
    #[test]
    fn unknown_persona_ack_with_no_personas_configured() {
        let ack = unknown_persona_ack("@robb:safehouse.local", "anything", &[]);
        assert!(ack.body.contains("none configured"));
    }

    // ---- additional §5.1 token-parsing edge cases -------------------------

    fn human(body: &str) -> Envelope {
        human_in_thread(body, None)
    }

    fn human_in_thread(body: &str, thread_agent: Option<&str>) -> Envelope {
        match from_event_json(
            &json!({ "body": body }),
            "@robb:safehouse.local",
            &personas(),
            thread_agent,
        ) {
            Inbound::Envelope(env, _unknown) => env,
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn synthesize_explicit_address_comma_suffix() {
        let env = human("@research_agent, confirm the source list");
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.body, "confirm the source list");
    }

    #[test]
    fn synthesize_explicit_address_no_trailing_body() {
        let env = human("@research_agent");
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.body, "");
    }

    #[test]
    fn synthesize_explicit_address_colon_no_trailing_body() {
        let env = human("@research_agent:");
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.body, "");
    }

    // ---- §5.2 — thread-reply routing --------------------------------------

    /// §5.2 — a human message with no `@token` but a resolved thread agent
    /// routes there directly; no token required.
    #[test]
    fn thread_reply_routes_to_thread_agent_without_token() {
        let env = human_in_thread("confirm the source list", Some("research_agent"));
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.from, "@robb:safehouse.local");
        assert_eq!(env.kind, "chat");
        assert_eq!(env.body, "confirm the source list");
    }

    /// §5.1 still takes priority over §5.2: an explicit `@token` wins even
    /// inside a tracked thread.
    #[test]
    fn explicit_address_overrides_thread_agent() {
        let env = human_in_thread("@writer-agent status?", Some("research_agent"));
        assert_eq!(env.to, "writer_agent");
        assert_eq!(env.body, "status?");
    }

    /// No `@token` and no tracked thread agent: falls through to §5.3
    /// broadcast, unchanged from prior behavior.
    #[test]
    fn no_token_and_no_thread_agent_still_broadcasts() {
        let env = human_in_thread("hmm, the timeline looks off", None);
        assert_eq!(env.to, "*");
        assert_eq!(env.kind, "chat");
    }

    // ---- to_event_content / thread_relation — §2 native threading --------

    #[test]
    fn to_event_content_without_relation_omits_relates_to() {
        let content = to_event_content(&env("writer_agent", "research_agent", "chat", "hi"), None);
        assert!(content.get("m.relates_to").is_none());
    }

    #[test]
    fn to_event_content_with_relation_embeds_it() {
        let relation = thread_relation("$root", "$latest");
        let content = to_event_content(
            &env("writer_agent", "research_agent", "task", "go"),
            Some(relation),
        );
        let relates_to = content
            .get("m.relates_to")
            .expect("m.relates_to must be present");
        assert_eq!(relates_to["rel_type"], "m.thread");
        assert_eq!(relates_to["event_id"], "$root");
        assert_eq!(relates_to["is_falling_back"], true);
        assert_eq!(relates_to["m.in_reply_to"]["event_id"], "$latest");
    }

    #[test]
    fn thread_relation_shape_matches_spec() {
        let relation = thread_relation("$abc", "$abc");
        assert_eq!(relation["rel_type"], "m.thread");
        assert_eq!(relation["event_id"], "$abc");
        assert_eq!(relation["m.in_reply_to"]["event_id"], "$abc");
    }

    #[test]
    fn thread_root_from_content_extracts_thread_relation() {
        let content = json!({
            "body": "confirm the source list",
            "m.relates_to": thread_relation("$root", "$latest"),
        });
        assert_eq!(thread_root_from_content(&content).as_deref(), Some("$root"));
    }

    #[test]
    fn thread_root_from_content_ignores_non_thread_relations() {
        let content = json!({
            "body": "a plain reply",
            "m.relates_to": { "rel_type": "m.reference", "event_id": "$other" },
        });
        assert!(thread_root_from_content(&content).is_none());
    }

    #[test]
    fn thread_root_from_content_none_when_absent() {
        let content = json!({ "body": "no relation here" });
        assert!(thread_root_from_content(&content).is_none());
    }
}
