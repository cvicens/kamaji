use chrono::Utc;

use crate::chat::ChatRef;
use crate::git;
use crate::goal;
use crate::state::AppState;

use super::{command_prompt, describe_push_outcome};

/// A `Shorthand` reference that didn't resolve against `state.checklist_cache`
/// (no recent `/goal list` for this chat, or the number's out of range) --
/// turned into a user-facing reply rather than an error, since it's an
/// expected outcome, not a bug.
fn no_recent_list_reply(cfg_name: &str) -> String {
    format!(
        "No recent /{cfg_name} list to reference in this chat -- run /{cfg_name} list first, \
         or use the full YYYY-MM-DD-N key."
    )
}

/// `/goal add|list|achieve|reopen`: mirrors `process_todo_command` exactly
/// -- never invokes Claude, `add`/`achieve`/`reopen` write directly to
/// `goals/<year>/<month>.md` in the notes repo and commit+push, `list` is
/// read-only and never touches git (beyond `state.checklist_cache`
/// bookkeeping for the `<n>` shorthand). Usage errors are unreachable in
/// practice (routing already replies and skips the queue for those,
/// mirroring `/todo`), but handled here defensively rather than assumed
/// impossible.
pub(super) async fn process_goal_command(
    state: &AppState,
    chat: &ChatRef,
    args: &[String],
) -> (String, bool, Option<String>, String) {
    let debug_prompt = command_prompt("goal", args);
    let action = match goal::parse_command(args) {
        Ok(action) => action,
        Err(usage) => return (usage.clone(), false, Some(usage), debug_prompt),
    };

    match action {
        goal::GoalAction::List { filter } => {
            match goal::list_entries(&state.config.notes_repo_path, filter) {
                Ok(entries) => {
                    if let Err(err) = goal::remember_list(&state.checklist_cache, chat, &entries) {
                        // Only the `<n>` shorthand for a *future* command is
                        // at stake here, not this list reply -- log and
                        // still show the list rather than fail the command.
                        tracing::error!(%err, "failed to cache goal list for shorthand reference");
                    }
                    (
                        goal::format_list(&entries, filter),
                        true,
                        None,
                        debug_prompt,
                    )
                }
                Err(err) => {
                    tracing::error!(%err, "failed to list goals");
                    let msg = format!("Failed to list goals: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
        goal::GoalAction::Add { tags, text } => {
            match goal::add_entry(&state.config.notes_repo_path, Utc::now(), &tags, &text) {
                Ok((key, path)) => {
                    let push_outcome = match git::commit_and_push(
                        &state.config.notes_repo_path,
                        std::slice::from_ref(&path),
                        &format!("Add goal {key}"),
                        state.config.git_timeout,
                        state.config.git_push_retries,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            tracing::error!(%err, "git commit failed for /goal add");
                            let msg = format!(
                                "Goal {key} written to disk, but commit failed: {err}\nPath: {}",
                                path.display()
                            );
                            return (msg.clone(), false, Some(msg), debug_prompt);
                        }
                    };
                    let confirmation = format!("Goal {key} added.");
                    let reply = describe_push_outcome(confirmation, push_outcome);
                    (reply, true, None, debug_prompt)
                }
                Err(err) => {
                    tracing::error!(%err, "failed to write goal entry");
                    let msg = format!("Failed to add goal: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
        goal::GoalAction::Achieve { reference } => {
            let key = match goal::resolve_reference(&state.checklist_cache, chat, reference) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    let msg = no_recent_list_reply("goal");
                    return (msg, true, None, debug_prompt);
                }
                Err(err) => {
                    tracing::error!(%err, "failed to resolve goal reference");
                    let msg = format!("Failed to resolve goal reference: {err}");
                    return (msg.clone(), false, Some(msg), debug_prompt);
                }
            };
            match goal::achieve_entry(&state.config.notes_repo_path, key) {
                Ok(goal::AchieveOutcome::AlreadyAchieved) => (
                    format!("Goal {key} is already achieved."),
                    true,
                    None,
                    debug_prompt,
                ),
                Ok(goal::AchieveOutcome::Achieved(path)) => {
                    let push_outcome = match git::commit_and_push(
                        &state.config.notes_repo_path,
                        std::slice::from_ref(&path),
                        &format!("Achieve goal {key}"),
                        state.config.git_timeout,
                        state.config.git_push_retries,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            tracing::error!(%err, "git commit failed for /goal achieve");
                            let msg = format!(
                                "Goal {key} achieved on disk, but commit failed: {err}\nPath: {}",
                                path.display()
                            );
                            return (msg.clone(), false, Some(msg), debug_prompt);
                        }
                    };
                    let confirmation = format!("Goal {key} achieved.");
                    let reply = describe_push_outcome(confirmation, push_outcome);
                    (reply, true, None, debug_prompt)
                }
                Err(crate::error::ChecklistError::NotFound(_)) => {
                    (format!("Goal {key} not found."), true, None, debug_prompt)
                }
                Err(err) => {
                    tracing::error!(%err, "failed to achieve goal");
                    let msg = format!("Failed to achieve goal: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
        goal::GoalAction::Reopen { reference } => {
            let key = match goal::resolve_reference(&state.checklist_cache, chat, reference) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    let msg = no_recent_list_reply("goal");
                    return (msg, true, None, debug_prompt);
                }
                Err(err) => {
                    tracing::error!(%err, "failed to resolve goal reference");
                    let msg = format!("Failed to resolve goal reference: {err}");
                    return (msg.clone(), false, Some(msg), debug_prompt);
                }
            };
            match goal::reopen_entry(&state.config.notes_repo_path, key) {
                Ok(goal::ReopenOutcome::AlreadyOpen) => (
                    format!("Goal {key} is already open."),
                    true,
                    None,
                    debug_prompt,
                ),
                Ok(goal::ReopenOutcome::Reopened(path)) => {
                    let push_outcome = match git::commit_and_push(
                        &state.config.notes_repo_path,
                        std::slice::from_ref(&path),
                        &format!("Reopen goal {key}"),
                        state.config.git_timeout,
                        state.config.git_push_retries,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            tracing::error!(%err, "git commit failed for /goal reopen");
                            let msg = format!(
                                "Goal {key} reopened on disk, but commit failed: {err}\nPath: {}",
                                path.display()
                            );
                            return (msg.clone(), false, Some(msg), debug_prompt);
                        }
                    };
                    let confirmation = format!("Goal {key} reopened.");
                    let reply = describe_push_outcome(confirmation, push_outcome);
                    (reply, true, None, debug_prompt)
                }
                Err(crate::error::ChecklistError::NotFound(_)) => {
                    (format!("Goal {key} not found."), true, None, debug_prompt)
                }
                Err(err) => {
                    tracing::error!(%err, "failed to reopen goal");
                    let msg = format!("Failed to reopen goal: {err}");
                    (msg.clone(), false, Some(msg), debug_prompt)
                }
            }
        }
    }
}
