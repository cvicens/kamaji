//! `/demonstrate`: links bitacora facts to matching open goals they
//! demonstrate progress on. Two-stage matching -- Stage 1 (Rust, cheap)
//! narrows candidates by tag overlap per fact, mirroring `align_job`'s
//! per-todo candidate generation with fact/todo roles swapped (a fact is
//! what generates candidate goals here, the same role a todo plays in
//! `/align`); Stage 2 (a Claude call per goal with candidates, on by
//! default via `Config::demonstrate_semantic_match`) judges which of those
//! candidates actually demonstrate the goal, rather than just sharing a
//! tag. A fact whose tag overlap would connect it to more than
//! `Config::demonstrate_noisy_tag_threshold` not-yet-linked open goals is
//! treated as noisy/too-generic and skipped entirely this run, the same
//! rationale as `/align`'s own threshold just applied on the fact side.

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;

use crate::bitacora::{self, FactRecord};
use crate::checklist::{Entry, EntryKey};
use crate::demonstrate::parse_scope;
use crate::git;
use crate::goal;
use crate::prompt::{self, DemonstrateCandidate, TokenUsage};
use crate::state::AppState;

use super::describe_push_outcome;

pub(super) async fn process_demonstrate_command(
    state: &AppState,
    args: &[String],
) -> (String, Option<TokenUsage>, bool, Option<String>, String) {
    let debug_prompt = "/demonstrate".to_string();
    let repo_root = &state.config.notes_repo_path;

    // Belt-and-suspenders: `kamajid::transport::dispatch_routed_job`
    // already pre-checks this the same way it does for `/todo`/`/goal`, so
    // this should be unreachable in practice.
    let scope = match parse_scope(args) {
        Ok(scope) => scope,
        Err(usage) => return (usage.clone(), None, false, Some(usage), debug_prompt),
    };

    let goals = match goal::list_entries(repo_root, goal::StatusFilter::Open) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::error!(%err, "failed to list goals for /demonstrate");
            let msg = format!("Failed to read goals: {err}");
            return (msg.clone(), None, false, Some(msg), debug_prompt);
        }
    };
    let facts = match bitacora::list_facts(repo_root, scope.months(Utc::now()).as_deref()) {
        Ok(records) => records,
        Err(err) => {
            tracing::error!(%err, "failed to list facts for /demonstrate");
            let msg = format!("Failed to read facts: {err}");
            return (msg.clone(), None, false, Some(msg), debug_prompt);
        }
    };

    let threshold = state.config.demonstrate_noisy_tag_threshold;
    let mut noisy: Vec<(String, Vec<EntryKey>)> = Vec::new();
    let mut goal_to_candidates: BTreeMap<EntryKey, Vec<&FactRecord>> = BTreeMap::new();

    for f in &facts {
        let candidates = candidate_goals(f, &goals);
        if candidates.is_empty() {
            continue;
        }
        if candidates.len() as u32 > threshold {
            noisy.push((
                f.wikilink_target.clone(),
                candidates.iter().map(|g| g.key).collect(),
            ));
            continue;
        }
        for g in candidates {
            goal_to_candidates.entry(g.key).or_default().push(f);
        }
    }

    let mut total_tokens: Option<TokenUsage> = None;
    let mut changed_paths = Vec::new();
    let mut newly_linked: Vec<(EntryKey, String)> = Vec::new();

    for (goal_key, candidate_facts) in &goal_to_candidates {
        let Some(g) = goals.iter().find(|g| g.key == *goal_key) else {
            continue;
        };

        let facts_to_link: Vec<&FactRecord> = if state.config.demonstrate_semantic_match {
            let candidates: Vec<DemonstrateCandidate> = candidate_facts
                .iter()
                .map(|f| DemonstrateCandidate {
                    id: f.wikilink_target.clone(),
                    title: f.title.clone(),
                    description: f.description.clone(),
                })
                .collect();
            match prompt::run_demonstrate_prompt(
                state.config.agent_flavor,
                &state.runner,
                &state.config.agent_bin,
                state.config.agent_timeout,
                &g.text,
                &candidates,
            )
            .await
            {
                Ok((result, tokens)) => {
                    total_tokens = merge_tokens(total_tokens, tokens);
                    candidate_facts
                        .iter()
                        .filter(|f| result.demonstrating.contains(&f.wikilink_target))
                        .copied()
                        .collect()
                }
                Err(err) => {
                    // One bad goal's semantic-match call shouldn't sink the
                    // whole run -- log and move on, same log-and-skip
                    // convention `/align` uses for a per-pair link failure.
                    tracing::error!(%err, goal = %goal_key, "demonstrate semantic-match call failed; skipping this goal this run");
                    continue;
                }
            }
        } else {
            candidate_facts.clone()
        };

        for f in facts_to_link {
            match goal::link_to_fact(repo_root, *goal_key, &f.wikilink_target) {
                Ok(goal::LinkOutcome::Linked(path)) => {
                    if !changed_paths.contains(&path) {
                        changed_paths.push(path);
                    }
                    newly_linked.push((*goal_key, f.wikilink_target.clone()));
                }
                // Nothing to write -- another run already got there first.
                Ok(goal::LinkOutcome::AlreadyLinked) => {}
                Err(err) => {
                    tracing::error!(%err, goal = %goal_key, fact = %f.wikilink_target, "failed to auto-link fact to goal");
                }
            }
        }
    }

    if !changed_paths.is_empty() {
        let message = format!(
            "Demonstrate: linked {} fact(s) to matching goal(s)",
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
                tracing::error!(%err, "git commit failed for /demonstrate auto-linking");
                let msg = format!(
                    "Demonstrate linked {} fact(s) on disk, but commit failed: {err}",
                    newly_linked.len()
                );
                return (msg.clone(), total_tokens, false, Some(msg), debug_prompt);
            }
        };
        // Re-list so the report reflects the links just written.
        let goals = match goal::list_entries(repo_root, goal::StatusFilter::Open) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::error!(%err, "failed to re-list goals after /demonstrate linking");
                goals
            }
        };
        let report = demonstrate_report(&goals, &facts, &noisy, &newly_linked);
        let reply = describe_push_outcome(report, push_outcome);
        return (reply, total_tokens, true, None, debug_prompt);
    }

    let report = demonstrate_report(&goals, &facts, &noisy, &newly_linked);
    (report, total_tokens, true, None, debug_prompt)
}

/// Open goals sharing at least one tag with `fact`, excluding goals `fact`
/// is already linked to -- the candidate set a `/demonstrate` run would
/// consider linking `fact` to, before the noisy-tag threshold is applied.
fn candidate_goals<'a>(fact: &FactRecord, goals: &'a [Entry]) -> Vec<&'a Entry> {
    goals
        .iter()
        .filter(|g| g.tags.iter().any(|tag| fact.tags.contains(tag)))
        .filter(|g| !g.links.contains(&fact.wikilink_target))
        .collect()
}

/// Sums token usage across the several Claude calls a `/demonstrate` run
/// can make (one per goal with candidates), unlike `/align`/`/todo`/`/goal`
/// which never invoke Claude at all and `/ingest`/`/fact` which only ever
/// make one call.
fn merge_tokens(acc: Option<TokenUsage>, new: Option<TokenUsage>) -> Option<TokenUsage> {
    match (acc, new) {
        (None, t) => t,
        (t, None) => t,
        (Some(a), Some(b)) => Some(TokenUsage {
            input: a.input + b.input,
            output: a.output + b.output,
            cache_creation: a.cache_creation + b.cache_creation,
            cache_read: a.cache_read + b.cache_read,
        }),
    }
}

/// Builds the `/demonstrate` report body from already-loaded open goals
/// (reflecting any links just written this run) and in-scope facts, plus
/// the noisy-candidate set and the (goal, fact) pairs newly linked this
/// run. Pure and filesystem-free, mirrors `align_job::align_report`'s
/// four-section shape.
fn demonstrate_report(
    goals: &[Entry],
    facts: &[FactRecord],
    noisy: &[(String, Vec<EntryKey>)],
    newly_linked: &[(EntryKey, String)],
) -> String {
    let noisy_targets: BTreeSet<&str> = noisy.iter().map(|(target, _)| target.as_str()).collect();
    let unmatched_facts: Vec<&FactRecord> = facts
        .iter()
        .filter(|f| {
            !noisy_targets.contains(f.wikilink_target.as_str())
                && !goals.iter().any(|g| g.links.contains(&f.wikilink_target))
        })
        .collect();

    let mut goal_to_facts: BTreeMap<EntryKey, Vec<&FactRecord>> = BTreeMap::new();
    for g in goals {
        for target in &g.links {
            if let Some(f) = facts.iter().find(|f| &f.wikilink_target == target) {
                goal_to_facts.entry(g.key).or_default().push(f);
            }
        }
    }
    let unmatched_goals: Vec<&Entry> = goals
        .iter()
        .filter(|g| !goal_to_facts.contains_key(&g.key))
        .collect();

    let mut lines = vec!["Goal/fact demonstration:".to_string(), String::new()];

    lines.push(format!(
        "Goals with no demonstrating facts ({}):",
        unmatched_goals.len()
    ));
    if unmatched_goals.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        lines.extend(
            unmatched_goals
                .iter()
                .map(|g| format!("  {}", format_goal(g))),
        );
    }
    lines.push(String::new());

    lines.push(format!(
        "Facts with no matching goal ({}):",
        unmatched_facts.len()
    ));
    if unmatched_facts.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        lines.extend(
            unmatched_facts
                .iter()
                .map(|f| format!("  {}", format_fact(f))),
        );
    }
    lines.push(String::new());

    lines.push(format!(
        "Facts with too many candidate goals, needs manual attention ({}):",
        noisy.len()
    ));
    if noisy.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (target, candidate_keys) in noisy {
            let Some(f) = facts.iter().find(|f| &f.wikilink_target == target) else {
                continue;
            };
            let candidates_display = candidate_keys
                .iter()
                .map(EntryKey::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "  {} ({} candidate goals: {})",
                format_fact(f),
                candidate_keys.len(),
                candidates_display
            ));
        }
    }
    lines.push(String::new());

    lines.push(format!(
        "Goals and their demonstrating facts ({} of {} goals have support):",
        goal_to_facts.len(),
        goals.len()
    ));
    if goal_to_facts.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (goal_key, linked) in &goal_to_facts {
            let Some(g) = goals.iter().find(|g| g.key == *goal_key) else {
                continue;
            };
            lines.push(format!("  goal {}:", format_goal(g)));
            for f in linked {
                let is_new = newly_linked
                    .iter()
                    .any(|(gk, target)| gk == goal_key && target == &f.wikilink_target);
                let suffix = if is_new { " (new)" } else { "" };
                lines.push(format!("    fact {}{suffix}", format_fact(f)));
            }
        }
    }

    lines.join("\n")
}

fn format_goal(entry: &Entry) -> String {
    format!("{} [{}] {}", entry.key, entry.tags.join(", "), entry.text)
}

fn format_fact(fact: &FactRecord) -> String {
    format!("[{}] {}", fact.tags.join(", "), fact.title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checklist::Status;

    fn goal(line: u32, tags: &[&str], text: &str) -> Entry {
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

    fn linked_goal(line: u32, tags: &[&str], text: &str, fact_targets: &[&str]) -> Entry {
        Entry {
            links: fact_targets.iter().map(|s| s.to_string()).collect(),
            ..goal(line, tags, text)
        }
    }

    fn fact(target: &str, tags: &[&str], title: &str) -> FactRecord {
        FactRecord {
            wikilink_target: target.to_string(),
            title: title.to_string(),
            description: title.to_string(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn candidate_goals_matches_on_shared_tag_excludes_already_linked() {
        let goals = [
            goal(1, &["health"], "run a marathon"),
            goal(2, &["work"], "ship the project"),
        ];
        let already_linked_target = "bitacora/2026/July/fact-a";
        let linked = linked_goal(1, &["health"], "run a marathon", &[already_linked_target]);
        let goals_with_link = vec![linked, goals[1].clone()];
        let f = fact(already_linked_target, &["health", "work"], "trained");
        let candidates = candidate_goals(&f, &goals_with_link);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].key, goals[1].key);
    }

    #[test]
    fn candidate_goals_empty_when_no_tag_overlap() {
        let goals = vec![goal(1, &["health"], "run a marathon")];
        let f = fact("bitacora/2026/July/fact-a", &["work"], "shipped it");
        assert!(candidate_goals(&f, &goals).is_empty());
    }

    #[test]
    fn demonstrate_report_no_overlap_lands_both_sides_unmatched() {
        let goals = vec![goal(1, &["health"], "run a marathon")];
        let facts = vec![fact("bitacora/2026/July/fact-a", &["work"], "shipped it")];
        let report = demonstrate_report(&goals, &facts, &[], &[]);
        assert!(report.contains("Goals with no demonstrating facts (1):"));
        assert!(report.contains("2026-07-17-1 [health] run a marathon"));
        assert!(report.contains("Facts with no matching goal (1):"));
        assert!(report.contains("[work] shipped it"));
        assert!(report.contains("Goals and their demonstrating facts (0 of 1 goals have support):"));
    }

    #[test]
    fn demonstrate_report_linked_fact_grouped_under_its_goal_and_marked_new() {
        let target = "bitacora/2026/July/fact-a";
        let goals = vec![linked_goal(1, &["work"], "ship the project", &[target])];
        let facts = vec![fact(target, &["work"], "wrote the doc")];
        let report = demonstrate_report(&goals, &facts, &[], &[(goals[0].key, target.to_string())]);
        assert!(report.contains("Goals with no demonstrating facts (0):"));
        assert!(report.contains("Facts with no matching goal (0):"));
        assert!(report.contains("Goals and their demonstrating facts (1 of 1 goals have support):"));
        assert!(report.contains("goal 2026-07-17-1 [work] ship the project:"));
        assert!(report.contains("fact [work] wrote the doc (new)"));
    }

    #[test]
    fn demonstrate_report_previously_linked_fact_has_no_new_marker() {
        let target = "bitacora/2026/July/fact-a";
        let goals = vec![linked_goal(1, &["work"], "ship the project", &[target])];
        let facts = vec![fact(target, &["work"], "wrote the doc")];
        let report = demonstrate_report(&goals, &facts, &[], &[]);
        assert!(report.contains("fact [work] wrote the doc"));
        assert!(!report.contains("(new)"));
    }

    #[test]
    fn demonstrate_report_one_fact_supports_two_goals() {
        let target = "bitacora/2026/July/fact-a";
        let goals = vec![
            linked_goal(1, &["health"], "run a marathon", &[target]),
            linked_goal(2, &["work"], "ship the project", &[target]),
        ];
        let facts = vec![fact(target, &["health", "work"], "trained and wrote docs")];
        let report = demonstrate_report(&goals, &facts, &[], &[]);
        assert!(report.contains("Goals and their demonstrating facts (2 of 2 goals have support):"));
        assert!(report.contains("goal 2026-07-17-1 [health] run a marathon:"));
        assert!(report.contains("goal 2026-07-17-2 [work] ship the project:"));
        assert_eq!(
            report
                .matches("fact [health, work] trained and wrote docs")
                .count(),
            2
        );
    }

    #[test]
    fn demonstrate_report_noisy_fact_excluded_from_unmatched_and_listed_with_candidates() {
        let goals = vec![goal(1, &["work"], "goal a"), goal(2, &["work"], "goal b")];
        let facts = vec![fact("bitacora/2026/July/fact-a", &["work"], "a noisy fact")];
        let noisy = vec![(
            facts[0].wikilink_target.clone(),
            vec![goals[0].key, goals[1].key],
        )];
        let report = demonstrate_report(&goals, &facts, &noisy, &[]);
        assert!(report.contains("Facts with no matching goal (0):"));
        assert!(report.contains("Facts with too many candidate goals, needs manual attention (1):"));
        assert!(
            report.contains("[work] a noisy fact (2 candidate goals: 2026-07-17-1, 2026-07-17-2)")
        );
    }

    #[test]
    fn demonstrate_report_both_empty() {
        let report = demonstrate_report(&[], &[], &[], &[]);
        assert!(report.contains("Goals with no demonstrating facts (0):"));
        assert!(report.contains("Facts with no matching goal (0):"));
        assert!(report.contains("Facts with too many candidate goals, needs manual attention (0):"));
        assert!(report.contains("Goals and their demonstrating facts (0 of 0 goals have support):"));
    }

    #[test]
    fn demonstrate_report_untagged_entries_never_match() {
        let goals = vec![goal(1, &[], "goal with no tags")];
        let facts = vec![fact("bitacora/2026/July/fact-a", &[], "fact with no tags")];
        let report = demonstrate_report(&goals, &facts, &[], &[]);
        assert!(report.contains("Goals with no demonstrating facts (1):"));
        assert!(report.contains("Facts with no matching goal (1):"));
    }

    #[test]
    fn merge_tokens_sums_across_calls() {
        let a = TokenUsage {
            input: 10,
            output: 5,
            cache_creation: 1,
            cache_read: 2,
        };
        let b = TokenUsage {
            input: 20,
            output: 15,
            cache_creation: 3,
            cache_read: 4,
        };
        let merged = merge_tokens(Some(a), Some(b)).unwrap();
        assert_eq!(merged.input, 30);
        assert_eq!(merged.output, 20);
        assert_eq!(merged.cache_creation, 4);
        assert_eq!(merged.cache_read, 6);
    }

    #[test]
    fn merge_tokens_none_plus_none_is_none() {
        assert!(merge_tokens(None, None).is_none());
    }

    #[test]
    fn merge_tokens_one_side_none_keeps_the_other() {
        let a = TokenUsage {
            input: 10,
            output: 5,
            cache_creation: 0,
            cache_read: 0,
        };
        assert_eq!(merge_tokens(Some(a.clone()), None).unwrap().input, 10);
        assert_eq!(merge_tokens(None, Some(a)).unwrap().input, 10);
    }
}
