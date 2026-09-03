use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("redb database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("job payload (de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Transport-level failures from launching any agent subprocess -- generic
/// across whichever binary `agent::invoke` spawns (`claude` today, a future
/// OpenCode/Hermes) and across which `agent::Runner` launches it (direct
/// exec or OpenShell-wrapped). Envelope/schema failures (parsing whichever
/// agent's output into the shared `AgentEnvelope`/`IngestResult`/
/// `FactResult` shapes) live on `PromptError` instead.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("failed to spawn agent process: {0}")]
    Spawn(#[source] std::io::Error),
    // The payload is piped to the child's stdin rather than passed as an
    // argv argument: fetched URL content can push a single Claude prompt past
    // the kernel's per-argument limit (`MAX_ARG_STRLEN`, 128 KiB), which
    // surfaced as E2BIG "Argument list too long" at spawn time. stdin has no
    // such cap.
    #[error("failed to write to agent stdin: {0}")]
    StdinWrite(#[source] std::io::Error),
    #[error("agent stdin was not captured")]
    StdinUnavailable,
    #[error("agent invocation timed out")]
    Timeout,
    #[error("agent process exited with non-zero status: {status}, stderr: {stderr}")]
    NonZeroExit { status: String, stderr: String },
    /// The OpenShell gateway/sandbox boundary itself rejected the request
    /// (missing sandbox, auth failure, connect/transport failure) -- distinct
    /// from `NonZeroExit`, which is the wrapped agent running *inside* the
    /// sandbox and exiting non-zero on its own terms.
    /// `openshell_sdk::OpenShellClient::exec` never returns `Err` for the
    /// latter case, so this split costs no heuristic: the SDK's own `Result`
    /// boundary already draws it. Also surfaces from `agent::Runner::connect`
    /// at startup (gateway unreachable / sandbox never became ready), not
    /// just from a live `invoke()` call.
    #[error("openshell sandbox rejected the request: {0}")]
    SandboxRejected(#[source] openshell_sdk::SdkError),
    /// Reading the client cert/key/CA material for `agent::connect_mtls`
    /// (`OPENSHELL_MTLS_DIR`) failed -- distinct from `SandboxRejected` since
    /// this happens before any connection is attempted at all.
    #[error("failed to read OpenShell mTLS material at {path}: {source}")]
    MtlsMaterial {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Building the mTLS `Channel` itself failed (bad gateway URL, invalid
    /// PEM, TLS handshake/connect failure). Same failure class as
    /// `SandboxRejected` conceptually (the gateway boundary rejecting the
    /// connection), but kept separate because it comes from kamaji's own
    /// hand-built `Channel` (`connect_mtls`), not from
    /// `openshell_sdk::OpenShellClient::connect`.
    #[error("failed to build OpenShell mTLS channel: {0}")]
    MtlsChannel(#[source] tonic::transport::Error),
}

/// Envelope/schema-layer failures on top of `AgentError`'s transport layer --
/// shared by every `AgentFlavor` (`claude::invoke_claude`,
/// `codex::invoke_codex`/`parse_codex_jsonl`) and returned by all three of
/// `prompt.rs`'s entry points (`run_ingest_prompt`/`run_fact_prompt`/
/// `run_agent_query`), which is also why it lives here rather than as a
/// separate `CodexError`: one error type regardless of which flavor ran,
/// matching `AgentEnvelope` already being one shared success shape.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error("agent output envelope was not valid JSON: {0}")]
    EnvelopeParse(#[source] serde_json::Error),
    #[error("agent output envelope had no \"result\" text field")]
    MissingResult,
    #[error("agent result was not valid ingest JSON: {source}, raw output: {raw}")]
    SchemaParse {
        #[source]
        source: serde_json::Error,
        raw: String,
    },
    #[error("agent returned importance {0}, expected an integer in 1..=5")]
    ImportanceOutOfRange(i64),
    #[error("agent returned value {0}, expected an integer in 1..=5")]
    ValueOutOfRange(i64),
    /// `codex::parse_codex_jsonl` found no `item.completed`/`agent_message`
    /// event anywhere in the stream. Individual unparseable/unrecognized
    /// JSONL lines are skipped rather than erroring (Codex may emit event
    /// types we don't model, and a stray malformed line shouldn't fail a job
    /// that still has a valid answer elsewhere in the stream) -- this is the
    /// one case where the whole stream genuinely had nothing usable.
    #[error("codex output had no \"item.completed\"/\"agent_message\" event")]
    CodexNoAgentMessage,
}

#[derive(Debug, thiserror::Error)]
pub enum NoteError {
    #[error("failed to create notes directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write note {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read note {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A caller-supplied fact identifier (`bitacora::FactDetail::wikilink_target`)
    /// that isn't shaped like one. Distinct from `NotFound` because it means
    /// the request was malformed -- possibly a path-traversal attempt -- not
    /// that a well-formed target simply has no file behind it; only
    /// `bitacora::fact_note_path` constructs it.
    #[error("invalid fact target: {0}")]
    InvalidTarget(String),
    /// A well-formed fact target with nothing behind it. Mirrors
    /// `ChecklistError::NotFound`'s role for the checklist domains; `notes.rs`
    /// never constructs either of these two.
    #[error("{0} not found")]
    NotFound(String),
}

#[derive(Debug, thiserror::Error)]
pub enum DebugLogError {
    #[error("failed to open debug log {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write debug log {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

// `AttachmentError` (teloxide/matrix-sdk-typed) and `MatrixClientError` live
// in `kamajid::error` -- both are platform-specific and core has no
// teloxide/matrix-sdk dependency to type them against. Core's
// `worker::process_fact_command` only ever sees an already-resolved
// `ResolvedAttachment`, never a download failure.

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("connection closed before a complete message was read")]
    UnexpectedEof,
    #[error("i/o error on ipc socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed ipc message: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Shared by `todo.rs`/`goal.rs`'s common `checklist.rs` engine -- both
/// domains are "checklist entries addressed by date+position, stored as
/// dated markdown", so one error type serves both. Messages deliberately
/// don't name "todo" or "goal": `Read`/`Write`/`CreateDir` already carry
/// the path (which disambiguates domain on its own), and `NotFound`'s
/// caller already prefixes its own domain word (e.g. `worker.rs`'s `"Todo
/// {key} not found."` / `"Goal {key} not found."`). The redb/serde variants
/// are for `checklist::cache::ChecklistCache` (the last-shown-list store
/// backing shorthand-number resolution) -- same error surface as
/// `AuthError`, since it's the same kind of small redb-backed cache.
#[derive(Debug, thiserror::Error)]
pub enum ChecklistError {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} not found")]
    NotFound(String),
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("checklist cache (de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("session record (de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to spawn git {subcommand}: {source}")]
    Spawn {
        subcommand: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("git {subcommand} timed out")]
    Timeout { subcommand: &'static str },
    #[error("git {subcommand} failed: {stderr}")]
    CommandFailed {
        subcommand: &'static str,
        stderr: String,
    },
}
