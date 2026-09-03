use serde::Serialize;

use crate::checklist::EntryKey;
use crate::error::ChecklistError;
use crate::git;
use crate::queue::TodoApiOp;
use crate::state::AppState;
use crate::todo;

/// The JSON shape delivered back to `kamajid::transport::rest`'s
/// `/api/todos/*` handlers -- passed straight through as the HTTP response
/// body (see that module), so this is the actual wire contract with the
/// browser, not just an internal type. `ok: false` covers both "the
/// operation failed" and "not found"/"already in that state" -- the browser
/// only needs `message` to show the user, it doesn't branch on failure
/// reason. `entry` is only ever populated on a successful `Add` (so the UI
/// can render the new row without a full re-fetch); every other op leaves
/// it `None` and expects the caller to re-fetch `GET /api/todos` if it
/// wants the updated list.
#[derive(Serialize)]
pub(super) struct TodoApiReply {
    ok: bool,
    message: String,
    entry: Option<todo::TodoEntry>,
}

impl TodoApiReply {
    fn ok(message: impl Into<String>, entry: Option<todo::TodoEntry>) -> Self {
        TodoApiReply {
            ok: true,
            message: message.into(),
            entry,
        }
    }

    fn err(message: impl Into<String>) -> Self {
        TodoApiReply {
            ok: false,
            message: message.into(),
            entry: None,
        }
    }
}

/// Runs one `TodoApiOp` and returns `(json_reply, is_success)` --
/// `is_success` feeds `job_history`'s status, `json_reply` is what
/// `transport::send_reply` delivers to the REST handler's waiter. Unlike
/// `todo_job::process_todo_command`, there's no usage-error case to handle
/// up front: every `TodoApiOp` is already a fully-typed, validated request
/// by construction (the REST layer built it from a parsed JSON body/path
/// segment), not free-text a user typed.
pub(super) async fn process(state: &AppState, op: &TodoApiOp) -> (String, bool) {
    let reply = match op {
        TodoApiOp::Add { text, tags } => handle_add(state, text, tags).await,
        TodoApiOp::Resolve { key } => handle_resolve(state, key).await,
        TodoApiOp::Reopen { key } => handle_reopen(state, key).await,
        TodoApiOp::Edit { key, text, tags } => handle_edit(state, key, text, tags).await,
        TodoApiOp::Delete { key } => handle_delete(state, key).await,
    };
    let is_success = reply.ok;
    let json = serde_json::to_string(&reply).unwrap_or_else(|err| {
        tracing::error!(%err, "failed to serialize todo_api reply");
        r#"{"ok":false,"message":"internal serialization error","entry":null}"#.to_string()
    });
    (json, is_success)
}

fn parse_key(key: &str) -> Result<EntryKey, TodoApiReply> {
    key.parse::<EntryKey>()
        .map_err(|_| TodoApiReply::err(format!("Invalid todo key: {key}")))
}

async fn handle_add(state: &AppState, text: &str, tags: &[String]) -> TodoApiReply {
    if text.trim().is_empty() {
        return TodoApiReply::err("text must not be empty");
    }
    let when = chrono::Utc::now();
    match todo::add_entry(&state.config.notes_repo_path, when, tags, text) {
        Ok((key, path)) => {
            match commit(state, &path, &format!("Add todo {key}"), "web todo add").await {
                Ok(()) => {
                    let entry = todo::TodoEntry {
                        key,
                        tags: tags.to_vec(),
                        text: text.to_string(),
                        status: crate::checklist::Status::Open,
                        links: Vec::new(),
                    };
                    TodoApiReply::ok(format!("Todo {key} added."), Some(entry))
                }
                Err(msg) => TodoApiReply::err(format!(
                    "Todo {key} written to disk, but commit failed: {msg}"
                )),
            }
        }
        Err(err) => {
            tracing::error!(%err, "failed to add todo via web api");
            TodoApiReply::err(format!("Failed to add todo: {err}"))
        }
    }
}

async fn handle_resolve(state: &AppState, key: &str) -> TodoApiReply {
    let key = match parse_key(key) {
        Ok(k) => k,
        Err(reply) => return reply,
    };
    match todo::resolve_entry(&state.config.notes_repo_path, key) {
        Ok(todo::ResolveOutcome::AlreadyResolved) => {
            TodoApiReply::ok(format!("Todo {key} is already resolved."), None)
        }
        Ok(todo::ResolveOutcome::Resolved(path)) => {
            match commit(
                state,
                &path,
                &format!("Resolve todo {key}"),
                "web todo resolve",
            )
            .await
            {
                Ok(()) => TodoApiReply::ok(format!("Todo {key} resolved."), None),
                Err(msg) => TodoApiReply::err(format!(
                    "Todo {key} resolved on disk, but commit failed: {msg}"
                )),
            }
        }
        Err(ChecklistError::NotFound(_)) => TodoApiReply::err(format!("Todo {key} not found.")),
        Err(err) => {
            tracing::error!(%err, "failed to resolve todo via web api");
            TodoApiReply::err(format!("Failed to resolve todo: {err}"))
        }
    }
}

async fn handle_reopen(state: &AppState, key: &str) -> TodoApiReply {
    let key = match parse_key(key) {
        Ok(k) => k,
        Err(reply) => return reply,
    };
    match todo::reopen_entry(&state.config.notes_repo_path, key) {
        Ok(todo::ReopenOutcome::AlreadyOpen) => {
            TodoApiReply::ok(format!("Todo {key} is already open."), None)
        }
        Ok(todo::ReopenOutcome::Reopened(path)) => {
            match commit(
                state,
                &path,
                &format!("Reopen todo {key}"),
                "web todo reopen",
            )
            .await
            {
                Ok(()) => TodoApiReply::ok(format!("Todo {key} reopened."), None),
                Err(msg) => TodoApiReply::err(format!(
                    "Todo {key} reopened on disk, but commit failed: {msg}"
                )),
            }
        }
        Err(ChecklistError::NotFound(_)) => TodoApiReply::err(format!("Todo {key} not found.")),
        Err(err) => {
            tracing::error!(%err, "failed to reopen todo via web api");
            TodoApiReply::err(format!("Failed to reopen todo: {err}"))
        }
    }
}

async fn handle_edit(state: &AppState, key: &str, text: &str, tags: &[String]) -> TodoApiReply {
    let key = match parse_key(key) {
        Ok(k) => k,
        Err(reply) => return reply,
    };
    if text.trim().is_empty() {
        return TodoApiReply::err("text must not be empty");
    }
    match todo::edit_entry(&state.config.notes_repo_path, key, tags, text) {
        Ok(path) => {
            match commit(state, &path, &format!("Edit todo {key}"), "web todo edit").await {
                Ok(()) => TodoApiReply::ok(format!("Todo {key} updated."), None),
                Err(msg) => TodoApiReply::err(format!(
                    "Todo {key} updated on disk, but commit failed: {msg}"
                )),
            }
        }
        Err(ChecklistError::NotFound(msg)) => {
            TodoApiReply::err(format!("Todo not editable: {msg}"))
        }
        Err(err) => {
            tracing::error!(%err, "failed to edit todo via web api");
            TodoApiReply::err(format!("Failed to edit todo: {err}"))
        }
    }
}

async fn handle_delete(state: &AppState, key: &str) -> TodoApiReply {
    let key = match parse_key(key) {
        Ok(k) => k,
        Err(reply) => return reply,
    };
    match todo::hard_delete_entry(&state.config.notes_repo_path, key) {
        Ok(path) => {
            match commit(
                state,
                &path,
                &format!("Delete todo {key}"),
                "web todo delete",
            )
            .await
            {
                Ok(()) => TodoApiReply::ok(format!("Todo {key} deleted."), None),
                Err(msg) => TodoApiReply::err(format!(
                    "Todo {key} deleted on disk, but commit failed: {msg}"
                )),
            }
        }
        Err(ChecklistError::NotFound(msg)) => {
            TodoApiReply::err(format!("Todo not deletable: {msg}"))
        }
        Err(err) => {
            tracing::error!(%err, "failed to delete todo via web api");
            TodoApiReply::err(format!("Failed to delete todo: {err}"))
        }
    }
}

/// Shared commit+push tail for every write op above -- same
/// `git::commit_and_push` call `todo_job.rs` makes, just collapsed to
/// `Result<(), String>` since every caller here wants the same "log and
/// turn into a user-facing message" treatment on failure, unlike
/// `todo_job.rs` which needs the full `PushOutcome` to describe a
/// rebase-and-push in the chat reply. A `CommittedNotPushed`/
/// `PushedAfterRebase` outcome is still reported as success here (the note
/// itself is safely committed either way) -- the web UI doesn't currently
/// surface the push-outcome nuance chat replies do; that's a fine gap to
/// leave for a follow-up if it matters in practice, not a correctness bug.
async fn commit(
    state: &AppState,
    path: &std::path::Path,
    message: &str,
    log_context: &str,
) -> Result<(), String> {
    git::commit_and_push(
        &state.config.notes_repo_path,
        std::slice::from_ref(&path.to_path_buf()),
        message,
        state.config.git_timeout,
        state.config.git_push_retries,
    )
    .await
    .map(|_push_outcome| ())
    .map_err(|err| {
        tracing::error!(%err, log_context, "git commit failed");
        err.to_string()
    })
}
