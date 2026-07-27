//! Envelope v1 — wire format per docs/protocol/envelope-v1.md.
//!
//! The daemon is the only writer of `from` (§6) and the only place rendering
//! happens (§8). Agents never see Matrix event JSON; they see envelopes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const ENVELOPE_KEY: &str = "org.safehouse.envelope";
pub const KNOWN_TYPES: [&str; 4] = ["chat", "task", "handoff", "ack"];

/// The single envelope version this daemon understands (§9). Receivers MUST
/// ignore envelopes with any other `v` rather than guessing (§7.2).
pub const SUPPORTED_VERSION: u32 = 1;

/// The result of interpreting an inbound room event (§7 dispatch).
#[derive(Clone, Debug)]
pub enum Inbound {
    /// A supported-version envelope, or one synthesized for a human message
    /// (§5). Safe to dispatch to local agents.
    Envelope(Envelope),
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
/// Version is gated **explicitly** (§7.2, §9): when the event carries an
/// envelope whose `v` is not [`SUPPORTED_VERSION`], this returns
/// [`Inbound::UnsupportedVersion`] rather than guess-parsing it or falling back
/// to human-message synthesis. Guessing would let a future `v: 2` envelope be
/// dispatched as though the daemon user were a human speaker.
pub fn from_event_json(content: &Value, sender: &str, personas: &[String]) -> Inbound {
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
            return Inbound::Envelope(env);
        }
    }
    let body = content
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Inbound::Envelope(synthesize_for_human(sender, body, personas))
}

fn synthesize_for_human(sender: &str, body: &str, personas: &[String]) -> Envelope {
    if let Some(rest) = body.strip_prefix('@') {
        let token: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ':' && *c != ',')
            .collect();
        let normalized = token.replace('-', "_");
        if personas.iter().any(|p| p == &normalized) {
            let stripped = rest[token.len()..]
                .trim_start_matches([':', ','])
                .trim()
                .to_owned();
            return Envelope {
                v: 1,
                from: sender.to_owned(),
                to: normalized,
                kind: "chat".to_owned(),
                task_id: None,
                body: stripped,
            };
        }
    }
    // §5.3 — unaddressed human message: broadcast, wakes no one.
    Envelope {
        v: 1,
        from: sender.to_owned(),
        to: "*".to_owned(),
        kind: "chat".to_owned(),
        task_id: None,
        body: body.to_owned(),
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
        match from_event_json(&content, "@robb:safehouse.local", &personas()) {
            Inbound::Envelope(env) => {
                assert_eq!(env.v, SUPPORTED_VERSION);
                assert_eq!(env.from, "writer_agent");
                assert_eq!(env.to, "research_agent");
                assert_eq!(env.kind, "handoff");
                assert_eq!(env.body, "confirm sources");
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn future_version_is_not_dispatched() {
        // Regression for #6: a well-formed-but-future envelope must NOT be
        // guess-parsed or synthesized as a human message — it is gated on `v`.
        let content = envelope_content(2);
        match from_event_json(&content, "@robb:safehouse.local", &personas()) {
            Inbound::UnsupportedVersion(v) => assert_eq!(v, 2),
            other => panic!("expected UnsupportedVersion(2), got {other:?}"),
        }
    }

    #[test]
    fn large_future_version_preserved_without_truncation() {
        let big = u64::from(u32::MAX) + 5;
        let content = envelope_content(big);
        match from_event_json(&content, "@robb:safehouse.local", &personas()) {
            Inbound::UnsupportedVersion(v) => assert_eq!(v, big),
            other => panic!("expected UnsupportedVersion({big}), got {other:?}"),
        }
    }

    #[test]
    fn addressed_human_message_synthesizes_envelope() {
        let content =
            json!({ "msgtype": "m.text", "body": "@research-agent confirm the source list" });
        match from_event_json(&content, "@robb:safehouse.local", &personas()) {
            Inbound::Envelope(env) => {
                assert_eq!(env.v, SUPPORTED_VERSION);
                assert_eq!(env.from, "@robb:safehouse.local");
                assert_eq!(env.to, "research_agent");
                assert_eq!(env.kind, "chat");
                assert_eq!(env.body, "confirm the source list");
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }

    #[test]
    fn unaddressed_human_message_broadcasts() {
        let content = json!({ "msgtype": "m.text", "body": "hmm, the timeline looks off" });
        match from_event_json(&content, "@robb:safehouse.local", &personas()) {
            Inbound::Envelope(env) => {
                assert_eq!(env.to, "*");
                assert_eq!(env.kind, "chat");
                assert_eq!(env.body, "hmm, the timeline looks off");
            }
            other => panic!("expected Envelope, got {other:?}"),
        }
    }
}
