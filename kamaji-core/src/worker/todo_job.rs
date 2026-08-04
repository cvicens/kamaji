use chrono::Utc;

use crate::chat::ChatRef;
use crate::git;
use crate::state::AppState;
use crate::todo;

use super::{command_prompt, describe_push_outcome};

/// A `Shorthand` reference that didn't resolve against `state.checklist_cache`
/// (no recent `/todo list` for this chat, or the number's out of range) --
/// turned into a user-facing reply rather than an error, since it's an
/// expected outcome, not a bug.
fn no_recent_list_reply(cfg_name: &str) -> String {
    format!(
        "No recent /{cfg_name} list to reference in this chat -- run /{cfg_name} list first, \
         or use the full YYYY-MM-DD-N key."
    )
}

/// `/todo add|list|resolve|reopen`: unlike `/ingest`/`/fact`, never invokes
/// Claude -- `add`/`resolve`/`reopen` write directly to
/// `todo/<year>/<month>.md` in the notes repo and commit+push, `list` is
/// read-only and never touches git (it does write to `state.checklist_cache`,
/// a redb cache table, but that's bookkeeping for the `<n>` shorthand, not
/// the notes repo). Usage errors are unreachable in practice (routing
/// already replies and skips the queue for those, mirroring `/ingest`/
/// `/fact`), but handled here defensively rather than assumed impossible.
pub(super) async fn process_todo_command(
    state: &AppState,
    chat: &ChatRef,
    args: &[String],
) -> (String, bool, Option<String>, String) {
    let debug_prompt = command_prompt("todo", args);
    let action = match todo::parse_command(args) {
        Ok(action) => action,
        Err(usage) => return (usage.clone(), false, Some(usage), debug_prompt),
    };

    match action {
        todo::TodoAction::List { filter } => {
            match todo::list_entries(&state.config.notes_repo_path, filter) {
                Ok(entries) => {
                    if let Err(err) = todo::remember_list(&state.checklist_cache, chat, &entries) {
                        // Only the `<n>` shorthand for a *future* command is
                        // at stake here, not this list reply -- log and
                        // still show the list rather than fail the command.
                        tracing::error!(%err, "failed to cache todo list for shorthand reference");
                    }
                    (
                        todo::format_list(&entries, filter),
                        true,
                        None,
                        debug_prompt,
                    )
                }
                Err(err) => {
                    tracing::error!(%err, "failed to list todos");
                    let msg = format!("Failed to list todos: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
        todo::TodoAction::Add { tags, text } => {
            match todo::add_entry(&state.config.notes_repo_path, Utc::now(), &tags, &text) {
                Ok((key, path)) => {
                    let push_outcome = match git::commit_and_push(
                        &state.config.notes_repo_path,
                        std::slice::from_ref(&path),
                        &format!("Add todo {key}"),
                        state.config.git_timeout,
                        state.config.git_push_retries,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            tracing::error!(%err, "git commit failed for /todo add");
                            let msg = format!(
                                "Todo {key} written to disk, but commit failed: {err}\nPath: {}",
                                path.display()
                            );
                            return (msg.clone(), false, Some(msg), debug_prompt);
                        }
                    };
                    let confirmation = format!("Todo {key} added.");
                    let reply = describe_push_outcome(confirmation, push_outcome);
                    (reply, true, None, debug_prompt)
                }
                Err(err) => {
                    tracing::error!(%err, "failed to write todo entry");
                    let msg = format!("Failed to add todo: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
        todo::TodoAction::Resolve { reference } => {
            let key = match todo::resolve_reference(&state.checklist_cache, chat, reference) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    let msg = no_recent_list_reply("todo");
                    return (msg, true, None, debug_prompt);
                }
                Err(err) => {
                    tracing::error!(%err, "failed to resolve todo reference");
                    let msg = format!("Failed to resolve todo reference: {err}");
                    return (msg.clone(), false, Some(msg), debug_prompt);
                }
            };
            match todo::resolve_entry(&state.config.notes_repo_path, key) {
                Ok(todo::ResolveOutcome::AlreadyResolved) => (
                    format!("Todo {key} is already resolved."),
                    true,
                    None,
                    debug_prompt,
                ),
                Ok(todo::ResolveOutcome::Resolved(path)) => {
                    let push_outcome = match git::commit_and_push(
                        &state.config.notes_repo_path,
                        std::slice::from_ref(&path),
                        &format!("Resolve todo {key}"),
                        state.config.git_timeout,
                        state.config.git_push_retries,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            tracing::error!(%err, "git commit failed for /todo resolve");
                            let msg = format!(
                                "Todo {key} resolved on disk, but commit failed: {err}\nPath: {}",
                                path.display()
                            );
                            return (msg.clone(), false, Some(msg), debug_prompt);
                        }
                    };
                    let confirmation = format!("Todo {key} resolved.");
                    let reply = describe_push_outcome(confirmation, push_outcome);
                    (reply, true, None, debug_prompt)
                }
                Err(crate::error::ChecklistError::NotFound(_)) => {
                    (format!("Todo {key} not found."), true, None, debug_prompt)
                }
                Err(err) => {
                    tracing::error!(%err, "failed to resolve todo");
                    let msg = format!("Failed to resolve todo: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
        todo::TodoAction::Reopen { reference } => {
            let key = match todo::resolve_reference(&state.checklist_cache, chat, reference) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    let msg = no_recent_list_reply("todo");
                    return (msg, true, None, debug_prompt);
                }
                Err(err) => {
                    tracing::error!(%err, "failed to resolve todo reference");
                    let msg = format!("Failed to resolve todo reference: {err}");
                    return (msg.clone(), false, Some(msg), debug_prompt);
                }
            };
            match todo::reopen_entry(&state.config.notes_repo_path, key) {
                Ok(todo::ReopenOutcome::AlreadyOpen) => (
                    format!("Todo {key} is already open."),
                    true,
                    None,
                    debug_prompt,
                ),
                Ok(todo::ReopenOutcome::Reopened(path)) => {
                    let push_outcome = match git::commit_and_push(
                        &state.config.notes_repo_path,
                        std::slice::from_ref(&path),
                        &format!("Reopen todo {key}"),
                        state.config.git_timeout,
                        state.config.git_push_retries,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            tracing::error!(%err, "git commit failed for /todo reopen");
                            let msg = format!(
                                "Todo {key} reopened on disk, but commit failed: {err}\nPath: {}",
                                path.display()
                            );
                            return (msg.clone(), false, Some(msg), debug_prompt);
                        }
                    };
                    let confirmation = format!("Todo {key} reopened.");
                    let reply = describe_push_outcome(confirmation, push_outcome);
                    (reply, true, None, debug_prompt)
                }
                Err(crate::error::ChecklistError::NotFound(_)) => {
                    (format!("Todo {key} not found."), true, None, debug_prompt)
                }
                Err(err) => {
                    tracing::error!(%err, "failed to reopen todo");
                    let msg = format!("Failed to reopen todo: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
        todo::TodoAction::Link {
            todo_reference,
            goal_key,
        } => {
            let todo_key =
                match todo::resolve_reference(&state.checklist_cache, chat, todo_reference) {
                    Ok(Some(key)) => key,
                    Ok(None) => {
                        let msg = no_recent_list_reply("todo");
                        return (msg, true, None, debug_prompt);
                    }
                    Err(err) => {
                        tracing::error!(%err, "failed to resolve todo reference");
                        let msg = format!("Failed to resolve todo reference: {err}");
                        return (msg.clone(), false, Some(msg), debug_prompt);
                    }
                };
            match todo::link_to_goal(&state.config.notes_repo_path, todo_key, goal_key) {
                Ok(todo::LinkOutcome::AlreadyLinked) => (
                    format!("Todo {todo_key} is already linked to goal {goal_key}."),
                    true,
                    None,
                    debug_prompt,
                ),
                Ok(todo::LinkOutcome::Linked(path)) => {
                    let push_outcome = match git::commit_and_push(
                        &state.config.notes_repo_path,
                        std::slice::from_ref(&path),
                        &format!("Link todo {todo_key} to goal {goal_key}"),
                        state.config.git_timeout,
                        state.config.git_push_retries,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            tracing::error!(%err, "git commit failed for /todo link");
                            let msg = format!(
                                "Todo {todo_key} linked to goal {goal_key} on disk, but commit failed: {err}\nPath: {}",
                                path.display()
                            );
                            return (msg.clone(), false, Some(msg), debug_prompt);
                        }
                    };
                    let confirmation = format!("Todo {todo_key} linked to goal {goal_key}.");
                    let reply = describe_push_outcome(confirmation, push_outcome);
                    (reply, true, None, debug_prompt)
                }
                // Unlike Resolve/Reopen above, this can't reconstruct "Todo
                // {key} not found" -- the NotFound could be about either the
                // todo or the goal, so it uses the embedded string directly
                // (see `todo::link_to_goal`'s doc comment).
                Err(crate::error::ChecklistError::NotFound(msg)) => (
                    format!("Link failed: {msg} not found."),
                    true,
                    None,
                    debug_prompt,
                ),
                Err(err) => {
                    tracing::error!(%err, "failed to link todo to goal");
                    let msg = format!("Failed to link todo to goal: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
    }
}
