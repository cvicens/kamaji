use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::chat::ChatRef;
use crate::checklist::{self, cache::ChecklistCache, Config};
use crate::error::ChecklistError;
use crate::goal;

pub const CFG: Config = Config {
    command_name: "todo",
    folder: "todo",
    plural_noun: "todos",
    close_subcommand: "resolve",
    reopen_subcommand: "reopen",
    closed_verb: "resolved",
    okf_type: "todo",
    link_field: Some("link"),
};

pub use checklist::{EntryKey, EntryReference, StatusFilter};
pub type TodoEntry = checklist::Entry;

/// The parsed, validated shape of a `/todo <...>` command. `commands::mode`
/// registers `todo` as a single `CommandMode::Queued` command (its four
/// subcommands don't each get their own top-level command name), so
/// `parse_command` is what turns the raw arg list into one of these four
/// concrete actions -- called from both `kamajid::transport::dispatch_routed_job`
/// (to reply with a usage error and skip the queue entirely, mirroring
/// `/ingest`/`/fact`) and `worker::process_todo_command` (to actually run it).
pub enum TodoAction {
    Add {
        tags: Vec<String>,
        text: String,
    },
    List {
        filter: StatusFilter,
    },
    Resolve {
        reference: EntryReference,
    },
    Reopen {
        reference: EntryReference,
    },
    /// `/todo link <ref> <goal key>` -- points a todo at a goal via an
    /// Obsidian wikilink (see `link_to_goal`). `goal_key` is deliberately a
    /// full `EntryKey`, never `EntryReference`/shorthand: shorthand caches
    /// are per-chat-per-domain, so resolving a bare number against the
    /// *other* domain's cache would be ambiguous.
    Link {
        todo_reference: EntryReference,
        goal_key: EntryKey,
    },
}

/// What `resolve_entry` found. Kept distinct from a plain `Result<PathBuf,
/// ChecklistError>` so the "already resolved" case (a no-op: nothing to
/// write, nothing to commit) is a normal, expected outcome for the caller.
pub enum ResolveOutcome {
    Resolved(PathBuf),
    AlreadyResolved,
}

/// What `link_to_goal` found. Kept distinct from a plain `Result<PathBuf,
/// ChecklistError>` so the "already linked" case (a no-op: nothing to
/// write, nothing to commit) is a normal, expected outcome for the caller --
/// same idempotency contract as `ResolveOutcome`/`ReopenOutcome`.
pub enum LinkOutcome {
    Linked(PathBuf),
    AlreadyLinked,
}

/// What `reopen_entry` found -- mirrors `ResolveOutcome` for the reverse
/// direction.
pub enum ReopenOutcome {
    Reopened(PathBuf),
    AlreadyOpen,
}

/// `"link"` is intercepted here, before delegating to
/// `checklist::parse_command` -- it's not a generic checklist concept (only
/// `/todo` has it, `/goal` doesn't), so `checklist::Action` never needs to
/// know it exists.
fn link_usage() -> String {
    format!(
        "{}\n/todo link <ref> <goal YYYY-MM-DD-N>",
        checklist::usage_text(&CFG)
    )
}

pub fn parse_command(args: &[String]) -> Result<TodoAction, String> {
    if let Some((sub, rest)) = args.split_first() {
        if sub == "link" {
            let [todo_ref, goal_key] = rest else {
                return Err(link_usage());
            };
            let todo_reference = todo_ref.parse().map_err(|_| link_usage())?;
            let goal_key = goal_key.parse().map_err(|_| link_usage())?;
            return Ok(TodoAction::Link {
                todo_reference,
                goal_key,
            });
        }
    }
    match checklist::parse_command(&CFG, args)? {
        checklist::Action::Add { tags, text } => Ok(TodoAction::Add { tags, text }),
        checklist::Action::List { filter } => Ok(TodoAction::List { filter }),
        checklist::Action::Close { reference } => Ok(TodoAction::Resolve { reference }),
        checklist::Action::Reopen { reference } => Ok(TodoAction::Reopen { reference }),
    }
}

/// Writes a `/todo add` entry to `<repo_root>/todo/<YYYY>/<MM>.md`,
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

/// Lists every entry across all `todo/**/*.md` files matching `filter`,
/// oldest first.
pub fn list_entries(
    repo_root: &Path,
    filter: StatusFilter,
) -> Result<Vec<TodoEntry>, ChecklistError> {
    checklist::list_entries(&CFG, repo_root, filter)
}

/// Resolves `reference` into a concrete `EntryKey` -- direct if it's
/// already a key, or looked up against the most recently shown `/todo list`
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
/// follow-up `/todo resolve|reopen <n>` can refer back to them.
pub fn remember_list(
    cache: &ChecklistCache,
    chat: &ChatRef,
    entries: &[TodoEntry],
) -> Result<(), ChecklistError> {
    let keys: Vec<EntryKey> = entries.iter().map(|e| e.key).collect();
    cache.remember(&CFG, chat, &keys)
}

/// Flips the entry at `key`'s checkbox to closed. Returns
/// `ChecklistError::NotFound` if `key` doesn't address a real entry.
pub fn resolve_entry(repo_root: &Path, key: EntryKey) -> Result<ResolveOutcome, ChecklistError> {
    match checklist::close_entry(&CFG, repo_root, key)? {
        checklist::CloseOutcome::Closed(path) => Ok(ResolveOutcome::Resolved(path)),
        checklist::CloseOutcome::AlreadyClosed => Ok(ResolveOutcome::AlreadyResolved),
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

/// Renders a `/todo list` reply. Public (not just used internally) so
/// `worker::process_todo_command` can call it directly on the result of
/// `list_entries`.
pub fn format_list(entries: &[TodoEntry], filter: StatusFilter) -> String {
    checklist::format_list(&CFG, entries, filter)
}

/// Points the todo at `todo_key` at the goal `goal_key` via an Obsidian
/// wikilink in the todo's frontmatter -- one-directional: nothing is written
/// to the goal file, Obsidian's own backlink graph computes the reverse
/// view. Validates the goal exists first (a one-directional dependency,
/// `todo.rs` -> `goal.rs`; `goal.rs` never imports `todo.rs`). Both failure
/// modes (todo not found/still-legacy, goal not found/still-legacy) surface
/// as `ChecklistError::NotFound` with a differentiated embedded string
/// (`"2026-08-03-2"` vs `"goal 2026-08-03-2"`) -- see
/// `worker::process_todo_command`'s `Link` arm for how that string is used
/// in the reply, since it can't reconstruct "Todo {key} not found" the way
/// `Resolve`/`Reopen` do (the `NotFound` here could be about either key).
pub fn link_to_goal(
    repo_root: &Path,
    todo_key: EntryKey,
    goal_key: EntryKey,
) -> Result<LinkOutcome, ChecklistError> {
    if !goal::entry_exists(repo_root, goal_key) {
        return Err(ChecklistError::NotFound(format!("goal {goal_key}")));
    }
    match checklist::add_link(&CFG, repo_root, todo_key, &goal::wikilink_target(goal_key))? {
        Some(path) => Ok(LinkOutcome::Linked(path)),
        None => Ok(LinkOutcome::AlreadyLinked),
    }
}

/// Rewrites `key`'s tags/text in place -- web-API-only, see
/// `checklist::edit_entry`'s doc comment for why this has no
/// `TodoAction`/`parse_command` counterpart.
pub fn edit_entry(
    repo_root: &Path,
    key: EntryKey,
    tags: &[String],
    text: &str,
) -> Result<PathBuf, ChecklistError> {
    checklist::edit_entry(&CFG, repo_root, key, tags, text)
}

/// Permanently removes the entry at `key` -- web-API-only, see
/// `checklist::delete_entry`'s doc comment for why this deviates from the
/// resolve-not-delete precedent the rest of this module follows.
pub fn hard_delete_entry(repo_root: &Path, key: EntryKey) -> Result<PathBuf, ChecklistError> {
    checklist::delete_entry(&CFG, repo_root, key)
}

/// Every todo -- open or closed -- whose `link` list points at
/// `goal_target` (a wikilink target from `goal::wikilink_target`). The
/// inbound half of the one-directional todo->goal link: goals carry no
/// backlinks, so answering "what still references this goal?" means
/// scanning the todo side, which is exactly what `align_job`'s
/// `candidate_goals` does per-todo in the other direction.
///
/// Both status filters, not just open: a resolved todo's link is still a
/// real reference into the goal's file, and deleting the goal out from under
/// it would break it just the same.
pub fn entries_linking_to(
    repo_root: &Path,
    goal_target: &str,
) -> Result<Vec<EntryKey>, ChecklistError> {
    let mut keys = Vec::new();
    for filter in [StatusFilter::Open, StatusFilter::Closed] {
        for entry in checklist::list_entries(&CFG, repo_root, filter)? {
            if entry.links.iter().any(|l| l == goal_target) {
                keys.push(entry.key);
            }
        }
    }
    keys.sort();
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 17, 10, 30, 0).unwrap()
    }

    // The two tests below are the ones TODO.md names explicitly for
    // updating: tags stay in `text` verbatim (recognized, not removed) now
    // that the fix lives in `checklist::parse_command`'s "add" branch. The
    // rest of the generic engine (key derivation, add/list/resolve/reopen
    // round trip, day-section grouping, etc.) is covered in
    // `checklist.rs`'s own test module, and the `#tag` recognizer itself in
    // `tags.rs`'s -- this file only proves the `/todo` wrapper wires up
    // correctly.

    #[test]
    fn parse_command_add_with_tags_at_the_end() {
        let args: Vec<String> = ["add", "finish", "the", "report", "#work", "#urgent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&args).unwrap() {
            TodoAction::Add { tags, text } => {
                assert_eq!(tags, vec!["work", "urgent"]);
                assert_eq!(text, "finish the report #work #urgent");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_add_with_tags_interspersed_in_the_text() {
        let args: Vec<String> = ["add", "#work", "finish", "the", "#urgent", "report"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&args).unwrap() {
            TodoAction::Add { tags, text } => {
                // Tags can appear anywhere -- order they were found in is
                // preserved for tags, and the surrounding text keeps every
                // token exactly as typed, `#tag` substrings included.
                assert_eq!(tags, vec!["work", "urgent"]);
                assert_eq!(text, "#work finish the #urgent report");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_requires_a_subcommand() {
        assert!(parse_command(&[]).is_err());
    }

    #[test]
    fn parse_command_resolve_accepts_a_shorthand_number() {
        let args: Vec<String> = ["resolve", "3"].iter().map(|s| s.to_string()).collect();
        match parse_command(&args).unwrap() {
            TodoAction::Resolve {
                reference: EntryReference::Shorthand(n),
            } => assert_eq!(n, 3),
            _ => panic!("expected Resolve{{Shorthand}}"),
        }
    }

    #[test]
    fn parse_command_reopen_accepts_a_full_key() {
        let args: Vec<String> = ["reopen", "2026-07-17-1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&args).unwrap() {
            TodoAction::Reopen {
                reference: EntryReference::Key(key),
            } => assert_eq!(key.to_string(), "2026-07-17-1"),
            _ => panic!("expected Reopen{{Key}}"),
        }
    }

    #[test]
    fn add_list_resolve_reopen_round_trip_through_the_todo_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let (key, path) = add_entry(
            dir.path(),
            when(),
            &["work".to_string()],
            "finish the report",
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("todo/2026/2026-07-17-1.md"));

        let open = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].key, key);

        match resolve_entry(dir.path(), key).unwrap() {
            ResolveOutcome::Resolved(_) => {}
            ResolveOutcome::AlreadyResolved => panic!("expected Resolved"),
        }

        let closed = list_entries(dir.path(), StatusFilter::Closed).unwrap();
        assert_eq!(closed.len(), 1);

        let out = format_list(&closed, StatusFilter::Closed);
        assert!(out.contains("resolved"));

        match reopen_entry(dir.path(), key).unwrap() {
            ReopenOutcome::Reopened(_) => {}
            ReopenOutcome::AlreadyOpen => panic!("expected Reopened"),
        }
        let open_again = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open_again.len(), 1);
    }

    #[test]
    fn edit_entry_and_hard_delete_entry_wrap_the_checklist_engine() {
        let dir = tempfile::tempdir().unwrap();
        let (key, path) = add_entry(dir.path(), when(), &["a".to_string()], "old text").unwrap();

        let edited_path = edit_entry(dir.path(), key, &["b".to_string()], "new text").unwrap();
        assert_eq!(edited_path, path);
        let entries = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(entries[0].text, "new text");
        assert_eq!(entries[0].tags, vec!["b"]);

        let deleted_path = hard_delete_entry(dir.path(), key).unwrap();
        assert_eq!(deleted_path, path);
        assert!(list_entries(dir.path(), StatusFilter::Open)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn entries_linking_to_finds_open_and_closed_inbound_links() {
        // The check behind "a goal that todos still link to can't be
        // deleted". A *resolved* todo's link is still a real reference into
        // the goal's file, so it has to count -- listing only open todos
        // would let a delete through that breaks it.
        let dir = tempfile::tempdir().unwrap();
        let (goal_key, _) = goal::add_entry(dir.path(), when(), &[], "a goal").unwrap();
        let (other_goal, _) = goal::add_entry(dir.path(), when(), &[], "another goal").unwrap();
        let target = goal::wikilink_target(goal_key);

        let (linked_open, _) = add_entry(dir.path(), when(), &[], "supports it").unwrap();
        let (linked_closed, _) = add_entry(dir.path(), when(), &[], "also supports it").unwrap();
        add_entry(dir.path(), when(), &[], "unrelated").unwrap();
        link_to_goal(dir.path(), linked_open, goal_key).unwrap();
        link_to_goal(dir.path(), linked_closed, goal_key).unwrap();
        resolve_entry(dir.path(), linked_closed).unwrap();

        let linking = entries_linking_to(dir.path(), &target).unwrap();
        assert_eq!(linking, vec![linked_open, linked_closed]);

        // A goal nothing points at has no inbound links at all.
        let other = goal::wikilink_target(other_goal);
        assert!(entries_linking_to(dir.path(), &other).unwrap().is_empty());
    }

    #[test]
    fn resolve_reference_shorthand_uses_the_cached_list() {
        let cache_dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&cache_dir.path().join("test.redb"));
        let cache = ChecklistCache::new(std::sync::Arc::new(db));
        let chat = ChatRef::Telegram { chat_id: 1 };

        let dir = tempfile::tempdir().unwrap();
        let (key, _) = add_entry(dir.path(), when(), &[], "something").unwrap();
        let entries = list_entries(dir.path(), StatusFilter::Open).unwrap();
        remember_list(&cache, &chat, &entries).unwrap();

        let resolved = resolve_reference(&cache, &chat, EntryReference::Shorthand(1)).unwrap();
        assert_eq!(resolved, Some(key));
    }

    #[test]
    fn resolve_reference_key_needs_no_cache() {
        let cache_dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&cache_dir.path().join("test.redb"));
        let cache = ChecklistCache::new(std::sync::Arc::new(db));
        let chat = ChatRef::Telegram { chat_id: 1 };

        let key: EntryKey = "2026-07-17-1".parse().unwrap();
        let resolved = resolve_reference(&cache, &chat, EntryReference::Key(key)).unwrap();
        assert_eq!(resolved, Some(key));
    }

    #[test]
    fn parse_command_link_accepts_key_and_goal_key() {
        let args: Vec<String> = ["link", "2026-07-17-1", "2026-08-02-1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&args).unwrap() {
            TodoAction::Link {
                todo_reference: EntryReference::Key(todo_key),
                goal_key,
            } => {
                assert_eq!(todo_key.to_string(), "2026-07-17-1");
                assert_eq!(goal_key.to_string(), "2026-08-02-1");
            }
            _ => panic!("expected Link"),
        }
    }

    #[test]
    fn parse_command_link_accepts_shorthand_todo_reference() {
        let args: Vec<String> = ["link", "3", "2026-08-02-1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&args).unwrap() {
            TodoAction::Link {
                todo_reference: EntryReference::Shorthand(n),
                ..
            } => assert_eq!(n, 3),
            _ => panic!("expected Link{{Shorthand}}"),
        }
    }

    #[test]
    fn parse_command_link_rejects_malformed_args() {
        assert!(parse_command(&["link".to_string()]).is_err());
        assert!(parse_command(&["link".to_string(), "1".to_string()]).is_err());
        assert!(
            parse_command(&["link".to_string(), "1".to_string(), "not-a-key".to_string()]).is_err()
        );
    }

    #[test]
    fn link_to_goal_happy_path_writes_wikilink() {
        let dir = tempfile::tempdir().unwrap();
        let (todo_key, _) = add_entry(dir.path(), when(), &[], "work toward the goal").unwrap();
        let (goal_key, _) = goal::add_entry(dir.path(), when(), &[], "run a marathon").unwrap();

        match link_to_goal(dir.path(), todo_key, goal_key).unwrap() {
            LinkOutcome::Linked(_) => {}
            LinkOutcome::AlreadyLinked => panic!("expected Linked"),
        }

        let todos = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(todos[0].links, vec![goal::wikilink_target(goal_key)]);
    }

    #[test]
    fn link_to_goal_is_idempotent_and_supports_multiple_goals() {
        let dir = tempfile::tempdir().unwrap();
        let (todo_key, _) = add_entry(dir.path(), when(), &[], "work toward the goals").unwrap();
        let (goal_a, _) = goal::add_entry(dir.path(), when(), &[], "goal a").unwrap();
        let (goal_b, _) = goal::add_entry(dir.path(), when(), &[], "goal b").unwrap();

        link_to_goal(dir.path(), todo_key, goal_a).unwrap();
        link_to_goal(dir.path(), todo_key, goal_b).unwrap();

        match link_to_goal(dir.path(), todo_key, goal_a).unwrap() {
            LinkOutcome::AlreadyLinked => {}
            LinkOutcome::Linked(_) => panic!("expected AlreadyLinked"),
        }

        let todos = list_entries(dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(
            todos[0].links,
            vec![goal::wikilink_target(goal_a), goal::wikilink_target(goal_b)]
        );
    }

    #[test]
    fn link_to_goal_missing_goal_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let (todo_key, _) = add_entry(dir.path(), when(), &[], "a todo").unwrap();
        let bogus_goal: EntryKey = "2026-08-02-1".parse().unwrap();

        assert!(matches!(
            link_to_goal(dir.path(), todo_key, bogus_goal),
            Err(ChecklistError::NotFound(_))
        ));
    }

    #[test]
    fn link_to_goal_missing_todo_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let (goal_key, _) = goal::add_entry(dir.path(), when(), &[], "a goal").unwrap();
        let bogus_todo: EntryKey = "2026-07-17-1".parse().unwrap();

        assert!(matches!(
            link_to_goal(dir.path(), bogus_todo, goal_key),
            Err(ChecklistError::NotFound(_))
        ));
    }
}
