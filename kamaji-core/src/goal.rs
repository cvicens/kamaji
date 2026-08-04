use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::chat::ChatRef;
use crate::checklist::{self, cache::ChecklistCache, Config};
use crate::error::ChecklistError;

const CFG: Config = Config {
    command_name: "goal",
    folder: "goals",
    plural_noun: "goals",
    close_subcommand: "achieve",
    reopen_subcommand: "reopen",
    closed_verb: "achieved",
    okf_type: "goal",
    // Goals never link out to another *checklist* entry -- only todos link
    // to goals (one-directional), and Obsidian's own backlink graph is what
    // surfaces the reverse view on a goal's page (see
    // `todo.rs::link_to_goal`). A goal *can* link out to a bitacora fact
    // that demonstrates it, though (see `link_to_fact` below,
    // `/demonstrate`) -- facts aren't `EntryKey`-addressable and can't hold
    // an outbound link themselves, so the link has to live here instead,
    // same one-directional/no-reverse-write shape as todo->goal.
    link_field: Some("demonstrated_by"),
};

pub use checklist::{EntryKey, EntryReference, StatusFilter};
pub type GoalEntry = checklist::Entry;

/// The parsed, validated shape of a `/goal <...>` command -- same tag
/// mechanism and subcommand shape as `/todo` (`TodoAction`), but `achieve`
/// instead of `resolve` since "resolved" reads oddly for a longer-lived
/// goal. `commands::mode` registers `goal` as a single `CommandMode::Queued`
/// command; `parse_command` turns the raw arg list into one of these four
/// concrete actions, called from both `kamajid::transport::dispatch_routed_job`
/// (usage error, skip the queue) and `worker::process_goal_command`.
pub enum GoalAction {
    Add { tags: Vec<String>, text: String },
    List { filter: StatusFilter },
    Achieve { reference: EntryReference },
    Reopen { reference: EntryReference },
}

/// What `achieve_entry` found. Kept distinct from a plain `Result<PathBuf,
/// ChecklistError>` so the "already achieved" case (a no-op: nothing to
/// write, nothing to commit) is a normal, expected outcome for the caller.
pub enum AchieveOutcome {
    Achieved(PathBuf),
    AlreadyAchieved,
}

/// What `reopen_entry` found -- mirrors `AchieveOutcome` for the reverse
/// direction.
pub enum ReopenOutcome {
    Reopened(PathBuf),
    AlreadyOpen,
}

/// What `link_to_fact` found. Kept distinct from a plain `Result<PathBuf,
/// ChecklistError>` so the "already linked" case (a no-op: nothing to
/// write, nothing to commit) is a normal, expected outcome for the caller --
/// same idempotency contract as `todo::LinkOutcome`. Goal's own type, not
/// shared with `todo::LinkOutcome` -- this codebase gives each domain its
/// own small outcome enum rather than a generic cross-domain one.
pub enum LinkOutcome {
    Linked(PathBuf),
    AlreadyLinked,
}

pub fn parse_command(args: &[String]) -> Result<GoalAction, String> {
    match checklist::parse_command(&CFG, args)? {
        checklist::Action::Add { tags, text } => Ok(GoalAction::Add { tags, text }),
        checklist::Action::List { filter } => Ok(GoalAction::List { filter }),
        checklist::Action::Close { reference } => Ok(GoalAction::Achieve { reference }),
        checklist::Action::Reopen { reference } => Ok(GoalAction::Reopen { reference }),
    }
}

/// Writes a `/goal add` entry to `<repo_root>/goals/<YYYY>/<MM>.md`,
/// inserting it under `when`'s `## DD-MM-YYYY` day section (creating the
/// file/year folder/day section as needed). Returns the new entry's key and
/// the path written, relative to `repo_root`, for use in
/// `git::commit_and_push`.
pub fn add_entry(
    repo_root: &Path,
    when: DateTime<Utc>,
    tags: &[String],
    text: &str,
) -> Result<(EntryKey, PathBuf), ChecklistError> {
    checklist::add_entry(&CFG, repo_root, when, tags, text)
}

/// Lists every entry across all `goals/**/*.md` files matching `filter`,
/// oldest first.
pub fn list_entries(
    repo_root: &Path,
    filter: StatusFilter,
) -> Result<Vec<GoalEntry>, ChecklistError> {
    checklist::list_entries(&CFG, repo_root, filter)
}

/// Resolves `reference` into a concrete `EntryKey` -- direct if it's
/// already a key, or looked up against the most recently shown `/goal list`
/// for `chat` if it's a plain shorthand number. `Ok(None)` means there's
/// nothing to resolve it against (no recent list, or out of range).
pub fn resolve_reference(
    cache: &ChecklistCache,
    chat: &ChatRef,
    reference: EntryReference,
) -> Result<Option<EntryKey>, ChecklistError> {
    checklist::resolve_reference(&CFG, cache, chat, reference)
}

/// Remembers the order `entries` were just shown in for `chat`, so a
/// follow-up `/goal achieve|reopen <n>` can refer back to them.
pub fn remember_list(
    cache: &ChecklistCache,
    chat: &ChatRef,
    entries: &[GoalEntry],
) -> Result<(), ChecklistError> {
    let keys: Vec<EntryKey> = entries.iter().map(|e| e.key).collect();
    cache.remember(&CFG, chat, &keys)
}

/// Flips the entry at `key`'s checkbox to closed. Returns
/// `ChecklistError::NotFound` if `key` doesn't address a real entry.
pub fn achieve_entry(repo_root: &Path, key: EntryKey) -> Result<AchieveOutcome, ChecklistError> {
    match checklist::close_entry(&CFG, repo_root, key)? {
        checklist::CloseOutcome::Closed(path) => Ok(AchieveOutcome::Achieved(path)),
        checklist::CloseOutcome::AlreadyClosed => Ok(AchieveOutcome::AlreadyAchieved),
    }
}

/// Flips the entry at `key`'s checkbox back open. Returns
/// `ChecklistError::NotFound` if `key` doesn't address a real entry.
pub fn reopen_entry(repo_root: &Path, key: EntryKey) -> Result<ReopenOutcome, ChecklistError> {
    match checklist::open_entry(&CFG, repo_root, key)? {
        checklist::OpenOutcome::Opened(path) => Ok(ReopenOutcome::Reopened(path)),
        checklist::OpenOutcome::AlreadyOpen => Ok(ReopenOutcome::AlreadyOpen),
    }
}

/// Renders a `/goal list` reply. Public (not just used internally) so
/// `worker::process_goal_command` can call it directly on the result of
/// `list_entries`.
pub fn format_list(entries: &[GoalEntry], filter: StatusFilter) -> String {
    checklist::format_list(&CFG, entries, filter)
}

/// Whether `key` addresses a goal already in the new per-entry format --
/// used by `todo.rs::link_to_goal` to validate a link target before
/// writing it (v1 only supports linking to an already-new-format goal; a
/// goal still in a legacy month-file can't be linked to until migrated).
pub fn entry_exists(repo_root: &Path, key: EntryKey) -> bool {
    checklist::entry_exists(&CFG, repo_root, key)
}

/// The Obsidian wikilink target string for goal `key`, e.g.
/// `goals/2026/2026-08-03-2` -- what a linking todo's frontmatter points at.
pub fn wikilink_target(key: EntryKey) -> String {
    checklist::entry_wikilink_target(&CFG, key)
}

/// Points the goal at `goal_key` at the bitacora fact `fact_target` (a
/// wikilink target from `bitacora::FactRecord::wikilink_target`) via an
/// Obsidian wikilink in the goal's `demonstrated_by` frontmatter --
/// one-directional, nothing is written to the fact file (which couldn't
/// hold a link anyway; see `CFG`'s doc comment). Unlike
/// `todo::link_to_goal`, there's no existence check on `fact_target`: it
/// always comes from a fact `bitacora::list_facts` just read off disk in
/// the same `/demonstrate` run, so it's known-good by construction --
/// `checklist::add_link` itself still returns `NotFound` if `goal_key`
/// doesn't address a real (new-format) goal.
pub fn link_to_fact(
    repo_root: &Path,
    goal_key: EntryKey,
    fact_target: &str,
) -> Result<LinkOutcome, ChecklistError> {
    match checklist::add_link(&CFG, repo_root, goal_key, fact_target)? {
        Some(path) => Ok(LinkOutcome::Linked(path)),
        None => Ok(LinkOutcome::AlreadyLinked),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 17, 10, 30, 0).unwrap()
    }

    #[test]
    fn parse_command_requires_a_subcommand() {
        assert!(parse_command(&[]).is_err());
    }

    #[test]
    fn parse_command_rejects_unknown_subcommand() {
        assert!(parse_command(&["bogus".to_string()]).is_err());
    }

    #[test]
    fn parse_command_add_keeps_tags_in_text() {
        let args: Vec<String> = ["add", "#health", "run", "a", "marathon"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&args).unwrap() {
            GoalAction::Add { tags, text } => {
                assert_eq!(tags, vec!["health"]);
                assert_eq!(text, "#health run a marathon");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_add_with_no_text_is_an_error() {
        let args: Vec<String> = ["add"].iter().map(|s| s.to_string()).collect();
        assert!(parse_command(&args).is_err());
    }

    #[test]
    fn parse_command_add_with_only_a_tag_is_not_an_error() {
        // A lone `#tag` still counts as text now that tags aren't stripped
        // out of the narrative -- `text` ends up being just "#work", which
        // is non-empty.
        let args: Vec<String> = ["add", "#work"].iter().map(|s| s.to_string()).collect();
        match parse_command(&args).unwrap() {
            GoalAction::Add { tags, text } => {
                assert_eq!(tags, vec!["work"]);
                assert_eq!(text, "#work");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_list_defaults_to_open() {
        let args: Vec<String> = ["list".to_string()].to_vec();
        match parse_command(&args).unwrap() {
            GoalAction::List { filter } => assert_eq!(filter, StatusFilter::Open),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_command_list_close() {
        let args: Vec<String> = ["list", "close"].iter().map(|s| s.to_string()).collect();
        match parse_command(&args).unwrap() {
            GoalAction::List { filter } => assert_eq!(filter, StatusFilter::Closed),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_command_achieve_accepts_key_or_shorthand() {
        let args: Vec<String> = ["achieve", "not-a-number"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_command(&args).is_err());

        let args: Vec<String> = ["achieve", "7"].iter().map(|s| s.to_string()).collect();
        match parse_command(&args).unwrap() {
            GoalAction::Achieve {
                reference: EntryReference::Shorthand(n),
            } => assert_eq!(n, 7),
            _ => panic!("expected Achieve{{Shorthand}}"),
        }
    }

    #[test]
    fn add_entry_writes_expected_path_and_line() {
        let dir = tempfile::tempdir().unwrap();
        let (key, path) = add_entry(
            dir.path(),
            when(),
            &["health".to_string()],
            "run a marathon",
        )
        .unwrap();
        assert_eq!(key.to_string(), "2026-07-17-1");
        assert_eq!(path, PathBuf::from("goals/2026/2026-07-17-1.md"));

        let contents = std::fs::read_to_string(dir.path().join(&path)).unwrap();
        assert!(contents.contains("type: goal\n"));
        assert!(contents.contains("title: \"run a marathon\"\n"));
        assert!(contents.contains("status: open\n"));
        // Goals now always carry a `demonstrated_by` link list (even when
        // empty), same as todo's always-there `link: []`.
        assert!(contents.contains("demonstrated_by: []\n"));
        assert!(contents.contains("run a marathon"));
    }

    #[test]
    fn add_entry_resets_line_per_day_across_different_months() {
        let dir = tempfile::tempdir().unwrap();
        let (first_key, first_path) = add_entry(dir.path(), when(), &[], "july goal").unwrap();
        let august = Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap();
        let (second_key, path) = add_entry(dir.path(), august, &[], "august goal").unwrap();
        assert_eq!(first_key.to_string(), "2026-07-17-1");
        assert_eq!(second_key.to_string(), "2026-08-01-1");
        assert_eq!(first_path, PathBuf::from("goals/2026/2026-07-17-1.md"));
        assert_eq!(path, PathBuf::from("goals/2026/2026-08-01-1.md"));
    }

    #[test]
    fn add_list_achieve_reopen_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (key, _) = add_entry(
            dir.path(),
            when(),
            &["health".to_string()],
            "run a marathon",
        )
        .unwrap();

        let open = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].key, key);

        match achieve_entry(dir.path(), key).unwrap() {
            AchieveOutcome::Achieved(_) => {}
            AchieveOutcome::AlreadyAchieved => panic!("expected Achieved"),
        }

        let closed = list_entries(dir.path(), StatusFilter::Closed).unwrap();
        assert_eq!(closed.len(), 1);

        let out = format_list(&closed, StatusFilter::Closed);
        assert!(out.contains("achieved"));

        match reopen_entry(dir.path(), key).unwrap() {
            ReopenOutcome::Reopened(_) => {}
            ReopenOutcome::AlreadyOpen => panic!("expected Reopened"),
        }
        assert_eq!(
            list_entries(dir.path(), StatusFilter::Open).unwrap().len(),
            1
        );
    }

    #[test]
    fn achieve_entry_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (key, _) = add_entry(dir.path(), when(), &[], "run a marathon").unwrap();
        achieve_entry(dir.path(), key).unwrap();

        match achieve_entry(dir.path(), key).unwrap() {
            AchieveOutcome::AlreadyAchieved => {}
            AchieveOutcome::Achieved(_) => panic!("expected AlreadyAchieved"),
        }
    }

    #[test]
    fn achieve_entry_not_found_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        add_entry(dir.path(), when(), &[], "something").unwrap();
        let bogus: EntryKey = "2026-07-17-999".parse().unwrap();
        assert!(matches!(
            achieve_entry(dir.path(), bogus),
            Err(ChecklistError::NotFound(_))
        ));
    }

    #[test]
    fn entry_exists_true_after_add_false_for_bogus_key() {
        let dir = tempfile::tempdir().unwrap();
        let (key, _) = add_entry(dir.path(), when(), &[], "run a marathon").unwrap();
        assert!(entry_exists(dir.path(), key));
        let bogus: EntryKey = "2026-07-17-999".parse().unwrap();
        assert!(!entry_exists(dir.path(), bogus));
    }

    #[test]
    fn wikilink_target_includes_folder_and_year() {
        let key: EntryKey = "2026-08-03-2".parse().unwrap();
        assert_eq!(wikilink_target(key), "goals/2026/2026-08-03-2");
    }

    #[test]
    fn link_to_fact_happy_path_writes_wikilink() {
        let dir = tempfile::tempdir().unwrap();
        let (goal_key, _) = add_entry(dir.path(), when(), &[], "run a marathon").unwrap();
        let target = "bitacora/2026/July/20260714-153045-fixed-prod-outage";

        match link_to_fact(dir.path(), goal_key, target).unwrap() {
            LinkOutcome::Linked(_) => {}
            LinkOutcome::AlreadyLinked => panic!("expected Linked"),
        }

        let goals = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(goals[0].links, vec![target.to_string()]);
    }

    #[test]
    fn link_to_fact_is_idempotent_and_supports_multiple_facts() {
        let dir = tempfile::tempdir().unwrap();
        let (goal_key, _) = add_entry(dir.path(), when(), &[], "run a marathon").unwrap();
        let fact_a = "bitacora/2026/July/fact-a";
        let fact_b = "bitacora/2026/July/fact-b";

        link_to_fact(dir.path(), goal_key, fact_a).unwrap();
        link_to_fact(dir.path(), goal_key, fact_b).unwrap();

        match link_to_fact(dir.path(), goal_key, fact_a).unwrap() {
            LinkOutcome::AlreadyLinked => {}
            LinkOutcome::Linked(_) => panic!("expected AlreadyLinked"),
        }

        let goals = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(goals[0].links, vec![fact_a.to_string(), fact_b.to_string()]);
    }

    #[test]
    fn link_to_fact_missing_goal_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let bogus_goal: EntryKey = "2026-07-17-1".parse().unwrap();
        assert!(matches!(
            link_to_fact(dir.path(), bogus_goal, "bitacora/2026/July/fact-a"),
            Err(ChecklistError::NotFound(_))
        ));
    }
}
