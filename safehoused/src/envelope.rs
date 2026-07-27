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
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
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
    let suffix = if env.kind == "chat" { String::new() } else { format!(" · {}", env.kind) };
    let plain = format!("{from} → {to}{suffix}\n{}", env.body);
    let html_suffix =
        if env.kind == "chat" { String::new() } else { format!(" · <i>{}</i>", html_escape(&env.kind)) };
    let html = format!(
        "<b>{} → {}</b>{}<br/>{}",
        html_escape(&from),
        html_escape(&to),
        html_suffix,
        html_escape(&env.body)
    );
    (plain, html)
}

/// Full `m.room.message` content carrying the envelope.
pub fn to_event_content(env: &Envelope) -> Value {
    let (plain, html) = render(env);
    json!({
        "msgtype": "m.text",
        "body": plain,
        "format": "org.matrix.custom.html",
        "formatted_body": html,
        ENVELOPE_KEY: env,
    })
}

/// Extract an envelope from decrypted event JSON, or synthesize one for a
/// human message (§5). `personas` is the local allowlist used to resolve a
/// leading `@persona` token (§5.1).
///
/// Returns the resolved envelope plus, when a human's leading `@token`
/// failed to resolve to any locally-known persona, the raw token as typed
/// (`Some("typo-agent")`). Messages that already carried an envelope (agent
/// traffic) never produce a token — the caller uses it to post the visible
/// ack §5.1 requires; silent misdelivery is explicitly disallowed there.
pub fn from_event_json(
    content: &Value,
    sender: &str,
    personas: &[String],
) -> (Envelope, Option<String>) {
    if let Some(env) = content.get(ENVELOPE_KEY) {
        if let Ok(env) = serde_json::from_value::<Envelope>(env.clone()) {
            return (env, None);
        }
    }
    let body = content.get("body").and_then(Value::as_str).unwrap_or_default();
    synthesize_for_human(sender, body, personas)
}

fn synthesize_for_human(sender: &str, body: &str, personas: &[String]) -> (Envelope, Option<String>) {
    if let Some(rest) = body.strip_prefix('@') {
        let token: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ':' && *c != ',')
            .collect();
        if !token.is_empty() {
            let normalized = token.replace('-', "_");
            if personas.iter().any(|p| p == &normalized) {
                let stripped = rest[token.len()..].trim_start_matches([':', ',']).trim().to_owned();
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
        personas.iter().map(|p| p.replace('_', "-")).collect::<Vec<_>>().join(", ")
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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (env, unknown) = from_event_json(&content, "@robb:safehouse.local", &[]);
        assert_eq!(env.v, 1);
        assert_eq!(env.from, "writer_agent");
        assert_eq!(env.to, "research_agent");
        assert_eq!(env.kind, "handoff");
        assert_eq!(env.task_id.as_deref(), Some("source_check"));
        assert_eq!(env.body, "Need the source list confirmed.");
        assert!(unknown.is_none());
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
        let (env, unknown) = from_event_json(&content, "@robb:safehouse.local", &[]);
        assert_eq!(env.from, "@robb:safehouse.local");
        assert_eq!(env.to, "*");
        assert_eq!(env.kind, "chat");
        assert_eq!(env.body, "hmm, the timeline looks off");
        assert!(unknown.is_none());
    }

    #[test]
    fn from_event_json_no_body_field_defaults_to_empty() {
        let content = json!({});
        let (env, _unknown) = from_event_json(&content, "@robb:safehouse.local", &[]);
        assert_eq!(env.body, "");
        assert_eq!(env.to, "*");
    }

    // ---- from_event_json / synthesize_for_human — human synthesis
    // (§5.1 / §5.3) -----------------------------------------------------

    fn personas() -> Vec<String> {
        vec!["research_agent".to_owned(), "writer_agent".to_owned()]
    }

    /// §5.1 — a known persona addresses correctly and strips the token,
    /// unaffected by the unknown-persona handling added alongside it.
    #[test]
    fn known_persona_address_unchanged() {
        let (env, unknown) = synthesize_for_human(
            "@robb:safehouse.local",
            "@research-agent confirm the source list",
            &personas(),
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
        let (env, unknown) =
            synthesize_for_human("@robb:safehouse.local", "@research-agent: hello", &personas());
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
        let (env, unknown) =
            synthesize_for_human("@robb:safehouse.local", "@typo-agent do the thing", &personas());
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
        let (env, unknown) =
            synthesize_for_human("@robb:safehouse.local", "hmm, the timeline looks off", &personas());
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
        let (env, unknown) = synthesize_for_human("@robb:safehouse.local", "@ hello", &personas());
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
        let (env, unknown) = from_event_json(&content, "@safehoused-hosta:server", &personas());
        assert_eq!(env.from, "writer_agent");
        assert_eq!(env.to, "does_not_exist");
        assert!(unknown.is_none());
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
        from_event_json(&json!({"body": body}), "@robb:safehouse.local", &personas()).0
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
}
