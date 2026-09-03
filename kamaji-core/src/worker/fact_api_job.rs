//! The web UI's structured `/fact` write path (`PATCH`/`DELETE /api/facts`).
//! Separate from `checklist_api_job` because a fact is not a checklist entry:
//! no `EntryKey`, no open/closed status, identity is a file path, and one
//! entry is 2-3 files on disk (see `bitacora::write_fact`).

use serde::Serialize;

use crate::bitacora;
use crate::error::NoteError;
use crate::goal;
use crate::queue::FactApiOp;
use crate::state::AppState;

use super::checklist_api_job::commit;

/// The JSON shape delivered back to `kamajid::transport::rest`'s
/// `/api/facts` handlers -- same `ok`/`message` contract the checklist API
/// already has with the browser (see `checklist_api_job::ChecklistApiReply`),
/// minus the `entry` field: neither op here produces a row the UI can render
/// without re-fetching, since one is a rewrite and the other a removal.
#[derive(Serialize)]
pub(super) struct FactApiReply {
    ok: bool,
    message: String,
}

impl FactApiReply {
    fn ok(message: impl Into<String>) -> Self {
        FactApiReply {
            ok: true,
            message: message.into(),
        }
    }

    fn err(message: impl Into<String>) -> Self {
        FactApiReply {
            ok: false,
            message: message.into(),
        }
    }
}

/// Runs one `FactApiOp` and returns `(json_reply, is_success)`, matching
/// `checklist_api_job::process`'s contract with the worker loop.
pub(super) async fn process(state: &AppState, op: &FactApiOp) -> (String, bool) {
    let reply = match op {
        FactApiOp::Edit {
            target,
            title,
            summary,
            value,
            tags,
        } => handle_edit(state, target, title, summary, *value, tags).await,
        FactApiOp::Delete { target } => handle_delete(state, target).await,
    };
    let is_success = reply.ok;
    let json = serde_json::to_string(&reply).unwrap_or_else(|err| {
        tracing::error!(%err, "failed to serialize fact_api reply");
        r#"{"ok":false,"message":"internal serialization error"}"#.to_string()
    });
    (json, is_success)
}

/// The `value` range every fact is already held to -- `prompt::parse_fact_result`
/// rejects an agent response outside it with `PromptError::ValueOutOfRange`,
/// so a hand-edited value must not be allowed to smuggle a fact out of that
/// range through the back door.
const VALUE_RANGE: std::ops::RangeInclusive<i64> = 1..=5;

async fn handle_edit(
    state: &AppState,
    target: &str,
    title: &str,
    summary: &str,
    value: i64,
    tags: &[String],
) -> FactApiReply {
    if title.trim().is_empty() {
        return FactApiReply::err("title must not be empty");
    }
    if summary.trim().is_empty() {
        return FactApiReply::err("summary must not be empty");
    }
    if !VALUE_RANGE.contains(&value) {
        return FactApiReply::err(format!("value must be an integer from 1 to 5, got {value}"));
    }

    let repo = &state.config.notes_repo_path;
    match bitacora::edit_fact(repo, target, title.trim(), summary.trim(), value, tags) {
        Ok(path) => {
            let subject = format!("Edit fact {}", short_name(target));
            match commit(state, &[path], &subject, "web fact edit").await {
                Ok(()) => {
                    FactApiReply::ok("Fact updated. The original message (.orig) is unchanged.")
                }
                Err(msg) => {
                    FactApiReply::err(format!("Fact updated on disk, but commit failed: {msg}"))
                }
            }
        }
        Err(err @ (NoteError::InvalidTarget(_) | NoteError::NotFound(_))) => {
            FactApiReply::err(format!("Fact not editable: {err}"))
        }
        Err(err) => {
            tracing::error!(%err, "failed to edit fact via web api");
            FactApiReply::err(format!("Failed to edit fact: {err}"))
        }
    }
}

async fn handle_delete(state: &AppState, target: &str) -> FactApiReply {
    let repo = &state.config.notes_repo_path;

    // Counted *before* the delete, while the target still exists, so the
    // reply can name what was orphaned. Goals link to facts one-directionally
    // (`/demonstrate` writes `demonstrated_by`) and a fact can't hold a
    // backlink, so a dangling wikilink is the accepted outcome here -- but an
    // unreported one would not be. A failure to count is not a reason to
    // refuse the delete the user explicitly confirmed; it just costs the
    // detail in the message.
    let orphaned = match goal::list_entries(repo, goal::StatusFilter::Open).and_then(|mut open| {
        goal::list_entries(repo, goal::StatusFilter::Closed).map(|closed| {
            open.extend(closed);
            open
        })
    }) {
        Ok(goals) => Some(
            goals
                .iter()
                .filter(|g| g.links.iter().any(|l| l == target))
                .map(|g| g.key.to_string())
                .collect::<Vec<_>>(),
        ),
        Err(err) => {
            tracing::warn!(%err, "failed to count goals linking to a fact being deleted");
            None
        }
    };

    match bitacora::delete_fact(repo, target) {
        Ok(removed) => {
            let file_count = removed.len();
            let subject = format!("Delete fact {}", short_name(target));
            match commit(state, &removed, &subject, "web fact delete").await {
                Ok(()) => FactApiReply::ok(format!(
                    "Fact deleted ({file_count} file(s) removed).{}",
                    describe_orphans(orphaned.as_deref())
                )),
                Err(msg) => {
                    FactApiReply::err(format!("Fact deleted on disk, but commit failed: {msg}"))
                }
            }
        }
        Err(err @ (NoteError::InvalidTarget(_) | NoteError::NotFound(_))) => {
            FactApiReply::err(format!("Fact not deletable: {err}"))
        }
        Err(err) => {
            tracing::error!(%err, "failed to delete fact via web api");
            FactApiReply::err(format!("Failed to delete fact: {err}"))
        }
    }
}

/// The trailing sentence naming the goals whose `demonstrated_by` links now
/// dangle. `None` means the count itself failed (see `handle_delete`), which
/// is said plainly rather than being rendered as "no goals affected".
fn describe_orphans(orphaned: Option<&[String]>) -> String {
    match orphaned {
        None => " (couldn't check which goals linked to it)".to_string(),
        Some([]) => String::new(),
        Some(keys) => format!(
            " {} goal(s) still link to it ({}); those links now dangle.",
            keys.len(),
            keys.join(", ")
        ),
    }
}

/// Just the `<stamp>-<slug>` part of a wikilink target, for a commit subject
/// that doesn't repeat the year/month already implied by the path git shows.
fn short_name(target: &str) -> &str {
    target.rsplit('/').next().unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_name_takes_the_last_path_segment() {
        assert_eq!(
            short_name("bitacora/2026/July/20260714-153045-fixed-prod-outage"),
            "20260714-153045-fixed-prod-outage"
        );
        assert_eq!(short_name("no-slashes"), "no-slashes");
    }

    #[test]
    fn describe_orphans_distinguishes_none_empty_and_some() {
        // A failed count must never read as "nothing linked to it".
        assert!(describe_orphans(None).contains("couldn't check"));
        assert_eq!(describe_orphans(Some(&[])), "");
        let keys = ["2026-08-02-1".to_string(), "2026-08-04-3".to_string()];
        let text = describe_orphans(Some(&keys));
        assert!(text.contains("2 goal(s)"));
        assert!(text.contains("2026-08-02-1, 2026-08-04-3"));
        assert!(text.contains("dangle"));
    }

    #[test]
    fn value_range_matches_the_fact_prompt_contract() {
        assert!(VALUE_RANGE.contains(&1));
        assert!(VALUE_RANGE.contains(&5));
        assert!(!VALUE_RANGE.contains(&0));
        assert!(!VALUE_RANGE.contains(&6));
    }
}
