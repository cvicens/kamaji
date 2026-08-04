//! `/align`: loads open goals/TODOs, auto-links TODOs to goals they share a
//! tag with (writing to the notes git repo -- this is why `align` is
//! `CommandMode::Queued`, not `Sync`, unlike its Phase 1 read-only-report
//! incarnation), and reports the resulting alignment. A TODO whose tag
//! overlap would connect it to more than `Config::align_noisy_tag_threshold`
//! not-yet-linked open goals is treated as a noisy/too-generic match and
//! auto-linked to *none* of them -- surfaced in its own report section for
//! manual `/todo link` instead of guessing which (if any) is right.

use std::collections::{BTreeMap, BTreeSet};

use crate::checklist::{Entry, EntryKey};
use crate::git;
use crate::goal;
use crate::state::AppState;
use crate::todo;

use super::describe_push_outcome;

pub(super) async fn process_align_command(
    state: &AppState,
) -> (String, bool, Option<String>, String) {
    let debug_prompt = "/align".to_string();
    let repo_root = &state.config.notes_repo_path;

    let goals = match goal::list_entries(repo_root, goal::StatusFilter::Open) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::error!(%err, "failed to list goals for /align");
            let msg = format!("Failed to read goals: {err}");
            return (msg.clone(), false, Some(msg), debug_prompt);
        }
    };
    let todos = match todo::list_entries(repo_root, todo::StatusFilter::Open) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::error!(%err, "failed to list todos for /align");
            let msg = format!("Failed to read todos: {err}");
            return (msg.clone(), false, Some(msg), debug_prompt);
        }
    };

    let threshold = state.config.align_noisy_tag_threshold;
    let mut changed_paths = Vec::new();
    let mut newly_linked: Vec<(EntryKey, EntryKey)> = Vec::new();
    let mut noisy: Vec<(EntryKey, Vec<EntryKey>)> = Vec::new();

    for t in &todos {
        let candidates = candidate_goals(t, &goals);
        if candidates.is_empty() {
            continue;
        }
        if candidates.len() as u32 > threshold {
            noisy.push((t.key, candidates.iter().map(|g| g.key).collect()));
            continue;
        }
        for g in candidates {
            match todo::link_to_goal(repo_root, t.key, g.key) {
                Ok(todo::LinkOutcome::Linked(path)) => {
                    if !changed_paths.contains(&path) {
                        changed_paths.push(path);
                    }
                    newly_linked.push((t.key, g.key));
                }
                // Nothing to write -- another run (or a manual /todo link)
                // already got there first.
                Ok(todo::LinkOutcome::AlreadyLinked) => {}
                Err(err) => {
                    // One bad pair shouldn't sink the whole run -- log and
                    // keep going with the rest, per the codebase's
                    // log-and-skip convention.
                    tracing::error!(%err, todo = %t.key, goal = %g.key, "failed to auto-link todo to goal");
                }
            }
        }
    }

    if !changed_paths.is_empty() {
        let message = format!(
            "Align: linked {} todo(s) to matching goal(s)",
            newly_linked.len()
        );
        let push_outcome = match git::commit_and_push(
            repo_root,
            &changed_paths,
            &message,
            state.config.git_timeout,
            state.config.git_push_retries,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::error!(%err, "git commit failed for /align auto-linking");
                let msg = format!(
                    "Align linked {} todo(s) on disk, but commit failed: {err}",
                    newly_linked.len()
                );
                return (msg.clone(), false, Some(msg), debug_prompt);
            }
        };
        // Re-list so the report reflects the links just written.
        let todos = match todo::list_entries(repo_root, todo::StatusFilter::Open) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::error!(%err, "failed to re-list todos after /align linking");
                todos
            }
        };
        let report = align_report(&goals, &todos, &noisy, &newly_linked);
        let reply = describe_push_outcome(report, push_outcome);
        return (reply, true, None, debug_prompt);
    }

    let report = align_report(&goals, &todos, &noisy, &newly_linked);
    (report, true, None, debug_prompt)
}

/// Open goals sharing at least one tag with `todo`, excluding goals `todo`
/// already links to -- the candidate set a `/align` run would auto-link
/// `todo` to, before the noisy-tag threshold is applied.
fn candidate_goals<'a>(todo: &Entry, goals: &'a [Entry]) -> Vec<&'a Entry> {
    goals
        .iter()
        .filter(|g| g.tags.iter().any(|tag| todo.tags.contains(tag)))
        .filter(|g| !todo.links.contains(&goal::wikilink_target(g.key)))
        .collect()
}

/// Builds the `/align` report body from already-loaded open goals/TODOs
/// (`todos` reflecting any links just written this run), plus the
/// noisy-candidate set and the (todo, goal) pairs newly linked this run (for
/// the `(new)` marker). Pure and filesystem-free -- the grouping logic is
/// the part actually worth testing, `process_align_command` is the only
/// caller that touches disk/git.
///
/// A TODO with any link is accounted for under "Goals and their linked
/// TODOs" for every goal it links to (a TODO can support several goals); a
/// TODO with candidates that got skipped for being noisy gets its own
/// section instead of silently vanishing; everything else falls into the
/// plain "no linked goal"/"no linked TODO" gap-finder sections.
fn align_report(
    goals: &[Entry],
    todos: &[Entry],
    noisy: &[(EntryKey, Vec<EntryKey>)],
    newly_linked: &[(EntryKey, EntryKey)],
) -> String {
    let goal_by_target: std::collections::HashMap<String, &Entry> = goals
        .iter()
        .map(|g| (goal::wikilink_target(g.key), g))
        .collect();

    let noisy_keys: BTreeSet<EntryKey> = noisy.iter().map(|(k, _)| *k).collect();
    let unmatched_todos: Vec<&Entry> = todos
        .iter()
        .filter(|t| t.links.is_empty() && !noisy_keys.contains(&t.key))
        .collect();

    let mut goal_to_todos: BTreeMap<EntryKey, Vec<&Entry>> = BTreeMap::new();
    for t in todos {
        for target in &t.links {
            if let Some(g) = goal_by_target.get(target) {
                goal_to_todos.entry(g.key).or_default().push(t);
            }
        }
    }
    let unmatched_goals: Vec<&Entry> = goals
        .iter()
        .filter(|g| !goal_to_todos.contains_key(&g.key))
        .collect();

    let mut lines = vec!["Goal/TODO alignment:".to_string(), String::new()];

    lines.push(format!(
        "Goals with no linked TODO ({}):",
        unmatched_goals.len()
    ));
    if unmatched_goals.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        lines.extend(
            unmatched_goals
                .iter()
                .map(|g| format!("  {}", format_entry(g))),
        );
    }
    lines.push(String::new());

    lines.push(format!(
        "TODOs with no linked goal ({}):",
        unmatched_todos.len()
    ));
    if unmatched_todos.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        lines.extend(
            unmatched_todos
                .iter()
                .map(|t| format!("  {}", format_entry(t))),
        );
    }
    lines.push(String::new());

    lines.push(format!(
        "TODOs with too many candidate goals, needs manual /todo link ({}):",
        noisy.len()
    ));
    if noisy.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (todo_key, candidate_keys) in noisy {
            let Some(t) = todos.iter().find(|t| t.key == *todo_key) else {
                continue;
            };
            let candidates_display = candidate_keys
                .iter()
                .map(EntryKey::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "  {} ({} candidate goals: {})",
                format_entry(t),
                candidate_keys.len(),
                candidates_display
            ));
        }
    }
    lines.push(String::new());

    lines.push(format!(
        "Goals and their linked TODOs ({} of {} goals have support):",
        goal_to_todos.len(),
        goals.len()
    ));
    if goal_to_todos.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (goal_key, linked) in &goal_to_todos {
            let Some(g) = goals.iter().find(|g| g.key == *goal_key) else {
                continue;
            };
            lines.push(format!("  goal {}:", format_entry(g)));
            for t in linked {
                let is_new = newly_linked
                    .iter()
                    .any(|(tk, gk)| tk == &t.key && gk == goal_key);
                let suffix = if is_new { " (new)" } else { "" };
                lines.push(format!("    todo {}{suffix}", format_entry(t)));
            }
        }
    }

    lines.join("\n")
}

fn format_entry(entry: &Entry) -> String {
    format!("{} [{}] {}", entry.key, entry.tags.join(", "), entry.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checklist::Status;

    fn entry(line: u32, tags: &[&str], text: &str) -> Entry {
        Entry {
            key: EntryKey {
                date: chrono::NaiveDate::from_ymd_opt(2026, 7, 17).unwrap(),
                line,
            },
            tags: tags.iter().map(|s| s.to_string()).collect(),
            text: text.to_string(),
            status: Status::Open,
            links: Vec::new(),
        }
    }

    fn linked_entry(line: u32, tags: &[&str], text: &str, link_targets: &[&str]) -> Entry {
        Entry {
            links: link_targets.iter().map(|s| s.to_string()).collect(),
            ..entry(line, tags, text)
        }
    }

    #[test]
    fn candidate_goals_matches_on_shared_tag_excludes_already_linked() {
        let goals = vec![
            entry(1, &["health"], "run a marathon"),
            entry(2, &["work"], "ship the project"),
        ];
        let already_linked_target = goal::wikilink_target(goals[0].key);
        let t = linked_entry(
            3,
            &["health", "work"],
            "train",
            &[already_linked_target.as_str()],
        );
        let candidates = candidate_goals(&t, &goals);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].key, goals[1].key);
    }

    #[test]
    fn candidate_goals_empty_when_no_tag_overlap() {
        let goals = vec![entry(1, &["health"], "run a marathon")];
        let t = entry(2, &["work"], "finish the report");
        assert!(candidate_goals(&t, &goals).is_empty());
    }

    #[test]
    fn align_report_no_overlap_lands_both_sides_unmatched() {
        let goals = vec![entry(1, &["health"], "run a marathon")];
        let todos = vec![entry(2, &["work"], "finish the report")];
        let report = align_report(&goals, &todos, &[], &[]);
        assert!(report.contains("Goals with no linked TODO (1):"));
        assert!(report.contains("2026-07-17-1 [health] run a marathon"));
        assert!(report.contains("TODOs with no linked goal (1):"));
        assert!(report.contains("2026-07-17-2 [work] finish the report"));
        assert!(report.contains("Goals and their linked TODOs (0 of 1 goals have support):"));
    }

    #[test]
    fn align_report_linked_todo_grouped_under_its_goal_and_marked_new() {
        let goals = vec![entry(1, &["work"], "ship the project")];
        let target = goal::wikilink_target(goals[0].key);
        let todos = vec![linked_entry(
            2,
            &["work"],
            "write the doc",
            &[target.as_str()],
        )];
        let report = align_report(&goals, &todos, &[], &[(todos[0].key, goals[0].key)]);
        assert!(report.contains("Goals with no linked TODO (0):"));
        assert!(report.contains("TODOs with no linked goal (0):"));
        assert!(report.contains("Goals and their linked TODOs (1 of 1 goals have support):"));
        assert!(report.contains("goal 2026-07-17-1 [work] ship the project:"));
        assert!(report.contains("todo 2026-07-17-2 [work] write the doc (new)"));
    }

    #[test]
    fn align_report_previously_linked_todo_has_no_new_marker() {
        let goals = vec![entry(1, &["work"], "ship the project")];
        let target = goal::wikilink_target(goals[0].key);
        let todos = vec![linked_entry(
            2,
            &["work"],
            "write the doc",
            &[target.as_str()],
        )];
        // No newly_linked entries -- this link already existed before this run.
        let report = align_report(&goals, &todos, &[], &[]);
        assert!(report.contains("todo 2026-07-17-2 [work] write the doc"));
        assert!(!report.contains("(new)"));
    }

    #[test]
    fn align_report_one_todo_supports_two_goals() {
        let goals = vec![
            entry(1, &["health"], "run a marathon"),
            entry(2, &["work"], "ship the project"),
        ];
        let targets = [
            goal::wikilink_target(goals[0].key),
            goal::wikilink_target(goals[1].key),
        ];
        let todos = vec![linked_entry(
            3,
            &["health", "work"],
            "train and write docs",
            &[targets[0].as_str(), targets[1].as_str()],
        )];
        let report = align_report(&goals, &todos, &[], &[]);
        assert!(report.contains("Goals and their linked TODOs (2 of 2 goals have support):"));
        assert!(report.contains("goal 2026-07-17-1 [health] run a marathon:"));
        assert!(report.contains("goal 2026-07-17-2 [work] ship the project:"));
        // Appears under both goals, since one todo can support several.
        assert_eq!(
            report
                .matches("todo 2026-07-17-3 [health, work] train and write docs")
                .count(),
            2
        );
    }

    #[test]
    fn align_report_noisy_todo_excluded_from_unmatched_and_listed_with_candidates() {
        let goals = vec![entry(1, &["work"], "goal a"), entry(2, &["work"], "goal b")];
        let todos = vec![entry(3, &["work"], "a noisy todo")];
        let noisy = vec![(todos[0].key, vec![goals[0].key, goals[1].key])];
        let report = align_report(&goals, &todos, &noisy, &[]);
        assert!(report.contains("TODOs with no linked goal (0):"));
        assert!(
            report.contains("TODOs with too many candidate goals, needs manual /todo link (1):")
        );
        assert!(report.contains(
            "2026-07-17-3 [work] a noisy todo (2 candidate goals: 2026-07-17-1, 2026-07-17-2)"
        ));
    }

    #[test]
    fn align_report_both_empty() {
        let report = align_report(&[], &[], &[], &[]);
        assert!(report.contains("Goals with no linked TODO (0):"));
        assert!(report.contains("TODOs with no linked goal (0):"));
        assert!(
            report.contains("TODOs with too many candidate goals, needs manual /todo link (0):")
        );
        assert!(report.contains("Goals and their linked TODOs (0 of 0 goals have support):"));
    }

    #[test]
    fn align_report_untagged_entries_never_match() {
        let goals = vec![entry(1, &[], "goal with no tags")];
        let todos = vec![entry(2, &[], "todo with no tags")];
        let report = align_report(&goals, &todos, &[], &[]);
        assert!(report.contains("Goals with no linked TODO (1):"));
        assert!(report.contains("TODOs with no linked goal (1):"));
    }
}
