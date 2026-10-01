//! Local voice-note transcription (#200) — the opt-in step that turns an
//! `m.audio` event into text an agent can act on.
//!
//! Without this, an agent behind safehoused receives the voice note's *file
//! name* ("Voice message.ogg") as the envelope body, because envelope
//! synthesis (§5) reads the Matrix `body` field and for a media event that is
//! all the sender's client puts there. safehoused is the only component that
//! can do better: it holds the room keys, so it is the only one that can
//! download and decrypt the attachment.
//!
//! Every design choice here is fail-safe, mirroring `egress.rs`:
//!
//! - **Off by default.** With no `[transcribe]` block in config nothing in
//!   this module ever runs and the daemon behaves byte-for-byte as before
//!   (see [`Config`](crate::Config)).
//! - **Local subprocess only.** The configured `command` is executed as a
//!   local process with the decrypted audio on **stdin**; this module never
//!   opens a network connection of its own. That is load-bearing, not
//!   stylistic: the audio arrived end-to-end encrypted, and handing it to a
//!   hosted transcription API would undo that. A hosted backend is therefore
//!   something an operator must build deliberately (by pointing `command` at
//!   their own wrapper), never something the daemon does implicitly. The
//!   recommended transcriber is whisper.cpp on the host.
//! - **Bounded, four ways — with two caveats.** A size cap (`max_bytes`), a
//!   duration cap (`max_seconds`), a wall-clock cap (`timeout_seconds`), and
//!   a single-flight slot so a burst of voice notes can never fan out into N
//!   concurrent transcriber processes on a 2-vCPU host. The caveats:
//!   - *Sync-loop stall.* `on_message` awaits transcription inline, and
//!     matrix-sdk 0.18 awaits every event-handler future to completion inside
//!     sync-response processing (`event_handler::call_event_handlers`) before
//!     the next `/sync` is issued. So while a voice note is transcribing the
//!     daemon processes **no event in any room**, not merely no other voice
//!     notes. `timeout_seconds` is applied to three *sequential* stages —
//!     download, single-flight slot wait, subprocess — so the worst case per
//!     voice note is up to ~3x `timeout_seconds` (6 minutes at the default
//!     120) of daemon-wide stall. Because handler invocations are already
//!     serialized by the sync loop, the single-flight slot is in practice only
//!     observable in tests; it is not what bounds concurrency in production.
//!   - *`max_bytes` is not a pre-download memory ceiling.* The pre-download
//!     check uses the sender-controlled `info.size` and only fires when that
//!     is present; a sender can omit or understate it. matrix-sdk 0.18's
//!     `Media::get_file` has no streaming or size-capped variant, so the whole
//!     attachment is buffered before the post-download `max_bytes` check can
//!     reject it. The real memory bound in that case is the homeserver's own
//!     media-upload size limit.
//! - **Never a silent drop.** Every failure path — download/decrypt error,
//!   missing binary, non-zero exit, timeout, empty output, over-cap audio —
//!   falls back to today's behaviour (the file name) **plus a visible note
//!   saying transcription did not happen and why ([`fallback_body`]). The
//!   message itself is always delivered.
//!
//! ## What the configured command receives
//!
//! argv exactly as configured (with the optional `{max_seconds}` /
//! `{max_millis}` placeholders substituted, see [`render_command`]), the raw
//! decrypted media bytes on stdin, and nothing else. Whatever it writes to
//! stdout is the transcript; stderr is used only to explain a non-zero exit.
//! Container/codec handling is the command's business — a voice note is
//! typically Opus-in-Ogg, which `whisper-cli` cannot read directly, so in
//! practice `command` points at a small wrapper that pipes stdin through
//! `ffmpeg` into whisper.cpp.
//!
//! ## The duration cap, honestly
//!
//! Trimming an encoded Opus/Ogg stream is a container-aware operation this
//! daemon deliberately does not attempt. So the `max_seconds` cap is applied
//! one of two ways, and the resulting body always says which:
//!
//! - if `command` contains a `{max_seconds}` / `{max_millis}` placeholder, it
//!   is substituted and the transcriber does the trimming (whisper.cpp's
//!   `-d <ms>`), and the body says `first 10:00 transcribed`;
//! - otherwise over-long audio is **not** transcribed at all and the body
//!   says so. Silently transcribing an hour of audio because the config could
//!   not express a limit is exactly the unbounded work the cap exists to
//!   prevent.

use std::{future::Future, process::Stdio, sync::Arc, time::Duration};

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tokio::{io::AsyncWriteExt as _, process::Command, sync::Mutex, time::timeout};

/// Hard ceiling on the transcript length folded into an envelope body. A
/// mis-configured command (`cat` on a 20 MiB file, a transcriber echoing its
/// own debug log) must not be able to push an arbitrarily large body into
/// every connected agent's mailbox.
pub const MAX_TRANSCRIPT_CHARS: usize = 16_000;

/// Default download cap: 25 MiB, comfortably above any plausible voice note
/// (minutes of Opus) and well below "someone dropped an album in the room".
pub const DEFAULT_MAX_BYTES: u64 = 25 * 1024 * 1024;

fn default_max_seconds() -> u64 {
    600
}

fn default_timeout_seconds() -> u64 {
    120
}

fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}

/// The optional `[transcribe]` block on the daemon [`Config`](crate::Config).
/// Absent = the whole subsystem is disabled (zero behavior change). Same flat,
/// explicit, `deny_unknown_fields` style as [`EgressConfig`](crate::egress::EgressConfig).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscribeConfig {
    /// argv of the local transcriber. `command[0]` must be an **absolute
    /// path** — a PATH-resolved name is refused at boot so what runs against
    /// decrypted audio can never depend on the daemon's ambient environment.
    /// Receives the media bytes on stdin and writes the transcript to stdout.
    pub command: Vec<String>,
    /// Duration cap. Audio longer than this is either trimmed by the command
    /// (when `command` carries a `{max_seconds}`/`{max_millis}` placeholder)
    /// or not transcribed at all — see the module docs. Never silently
    /// partial.
    #[serde(default = "default_max_seconds")]
    pub max_seconds: u64,
    /// Wall-clock cap on the transcriber subprocess, and on the media
    /// download, and on how long a queued voice note waits for the
    /// single-flight slot. A transcriber that exceeds it is killed.
    ///
    /// These are three *sequential* bounds, and transcription is awaited
    /// inline in `on_message`, which blocks the matrix-sdk sync loop (no
    /// event in any room is processed meanwhile). The worst-case stall per
    /// voice note is therefore up to ~3x this value, not 1x. The single-flight
    /// slot only contends in tests: the sync loop already serializes handlers.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Size cap, checked against the event's advertised `info.size` *before*
    /// downloading (only when the sender included it) and against the real
    /// byte count after. `info.size` is sender-controlled and can be omitted
    /// or understated, in which case the whole attachment is buffered in
    /// memory first (matrix-sdk 0.18 has no streaming/size-capped download);
    /// the real memory bound is then the homeserver's upload limit, so this is
    /// not a hard pre-download memory ceiling.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// Also post the transcript back into the room as a threaded notice under
    /// the voice note, so the humans see what the agents saw. Off by default.
    #[serde(default)]
    pub post_transcript: bool,
}

/// Boot-time fail-safe guards, mirroring
/// [`validate_egress_config`](crate::egress::validate_egress_config): a
/// `[transcribe]` block that cannot possibly work must fail the boot rather
/// than leave the daemon running a silently-broken feature that only shows up
/// as a "transcription failed" note on every voice note.
pub fn validate_transcribe_config(cfg: &TranscribeConfig) -> std::result::Result<(), String> {
    let Some(program) = cfg.command.first() else {
        return Err(
            "transcribe.command is empty — configure the transcriber argv \
                    (e.g. [\"/usr/local/bin/whisper-wrapper\"])"
                .to_owned(),
        );
    };
    if !program.starts_with('/') {
        return Err(format!(
            "transcribe.command[0] ({program:?}) is not an absolute path — refusing to resolve \
             the transcriber through PATH, since it is handed decrypted end-to-end-encrypted audio"
        ));
    }
    if cfg.timeout_seconds == 0 {
        return Err(
            "transcribe.timeout_seconds is 0 — a transcription with no time bound \
                    could hang a voice note's delivery indefinitely"
                .to_owned(),
        );
    }
    if cfg.max_seconds == 0 {
        return Err(
            "transcribe.max_seconds is 0 — no audio would ever be eligible; omit the \
             [transcribe] block instead to disable transcription"
                .to_owned(),
        );
    }
    if cfg.max_bytes == 0 {
        return Err(
            "transcribe.max_bytes is 0 — no audio would ever be eligible; omit the \
             [transcribe] block instead to disable transcription"
                .to_owned(),
        );
    }
    Ok(())
}

/// What this daemon needs to know about an `m.audio` event, read straight from
/// the raw decrypted event `content` rather than via ruma's typed
/// `AudioMessageEventContent`: the MSC3245 voice marker lives behind an
/// unstable cargo feature there, and the rest (file name, duration, size) is
/// plain JSON. Keeps the parsing pure and unit-testable, the same way
/// `envelope.rs` works over `Value`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioNote {
    /// The Matrix `body` — for a media event, the file name. This is exactly
    /// what an agent sees today, and what the fallback body preserves.
    pub filename: String,
    /// `info.duration`, in milliseconds. Absent on clients that omit it.
    pub duration_ms: Option<u64>,
    /// `info.size`, in bytes, as advertised by the sender.
    pub size_bytes: Option<u64>,
    /// Whether the event carries MSC3245's `org.matrix.msc3245.voice` marker
    /// — i.e. it is a voice note rather than an attached audio file. Only
    /// affects wording.
    pub is_voice: bool,
}

impl AudioNote {
    /// `"voice note"` / `"audio"` — the noun used in the synthesized body.
    fn label(&self) -> &'static str {
        if self.is_voice {
            "voice note"
        } else {
            "audio"
        }
    }

    fn duration_secs(&self) -> Option<u64> {
        // Round to the nearest second for display; a 41_500 ms note reads
        // "0:42", not "0:41".
        self.duration_ms.map(|ms| (ms + 500) / 1000)
    }
}

/// `Some` iff `content` is an `m.audio` message. Never looks at anything else:
/// an `m.text`/`m.image`/envelope-carrying event returns `None` and the
/// caller's behavior is unchanged.
pub fn audio_note_from_content(content: &Value) -> Option<AudioNote> {
    if content.get("msgtype").and_then(Value::as_str) != Some("m.audio") {
        return None;
    }
    let info = content.get("info");
    Some(AudioNote {
        filename: content
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        duration_ms: info.and_then(|i| i.get("duration")).and_then(Value::as_u64),
        size_bytes: info.and_then(|i| i.get("size")).and_then(Value::as_u64),
        is_voice: content.get("org.matrix.msc3245.voice").is_some(),
    })
}

/// What to do with an `m.audio` event, decided **before** anything is
/// downloaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Download and transcribe. `truncate_to_seconds` is `Some` when the
    /// duration cap applies and the configured command can honour it via a
    /// placeholder — the body then says the transcript is partial.
    Run { truncate_to_seconds: Option<u64> },
    /// Do not download at all; deliver the fallback body carrying this
    /// reason.
    Skip(String),
}

/// Apply the pre-download caps. Unknown duration/size are *not* treated as
/// over-cap: the sender simply omitted the field, and the post-download byte
/// check still bounds the real work.
pub fn plan(cfg: &TranscribeConfig, note: &AudioNote) -> Plan {
    if let Some(size) = note.size_bytes {
        if size > cfg.max_bytes {
            return Plan::Skip(format!(
                "not transcribed: {size} bytes exceeds the {} byte transcribe.max_bytes cap",
                cfg.max_bytes
            ));
        }
    }
    if let Some(secs) = note.duration_secs() {
        if secs > cfg.max_seconds {
            if command_can_truncate(cfg) {
                return Plan::Run {
                    truncate_to_seconds: Some(cfg.max_seconds),
                };
            }
            return Plan::Skip(format!(
                "not transcribed: {} is longer than the {} transcribe.max_seconds cap, and \
                 transcribe.command has no {{max_seconds}}/{{max_millis}} placeholder to \
                 trim with",
                format_duration(secs),
                format_duration(cfg.max_seconds)
            ));
        }
    }
    Plan::Run {
        truncate_to_seconds: None,
    }
}

/// `0:42`, `10:00`, `1:02:03`.
pub fn format_duration(total_seconds: u64) -> String {
    let (h, m, s) = (
        total_seconds / 3600,
        (total_seconds % 3600) / 60,
        total_seconds % 60,
    );
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The success body: `🎙 (voice note, 0:42) <transcript>`, per the issue's
/// sketch. A truncated transcript says so; an unknown duration just omits it.
pub fn transcript_body(
    note: &AudioNote,
    transcript: &str,
    truncated_to_seconds: Option<u64>,
) -> String {
    let mut parts = vec![note.label().to_owned()];
    if let Some(secs) = note.duration_secs() {
        parts.push(format_duration(secs));
    }
    if let Some(limit) = truncated_to_seconds {
        parts.push(format!("first {} transcribed", format_duration(limit)));
    }
    format!("🎙 ({}) {}", parts.join(", "), transcript.trim())
}

/// The fallback body: today's behavior (the file name) **plus** a visible note
/// that transcription did not happen, and why. Never a silent drop.
pub fn fallback_body(note: &AudioNote, reason: &str) -> String {
    let mut head = note.label().to_owned();
    if let Some(secs) = note.duration_secs() {
        head.push_str(&format!(", {}", format_duration(secs)));
    }
    format!("🎙 ({head} — {reason}) {}", note.filename)
}

/// The outcome of one voice note. Either way a body is produced and the
/// message is delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transcription {
    /// Transcription succeeded; `body` replaces the Matrix `body` for
    /// envelope synthesis and `transcript` is the bare text (used for the
    /// optional `post_transcript` notice).
    Transcribed { body: String, transcript: String },
    /// Transcription was skipped or failed; `body` is the file name plus a
    /// visible note. `reason` is also logged.
    Fallback { body: String, reason: String },
}

impl Transcription {
    /// The body to synthesize the envelope from, whichever outcome this is.
    pub fn body(&self) -> &str {
        match self {
            Transcription::Transcribed { body, .. } | Transcription::Fallback { body, .. } => body,
        }
    }

    fn fallback(note: &AudioNote, reason: String) -> Self {
        Transcription::Fallback {
            body: fallback_body(note, &reason),
            reason,
        }
    }
}

/// Fold an outcome into the event `content` that envelope synthesis will read,
/// in place. Kept here (rather than inline in `on_message`) so the rewrite
/// itself is unit-testable without a `Client` or a homeserver.
///
/// Rewrites `body`, and drops `formatted_body`/`format`: an `m.audio` event's
/// HTML caption, if any, described the *file*, and leaving it would have
/// `envelope::matrix_meta_from_content` hand agents markup that contradicts
/// the body they were given. Nothing else about the content is touched — the
/// `url`/`file`/`info` keys stay exactly as the sender wrote them, and the
/// original room event is of course never modified at all.
pub fn apply_to_content(content: &mut Value, outcome: &Transcription) {
    let Some(map) = content.as_object_mut() else {
        return;
    };
    map.insert("body".to_owned(), Value::String(outcome.body().to_owned()));
    map.remove("formatted_body");
    map.remove("format");
}

/// Whether the configured command can itself honour the duration cap.
fn command_can_truncate(cfg: &TranscribeConfig) -> bool {
    cfg.command
        .iter()
        .any(|arg| arg.contains("{max_seconds}") || arg.contains("{max_millis}"))
}

/// Substitute the duration-cap placeholders into argv. With no truncation in
/// play the placeholders still resolve (to the configured cap) so a command
/// written with `-d {max_millis}` behaves consistently on short audio too.
pub fn render_command(cfg: &TranscribeConfig, truncate_to_seconds: Option<u64>) -> Vec<String> {
    let secs = truncate_to_seconds.unwrap_or(cfg.max_seconds);
    cfg.command
        .iter()
        .map(|arg| {
            arg.replace("{max_seconds}", &secs.to_string())
                .replace("{max_millis}", &(secs * 1000).to_string())
        })
        .collect()
}

/// Trim and bound a transcriber's stdout.
fn clamp_transcript(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.chars().count() <= MAX_TRANSCRIPT_CHARS {
        return trimmed.to_owned();
    }
    let kept: String = trimmed.chars().take(MAX_TRANSCRIPT_CHARS).collect();
    format!("{kept}… (transcript truncated)")
}

/// The transcription runtime: config plus the single-flight slot. Constructed
/// only when `[transcribe]` is present in config, and cloned into the
/// event-dispatch path as an `Option<Arc<_>>` so the disabled case stays a
/// pure no-op.
pub struct Transcriber {
    config: TranscribeConfig,
    /// Single-flight slot. One transcriber process at a time, daemon-wide —
    /// the acceptance criterion, and the reason a burst of voice notes cannot
    /// swamp a small host. Waiting for it is bounded by `timeout_seconds`.
    slot: Mutex<()>,
}

impl Transcriber {
    /// Validate config and build the runtime. Fails the boot on a config that
    /// cannot work (`Egress::open`'s precedent).
    pub fn open(config: TranscribeConfig) -> Result<Arc<Self>> {
        validate_transcribe_config(&config)
            .map_err(|e| anyhow::anyhow!("invalid transcribe config: {e}"))?;
        Ok(Arc::new(Self {
            config,
            slot: Mutex::new(()),
        }))
    }

    /// Whether to echo the transcript back into the room (config
    /// `post_transcript`).
    pub fn post_transcript(&self) -> bool {
        self.config.post_transcript
    }

    /// One-line boot summary for the startup log.
    pub fn describe(&self) -> String {
        format!(
            "{} (max {}, timeout {}s, max {} bytes{})",
            self.config.command.join(" "),
            format_duration(self.config.max_seconds),
            self.config.timeout_seconds,
            self.config.max_bytes,
            if self.config.post_transcript {
                ", posting transcripts to the room"
            } else {
                ""
            }
        )
    }

    /// The whole pipeline for one `m.audio` event: caps, download (via the
    /// caller's `fetch`, which is where matrix-sdk's media API lives — kept
    /// out of this module so the pipeline is testable without a homeserver),
    /// post-download size check, then the single-flight subprocess.
    ///
    /// Infallible by construction: every error becomes a
    /// [`Transcription::Fallback`] carrying a deliverable body.
    pub async fn transcribe<F, Fut>(&self, note: &AudioNote, fetch: F) -> Transcription
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = std::result::Result<Vec<u8>, String>>,
    {
        let truncate_to_seconds = match plan(&self.config, note) {
            Plan::Skip(reason) => return Transcription::fallback(note, reason),
            Plan::Run {
                truncate_to_seconds,
            } => truncate_to_seconds,
        };

        let bound = Duration::from_secs(self.config.timeout_seconds);
        let audio = match timeout(bound, fetch()).await {
            Err(_) => {
                return Transcription::fallback(
                    note,
                    format!(
                        "transcription failed: downloading the audio took longer than {}s",
                        self.config.timeout_seconds
                    ),
                )
            }
            Ok(Err(err)) => {
                return Transcription::fallback(
                    note,
                    format!("transcription failed: could not fetch/decrypt the audio: {err}"),
                )
            }
            Ok(Ok(bytes)) => bytes,
        };

        if audio.is_empty() {
            return Transcription::fallback(
                note,
                "transcription failed: the decrypted audio was empty".to_owned(),
            );
        }
        if audio.len() as u64 > self.config.max_bytes {
            return Transcription::fallback(
                note,
                format!(
                    "not transcribed: downloaded {} bytes, over the {} byte \
                     transcribe.max_bytes cap",
                    audio.len(),
                    self.config.max_bytes
                ),
            );
        }

        match self.run_command(audio, truncate_to_seconds).await {
            Ok(transcript) if !transcript.is_empty() => Transcription::Transcribed {
                body: transcript_body(note, &transcript, truncate_to_seconds),
                transcript,
            },
            Ok(_) => Transcription::fallback(
                note,
                "transcription failed: the transcriber produced no text".to_owned(),
            ),
            Err(err) => Transcription::fallback(note, format!("transcription failed: {err}")),
        }
    }

    /// Run the configured command with `audio` on stdin, one at a time,
    /// under the wall-clock bound. Returns the trimmed, length-clamped
    /// stdout.
    async fn run_command(
        &self,
        audio: Vec<u8>,
        truncate_to_seconds: Option<u64>,
    ) -> std::result::Result<String, String> {
        let argv = render_command(&self.config, truncate_to_seconds);
        let program = argv
            .first()
            .cloned()
            .ok_or_else(|| "transcribe.command is empty".to_owned())?;
        let bound = Duration::from_secs(self.config.timeout_seconds);

        // Single-flight. Waiting is bounded too: a backlog must degrade to
        // "delivered with a note" rather than pile up unboundedly behind a
        // slow transcriber.
        let _slot = timeout(bound, self.slot.lock()).await.map_err(|_| {
            format!(
                "another transcription still held the single-flight slot after {}s",
                self.config.timeout_seconds
            )
        })?;

        let mut child = Command::new(&program)
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A timed-out transcriber is dropped below; without this it would
            // be left running (and still holding a core) after we gave up.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not run {program}: {e}"))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("{program} has no stdin pipe"))?;
        // Written from a separate task so a transcriber that streams output
        // while we are still feeding it cannot deadlock on a full pipe. Write
        // errors are deliberately ignored: a command that does not read stdin
        // (EPIPE) is the command's business — its exit status and stdout are
        // what we judge it on.
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&audio).await;
            let _ = stdin.shutdown().await;
        });

        let output = match timeout(bound, child.wait_with_output()).await {
            Err(_) => {
                writer.abort();
                return Err(format!(
                    "{program} exceeded the {}s transcribe.timeout_seconds cap (killed)",
                    self.config.timeout_seconds
                ));
            }
            Ok(Err(e)) => {
                writer.abort();
                return Err(format!("{program} failed: {e}"));
            }
            Ok(Ok(output)) => output,
        };
        writer.abort();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.lines().next().unwrap_or("").trim().to_owned();
            return Err(format!(
                "{program} exited with {}{}",
                output.status,
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
        }
        Ok(clamp_transcript(&String::from_utf8_lossy(&output.stdout)))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cfg(command: &[&str]) -> TranscribeConfig {
        TranscribeConfig {
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            max_seconds: 600,
            timeout_seconds: 10,
            max_bytes: DEFAULT_MAX_BYTES,
            post_transcript: false,
        }
    }

    fn voice_note(duration_ms: Option<u64>, size: Option<u64>) -> AudioNote {
        AudioNote {
            filename: "Voice message.ogg".to_owned(),
            duration_ms,
            size_bytes: size,
            is_voice: true,
        }
    }

    // --- config validation (boot-time fail-safe) ------------------------

    #[test]
    fn empty_command_is_a_boot_error() {
        assert!(validate_transcribe_config(&cfg(&[])).is_err());
    }

    #[test]
    fn path_resolved_command_is_a_boot_error() {
        // The command is handed decrypted E2EE audio; it must not be
        // whatever PATH happens to resolve "whisper-cli" to.
        assert!(validate_transcribe_config(&cfg(&["whisper-cli", "-f", "-"])).is_err());
        assert!(
            validate_transcribe_config(&cfg(&["/usr/local/bin/whisper-cli", "-f", "-"])).is_ok()
        );
    }

    #[test]
    fn zero_bounds_are_boot_errors() {
        for mutate in [
            |c: &mut TranscribeConfig| c.timeout_seconds = 0,
            |c: &mut TranscribeConfig| c.max_seconds = 0,
            |c: &mut TranscribeConfig| c.max_bytes = 0,
        ] {
            let mut config = cfg(&["/bin/cat"]);
            mutate(&mut config);
            assert!(
                validate_transcribe_config(&config).is_err(),
                "an unbounded transcription must not boot"
            );
        }
    }

    // --- m.audio detection ----------------------------------------------

    #[test]
    fn non_audio_content_is_ignored() {
        // The regression that keeps every other message path untouched.
        assert!(audio_note_from_content(&json!({ "msgtype": "m.text", "body": "hi" })).is_none());
        assert!(audio_note_from_content(&json!({ "body": "no msgtype" })).is_none());
        assert!(
            audio_note_from_content(&json!({ "msgtype": "m.image", "body": "a.png" })).is_none()
        );
    }

    #[test]
    fn voice_note_content_is_parsed() {
        let note = audio_note_from_content(&json!({
            "msgtype": "m.audio",
            "body": "Voice message.ogg",
            "info": { "duration": 41_500, "size": 12_345, "mimetype": "audio/ogg" },
            "org.matrix.msc3245.voice": {},
        }))
        .expect("m.audio is recognized");
        assert_eq!(note.filename, "Voice message.ogg");
        assert_eq!(note.duration_ms, Some(41_500));
        assert_eq!(note.size_bytes, Some(12_345));
        assert!(note.is_voice);
    }

    #[test]
    fn plain_audio_attachment_is_parsed_without_the_voice_marker() {
        let note = audio_note_from_content(&json!({
            "msgtype": "m.audio",
            "body": "interview.m4a",
        }))
        .expect("m.audio without info is still recognized");
        assert!(!note.is_voice);
        assert_eq!(note.duration_ms, None);
        assert_eq!(note.size_bytes, None);
    }

    // --- caps (decided before any download) -----------------------------

    #[test]
    fn oversized_audio_is_skipped_before_download() {
        let plan = plan(
            &cfg(&["/bin/cat"]),
            &voice_note(Some(1_000), Some(99_999_999)),
        );
        match plan {
            Plan::Skip(reason) => assert!(reason.contains("max_bytes"), "{reason}"),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn overlong_audio_truncates_when_the_command_takes_a_placeholder() {
        let mut config = cfg(&["/bin/whisper", "-d", "{max_millis}"]);
        config.max_seconds = 60;
        assert_eq!(
            plan(&config, &voice_note(Some(600_000), None)),
            Plan::Run {
                truncate_to_seconds: Some(60)
            }
        );
        // ...and the placeholder is substituted with that cap.
        assert_eq!(
            render_command(&config, Some(60)),
            vec![
                "/bin/whisper".to_owned(),
                "-d".to_owned(),
                "60000".to_owned()
            ]
        );
    }

    #[test]
    fn overlong_audio_without_a_placeholder_is_skipped_not_silently_partial() {
        let mut config = cfg(&["/bin/whisper", "-f", "-"]);
        config.max_seconds = 60;
        match plan(&config, &voice_note(Some(600_000), None)) {
            Plan::Skip(reason) => {
                assert!(reason.contains("10:00"), "{reason}");
                assert!(reason.contains("1:00"), "{reason}");
            }
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn unknown_duration_and_size_still_run() {
        assert_eq!(
            plan(&cfg(&["/bin/cat"]), &voice_note(None, None)),
            Plan::Run {
                truncate_to_seconds: None
            }
        );
    }

    // --- body formatting -------------------------------------------------

    #[test]
    fn durations_render_as_clock_time() {
        assert_eq!(format_duration(42), "0:42");
        assert_eq!(format_duration(600), "10:00");
        assert_eq!(format_duration(3_723), "1:02:03");
    }

    #[test]
    fn transcript_body_matches_the_issues_sketch() {
        assert_eq!(
            transcript_body(&voice_note(Some(41_500), None), "  ship it  ", None),
            "🎙 (voice note, 0:42) ship it"
        );
    }

    #[test]
    fn truncated_transcript_body_says_so() {
        let body = transcript_body(&voice_note(Some(900_000), None), "part one", Some(600));
        assert!(body.contains("first 10:00 transcribed"), "{body}");
    }

    #[test]
    fn fallback_body_keeps_the_filename_and_names_the_failure() {
        let body = fallback_body(
            &voice_note(Some(41_500), None),
            "transcription failed: /bin/whisper exited with exit status: 1",
        );
        // Today's behavior (the filename) is preserved...
        assert!(body.contains("Voice message.ogg"), "{body}");
        // ...plus a visible marker that transcription did not happen.
        assert!(body.contains("transcription failed"), "{body}");
    }

    // --- the pipeline ----------------------------------------------------

    async fn fetch_ok(bytes: &'static [u8]) -> std::result::Result<Vec<u8>, String> {
        Ok(bytes.to_vec())
    }

    #[tokio::test]
    async fn successful_transcription_replaces_the_body() {
        // /bin/cat is a transcriber that "transcribes" stdin verbatim.
        let t = Transcriber::open(cfg(&["/bin/cat"])).unwrap();
        let out = t
            .transcribe(&voice_note(Some(41_500), None), || {
                fetch_ok(b"ship the thing")
            })
            .await;
        assert_eq!(
            out,
            Transcription::Transcribed {
                body: "🎙 (voice note, 0:42) ship the thing".to_owned(),
                transcript: "ship the thing".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn a_download_failure_falls_back_to_the_filename() {
        let t = Transcriber::open(cfg(&["/bin/cat"])).unwrap();
        let out = t
            .transcribe(&voice_note(Some(41_500), None), || async {
                Err("M_NOT_FOUND".to_owned())
            })
            .await;
        assert!(out.body().contains("Voice message.ogg"), "{out:?}");
        assert!(out.body().contains("M_NOT_FOUND"), "{out:?}");
    }

    #[tokio::test]
    async fn a_missing_transcriber_binary_falls_back_to_the_filename() {
        let t = Transcriber::open(cfg(&["/nonexistent/whisper-cli"])).unwrap();
        let out = t
            .transcribe(&voice_note(Some(41_500), None), || fetch_ok(b"audio"))
            .await;
        match out {
            Transcription::Fallback { body, reason } => {
                assert!(body.contains("Voice message.ogg"), "{body}");
                assert!(reason.contains("transcription failed"), "{reason}");
            }
            other => panic!("expected a fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_transcriber_falls_back_with_its_stderr() {
        let t = Transcriber::open(cfg(&[
            "/bin/sh",
            "-c",
            "cat >/dev/null; echo 'model not found' >&2; exit 2",
        ]))
        .unwrap();
        let out = t
            .transcribe(&voice_note(Some(41_500), None), || fetch_ok(b"audio"))
            .await;
        assert!(out.body().contains("model not found"), "{out:?}");
        assert!(out.body().contains("Voice message.ogg"), "{out:?}");
    }

    #[tokio::test]
    async fn an_empty_transcript_is_a_fallback_not_an_empty_body() {
        let t = Transcriber::open(cfg(&["/bin/sh", "-c", "cat >/dev/null"])).unwrap();
        let out = t
            .transcribe(&voice_note(Some(41_500), None), || fetch_ok(b"audio"))
            .await;
        assert!(out.body().contains("produced no text"), "{out:?}");
    }

    #[tokio::test]
    async fn a_hung_transcriber_is_killed_at_the_timeout() {
        let mut config = cfg(&["/bin/sleep", "30"]);
        config.timeout_seconds = 1;
        let t = Transcriber::open(config).unwrap();
        let out = t
            .transcribe(&voice_note(Some(41_500), None), || fetch_ok(b"audio"))
            .await;
        assert!(out.body().contains("timeout_seconds"), "{out:?}");
        assert!(out.body().contains("Voice message.ogg"), "{out:?}");
    }

    #[tokio::test]
    async fn transcriptions_are_serialized_never_concurrent() {
        // The single-flight criterion: two voice notes arriving together must
        // not put two transcriber processes on the host at once. The fake
        // transcriber brackets its run in a shared log, so interleaving would
        // show up as "start,start,end,end".
        let log = std::env::temp_dir().join(format!(
            "safehoused-transcribe-single-flight-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&log);
        let script = format!(
            "cat >/dev/null; echo start >>{0}; sleep 0.3; echo end >>{0}; echo ok",
            log.display()
        );
        let t = Transcriber::open(cfg(&["/bin/sh", "-c", &script])).unwrap();

        let note = voice_note(Some(1_000), None);
        let (a, b) = tokio::join!(
            t.transcribe(&note, || fetch_ok(b"one")),
            t.transcribe(&note, || fetch_ok(b"two"))
        );
        assert!(matches!(a, Transcription::Transcribed { .. }), "{a:?}");
        assert!(matches!(b, Transcription::Transcribed { .. }), "{b:?}");

        let observed = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_file(&log);
        assert_eq!(
            observed.split_whitespace().collect::<Vec<_>>(),
            vec!["start", "end", "start", "end"],
            "the two transcriptions overlapped"
        );
    }

    #[tokio::test]
    async fn an_oversized_download_is_rejected_after_the_fact_too() {
        // `info.size` is sender-advertised and may lie or be absent; the real
        // byte count is checked as well.
        let mut config = cfg(&["/bin/cat"]);
        config.max_bytes = 4;
        let t = Transcriber::open(config).unwrap();
        let out = t
            .transcribe(&voice_note(Some(1_000), None), || {
                fetch_ok(b"much larger than four bytes")
            })
            .await;
        assert!(out.body().contains("max_bytes"), "{out:?}");
    }

    // --- folding the outcome back into the event content ------------------

    fn audio_content() -> Value {
        json!({
            "msgtype": "m.audio",
            "body": "Voice message.ogg",
            "formatted_body": "<a href=\"mxc://x/y\">Voice message.ogg</a>",
            "format": "org.matrix.custom.html",
            "url": "mxc://example.com/abc",
            "info": { "duration": 41_500, "size": 12_345 },
        })
    }

    #[tokio::test]
    async fn a_successful_transcription_rewrites_the_body_in_the_event_content() {
        let t = Transcriber::open(cfg(&["/bin/cat"])).unwrap();
        let outcome = t
            .transcribe(&voice_note(Some(41_500), None), || {
                fetch_ok(b"ship the thing")
            })
            .await;
        let mut content = audio_content();
        apply_to_content(&mut content, &outcome);

        assert_eq!(content["body"], "🎙 (voice note, 0:42) ship the thing");
        // The file's HTML caption must not survive — it would contradict the
        // body agents were handed (#194's matrix meta reads it).
        assert!(content.get("formatted_body").is_none());
        assert!(content.get("format").is_none());
        // Everything else the sender wrote is left alone.
        assert_eq!(content["url"], "mxc://example.com/abc");
        assert_eq!(content["msgtype"], "m.audio");
        assert_eq!(content["info"]["duration"], 41_500);
    }

    #[tokio::test]
    async fn a_failed_transcription_leaves_the_filename_in_the_event_content() {
        // The load-bearing fallback: the message is still delivered, the file
        // name is still there, and the body says transcription failed.
        let t = Transcriber::open(cfg(&["/nonexistent/whisper-cli"])).unwrap();
        let outcome = t
            .transcribe(&voice_note(Some(41_500), None), || fetch_ok(b"audio"))
            .await;
        let mut content = audio_content();
        apply_to_content(&mut content, &outcome);

        let body = content["body"].as_str().unwrap();
        assert!(body.contains("Voice message.ogg"), "{body}");
        assert!(body.contains("transcription failed"), "{body}");
    }

    #[test]
    fn an_absurd_transcript_is_length_clamped() {
        let huge = "x".repeat(MAX_TRANSCRIPT_CHARS + 500);
        let clamped = clamp_transcript(&huge);
        assert!(clamped.ends_with("… (transcript truncated)"));
        assert!(clamped.chars().count() < huge.chars().count());
    }
}
