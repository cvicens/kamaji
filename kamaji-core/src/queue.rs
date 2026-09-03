use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};

use crate::chat::{ChatRef, MessageRef};
use crate::db::{PENDING, RUNNING, SEEN_MATRIX_EVENTS, SEEN_UPDATES};
use crate::error::QueueError;

/// Metadata for a document attached to a `Command` job (currently
/// only produced for `/fact`, via a caption-as-command message). Carries
/// just enough to re-download the file when the job is dequeued -- the
/// queue payload must survive a restart, so the bytes themselves are never
/// stored here, only the `file_id` needed to fetch them again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandAttachment {
    pub file_id: String,
    pub file_name: String,
    pub mime_type: Option<String>,
}

/// A structured `/todo` write operation from the web UI's REST API
/// (`kamajid::transport::rest`'s `/api/todos/*` routes) -- distinct from
/// `JobKind::Command { name: "todo", .. }` because that path is reachable
/// from chat/CLI, parses free-text args, and replies with a human-formatted
/// string. This one carries typed fields and its handler
/// (`worker::todo_api_job`) replies with a JSON string instead, so the
/// browser can render structured rows rather than parse chat text back
/// apart. `key` fields are `EntryKey`'s display string (`"2026-08-03-2"`),
/// not a structured `EntryKey`, since that's the shape a URL path segment
/// or JSON request body naturally carries -- parsed back with `.parse()` in
/// the handler. `Edit`/`Delete` have no `todo::TodoAction` counterpart on
/// purpose: they're new capabilities scoped to this access point only (see
/// TODO.md's "TODO management web UI" note).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum TodoApiOp {
    Add {
        text: String,
        tags: Vec<String>,
    },
    Resolve {
        key: String,
    },
    Reopen {
        key: String,
    },
    Edit {
        key: String,
        text: String,
        tags: Vec<String>,
    },
    Delete {
        key: String,
    },
}

/// What a job actually does. This is the tagged enum from the spec, matched
/// on directly by the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JobKind {
    Ingest {
        raw_text: String,
        urls: Vec<String>,
    },
    Command {
        name: String,
        args: Vec<String>,
        /// `#[serde(default)]` so `job_history`/`pending` entries written
        /// before this field existed keep deserializing on restart.
        #[serde(default)]
        attachment: Option<CommandAttachment>,
    },
    /// See `TodoApiOp`'s doc comment -- never constructed by chat/CLI
    /// parsing, only by `kamajid::transport::rest`'s `/api/todos/*` routes.
    TodoApi(TodoApiOp),
}

/// The full pending-table payload. `JobKind` alone doesn't carry enough
/// information for the worker to reply to the right place, so it's wrapped
/// with the chat/message the job originated from. `ChatRef`/`MessageRef` are
/// platform-tagged (Telegram ids are integers, Matrix ids are opaque
/// strings), not bare ints, so this pipeline can carry jobs from either
/// transport without a type mismatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub chat: ChatRef,
    pub reply_to: MessageRef,
    pub kind: JobKind,
}

/// Sequential job queue backed by redb.
///
/// The payload for a job lives in `pending` for its entire lifetime and is
/// only removed on `complete`. `running` is purely a lease marker (job_id ->
/// leased_at): `dequeue` skips any pending job that already has a live
/// lease, and `recover_stale` just deletes expired leases, which makes the
/// job eligible for `dequeue` again with its payload untouched. This means
/// a crash between dequeue and complete never loses a payload, matching the
/// `pending<u64, &str>` / `running<u64, u64>` schema as specified (no need
/// to duplicate the payload into `running`).
///
/// All methods are synchronous (redb is not async); callers on the tokio
/// side should treat these as fast local disk operations, not blocking
/// network calls.
pub struct Queue {
    pub db: Arc<Database>,
    next_job_id: AtomicU64,
}

impl Queue {
    /// Seeds the in-memory id counter from the highest job id present in
    /// `pending`, so freshly issued ids never collide with jobs already on
    /// disk from a previous run.
    pub fn new(db: Arc<Database>) -> Result<Self, QueueError> {
        let txn = db.begin_read()?;
        let mut max_id = 0u64;
        {
            let pending = txn.open_table(PENDING)?;
            for entry in pending.iter()? {
                let (k, _) = entry?;
                max_id = max_id.max(k.value());
            }
        }
        Ok(Queue {
            db,
            next_job_id: AtomicU64::new(max_id + 1),
        })
    }

    pub fn enqueue(&self, job: &Job) -> Result<u64, QueueError> {
        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let payload = serde_json::to_string(job)?;
        let txn = self.db.begin_write()?;
        {
            let mut pending = txn.open_table(PENDING)?;
            pending.insert(job_id, payload.as_str())?;
        }
        txn.commit()?;
        Ok(job_id)
    }

    /// Mints a fresh, unique job id without writing anything to `pending`.
    /// Used by `CommandMode::Sync` commands, which never enter the queue but
    /// still want an id to correlate with in the debug log. Shares the same
    /// counter as `enqueue` so ids never collide between sync and queued jobs.
    pub fn next_id(&self) -> u64 {
        self.next_job_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Finds the oldest pending job that isn't currently leased, marks it
    /// leased (inserts into `running`), and returns it. The payload stays
    /// in `pending` until `complete`. Returns `None` if there's no
    /// unleased pending job.
    pub fn dequeue(&self) -> Result<Option<(u64, Job)>, QueueError> {
        let txn = self.db.begin_write()?;
        let found = {
            let pending = txn.open_table(PENDING)?;
            let running = txn.open_table(RUNNING)?;
            let mut chosen = None;
            for entry in pending.iter()? {
                let (k, v) = entry?;
                let job_id = k.value();
                if running.get(job_id)?.is_none() {
                    chosen = Some((job_id, v.value().to_string()));
                    break;
                }
            }
            chosen
        };
        let Some((job_id, payload)) = found else {
            return Ok(None);
        };
        let job: Job = serde_json::from_str(&payload)?;
        {
            let mut running = txn.open_table(RUNNING)?;
            running.insert(job_id, now_unix())?;
        }
        txn.commit()?;
        Ok(Some((job_id, job)))
    }

    /// Removes a job from both tables once it has been fully processed.
    pub fn complete(&self, job_id: u64) -> Result<(), QueueError> {
        let txn = self.db.begin_write()?;
        {
            let mut pending = txn.open_table(PENDING)?;
            pending.remove(job_id)?;
            let mut running = txn.open_table(RUNNING)?;
            running.remove(job_id)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Deletes any lease older than or exactly `timeout_secs` old, making
    /// that job's (still-present) pending payload eligible for `dequeue`
    /// again. The `<=` comparison (not `<`) ensures that a timeout of 0
    /// recovers everything, which is what tests and "recover all stale on
    /// startup" want.
    pub fn recover_stale(&self, timeout_secs: u64) -> Result<usize, QueueError> {
        let cutoff = now_unix().saturating_sub(timeout_secs);
        let txn = self.db.begin_write()?;
        let mut recovered = 0usize;
        {
            let stale: Vec<u64> = {
                let running = txn.open_table(RUNNING)?;
                running
                    .iter()?
                    .filter_map(|entry| entry.ok())
                    .filter(|(_, leased_at)| leased_at.value() <= cutoff)
                    .map(|(k, _)| k.value())
                    .collect()
            };
            let mut running = txn.open_table(RUNNING)?;
            for job_id in stale {
                running.remove(job_id)?;
                recovered += 1;
            }
        }
        txn.commit()?;
        Ok(recovered)
    }

    /// Returns `true` if this update_id is new (and records it as seen),
    /// `false` if it was already processed.
    pub fn mark_seen(&self, update_id: i64) -> Result<bool, QueueError> {
        let txn = self.db.begin_write()?;
        let is_new = {
            let mut seen = txn.open_table(SEEN_UPDATES)?;
            let already = seen.get(update_id)?.is_some();
            if !already {
                seen.insert(update_id, ())?;
            }
            !already
        };
        txn.commit()?;
        Ok(is_new)
    }

    /// Same as `mark_seen`, but for Matrix's string-typed `event_id` --
    /// Matrix has no numeric update-id equivalent, so this is a separate
    /// table (`SEEN_MATRIX_EVENTS`) rather than trying to coerce event ids
    /// into `SEEN_UPDATES`'s `i64` key.
    pub fn mark_seen_matrix(&self, event_id: &str) -> Result<bool, QueueError> {
        let txn = self.db.begin_write()?;
        let is_new = {
            let mut seen = txn.open_table(SEEN_MATRIX_EVENTS)?;
            let already = seen.get(event_id)?.is_some();
            if !already {
                seen.insert(event_id, ())?;
            }
            !already
        };
        txn.commit()?;
        Ok(is_new)
    }

    pub fn pending_depth(&self) -> Result<u64, QueueError> {
        let txn = self.db.begin_read()?;
        let pending = txn.open_table(PENDING)?;
        Ok(pending.len()?)
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_queue() -> (Queue, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("test.redb"));
        let queue = Queue::new(Arc::new(db)).unwrap();
        (queue, dir)
    }

    fn sample_job(text: &str) -> Job {
        Job {
            chat: ChatRef::Telegram { chat_id: 1 },
            reply_to: MessageRef::Telegram { message_id: 1 },
            kind: JobKind::Ingest {
                raw_text: text.to_string(),
                urls: vec![],
            },
        }
    }

    #[test]
    fn enqueue_dequeue_complete_roundtrip() {
        let (queue, _dir) = test_queue();
        let job_id = queue.enqueue(&sample_job("hello")).unwrap();
        assert_eq!(queue.pending_depth().unwrap(), 1);

        let (dequeued_id, job) = queue.dequeue().unwrap().unwrap();
        assert_eq!(dequeued_id, job_id);
        match job.kind {
            JobKind::Ingest { raw_text, .. } => assert_eq!(raw_text, "hello"),
            _ => panic!("wrong job kind"),
        }

        // Job is leased: a second dequeue must not return the same job.
        assert!(queue.dequeue().unwrap().is_none());

        queue.complete(job_id).unwrap();
        assert_eq!(queue.pending_depth().unwrap(), 0);
    }

    #[test]
    fn dequeue_is_fifo() {
        let (queue, _dir) = test_queue();
        let first = queue.enqueue(&sample_job("first")).unwrap();
        let second = queue.enqueue(&sample_job("second")).unwrap();

        let (id, _) = queue.dequeue().unwrap().unwrap();
        assert_eq!(id, first);
        queue.complete(first).unwrap();

        let (id, _) = queue.dequeue().unwrap().unwrap();
        assert_eq!(id, second);
    }

    #[test]
    fn crash_recovery_requeues_stale_leases_without_losing_payload() {
        let (queue, _dir) = test_queue();
        let job_id = queue.enqueue(&sample_job("in flight")).unwrap();
        let (dequeued_id, _job) = queue.dequeue().unwrap().unwrap();
        assert_eq!(dequeued_id, job_id);

        // Simulate a crash: the lease exists but the job never completed.
        // A 0-second timeout means "anything leased is stale" for the test.
        let recovered = queue.recover_stale(0).unwrap();
        assert_eq!(recovered, 1);

        // Payload must still be there and dequeue-able again.
        let (redequeued_id, job) = queue.dequeue().unwrap().unwrap();
        assert_eq!(redequeued_id, job_id);
        match job.kind {
            JobKind::Ingest { raw_text, .. } => assert_eq!(raw_text, "in flight"),
            _ => panic!("wrong job kind"),
        }
    }

    #[test]
    fn recover_stale_leaves_fresh_leases_alone() {
        let (queue, _dir) = test_queue();
        let job_id = queue.enqueue(&sample_job("fresh")).unwrap();
        queue.dequeue().unwrap().unwrap();

        // A very long timeout means the just-created lease is not stale.
        let recovered = queue.recover_stale(3600).unwrap();
        assert_eq!(recovered, 0);
        assert!(queue.dequeue().unwrap().is_none());

        queue.complete(job_id).unwrap();
    }

    #[test]
    fn mark_seen_dedupes_update_ids() {
        let (queue, _dir) = test_queue();
        assert!(queue.mark_seen(42).unwrap());
        assert!(!queue.mark_seen(42).unwrap());
        assert!(queue.mark_seen(43).unwrap());
    }

    #[test]
    fn mark_seen_matrix_dedupes_event_ids() {
        let (queue, _dir) = test_queue();
        assert!(queue
            .mark_seen_matrix("$abc:matrix.pasoenfalso.com")
            .unwrap());
        assert!(!queue
            .mark_seen_matrix("$abc:matrix.pasoenfalso.com")
            .unwrap());
        assert!(queue
            .mark_seen_matrix("$def:matrix.pasoenfalso.com")
            .unwrap());
    }

    #[test]
    fn next_id_does_not_collide_with_enqueued_ids() {
        let (queue, _dir) = test_queue();
        let sync_id = queue.next_id();
        let queued_id = queue.enqueue(&sample_job("queued")).unwrap();
        assert_ne!(sync_id, queued_id);

        // next_id() must not have written anything to `pending`.
        assert_eq!(queue.pending_depth().unwrap(), 1);
    }

    #[test]
    fn concurrent_enqueues_yield_unique_ids() {
        let (queue, _dir) = test_queue();
        let queue = Arc::new(queue);
        let mut handles = vec![];
        for i in 0..8 {
            let queue = Arc::clone(&queue);
            handles.push(std::thread::spawn(move || {
                queue.enqueue(&sample_job(&format!("job {i}"))).unwrap()
            }));
        }
        let mut ids: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 8);
    }
}
