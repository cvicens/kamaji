//! The web UI's structured checklist write path (`/api/todos/*`,
//! `/api/goals/*`). One handler for both domains: `todo.rs` and `goal.rs` are
//! the same `checklist` engine with a different `Config` (see
//! `checklist/mod.rs`), so the only domain-specific things here are that
//! `Config` and the noun/verb wording in the user-facing messages -- both
//! resolved once, in `domain`, rather than duplicated per domain.

use serde::Serialize;

use crate::checklist::{self, Entry, EntryKey};
use crate::error::ChecklistError;
use crate::git;
use crate::goal;
use crate::queue::{ChecklistApiOp, ChecklistDomain};
use crate::state::AppState;
use crate::todo;

/// The JSON shape delivered back to `kamajid::transport::rest`'s
/// `/api/todos/*` and `/api/goals/*` handlers -- passed straight through as
/// the HTTP response body (see that module), so this is the actual wire
/// contract with the browser, not just an internal type. `ok: false` covers
/// both "the operation failed" and "not found"/"already in that state" --
/// the browser only needs `message` to show the user, it doesn't branch on
/// failure reason. `entry` is only ever populated on a successful `Add` (so
/// the UI can render the new row without a full re-fetch); every other op
/// leaves it `None` and expects the caller to re-fetch the list endpoint if
/// it wants the updated state.
#[derive(Serialize)]
pub(super) struct ChecklistApiReply {
    ok: bool,
    message: String,
    entry: Option<Entry>,
}

impl ChecklistApiReply {
    fn ok(message: impl Into<String>, entry: Option<Entry>) -> Self {
        ChecklistApiReply {
            ok: true,
            message: message.into(),
            entry,
        }
    }

    fn err(message: impl Into<String>) -> Self {
        ChecklistApiReply {
            ok: false,
            message: message.into(),
            entry: None,
        }
    }
}

/// Everything the handlers below need to speak one domain's language: its
/// `checklist::Config` (folder, OKF type, link field, closed verb) plus the
/// capitalised singular noun the chat-free web replies address it by. Kept
/// as one lookup so adding a third checklist domain is a single arm here,
/// not a grep for every `"Todo"` string literal.
struct Domain {
    cfg: checklist::Config,
    /// e.g. "Todo" / "Goal" -- sentence-initial in every message below.
    noun: &'static str,
}

fn domain(domain: ChecklistDomain) -> Domain {
    match domain {
        ChecklistDomain::Todo => Domain {
            cfg: todo::CFG,
            noun: "Todo",
        },
        ChecklistDomain::Goal => Domain {
            cfg: goal::CFG,
            noun: "Goal",
        },
    }
}

/// Runs one `ChecklistApiOp` against `which` domain and returns
/// `(json_reply, is_success)` -- `is_success` feeds `job_history`'s status,
/// `json_reply` is what `transport::send_reply` delivers to the REST
/// handler's waiter. Unlike `todo_job::process_todo_command`, there's no
/// usage-error case to handle up front: every `ChecklistApiOp` is already a
/// fully-typed, validated request by construction (the REST layer built it
/// from a parsed JSON body/path segment), not free-text a user typed.
pub(super) async fn process(
    state: &AppState,
    which: ChecklistDomain,
    op: &ChecklistApiOp,
) -> (String, bool) {
    let d = domain(which);
    let reply = match op {
        ChecklistApiOp::Add { text, tags } => handle_add(state, &d, text, tags).await,
        ChecklistApiOp::Resolve { key } => handle_set_status(state, &d, key, true).await,
        ChecklistApiOp::Reopen { key } => handle_set_status(state, &d, key, false).await,
        ChecklistApiOp::Edit { key, text, tags } => handle_edit(state, &d, key, text, tags).await,
        ChecklistApiOp::Delete { key } => handle_delete(state, which, &d, key).await,
    };
    let is_success = reply.ok;
    let json = serde_json::to_string(&reply).unwrap_or_else(|err| {
        tracing::error!(%err, "failed to serialize checklist_api reply");
        r#"{"ok":false,"message":"internal serialization error","entry":null}"#.to_string()
    });
    (json, is_success)
}

fn parse_key(d: &Domain, key: &str) -> Result<EntryKey, ChecklistApiReply> {
    key.parse::<EntryKey>()
        .map_err(|_| ChecklistApiReply::err(format!("Invalid {} key: {key}", d.cfg.command_name)))
}

async fn handle_add(
    state: &AppState,
    d: &Domain,
    text: &str,
    tags: &[String],
) -> ChecklistApiReply {
    if text.trim().is_empty() {
        return ChecklistApiReply::err("text must not be empty");
    }
    let when = chrono::Utc::now();
    let noun = d.noun;
    match checklist::add_entry(&d.cfg, &state.config.notes_repo_path, when, tags, text) {
        Ok((key, path)) => {
            let context = format!("web {} add", d.cfg.command_name);
            let subject = format!("Add {} {key}", d.cfg.command_name);
            match commit(state, &[path], &subject, &context).await {
                Ok(()) => {
                    let entry = Entry {
                        key,
                        tags: tags.to_vec(),
                        text: text.to_string(),
                        status: checklist::Status::Open,
                        links: Vec::new(),
                    };
                    ChecklistApiReply::ok(format!("{noun} {key} added."), Some(entry))
                }
                Err(msg) => ChecklistApiReply::err(format!(
                    "{noun} {key} written to disk, but commit failed: {msg}"
                )),
            }
        }
        Err(err) => {
            tracing::error!(%err, domain = d.cfg.command_name, "failed to add entry via web api");
            ChecklistApiReply::err(format!("Failed to add {}: {err}", d.cfg.command_name))
        }
    }
}

/// Close (`resolve`/`achieve`) and reopen collapsed into one handler: the two
/// differ only in the target `Status` and the verb in the reply, and the
/// already-in-that-state / not-found / commit-failed branches around them are
/// identical.
async fn handle_set_status(
    state: &AppState,
    d: &Domain,
    key: &str,
    close: bool,
) -> ChecklistApiReply {
    let key = match parse_key(d, key) {
        Ok(k) => k,
        Err(reply) => return reply,
    };
    let noun = d.noun;
    // "resolved"/"achieved" from Config; "open" is the same word in both
    // domains, and `reopen_subcommand` is the verb's imperative form.
    let (verb, already) = if close {
        (
            d.cfg.closed_verb,
            format!("{noun} {key} is already {}.", d.cfg.closed_verb),
        )
    } else {
        ("reopened", format!("{noun} {key} is already open."))
    };
    let target = if close {
        checklist::Status::Closed
    } else {
        checklist::Status::Open
    };

    let outcome = if close {
        checklist::close_entry(&d.cfg, &state.config.notes_repo_path, key).map(|o| match o {
            checklist::CloseOutcome::Closed(p) => Some(p),
            checklist::CloseOutcome::AlreadyClosed => None,
        })
    } else {
        checklist::open_entry(&d.cfg, &state.config.notes_repo_path, key).map(|o| match o {
            checklist::OpenOutcome::Opened(p) => Some(p),
            checklist::OpenOutcome::AlreadyOpen => None,
        })
    };

    match outcome {
        Ok(None) => ChecklistApiReply::ok(already, None),
        Ok(Some(path)) => {
            // Commit subjects stay imperative and lowercase-nouned
            // ("Resolve todo ..."/"Achieve goal ..."), matching every commit
            // already in the notes repo; the past-tense `closed_verb` is for
            // the reply the user reads, not the git log.
            let subject = match target {
                checklist::Status::Closed => format!(
                    "{} {} {key}",
                    capitalize(d.cfg.close_subcommand),
                    d.cfg.command_name
                ),
                checklist::Status::Open => format!(
                    "{} {} {key}",
                    capitalize(d.cfg.reopen_subcommand),
                    d.cfg.command_name
                ),
            };
            let context = format!("web {} {verb}", d.cfg.command_name);
            match commit(state, &[path], &subject, &context).await {
                Ok(()) => ChecklistApiReply::ok(format!("{noun} {key} {verb}."), None),
                Err(msg) => ChecklistApiReply::err(format!(
                    "{noun} {key} {verb} on disk, but commit failed: {msg}"
                )),
            }
        }
        Err(ChecklistError::NotFound(_)) => {
            ChecklistApiReply::err(format!("{noun} {key} not found."))
        }
        Err(err) => {
            tracing::error!(%err, domain = d.cfg.command_name, "failed to change entry status via web api");
            ChecklistApiReply::err(format!("Failed to update {}: {err}", d.cfg.command_name))
        }
    }
}

/// Commit subjects read as imperatives ("Resolve todo …"), so the verb
/// `Config` stores in past tense for chat replies needs its first letter
/// back. Only ever fed the short ASCII verbs in `checklist::Config`.
fn capitalize(verb: &str) -> String {
    let mut chars = verb.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

async fn handle_edit(
    state: &AppState,
    d: &Domain,
    key: &str,
    text: &str,
    tags: &[String],
) -> ChecklistApiReply {
    let key = match parse_key(d, key) {
        Ok(k) => k,
        Err(reply) => return reply,
    };
    if text.trim().is_empty() {
        return ChecklistApiReply::err("text must not be empty");
    }
    let noun = d.noun;
    // `checklist::edit_entry` preserves status, links and the creation
    // timestamp -- for a goal that link list is its `demonstrated_by`
    // wikilinks, written by `/demonstrate`, so an edit here must never be
    // able to drop them.
    match checklist::edit_entry(&d.cfg, &state.config.notes_repo_path, key, tags, text) {
        Ok(path) => {
            let context = format!("web {} edit", d.cfg.command_name);
            let subject = format!("Edit {} {key}", d.cfg.command_name);
            match commit(state, &[path], &subject, &context).await {
                Ok(()) => ChecklistApiReply::ok(format!("{noun} {key} updated."), None),
                Err(msg) => ChecklistApiReply::err(format!(
                    "{noun} {key} updated on disk, but commit failed: {msg}"
                )),
            }
        }
        Err(ChecklistError::NotFound(msg)) => {
            ChecklistApiReply::err(format!("{noun} not editable: {msg}"))
        }
        Err(err) => {
            tracing::error!(%err, domain = d.cfg.command_name, "failed to edit entry via web api");
            ChecklistApiReply::err(format!("Failed to edit {}: {err}", d.cfg.command_name))
        }
    }
}

async fn handle_delete(
    state: &AppState,
    which: ChecklistDomain,
    d: &Domain,
    key: &str,
) -> ChecklistApiReply {
    let key = match parse_key(d, key) {
        Ok(k) => k,
        Err(reply) => return reply,
    };
    let repo = &state.config.notes_repo_path;

    // Referential integrity, enforced here rather than in `goal.rs` or
    // `checklist/mod.rs`: todos link *to* goals one-directionally, so only a
    // caller that can see both domains can answer "is anything still
    // pointing at this?" -- and `checklist/mod.rs` must stay
    // domain-agnostic. Refusing (rather than deleting and leaving dangling
    // wikilinks, or silently rewriting every linking todo under one click)
    // is the deliberate choice: it's the only option that neither degrades
    // `/align` and the Obsidian graph without saying so, nor edits files the
    // user never looked at. The web UI greys the button out with the same
    // reason; this is the enforcement behind it.
    if let ChecklistDomain::Goal = which {
        match todo::entries_linking_to(repo, &goal::wikilink_target(key)) {
            Ok(linking) if !linking.is_empty() => {
                let keys = linking
                    .iter()
                    .map(EntryKey::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return ChecklistApiReply::err(format!(
                    "Goal {key} still has {} todo(s) linking to it ({keys}). Unlink them first.",
                    linking.len()
                ));
            }
            Ok(_) => {}
            Err(err) => {
                tracing::error!(%err, "failed to check inbound todo links before deleting a goal");
                return ChecklistApiReply::err(format!(
                    "Failed to check what links to goal {key}: {err}"
                ));
            }
        }
    }

    let noun = d.noun;
    match checklist::delete_entry(&d.cfg, repo, key) {
        Ok(path) => {
            let context = format!("web {} delete", d.cfg.command_name);
            let subject = format!("Delete {} {key}", d.cfg.command_name);
            match commit(state, &[path], &subject, &context).await {
                Ok(()) => ChecklistApiReply::ok(format!("{noun} {key} deleted."), None),
                Err(msg) => ChecklistApiReply::err(format!(
                    "{noun} {key} deleted on disk, but commit failed: {msg}"
                )),
            }
        }
        Err(ChecklistError::NotFound(msg)) => {
            ChecklistApiReply::err(format!("{noun} not deletable: {msg}"))
        }
        Err(err) => {
            tracing::error!(%err, domain = d.cfg.command_name, "failed to delete entry via web api");
            ChecklistApiReply::err(format!("Failed to delete {}: {err}", d.cfg.command_name))
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
///
/// Takes a slice, not a single path: a fact delete removes 2-3 files that
/// must land in one commit (see `fact_api_job`), which shares this helper.
pub(super) async fn commit(
    state: &AppState,
    paths: &[std::path::PathBuf],
    message: &str,
    log_context: &str,
) -> Result<(), String> {
    git::commit_and_push(
        &state.config.notes_repo_path,
        paths,
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

#[cfg(test)]
mod tests {
    use super::*;

    // The handlers themselves need a full `AppState` (config, queue, a real
    // notes git repo) to exercise, which no other worker module unit-tests
    // either -- the domain mapping and the message-wording helpers are the
    // parts that would silently go wrong, so those are what's covered here.
    // The engine below them is tested in `checklist/mod.rs`, and the
    // end-to-end path in the browser smoke test.

    #[test]
    fn domain_maps_to_the_right_config_and_noun() {
        let todo = domain(ChecklistDomain::Todo);
        assert_eq!(todo.noun, "Todo");
        assert_eq!(todo.cfg.folder, "todo");
        assert_eq!(todo.cfg.closed_verb, "resolved");

        let goal = domain(ChecklistDomain::Goal);
        assert_eq!(goal.noun, "Goal");
        assert_eq!(goal.cfg.folder, "goals");
        assert_eq!(goal.cfg.closed_verb, "achieved");
        // The goal domain is the one that carries `demonstrated_by`; an edit
        // going through the generic engine with the wrong cfg would drop it.
        assert_eq!(goal.cfg.link_field, Some("demonstrated_by"));
    }

    #[test]
    fn capitalize_turns_a_stored_verb_into_a_commit_subject() {
        // Commit subjects read "Resolve todo ..." / "Achieve goal ...", so
        // it's the imperative `close_subcommand` that gets capitalized, not
        // the past-tense `closed_verb` the chat reply uses.
        assert_eq!(capitalize(todo::CFG.close_subcommand), "Resolve");
        assert_eq!(capitalize(goal::CFG.close_subcommand), "Achieve");
        assert_eq!(capitalize(todo::CFG.reopen_subcommand), "Reopen");
        assert_eq!(capitalize(""), "");
    }
}
