use std::sync::Mutex;

use crate::agent::Runner;
use crate::auth::SessionStore;
use crate::checklist::cache::ChecklistCache;
use crate::config::Config;
use crate::queue::Queue;

/// Shared state handed to job-processing and command-dispatch code.
/// `last_note` is a plain in-memory cache for `/status`; it resets on
/// restart, which is fine since it's informational only, not part of the
/// durable job/queue state.
///
/// Deliberately platform-agnostic: the Telegram/Matrix client handles (and
/// the CLI socket's waiter registry) are daemon-only concerns, wrapped
/// around this in `kamajid::state::DaemonState` rather than living here.
/// `sessions` lives here (rather than daemon-only state) for the same
/// reason `queue` does: it's a redb-backed store, constructed from the same
/// `Arc<Database>` handle, with no HTTP/transport concerns of its own.
pub struct AppState {
    pub config: Config,
    pub queue: Queue,
    pub sessions: SessionStore,
    /// Backs `/todo`/`/goal`'s `<n>` shorthand (resolve/reopen by list
    /// position rather than the full `EntryKey`) -- a redb-backed cache
    /// keyed by chat/room, built from the same `Arc<Database>` as `queue`/
    /// `sessions`, for the same reason those live here rather than in
    /// `kamajid`'s daemon-only state.
    pub checklist_cache: ChecklistCache,
    pub last_note: Mutex<Option<String>>,
    /// Which mechanism launches Claude for this process's lifetime -- built
    /// once at startup via `agent::Runner::connect(config.openshell.as_ref())`
    /// (see `kamajid::main`), not reconstructed per job. The single
    /// sequential worker never runs two Claude invocations concurrently, so
    /// this needs no `Clone`/extra `Arc` beyond the one already wrapping
    /// `AppState` itself.
    pub runner: Runner,
}

impl AppState {
    pub fn record_last_note(&self, description: String) {
        match self.last_note.lock() {
            Ok(mut guard) => *guard = Some(description),
            Err(poisoned) => {
                tracing::warn!("last_note lock was poisoned, recovering");
                *poisoned.into_inner() = Some(description);
            }
        }
    }

    pub fn last_note_summary(&self) -> String {
        match self.last_note.lock() {
            Ok(guard) => guard.clone().unwrap_or_else(|| "none yet".to_string()),
            Err(poisoned) => poisoned
                .into_inner()
                .clone()
                .unwrap_or_else(|| "none yet".to_string()),
        }
    }
}
