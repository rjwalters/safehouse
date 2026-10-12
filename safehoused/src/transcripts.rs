//! Stored voice-note transcripts (#231), so the `read` op returns the same
//! body for an `m.audio` row that the live path put in the mailbox.
//!
//! The live path (`on_message` → `transcribe_audio_body`) rewrites a voice
//! note's body to `🎙 (voice note, 0:42) <transcript>` (or the file name plus
//! a failure note) before envelope synthesis. `read` rebuilds each row from
//! the raw timeline event instead, so without this module a voice note in
//! room history is only its file name. Here the live outcome is kept,
//! keyed by event id, and `read` folds it back in.
//!
//! - **Where.** In the matrix-sdk state store the daemon already opens with
//!   `store_passphrase` (`<state_dir>/store`), as custom values. That store
//!   hashes a custom value's *key* but writes the *value* in the clear, so
//!   every entry is sealed here with a [`StoreCipher`] of our own (XChaCha20-
//!   Poly1305, the same crate matrix-sdk-sqlite uses for its own tables). The
//!   cipher is itself stored in that state store, exported under
//!   `store_passphrase` (PBKDF2), at [`CIPHER_KEY`]. Entry keys are
//!   `hash_key(TABLE, event_id)`, so an event id is not readable from the
//!   key either.
//! - **What.** Text only: the body the live path delivered. Audio is never
//!   written anywhere (the download uses `get_file(.., use_cache: false)`).
//! - **Kept, not expired.** Room history is not deleted (claude-bot#282), so a
//!   stored transcript is not removed when the voice note is redacted. A
//!   redacted event carries no `msgtype`, so `read` never looks it up anyway.
//! - **Backfill.** A voice note `read` meets with nothing stored (older than
//!   transcription being switched on, or from before the daemon joined) is
//!   queued for transcription, at most `backfill_per_read` per `read`, and
//!   transcribed one at a time by [`VoiceNotes::run_backfill`]. The `read`
//!   never waits: it returns the file name until the transcript is stored.
//!   The backfill yields to live notes: a run starts only while no live
//!   transcription is in flight, and a live note cancels one in progress
//!   (`Transcriber::transcribe_backfill`), so it never stalls the sync loop.
//! - **Only final outcomes are stored.** A transcript, or a failure that a
//!   retry cannot change (over a cap, empty or undecodable audio, no text;
//!   `FailureKind::is_final`). A failure that might pass (download error or
//!   timeout, slot timeout, a killed, crashed or missing transcriber, a
//!   preempted backfill) is delivered live with its note but not stored, so a
//!   later `read` queues the note again. To keep a note that never stops
//!   failing from taking the per-read budget forever, it is retried at most
//!   [`MAX_BACKFILL_ATTEMPTS`] times per daemon run (a preempted run does not
//!   count; a restart starts the count again).

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use matrix_sdk::Client;
use matrix_sdk_store_encryption::StoreCipher;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::transcribe::{self, FailureKind, Transcriber, Transcription};

/// State-store custom key holding the passphrase-exported [`StoreCipher`].
const CIPHER_KEY: &[u8] = b"safehoused.transcripts.cipher.v1";

/// Table name mixed into every entry's key hash. Versioned so a future change
/// to the entry shape can start a fresh keyspace.
const TABLE: &str = "safehoused.transcripts.v1";

/// How many backfill runs a note may fail transiently before `read` stops
/// queueing it, until the daemon restarts.
pub const MAX_BACKFILL_ATTEMPTS: u32 = 3;

/// One stored transcript. A struct rather than a bare string so a field can be
/// added later without a new keyspace.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// The body the live path delivered: the transcript body, or the file
    /// name plus the failure note.
    body: String,
}

/// Encrypted per-event transcript store, backed by the client's state store.
pub struct TranscriptStore {
    client: Client,
    cipher: StoreCipher,
}

impl TranscriptStore {
    /// Open the store, creating its cipher on first use. Runs PBKDF2 once
    /// (import or export), so it costs a fraction of a second at boot.
    pub async fn open(client: Client, passphrase: &str) -> Result<Self> {
        let store = client.state_store();
        let existing = store
            .get_custom_value(CIPHER_KEY)
            .await
            .context("reading the transcript store key")?;
        let cipher = match existing {
            Some(exported) => StoreCipher::import(passphrase, &exported)
                .context("unlocking the transcript store key with store_passphrase")?,
            None => {
                let cipher = StoreCipher::new().context("creating the transcript store key")?;
                let exported = cipher
                    .export(passphrase)
                    .context("sealing the transcript store key")?;
                store
                    .set_custom_value_no_read(CIPHER_KEY, exported)
                    .await
                    .context("saving the transcript store key")?;
                cipher
            }
        };
        Ok(Self { client, cipher })
    }

    /// A store with a caller-supplied cipher, skipping the PBKDF2 round trip.
    #[cfg(test)]
    pub fn with_cipher(client: Client, cipher: StoreCipher) -> Self {
        Self { client, cipher }
    }

    fn key(&self, event_id: &str) -> [u8; 32] {
        self.cipher.hash_key(TABLE, event_id.as_bytes())
    }

    /// The stored body for `event_id`, if any.
    pub async fn get(&self, event_id: &str) -> Result<Option<String>> {
        let Some(sealed) = self
            .client
            .state_store()
            .get_custom_value(&self.key(event_id))
            .await
            .context("reading a stored transcript")?
        else {
            return Ok(None);
        };
        let entry: Entry = self
            .cipher
            .decrypt_value(&sealed)
            .context("decrypting a stored transcript")?;
        Ok(Some(entry.body))
    }

    /// Store `body` for `event_id`, replacing any earlier entry.
    pub async fn put(&self, event_id: &str, body: &str) -> Result<()> {
        let sealed = self
            .cipher
            .encrypt_value(&Entry {
                body: body.to_owned(),
            })
            .context("encrypting a transcript")?;
        self.client
            .state_store()
            .set_custom_value_no_read(&self.key(event_id), sealed)
            .await
            .context("writing a stored transcript")
    }
}

/// One voice note waiting for a backfill transcription.
#[derive(Clone, Debug)]
pub struct BackfillJob {
    pub event_id: String,
    /// The raw (decrypted) event `content`, from which the worker reads the
    /// audio's metadata and media source.
    pub content: Value,
}

/// The transcription runtime as `on_message` and `read` see it: the
/// [`Transcriber`], the store, and the backfill queue. Built only when
/// `[transcribe]` is configured.
pub struct VoiceNotes {
    transcriber: Arc<Transcriber>,
    store: TranscriptStore,
    /// Bounded to `backfill_per_read` slots (at least one): a `read` that
    /// finds the queue full queues nothing, and a later `read` retries.
    queue: mpsc::Sender<BackfillJob>,
    /// Event ids queued or being transcribed, so overlapping reads do not
    /// queue the same note twice.
    pending: Mutex<HashSet<String>>,
    /// Transient backfill failures per event id, this daemon run. A note at
    /// [`MAX_BACKFILL_ATTEMPTS`] is not queued again.
    attempts: Mutex<HashMap<String, u32>>,
}

impl VoiceNotes {
    /// Build the runtime. The receiver goes to [`Self::run_backfill`].
    pub fn new(
        transcriber: Arc<Transcriber>,
        store: TranscriptStore,
    ) -> (Arc<Self>, mpsc::Receiver<BackfillJob>) {
        let (queue, rx) = mpsc::channel(transcriber.backfill_per_read().max(1));
        (
            Arc::new(Self {
                transcriber,
                store,
                queue,
                pending: Mutex::new(HashSet::new()),
                attempts: Mutex::new(HashMap::new()),
            }),
            rx,
        )
    }

    pub fn transcriber(&self) -> &Arc<Transcriber> {
        &self.transcriber
    }

    /// The stored body for `event_id`. A store error is logged and read as
    /// "nothing stored": the caller falls back to what it would have done
    /// without this module.
    pub async fn stored(&self, event_id: &str) -> Option<String> {
        match self.store.get(event_id).await {
            Ok(body) => body,
            Err(err) => {
                eprintln!("safehoused: transcript lookup for {event_id} failed: {err:#}");
                None
            }
        }
    }

    /// Store one transcription outcome if it is final (a transcript, or a
    /// failure a retry cannot change); a transient failure is not stored, so
    /// a later `read` backfills the note. Returns whether it was stored.
    /// Errors are logged, never fatal: the message has already been delivered.
    pub async fn persist(&self, event_id: &str, outcome: &Transcription) -> bool {
        if !outcome.is_final() {
            return false;
        }
        if let Err(err) = self.store.put(event_id, outcome.body()).await {
            eprintln!("safehoused: storing the transcript for {event_id} failed: {err:#}");
            return false;
        }
        true
    }

    /// The `read` op's lookup. `rows` are `(event_id, content)` pairs,
    /// **newest first**, so the backfill budget goes to the most recent notes.
    /// Returns the stored body of every `m.audio` row that has one. Each
    /// `m.audio` row without one is queued for backfill, up to
    /// `backfill_per_read` per call. Never transcribes inline, so it never
    /// stalls the read.
    pub async fn bodies_for_read(&self, rows: &[(String, Value)]) -> HashMap<String, String> {
        let mut bodies = HashMap::new();
        let mut budget = self.transcriber.backfill_per_read();
        for (event_id, content) in rows {
            if transcribe::audio_note_from_content(content).is_none() {
                continue;
            }
            if let Some(body) = self.stored(event_id).await {
                bodies.insert(event_id.clone(), body);
                continue;
            }
            if budget == 0 {
                continue;
            }
            if self.attempts_exhausted(event_id) {
                continue;
            }
            let mut pending = self.pending.lock().expect("pending set lock");
            if pending.contains(event_id) {
                continue;
            }
            let job = BackfillJob {
                event_id: event_id.clone(),
                content: content.clone(),
            };
            if self.queue.try_send(job).is_ok() {
                pending.insert(event_id.clone());
                budget -= 1;
            } else {
                // Full: the worker is behind. Stop queueing, but keep looking
                // up stored bodies for the remaining rows.
                budget = 0;
            }
        }
        bodies
    }

    /// Record one backfill run's outcome against the note's retry budget.
    fn count_attempt(&self, event_id: &str, outcome: &Transcription) {
        let mut attempts = self.attempts.lock().expect("attempts lock");
        match outcome {
            Transcription::Fallback {
                kind: FailureKind::Preempted,
                ..
            } => {}
            o if o.is_final() => {
                attempts.remove(event_id);
            }
            _ => *attempts.entry(event_id.to_owned()).or_default() += 1,
        }
    }

    fn attempts_exhausted(&self, event_id: &str) -> bool {
        self.attempts
            .lock()
            .expect("attempts lock")
            .get(event_id)
            .is_some_and(|n| *n >= MAX_BACKFILL_ATTEMPTS)
    }

    /// Transcribe one queued note at backfill priority and store the outcome
    /// if it is final. `fetch` downloads and decrypts the audio from the
    /// event content. Skips the model run when something (a live delivery)
    /// stored a body meanwhile. A transient failure is left unstored for a
    /// later `read`; a preempted run does not count as an attempt.
    pub async fn backfill_one<F, Fut>(&self, job: BackfillJob, fetch: F)
    where
        F: FnOnce(Value) -> Fut,
        Fut: Future<Output = std::result::Result<Vec<u8>, String>>,
    {
        let BackfillJob { event_id, content } = job;
        if self.stored(&event_id).await.is_none() {
            if let Some(note) = transcribe::audio_note_from_content(&content) {
                let outcome = self
                    .transcriber
                    .transcribe_backfill(&note, || fetch(content))
                    .await;
                match &outcome {
                    Transcription::Transcribed { .. } => {
                        println!("safehoused: backfilled the transcript of {event_id}")
                    }
                    Transcription::Fallback { reason, kind, .. } => {
                        let retry = if kind.is_final() {
                            ""
                        } else {
                            " (not stored; a later read retries)"
                        };
                        eprintln!(
                            "safehoused: backfill of {event_id} not transcribed: {reason}{retry}"
                        )
                    }
                }
                self.count_attempt(&event_id, &outcome);
                self.persist(&event_id, &outcome).await;
            }
        }
        self.pending
            .lock()
            .expect("pending set lock")
            .remove(&event_id);
    }

    /// The backfill worker: one note at a time, for the life of the process.
    pub async fn run_backfill<F, Fut>(
        self: Arc<Self>,
        mut rx: mpsc::Receiver<BackfillJob>,
        fetch: F,
    ) where
        F: Fn(Value) -> Fut,
        Fut: Future<Output = std::result::Result<Vec<u8>, String>>,
    {
        while let Some(job) = rx.recv().await {
            self.backfill_one(job, &fetch).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::transcribe::{TranscribeConfig, DEFAULT_MAX_BYTES};

    async fn offline_client() -> Client {
        Client::builder()
            .homeserver_url("http://127.0.0.1:1")
            .build()
            .await
            .expect("building a client does not require network access")
    }

    fn config(command: &[&str], backfill_per_read: usize) -> TranscribeConfig {
        TranscribeConfig {
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            max_seconds: 600,
            timeout_seconds: 10,
            max_bytes: DEFAULT_MAX_BYTES,
            post_transcript: false,
            backfill_per_read,
        }
    }

    async fn voice_notes(
        command: &[&str],
        backfill_per_read: usize,
    ) -> (Arc<VoiceNotes>, mpsc::Receiver<BackfillJob>) {
        let store =
            TranscriptStore::with_cipher(offline_client().await, StoreCipher::new().unwrap());
        let transcriber = Transcriber::open(config(command, backfill_per_read)).unwrap();
        VoiceNotes::new(transcriber, store)
    }

    fn audio(filename: &str) -> Value {
        json!({
            "msgtype": "m.audio",
            "body": filename,
            "info": { "duration": 41_500 },
            "org.matrix.msc3245.voice": {},
        })
    }

    #[tokio::test]
    async fn a_stored_body_round_trips_and_is_encrypted_at_rest() {
        let client = offline_client().await;
        let store = TranscriptStore::with_cipher(client.clone(), StoreCipher::new().unwrap());
        assert_eq!(store.get("$a").await.unwrap(), None);
        store
            .put("$a", "🎙 (voice note, 0:42) ship it")
            .await
            .unwrap();
        assert_eq!(
            store.get("$a").await.unwrap().as_deref(),
            Some("🎙 (voice note, 0:42) ship it")
        );
        // What the state store holds is neither the text nor keyed by the id.
        let raw = client
            .state_store()
            .get_custom_value(&store.key("$a"))
            .await
            .unwrap()
            .expect("entry stored under the hashed key");
        assert!(!String::from_utf8_lossy(&raw).contains("ship it"));
        assert!(client
            .state_store()
            .get_custom_value(b"$a")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn the_store_key_survives_a_reopen_under_the_same_passphrase() {
        let client = offline_client().await;
        let first = TranscriptStore::open(client.clone(), "pass").await.unwrap();
        first.put("$a", "hello").await.unwrap();
        let second = TranscriptStore::open(client.clone(), "pass").await.unwrap();
        assert_eq!(second.get("$a").await.unwrap().as_deref(), Some("hello"));
        assert!(TranscriptStore::open(client, "wrong").await.is_err());
    }

    #[tokio::test]
    async fn read_queues_at_most_n_and_newest_first() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 2).await;
        let rows: Vec<(String, Value)> = (0..5)
            .map(|i| (format!("$new{i}"), audio("Voice message.ogg")))
            .collect();
        let bodies = voice.bodies_for_read(&rows).await;
        assert!(bodies.is_empty(), "nothing is transcribed inline");
        let queued: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|j| j.event_id)
            .collect();
        assert_eq!(queued, vec!["$new0", "$new1"]);
    }

    #[tokio::test]
    async fn a_full_queue_still_returns_stored_bodies_for_older_rows() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 2).await;
        voice
            .bodies_for_read(&[
                ("$p0".to_owned(), audio("p0.ogg")),
                ("$p1".to_owned(), audio("p1.ogg")),
            ])
            .await;
        voice.store.put("$old", "stored transcript").await.unwrap();
        // The queue is full: the uncached newer row fails to enqueue, but the
        // older stored row must still come back.
        let rows = vec![
            ("$new".to_owned(), audio("new.ogg")),
            ("$old".to_owned(), audio("old.ogg")),
        ];
        let bodies = voice.bodies_for_read(&rows).await;
        assert_eq!(
            bodies.get("$old").map(String::as_str),
            Some("stored transcript")
        );
        let queued: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|j| j.event_id)
            .collect();
        assert_eq!(queued, vec!["$p0", "$p1"], "$new must not be queued");
    }

    #[tokio::test]
    async fn a_note_already_queued_is_not_queued_again() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 3).await;
        let rows = vec![("$a".to_owned(), audio("a.ogg"))];
        voice.bodies_for_read(&rows).await;
        voice.bodies_for_read(&rows).await;
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "queued twice");
    }

    #[tokio::test]
    async fn zero_backfill_queues_nothing() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 0).await;
        voice
            .bodies_for_read(&[("$a".to_owned(), audio("a.ogg"))])
            .await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_audio_rows_are_never_looked_up_or_queued() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 2).await;
        let rows = vec![(
            "$t".to_owned(),
            json!({ "msgtype": "m.text", "body": "hi" }),
        )];
        assert!(voice.bodies_for_read(&rows).await.is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn backfill_transcribes_once_and_stores_the_outcome() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 2).await;
        let rows = vec![("$old".to_owned(), audio("Voice message.ogg"))];
        voice.bodies_for_read(&rows).await;
        let fetches = AtomicUsize::new(0);
        let fetch = |_content: Value| {
            fetches.fetch_add(1, Ordering::SeqCst);
            async { Ok(b"from the archive".to_vec()) }
        };
        voice.backfill_one(rx.try_recv().unwrap(), &fetch).await;

        let bodies = voice.bodies_for_read(&rows).await;
        assert_eq!(
            bodies.get("$old").map(String::as_str),
            Some("🎙 (voice note, 0:42) from the archive")
        );
        assert!(rx.try_recv().is_err(), "a stored note is not queued again");
        // A stale duplicate job does not run the model a second time.
        voice
            .backfill_one(
                BackfillJob {
                    event_id: "$old".to_owned(),
                    content: audio("Voice message.ogg"),
                },
                &fetch,
            )
            .await;
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_transient_backfill_failure_is_not_stored_and_is_retried() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 2).await;
        let rows = vec![("$old".to_owned(), audio("Voice message.ogg"))];
        voice.bodies_for_read(&rows).await;
        // A homeserver hiccup: nothing is stored...
        voice
            .backfill_one(rx.try_recv().unwrap(), |_| async {
                Err("M_UNKNOWN: try again".to_owned())
            })
            .await;
        assert_eq!(voice.stored("$old").await, None);
        // ...so the next read queues the note again, and that run succeeds.
        assert!(voice.bodies_for_read(&rows).await.is_empty());
        voice
            .backfill_one(rx.try_recv().expect("re-queued"), |_| async {
                Ok(b"second time lucky".to_vec())
            })
            .await;
        assert_eq!(
            voice
                .bodies_for_read(&rows)
                .await
                .get("$old")
                .map(String::as_str),
            Some("🎙 (voice note, 0:42) second time lucky")
        );
    }

    #[tokio::test]
    async fn a_missing_transcriber_is_not_stored_as_the_notes_outcome() {
        // An operator-fixable condition, not a fact about the note.
        let (voice, mut rx) = voice_notes(&["/nonexistent/whisper-cli"], 2).await;
        let rows = vec![("$old".to_owned(), audio("Voice message.ogg"))];
        voice.bodies_for_read(&rows).await;
        voice
            .backfill_one(rx.try_recv().unwrap(), |_| async { Ok(b"audio".to_vec()) })
            .await;
        assert!(voice.bodies_for_read(&rows).await.is_empty());
        assert!(rx.try_recv().is_ok(), "queued again for a later run");
    }

    #[tokio::test]
    async fn a_final_backfill_failure_is_stored_and_not_retried() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 2).await;
        // Twenty minutes, over the 600s cap with no placeholder to trim with.
        let mut long = audio("Voice message.ogg");
        long["info"]["duration"] = json!(20 * 60 * 1000);
        let rows = vec![("$long".to_owned(), long)];
        voice.bodies_for_read(&rows).await;
        voice
            .backfill_one(rx.try_recv().unwrap(), |_| async { Ok(b"audio".to_vec()) })
            .await;
        let body = voice.bodies_for_read(&rows).await.remove("$long").unwrap();
        assert!(body.contains("Voice message.ogg"), "{body}");
        assert!(body.contains("max_seconds"), "{body}");
        assert!(rx.try_recv().is_err(), "a final outcome is not retried");
    }

    #[tokio::test]
    async fn a_note_that_keeps_failing_stops_being_queued() {
        let (voice, mut rx) = voice_notes(&["/bin/cat"], 2).await;
        let rows = vec![("$gone".to_owned(), audio("Voice message.ogg"))];
        for _ in 0..MAX_BACKFILL_ATTEMPTS {
            voice.bodies_for_read(&rows).await;
            voice
                .backfill_one(rx.try_recv().expect("queued"), |_| async {
                    Err("M_NOT_FOUND".to_owned())
                })
                .await;
        }
        voice.bodies_for_read(&rows).await;
        assert!(rx.try_recv().is_err(), "retried past the attempt bound");
        assert_eq!(voice.stored("$gone").await, None, "still not stored");
    }

    #[tokio::test]
    async fn a_live_note_preempts_a_running_backfill() {
        // The backfill's transcriber would run for 30s; the live note's own
        // timeout is 5s. The live note is transcribed at once, and the
        // cancelled backfill is left unstored and queued again by a later read.
        let dir = std::env::temp_dir().join(format!(
            "safehoused-backfill-preempt-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let started = dir.join("started");
        let script = format!(
            "if [ \"$(cat)\" = slow ]; then touch {0}; sleep 30; fi; echo live text",
            started.display()
        );
        let store =
            TranscriptStore::with_cipher(offline_client().await, StoreCipher::new().unwrap());
        let mut cfg = config(&["/bin/sh", "-c", &script], 2);
        cfg.timeout_seconds = 5;
        let (voice, mut rx) = VoiceNotes::new(Transcriber::open(cfg).unwrap(), store);

        let rows = vec![("$old".to_owned(), audio("Voice message.ogg"))];
        voice.bodies_for_read(&rows).await;
        let job = rx.try_recv().unwrap();
        let backfill = {
            let voice = voice.clone();
            tokio::spawn(async move {
                voice
                    .backfill_one(job, |_| async { Ok(b"slow".to_vec()) })
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !started.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the backfill's transcriber never started");

        let begun = std::time::Instant::now();
        let note = transcribe::audio_note_from_content(&audio("live.ogg")).unwrap();
        let live = voice
            .transcriber()
            .transcribe(&note, || async { Ok(b"fast".to_vec()) })
            .await;
        assert!(
            matches!(live, Transcription::Transcribed { .. }),
            "{live:?}"
        );
        assert!(begun.elapsed() < std::time::Duration::from_secs(3));

        backfill.await.unwrap();
        assert_eq!(voice.stored("$old").await, None);
        voice.bodies_for_read(&rows).await;
        assert!(rx.try_recv().is_ok(), "the preempted note is queued again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
