//! Envelope v1 — wire format per docs/protocol/envelope-v1.md.
//!
//! The daemon is the only writer of `from` (§6) and the only place rendering
//! happens (§8). Agents never see Matrix event JSON; they see envelopes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const ENVELOPE_KEY: &str = "org.safehouse.envelope";
pub const KNOWN_TYPES: [&str; 4] = ["chat", "task", "handoff", "ack"];

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
pub fn from_event_json(content: &Value, sender: &str, personas: &[String]) -> Envelope {
    if let Some(env) = content.get(ENVELOPE_KEY) {
        if let Ok(env) = serde_json::from_value::<Envelope>(env.clone()) {
            return env;
        }
    }
    let body = content.get("body").and_then(Value::as_str).unwrap_or_default();
    synthesize_for_human(sender, body, personas)
}

fn synthesize_for_human(sender: &str, body: &str, personas: &[String]) -> Envelope {
    if let Some(rest) = body.strip_prefix('@') {
        let token: String = rest
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != ':' && *c != ',')
            .collect();
        let normalized = token.replace('-', "_");
        if personas.iter().any(|p| p == &normalized) {
            let stripped = rest[token.len()..].trim_start_matches([':', ',']).trim().to_owned();
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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
