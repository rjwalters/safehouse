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
//!   Each note is transcribed once; its outcome, success or failure, is
//!   stored, and later reads (and a late live delivery) reuse it.

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

use crate::transcribe::{self, Transcriber, Transcription};

/// State-store custom key holding the passphrase-exported [`StoreCipher`].
const CIPHER_KEY: &[u8] = b"safehoused.transcripts.cipher.v1";

/// Table name mixed into every entry's key hash. Versioned so a future change
/// to the entry shape can start a fresh keyspace.
const TABLE: &str = "safehoused.transcripts.v1";

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

    /// Store one transcription outcome (success or failure note). Errors are
    /// logged, never fatal: the message has already been delivered.
    pub async fn persist(&self, event_id: &str, outcome: &Transcription) {
        if let Err(err) = self.store.put(event_id, outcome.body()).await {
            eprintln!("safehoused: storing the transcript for {event_id} failed: {err:#}");
        }
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
                // Full: the worker is behind. Leave the rest for a later read.
                break;
            }
        }
        bodies
    }

    /// Transcribe one queued note and store the outcome. `fetch` downloads
    /// and decrypts the audio from the event content. Skips the model run
    /// when something (a live delivery) stored a body meanwhile.
    pub async fn backfill_one<F, Fut>(&self, job: BackfillJob, fetch: F)
    where
        F: FnOnce(Value) -> Fut,
        Fut: Future<Output = std::result::Result<Vec<u8>, String>>,
    {
        let BackfillJob { event_id, content } = job;
        if self.stored(&event_id).await.is_none() {
            if let Some(note) = transcribe::audio_note_from_content(&content) {
                let outcome = self.transcriber.transcribe(&note, || fetch(content)).await;
                match &outcome {
                    Transcription::Transcribed { .. } => {
                        println!("safehoused: backfilled the transcript of {event_id}")
                    }
                    Transcription::Fallback { reason, .. } => {
                        eprintln!("safehoused: backfill of {event_id} not transcribed: {reason}")
                    }
                }
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
    async fn a_failed_backfill_is_stored_and_not_retried() {
        let (voice, mut rx) = voice_notes(&["/nonexistent/whisper-cli"], 2).await;
        let rows = vec![("$old".to_owned(), audio("Voice message.ogg"))];
        voice.bodies_for_read(&rows).await;
        voice
            .backfill_one(rx.try_recv().unwrap(), |_| async { Ok(b"audio".to_vec()) })
            .await;
        let body = voice.bodies_for_read(&rows).await.remove("$old").unwrap();
        assert!(body.contains("Voice message.ogg"), "{body}");
        assert!(body.contains("transcription failed"), "{body}");
        assert!(rx.try_recv().is_err());
    }
}
