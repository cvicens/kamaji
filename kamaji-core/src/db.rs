use redb::TableDefinition;

/// job_id -> JSON-encoded `Job` (see queue.rs).
pub const PENDING: TableDefinition<u64, &str> = TableDefinition::new("pending");
/// job_id -> leased_at (unix seconds).
pub const RUNNING: TableDefinition<u64, u64> = TableDefinition::new("running");
/// Telegram update_id -> (), dedupe set for restart replay.
pub const SEEN_UPDATES: TableDefinition<i64, ()> = TableDefinition::new("seen_updates");
/// Matrix event_id -> (), dedupe set for restart replay. Separate from
/// `SEEN_UPDATES` because Matrix has no numeric update-id equivalent -- event
/// ids are opaque strings like `$xyz:server.example`.
pub const SEEN_MATRIX_EVENTS: TableDefinition<&str, ()> =
    TableDefinition::new("seen_matrix_events");
/// job_id -> JSON-encoded `JobHistoryRecord` — completed job log with tokens.
pub const JOB_HISTORY: TableDefinition<u64, &str> = TableDefinition::new("job_history");
/// bearer token -> JSON-encoded `SessionRecord` (see `auth.rs`) — issued by
/// the REST API's `/auth/login` after a valid TOTP code, checked on every
/// `/api/cli` request.
pub const SESSIONS: TableDefinition<&str, &str> = TableDefinition::new("sessions");
/// "{domain}:{chat_ref}" (e.g. "todo:telegram:42") -> JSON array of rendered
/// `EntryKey` strings, the most recently shown `/todo list` or `/goal list`
/// for that chat/room -- see `checklist::cache::ChecklistCache`. Lets
/// `/todo resolve <n>`/`/todo reopen <n>` use a short plain number instead
/// of the full `YYYY-MM-DD-N` key.
pub const CHECKLIST_LIST_CACHE: TableDefinition<&str, &str> =
    TableDefinition::new("checklist_list_cache");

/// Opens (or creates) the redb database and makes sure all tables exist.
/// Table creation only takes effect once a write transaction touching them
/// is committed, so we do an empty write here at startup.
///
/// This runs once during process startup, before the worker or bot loop
/// starts, so `expect()` follows the same convention as config parsing: fail
/// fast rather than limp along without a usable database.
pub fn open(path: &std::path::Path) -> redb::Database {
    let db = redb::Database::create(path).expect("failed to create/open redb database");
    let txn = db.begin_write().expect("begin_write on freshly opened db");
    {
        txn.open_table(PENDING).expect("create pending table");
        txn.open_table(RUNNING).expect("create running table");
        txn.open_table(SEEN_UPDATES)
            .expect("create seen_updates table");
        txn.open_table(SEEN_MATRIX_EVENTS)
            .expect("create seen_matrix_events table");
        txn.open_table(JOB_HISTORY)
            .expect("create job_history table");
        txn.open_table(SESSIONS).expect("create sessions table");
        txn.open_table(CHECKLIST_LIST_CACHE)
            .expect("create checklist_list_cache table");
    }
    txn.commit().expect("commit initial table creation");
    db
}
