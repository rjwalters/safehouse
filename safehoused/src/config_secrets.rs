//! Config secrets (issue #215): `*_file` credential references and the
//! redaction behind `safehoused --print-config`.
//!
//! Two halves, both about keeping the four config secrets (`password`,
//! `store_passphrase`, `recovery_passphrase`, and the ingest secret that in
//! practice rides inside `[egress].sink_url`) out of places they should not be:
//!
//! - **Credential references.** Each secret may be given as a literal *or* as
//!   `<name>_file = "/path"`, read once at boot. That is the form that composes
//!   with an age/SOPS-decrypting wrapper or a secret manager without the daemon
//!   knowing about either, and it lets `config.toml` itself be shared/reviewed.
//!   Errors name the field and the file, never the value.
//! - **Redaction.** `--print-config` redacts by key name (including
//!   `passphrase` — the case a `grep -vE "password|token|secret|key"` filter
//!   silently misses), by *value* (a secret echoed under an unexpected key), and
//!   inside URLs (userinfo and sensitive query parameters, the rest of the URL
//!   left readable).
//!
//! This is deliberately **not** `egress::redact`: that one applies an
//! operator's literal deny-pattern list to completion payloads. This one
//! redacts the daemon's own configuration and has nothing configurable.
//!
//! Which fields are secret is declared once, next to `Config`
//! (`crate::SECRET_FIELDS`); a unit test fails if a new field whose name looks
//! sensitive is added without being listed there.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;
use toml::Value;

/// What a redacted value is replaced with.
pub const REDACTED: &str = "REDACTED";

/// A secret shorter than this is only redacted where a value *equals* it, not
/// where it appears as a substring — otherwise a two-character test secret
/// would shred every string that happens to contain those two characters.
/// Equality is always checked regardless of length.
const MIN_SUBSTRING_SECRET_LEN: usize = 6;

/// How a secret field's value is redacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    /// The whole value is the secret (a password, a passphrase).
    Whole,
    /// The value is a URL whose userinfo and sensitive query parameters are
    /// secret; scheme, host and path stay readable (`[egress].sink_url`).
    Url,
}

/// One secret-bearing config field. Every entry also accepts a
/// `<name>_file` sibling holding a path to the secret instead of the literal.
#[derive(Clone, Copy, Debug)]
pub struct SecretField {
    /// Dotted path from the config root, e.g. `"password"` or
    /// `"egress.sink_url"`.
    pub path: &'static str,
    pub kind: SecretKind,
}

/// The key-name rule: a config key (or URL query parameter) whose name contains
/// any of these, case-insensitively, is treated as secret-bearing. `pass`
/// rather than `password` on purpose — it is what makes `store_passphrase` and
/// `recovery_passphrase` match.
const SENSITIVE_NAME_PARTS: &[&str] = &["pass", "secret", "token", "key"];

/// Whether a config key name looks secret-bearing.
pub fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_NAME_PARTS.iter().any(|part| lower.contains(part))
}

/// Whether a URL query/fragment parameter name looks secret-bearing: the
/// key-name rule plus the usual URL-auth spellings (`sig`, `auth`, ...).
fn is_sensitive_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    is_sensitive_name(&lower)
        || lower.contains("auth")
        || lower.contains("credential")
        || lower == "sig"
        || lower == "signature"
}

/// `<name>_file` keys hold a *path*, which is shown (that is the point of a
/// credential reference: the config becomes reviewable). Their names still
/// match the key-name rule (`password_file` contains `pass`), so they are
/// exempted from it explicitly.
fn is_file_reference(name: &str) -> bool {
    name.ends_with("_file")
}

// ---- credential references ---------------------------------------------------

/// Resolve one secret given as a literal and/or a `<name>_file` reference.
///
/// - both set → error (ambiguous; the operator must pick one);
/// - literal only → the literal;
/// - file only → the file's contents (see [`read_secret_file`]); a relative
///   path is resolved against `base_dir` (the config file's directory);
/// - neither → `Ok(None)` — the caller decides whether the secret is optional.
///
/// Every error names the field (and the file), never the value.
pub fn resolve_secret(
    name: &str,
    literal: Option<&str>,
    file: Option<&Path>,
    base_dir: &Path,
) -> Result<Option<String>, String> {
    match (literal, file) {
        (Some(_), Some(_)) => Err(format!(
            "both `{name}` and `{name}_file` are set — set exactly one of them"
        )),
        (Some(value), None) => Ok(Some(value.to_owned())),
        (None, Some(path)) => read_secret_file(name, &base_dir.join(path)).map(Some),
        (None, None) => Ok(None),
    }
}

/// [`resolve_secret`] for a mandatory secret: neither form set is an error.
pub fn require_secret(
    name: &str,
    literal: Option<&str>,
    file: Option<&Path>,
    base_dir: &Path,
) -> Result<String, String> {
    resolve_secret(name, literal, file, base_dir)?.ok_or_else(|| {
        format!("`{name}` is not set — set either `{name}` or `{name}_file` (a path to a file holding it)")
    })
}

/// Read a `<name>_file` secret: the whole file, with **one** trailing newline
/// (`\n` or `\r\n`) stripped, so `echo secret > file` works. Refuses a file
/// that is group- or world-accessible (the same 0600 stance the README takes
/// for `config.toml` itself), that is not a regular file, that is not UTF-8,
/// or whose secret is empty after stripping.
pub fn read_secret_file(name: &str, path: &Path) -> Result<String, String> {
    let shown = path.display();
    let meta =
        fs::metadata(path).map_err(|err| format!("`{name}_file` {shown}: cannot read: {err}"))?;
    if !meta.is_file() {
        return Err(format!("`{name}_file` {shown} is not a regular file"));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "`{name}_file` {shown} is accessible by group/other (mode {mode:03o}) — \
             refusing to use it; `chmod 600` it"
        ));
    }
    let bytes =
        fs::read(path).map_err(|err| format!("`{name}_file` {shown}: cannot read: {err}"))?;
    let mut secret = String::from_utf8(bytes)
        .map_err(|_| format!("`{name}_file` {shown} is not valid UTF-8"))?;
    if secret.ends_with("\r\n") {
        secret.truncate(secret.len() - 2);
    } else if secret.ends_with('\n') {
        secret.truncate(secret.len() - 1);
    }
    if secret.is_empty() {
        return Err(format!("`{name}_file` {shown} is empty"));
    }
    Ok(secret)
}

/// The directory a config file's relative `*_file` paths resolve against.
pub fn config_dir(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

// ---- URL redaction -----------------------------------------------------------

/// Redact a URL's userinfo and any sensitive query/fragment parameter values,
/// leaving everything else byte-for-byte as written:
/// `https://u:pw@h/ingest?key=abc&x=1` → `https://REDACTED@h/ingest?key=REDACTED&x=1`.
/// A string without `://` is returned unchanged.
///
/// Hand-rolled rather than via a URL parser on purpose: a parser normalizes
/// (case, percent-encoding, trailing slashes), and the operator should see the
/// URL they wrote, minus the secrets.
pub fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let authority_start = scheme_end + 3;
    let rest = &url[authority_start..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_len);

    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..authority_start]);
    match authority.rfind('@') {
        Some(at) => {
            out.push_str(REDACTED);
            out.push_str(&authority[at..]);
        }
        None => out.push_str(authority),
    }

    let (before_fragment, fragment) = match tail.find('#') {
        Some(i) => tail.split_at(i),
        None => (tail, ""),
    };
    match before_fragment.find('?') {
        Some(q) => {
            out.push_str(&before_fragment[..=q]);
            out.push_str(&redact_params(&before_fragment[q + 1..]));
        }
        None => out.push_str(before_fragment),
    }
    if let Some(fragment) = fragment.strip_prefix('#') {
        // OAuth-style `#access_token=...` fragments get the same treatment.
        out.push('#');
        out.push_str(&redact_params(fragment));
    }
    out
}

fn redact_params(params: &str) -> String {
    params
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) if is_sensitive_param(name) => format!("{name}={REDACTED}"),
            _ => pair.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// The secret parts of a URL — its userinfo (whole, and the password half) and
/// every sensitive parameter value — for value-based redaction elsewhere.
fn url_secret_parts(url: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let Some(scheme_end) = url.find("://") else {
        return parts;
    };
    let rest = &url[scheme_end + 3..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_len);
    if let Some(at) = authority.rfind('@') {
        let userinfo = &authority[..at];
        parts.push(userinfo.to_owned());
        if let Some((_, password)) = userinfo.split_once(':') {
            parts.push(password.to_owned());
        }
    }
    let (before_fragment, fragment) = tail.split_once('#').unwrap_or((tail, ""));
    let query = before_fragment.split_once('?').map_or("", |(_, q)| q);
    for pair in query.split('&').chain(fragment.split('&')) {
        if let Some((name, value)) = pair.split_once('=') {
            if is_sensitive_param(name) {
                parts.push(value.to_owned());
            }
        }
    }
    parts.retain(|p| !p.is_empty());
    parts
}

// ---- document redaction ------------------------------------------------------

/// The literal secret values in `doc`, for value-based redaction: each
/// [`SecretKind::Whole`] field's value, and the secret parts of each
/// [`SecretKind::Url`] field. `*_file` references contribute nothing — their
/// contents are never read by `--print-config`.
pub fn collect_secret_values(doc: &Value, fields: &[SecretField]) -> Vec<String> {
    let mut secrets = Vec::new();
    for field in fields {
        let mut node = Some(doc);
        for segment in field.path.split('.') {
            node = node.and_then(|n| n.get(segment));
        }
        let Some(Value::String(value)) = node else {
            continue;
        };
        match field.kind {
            SecretKind::Whole => secrets.push(value.clone()),
            SecretKind::Url => secrets.extend(url_secret_parts(value)),
        }
    }
    secrets.retain(|s| !s.is_empty());
    secrets.sort();
    secrets.dedup();
    secrets
}

/// Redact `doc` in place by key name, by value, and inside URLs.
pub fn redact_document(doc: &mut Value, secrets: &[String]) {
    redact_node(doc, false, secrets);
}

fn redact_node(node: &mut Value, under_sensitive_key: bool, secrets: &[String]) {
    match node {
        Value::Table(table) => {
            for (key, value) in table.iter_mut() {
                let sensitive = !is_file_reference(key) && is_sensitive_name(key);
                redact_node(value, under_sensitive_key || sensitive, secrets);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                redact_node(item, under_sensitive_key, secrets);
            }
        }
        Value::String(s) => {
            if under_sensitive_key {
                *s = REDACTED.to_owned();
            } else {
                *s = redact_string(s, secrets);
            }
        }
        Value::Integer(_) | Value::Float(_) | Value::Datetime(_) if under_sensitive_key => {
            *node = Value::String(REDACTED.to_owned());
        }
        _ => {}
    }
}

fn redact_string(s: &str, secrets: &[String]) -> String {
    if secrets.iter().any(|secret| secret == s) {
        return REDACTED.to_owned();
    }
    let mut out = if s.contains("://") {
        redact_url(s)
    } else {
        s.to_owned()
    };
    for secret in secrets {
        if secret.len() >= MIN_SUBSTRING_SECRET_LEN && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), REDACTED);
        }
    }
    out
}

/// Render a parsed document as TOML, redacted unless `redact` is false.
///
/// With redaction on, a final check refuses to return output that still
/// contains any (long-enough-to-check) secret value — defense in depth against
/// a future change to the walk above.
pub fn render_document(
    mut doc: Value,
    fields: &[SecretField],
    redact: bool,
) -> Result<String, String> {
    let secrets = collect_secret_values(&doc, fields);
    if redact {
        redact_document(&mut doc, &secrets);
    }
    let rendered = toml::to_string(&doc).map_err(|err| format!("rendering config: {err}"))?;
    if redact
        && secrets
            .iter()
            .any(|s| s.len() >= MIN_SUBSTRING_SECRET_LEN && rendered.contains(s.as_str()))
    {
        return Err("refusing to print: a secret value survived redaction".to_owned());
    }
    Ok(rendered)
}

// ---- parse errors that do not echo values -------------------------------------

/// Field names a `#[serde(deny_unknown_fields)]` struct accepts, recovered from
/// serde's own "unknown field `x`, expected one of `a`, `b`" message. Used to
/// keep field names readable in a sanitized parse error and by the drift test
/// over `crate::SECRET_FIELDS`. Fails safe: if the message format ever
/// changes this returns nothing, which only makes errors terser.
pub fn struct_field_names<T: DeserializeOwned>() -> Vec<String> {
    const PROBE: &str = "__safehoused_field_probe__";
    let Err(err) = toml::from_str::<T>(&format!("{PROBE} = 0\n")) else {
        return Vec::new();
    };
    let message = err.message();
    let Some((_, expected)) = message.split_once("expected") else {
        return Vec::new();
    };
    backtick_segments(expected)
        .into_iter()
        .filter(|name| name != PROBE)
        .collect()
}

fn backtick_segments(s: &str) -> Vec<String> {
    s.split('`')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, seg)| seg.to_owned())
        .collect()
}

/// A config parse error that never echoes a value from the file.
///
/// `toml::de::Error`'s `Display` quotes the offending source line, and serde's
/// own messages quote values (`invalid type: string "hunter2"`, `unknown
/// variant `hunter2``) — either can put a secret pasted into the wrong field
/// into a terminal, a journal, or an agent transcript. This keeps the line and
/// column, keeps the message, and replaces every quoted or backticked segment
/// with `…` unless it is a known field name or a key actually present in the
/// document (keys are names, not secrets).
pub fn sanitized_parse_error(raw: &str, err: &toml::de::Error, known_names: &[String]) -> String {
    let mut keep: Vec<String> = known_names.to_vec();
    if let Ok(doc) = toml::from_str::<Value>(raw) {
        collect_keys(&doc, &mut keep);
    }
    let message = scrub_quoted(err.message(), &keep);
    match err.span() {
        Some(span) => {
            let (line, column) = line_column(raw, span.start);
            format!("line {line}, column {column}: {message}")
        }
        None => message,
    }
}

fn collect_keys(node: &Value, out: &mut Vec<String>) {
    if let Value::Table(table) = node {
        for (key, value) in table {
            out.push(key.clone());
            collect_keys(value, out);
        }
    }
}

fn scrub_quoted(message: &str, keep: &[String]) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find(['`', '"']) {
        let quote = rest[start..].chars().next().unwrap_or('"');
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find(quote) {
            Some(end) => {
                let segment = &after[..end];
                out.push(quote);
                if keep.iter().any(|k| k == segment) {
                    out.push_str(segment);
                } else {
                    out.push('…');
                }
                out.push(quote);
                rest = &after[end + 1..];
            }
            None => {
                // An unterminated quote: drop everything after it.
                out.push(quote);
                out.push('…');
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn line_column(raw: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(raw.len());
    let before = raw.get(..offset).unwrap_or(raw);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn key_name_rule_matches_passphrase_not_just_password() {
        // The exact gap that leaked two passphrases into a transcript: a
        // `grep -vE "password|token|secret|key"` filter misses `passphrase`.
        for name in [
            "password",
            "store_passphrase",
            "recovery_passphrase",
            "api_key",
            "Access_Token",
            "client_secret",
            "KEY",
        ] {
            assert!(is_sensitive_name(name), "{name} should be sensitive");
        }
        for name in [
            "homeserver",
            "username",
            "state_dir",
            "personas",
            "sink_url",
            "deny_patterns",
        ] {
            assert!(!is_sensitive_name(name), "{name} should not be sensitive");
        }
    }

    #[test]
    fn url_query_secrets_are_redacted_and_the_rest_stays_readable() {
        assert_eq!(
            redact_url("https://h/ingest?key=abc&x=1"),
            "https://h/ingest?key=REDACTED&x=1"
        );
        assert_eq!(
            redact_url(
                "https://h/i?token=t&secret=s&access_token=a&password=p&sig=g&auth=z&page=2"
            ),
            "https://h/i?token=REDACTED&secret=REDACTED&access_token=REDACTED\
             &password=REDACTED&sig=REDACTED&auth=REDACTED&page=2"
        );
    }

    #[test]
    fn url_userinfo_is_redacted() {
        assert_eq!(
            redact_url("https://user:pw@h.example/path?x=1"),
            "https://REDACTED@h.example/path?x=1"
        );
        assert_eq!(redact_url("https://tokenonly@h/"), "https://REDACTED@h/");
    }

    #[test]
    fn url_edge_cases() {
        // No query, no secrets: unchanged.
        assert_eq!(redact_url("https://h/ingest"), "https://h/ingest");
        // Repeated key= — every occurrence redacted.
        assert_eq!(
            redact_url("https://h/?key=a&key=b"),
            "https://h/?key=REDACTED&key=REDACTED"
        );
        // Fragment parameters too.
        assert_eq!(
            redact_url("https://h/cb#access_token=abc&state=1"),
            "https://h/cb#access_token=REDACTED&state=1"
        );
        // Not a URL: unchanged.
        assert_eq!(redact_url("/var/lib/x?key=1"), "/var/lib/x?key=1");
    }

    fn fields() -> Vec<SecretField> {
        vec![
            SecretField {
                path: "password",
                kind: SecretKind::Whole,
            },
            SecretField {
                path: "egress.sink_url",
                kind: SecretKind::Url,
            },
        ]
    }

    #[test]
    fn a_secret_echoed_under_an_unexpected_key_is_redacted_by_value() {
        let doc: Value = toml::from_str(
            "password = \"hunter2-long\"\n\
             motd = \"hunter2-long\"\n\
             note = \"prefix hunter2-long suffix\"\n\
             [egress]\n\
             sink_url = \"https://h/ingest?key=ingest-secret-1&x=1\"\n\
             comment = \"ingest-secret-1\"\n",
        )
        .unwrap();
        let out = render_document(doc, &fields(), true).unwrap();
        assert!(!out.contains("hunter2-long"), "{out}");
        assert!(!out.contains("ingest-secret-1"), "{out}");
        assert!(out.contains("motd = \"REDACTED\""), "{out}");
        assert!(out.contains("prefix REDACTED suffix"), "{out}");
        assert!(out.contains("https://h/ingest?key=REDACTED&x=1"), "{out}");
    }

    #[test]
    fn short_secrets_are_redacted_by_equality_only() {
        let doc: Value = toml::from_str(
            "password = \"pw\"\n\
             other = \"pw\"\n\
             state_dir = \"/tmp/pwd\"\n",
        )
        .unwrap();
        let out = render_document(doc, &fields(), true).unwrap();
        assert!(out.contains("other = \"REDACTED\""), "{out}");
        assert!(out.contains("/tmp/pwd"), "{out}");
    }

    #[test]
    fn file_references_show_the_path_not_redacted() {
        let doc: Value = toml::from_str("password_file = \"/run/secrets/pw\"\n").unwrap();
        let out = render_document(doc, &fields(), true).unwrap();
        assert!(out.contains("password_file = \"/run/secrets/pw\""), "{out}");
    }

    #[test]
    fn no_redact_prints_values_verbatim() {
        let doc: Value = toml::from_str("password = \"hunter2-long\"\n").unwrap();
        let out = render_document(doc, &fields(), false).unwrap();
        assert!(out.contains("hunter2-long"), "{out}");
    }

    #[test]
    fn non_string_values_under_sensitive_keys_are_redacted() {
        let doc: Value =
            toml::from_str("api_key = 123456789\napi_keys = [\"a\", \"b\"]\n").unwrap();
        let out = render_document(doc, &fields(), true).unwrap();
        assert!(!out.contains("123456789"), "{out}");
        assert!(
            out.contains("api_keys = [\"REDACTED\", \"REDACTED\"]"),
            "{out}"
        );
    }

    #[test]
    fn scrub_keeps_known_names_and_hides_everything_else() {
        let keep = vec!["homeserver_mode".to_owned()];
        assert_eq!(
            scrub_quoted(
                "unknown variant `hunter2`, expected `sealed` in `homeserver_mode`",
                &keep
            ),
            "unknown variant `…`, expected `…` in `homeserver_mode`"
        );
        assert_eq!(
            scrub_quoted("invalid type: string \"hunter2\", expected u32", &keep),
            "invalid type: string \"…\", expected u32"
        );
        assert_eq!(scrub_quoted("bad `unterminated", &keep), "bad `…");
    }

    #[test]
    fn line_column_is_one_based() {
        assert_eq!(line_column("a = 1\nbb = 2\n", 0), (1, 1));
        assert_eq!(line_column("a = 1\nbb = 2\n", 9), (2, 4));
    }

    // ---- *_file ------------------------------------------------------------

    fn secret_file(contents: &[u8], mode: u32) -> (tempdir::Dir, PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("secret");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    /// A tiny self-cleaning temp dir — no new dev-dependency for it.
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub struct Dir(PathBuf);
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        impl Dir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "safehoused-secrets-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::SeqCst)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn secret_file_is_read_with_one_trailing_newline_stripped() {
        let (_d, path) = secret_file(b"s3cret\n", 0o600);
        assert_eq!(read_secret_file("password", &path).unwrap(), "s3cret");
        let (_d, path) = secret_file(b"s3cret\r\n", 0o600);
        assert_eq!(read_secret_file("password", &path).unwrap(), "s3cret");
        // Only ONE newline is stripped; no newline at all is fine too.
        let (_d, path) = secret_file(b"s3cret\n\n", 0o600);
        assert_eq!(read_secret_file("password", &path).unwrap(), "s3cret\n");
        let (_d, path) = secret_file(b"s3cret", 0o400);
        assert_eq!(read_secret_file("password", &path).unwrap(), "s3cret");
    }

    #[test]
    fn an_empty_secret_file_is_an_error() {
        let (_d, path) = secret_file(b"\n", 0o600);
        let err = read_secret_file("password", &path).unwrap_err();
        assert!(
            err.contains("password_file") && err.contains("empty"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_secret_file_names_the_field_and_the_file() {
        let err = read_secret_file("store_passphrase", Path::new("/nonexistent/sp")).unwrap_err();
        assert!(err.contains("store_passphrase_file"), "{err}");
        assert!(err.contains("/nonexistent/sp"), "{err}");
    }

    #[test]
    fn a_group_or_world_readable_secret_file_is_refused_without_echoing_it() {
        for mode in [0o640, 0o604, 0o644] {
            let (_d, path) = secret_file(b"do-not-print-me\n", mode);
            let err = read_secret_file("recovery_passphrase", &path).unwrap_err();
            assert!(err.contains("chmod 600"), "{err}");
            assert!(!err.contains("do-not-print-me"), "{err}");
        }
    }

    #[test]
    fn resolve_secret_pairs() {
        let base = Path::new("/");
        let (_d, path) = secret_file(b"from-file\n", 0o600);
        // literal only
        assert_eq!(
            resolve_secret("password", Some("lit"), None, base).unwrap(),
            Some("lit".to_owned())
        );
        // file only
        assert_eq!(
            resolve_secret("password", None, Some(&path), base).unwrap(),
            Some("from-file".to_owned())
        );
        // both: error naming the field, never the value
        let err = resolve_secret("password", Some("lit-value"), Some(&path), base).unwrap_err();
        assert!(
            err.contains("`password`") && err.contains("`password_file`"),
            "{err}"
        );
        assert!(
            !err.contains("lit-value") && !err.contains("from-file"),
            "{err}"
        );
        // neither: optional resolves to None, mandatory is an error
        assert_eq!(resolve_secret("password", None, None, base).unwrap(), None);
        let err = require_secret("password", None, None, base).unwrap_err();
        assert!(err.contains("password_file"), "{err}");
    }

    #[test]
    fn a_relative_secret_file_resolves_against_the_config_dir() {
        let (dir, _path) = secret_file(b"rel\n", 0o600);
        assert_eq!(
            resolve_secret("password", None, Some(Path::new("secret")), dir.path()).unwrap(),
            Some("rel".to_owned())
        );
        assert_eq!(config_dir(Path::new("config.toml")), PathBuf::from("."));
        assert_eq!(
            config_dir(Path::new("/etc/s/config.toml")),
            PathBuf::from("/etc/s")
        );
    }
}
