//! Guards the sealed-homeserver invariant this whole onboarding tool exists
//! to preserve (issue #94): `allow_registration` stays `false`, and account
//! creation goes through admin-room automation instead of self-service
//! registration. Rather than re-typing the config snippet here (drifting
//! silently from the one the operator actually deploys), this test parses
//! the documented config straight out of `docs/research/
//! 2026-07-26-homeserver.md`'s "## Config" fenced block — so a future edit
//! that flips the value (or drops the key) fails a test instead of only
//! being caught by an operator reading prose.

use std::{fs, path::PathBuf};

/// Extracts the first ```toml fenced code block from a markdown document.
fn first_toml_fence(markdown: &str) -> &str {
    let start = markdown
        .find("```toml")
        .expect("expected a ```toml fenced block in the doc")
        + "```toml".len();
    let rest = &markdown[start..];
    let end = rest.find("```").expect("unterminated ```toml fence");
    rest[..end].trim()
}

#[test]
fn documented_homeserver_config_keeps_registration_closed() {
    let doc_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/research/2026-07-26-homeserver.md");
    let markdown = fs::read_to_string(&doc_path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", doc_path.display()));
    let toml_block = first_toml_fence(&markdown);

    let parsed: toml::Value = toml::from_str(toml_block)
        .unwrap_or_else(|e| panic!("parsing documented config as TOML: {e}\n---\n{toml_block}"));
    let allow_registration = parsed
        .get("global")
        .and_then(|g| g.get("allow_registration"))
        .and_then(toml::Value::as_bool)
        .expect("documented config must set [global].allow_registration");

    assert!(
        !allow_registration,
        "docs/research/2026-07-26-homeserver.md now documents allow_registration = true — \
         this breaks the sealed-homeserver invariant issue #94's admin-room onboarding path \
         depends on (accounts are minted explicitly via the admin room, never self-registered)"
    );
}
