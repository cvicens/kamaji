use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;

use crate::error::ChecklistError;

pub mod cache;
mod entry_file;
mod line_format;

pub use line_format::format_list;

/// Generic "checklist entry addressed by date+position, stored as dated
/// markdown" engine shared by `todo.rs` and `goal.rs` -- both are the same
/// data shape (tags/text/open-or-closed, grouped under day-of-month
/// headings) with only the folder name, user-facing noun, closed/reopen
/// subcommand names, closed-state verb, and OKF/title wording differing.
/// `todo.rs`/`goal.rs` are thin wrappers each supplying their own `Config`
/// plus their own public `TodoAction`/`GoalAction` enums.
///
/// Entries carry no stored id: an entry's identity is `EntryKey { date,
/// line }` (see below), derived from *where* it sits in the file, not a
/// separately-tracked counter. That's what makes closing/reopening an
/// entry direct addressing (open one year/month file, jump to one day
/// heading) instead of a scan across every file ever written.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// e.g. "todo" / "goal" -- used in usage text (`/todo add ...`).
    pub command_name: &'static str,
    /// Top-level directory under the notes repo root, e.g. "todo" / "goals".
    pub folder: &'static str,
    /// Lowercase plural noun for list-summary text, e.g. "todos" / "goals".
    pub plural_noun: &'static str,
    /// Subcommand name that closes an entry, e.g. "resolve" / "achieve".
    pub close_subcommand: &'static str,
    /// Subcommand name that reopens a closed entry, e.g. "reopen" for both
    /// domains today -- kept as its own `Config` field (rather than a
    /// hardcoded string) for the same reason `close_subcommand` is one: a
    /// future domain might want different wording.
    pub reopen_subcommand: &'static str,
    /// Past-tense verb rendered in list-reply text for a closed entry, e.g.
    /// "resolved" / "achieved". Chat-reply-only -- neither storage format
    /// records anything beyond a status mark/field when closing.
    pub closed_verb: &'static str,
    /// OKF `type` for one entry file's frontmatter, e.g. "todo" / "goal".
    /// One OKF record per entry now (see `entry_file.rs`) -- a legacy
    /// month-file's own frontmatter (still read for backward compat) predates
    /// this and used the month-file-level "todo-list"/"goal-list" wording,
    /// but new entries are never written into that format anymore.
    pub okf_type: &'static str,
    /// Frontmatter key name for an optional single cross-reference wikilink
    /// to another checklist domain's entry, e.g. `Some("link")` for todo,
    /// `None` for goal. This module never learns *what* the link points to
    /// -- it only knows a domain may or may not carry one named field;
    /// `todo.rs` is what actually validates/points it at a goal.
    pub link_field: Option<&'static str>,
}

/// Status a `list` call filters on. Kept as a distinct type from `Status`
/// below even though the two are shape-identical today -- one is a query
/// parameter, the other a stored fact about an entry, and collapsing them
/// would make a future asymmetry (e.g. a filter option with no matching
/// entry state) awkward to add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Closed,
}

/// An entry's address: the day-section it lives under, plus its 1-based
/// position among checklist lines under that heading. Renders/parses as
/// `YYYY-MM-DD-N` (e.g. `2026-08-03-2`) -- what `/todo list` shows and what
/// `/todo resolve`/`/todo reopen` accept directly, and also exactly what a
/// human hand-adding a line in Obsidian can work out just by counting.
/// `Ord` follows field order (date, then line), which is chronological
/// list order -- the same order `list_entries` sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryKey {
    pub date: NaiveDate,
    pub line: u32,
}

impl fmt::Display for EntryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.date.format("%Y-%m-%d"), self.line)
    }
}

impl FromStr for EntryKey {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (date_part, line_part) = s.rsplit_once('-').ok_or(())?;
        let date = NaiveDate::parse_from_str(date_part, "%Y-%m-%d").map_err(|_| ())?;
        let line: u32 = line_part.parse().map_err(|_| ())?;
        if line == 0 {
            return Err(());
        }
        Ok(EntryKey { date, line })
    }
}

/// Serialized as its `Display` string (`"2026-08-03-2"`), not as a
/// `{date, line}` object -- this is the same shape `FromStr` parses back,
/// and the only shape the web UI (or anything else consuming
/// `/api/todos` JSON) needs to round-trip a key through a form field or a
/// URL path segment.
impl Serialize for EntryKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// What a user typed to identify an entry for `close`/`reopen`: either the
/// full `EntryKey` (`2026-08-03-2`), or a plain number referring to that
/// position in the most recently shown `/todo list`/`/goal list` for the
/// same chat/room (resolved against `cache::ChecklistCache` -- see
/// `resolve_reference` below). Parsing here only validates *shape*; it's
/// pure and sync, same as the rest of `parse_command`, so it stays usable
/// from `kamajid::transport::dispatch_routed_job`'s early usage-error check
/// with no chat/db context needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryReference {
    Key(EntryKey),
    Shorthand(u32),
}

impl FromStr for EntryReference {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(key) = s.parse::<EntryKey>() {
            return Ok(EntryReference::Key(key));
        }
        let n: u32 = s.parse().map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        Ok(EntryReference::Shorthand(n))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub key: EntryKey,
    pub tags: Vec<String>,
    pub text: String,
    pub status: Status,
    /// Cross-reference wikilink targets to another domain's entries (e.g. a
    /// todo's links to the goals it supports) -- a todo can support several
    /// goals, so this is a list, not a single optional value. Populated only
    /// for entries in the new per-entry format whose domain
    /// `Config::link_field` is `Some(..)`. Always empty for a legacy-format
    /// entry or a domain that doesn't use this field at all (goals, today).
    pub links: Vec<String>,
}

/// The parsed, validated shape of a `/<command_name> <...>` command.
pub enum Action {
    Add { tags: Vec<String>, text: String },
    List { filter: StatusFilter },
    Close { reference: EntryReference },
    Reopen { reference: EntryReference },
}

pub(crate) fn usage_text(cfg: &Config) -> String {
    format!(
        "Usage:\n/{name} add <text> #tag1 #tag2 ...\n\
         /{name} list [open|close]\n\
         /{name} {close} <YYYY-MM-DD-N or list number>\n\
         /{name} {reopen} <YYYY-MM-DD-N or list number>",
        name = cfg.command_name,
        close = cfg.close_subcommand,
        reopen = cfg.reopen_subcommand,
    )
}

pub fn parse_command(cfg: &Config, args: &[String]) -> Result<Action, String> {
    let usage = usage_text(cfg);
    let Some((sub, rest)) = args.split_first() else {
        return Err(usage);
    };
    match sub.as_str() {
        "add" => {
            // Tags are *recognized* out of `text` below but deliberately
            // left in place -- `text` is the original input joined
            // verbatim, `#tag` substrings included, so the stored text
            // matches what the user actually typed rather than silently
            // dropping words from their sentence.
            let text = rest
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            if text.trim().is_empty() {
                return Err(usage);
            }
            let tags = crate::tags::extract_tags(&text);
            Ok(Action::Add { tags, text })
        }
        "list" => {
            let filter = match rest.first().map(String::as_str) {
                None | Some("open") => StatusFilter::Open,
                Some("close") => StatusFilter::Closed,
                Some(_) => return Err(usage),
            };
            Ok(Action::List { filter })
        }
        sub if sub == cfg.close_subcommand => {
            let reference = rest
                .first()
                .and_then(|s| s.parse::<EntryReference>().ok())
                .ok_or(usage)?;
            Ok(Action::Close { reference })
        }
        sub if sub == cfg.reopen_subcommand => {
            let reference = rest
                .first()
                .and_then(|s| s.parse::<EntryReference>().ok())
                .ok_or(usage)?;
            Ok(Action::Reopen { reference })
        }
        _ => Err(usage),
    }
}

/// Enumerates `<repo_root>/<cfg.folder>/<year>/*.md` files in chronological
/// order (zero-padded year and month filenames sort lexically =
/// chronologically). Missing folder (first ever `add`) just yields no
/// files. Only `list_entries` needs this full enumeration now -- `add_entry`
/// and `close`/`reopen` address a single file directly via `EntryKey`.
fn checklist_files(cfg: &Config, repo_root: &Path) -> Vec<PathBuf> {
    let dir = repo_root.join(cfg.folder);
    let Ok(year_entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut year_dirs: Vec<PathBuf> = year_entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    year_dirs.sort();

    let mut files = Vec::new();
    for year_dir in year_dirs {
        let Ok(month_entries) = std::fs::read_dir(&year_dir) else {
            continue;
        };
        let mut month_files: Vec<PathBuf> = month_entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect();
        month_files.sort();
        files.extend(month_files);
    }
    files
}

fn month_file_path(cfg: &Config, repo_root: &Path, date: NaiveDate) -> PathBuf {
    repo_root
        .join(cfg.folder)
        .join(date.format("%Y").to_string())
        .join(format!("{}.md", date.format("%m")))
}

fn read_file(path: &Path) -> Result<String, ChecklistError> {
    std::fs::read_to_string(path).map_err(|source| ChecklistError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// Counts already-existing checklist lines under `day`'s `## DD-MM-YYYY`
/// heading in the legacy month-file, if one exists -- `0` if there's no
/// month-file for that year/month, or no section for that specific day yet.
/// Reuses the exact "count `parse_line` matches since the heading" rule the
/// legacy format has always used; factored out here (rather than only living
/// inside `list_entries`) because `next_line_for_day` also needs it.
fn legacy_day_entry_count(
    cfg: &Config,
    repo_root: &Path,
    day: NaiveDate,
) -> Result<u32, ChecklistError> {
    let file_path = month_file_path(cfg, repo_root, day);
    if !file_path.exists() {
        return Ok(0);
    }
    let contents = read_file(&file_path)?;
    let heading = line_format::render_day_heading(day);
    let lines: Vec<&str> = contents.lines().collect();
    let Some(heading_idx) = lines.iter().position(|l| *l == heading) else {
        return Ok(0);
    };
    let mut count = 0u32;
    for line in &lines[heading_idx + 1..] {
        if line.starts_with("## ") {
            break;
        }
        if line_format::parse_line(line).is_some() {
            count += 1;
        }
    }
    Ok(count)
}

/// The highest `line` already used by a new-format entry file for `day`,
/// or `0` if there are none yet. Bounded to listing one year-directory --
/// never a repo-wide scan.
fn new_format_day_entry_max_line(cfg: &Config, repo_root: &Path, day: NaiveDate) -> u32 {
    let year_dir = repo_root
        .join(cfg.folder)
        .join(day.format("%Y").to_string());
    let Ok(entries) = std::fs::read_dir(&year_dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .filter_map(|stem| stem.parse::<EntryKey>().ok())
        .filter(|key| key.date == day)
        .map(|key| key.line)
        .max()
        .unwrap_or(0)
}

/// The next 1-based `line` to use for a new entry on `day`, continuing the
/// count across *both* storage formats. This must look at the legacy
/// month-file too, not just new-format files: if it only counted new-format
/// entries, a day that already has legacy entries could mint a new entry
/// with a colliding `EntryKey` (e.g. both a legacy and a new entry at
/// `2026-08-03-1`), permanently shadowing the legacy one -- `set_status`
/// dispatches to the new-format file first, so a same-keyed new-format file
/// would make the legacy entry unreachable via resolve/reopen forever.
fn next_line_for_day(
    cfg: &Config,
    repo_root: &Path,
    day: NaiveDate,
) -> Result<u32, ChecklistError> {
    let new_max = new_format_day_entry_max_line(cfg, repo_root, day);
    let legacy_count = legacy_day_entry_count(cfg, repo_root, day)?;
    Ok(new_max.max(legacy_count) + 1)
}

/// Writes a brand-new entry to `<repo_root>/<cfg.folder>/<YYYY>/<key>.md`
/// (the new per-entry format -- see `entry_file.rs`), creating the year
/// folder as needed. Returns the new entry's key and the path written,
/// relative to `repo_root`, for use in `git::commit_and_push`. No
/// cross-format global id/counter scan: the key is derived from `when`'s
/// date and `next_line_for_day`, which is bounded to one year-directory
/// listing plus at most one legacy file read.
pub fn add_entry(
    cfg: &Config,
    repo_root: &Path,
    when: DateTime<Utc>,
    tags: &[String],
    text: &str,
) -> Result<(EntryKey, PathBuf), ChecklistError> {
    let day = when.date_naive();

    let year_dir = repo_root
        .join(cfg.folder)
        .join(day.format("%Y").to_string());
    std::fs::create_dir_all(&year_dir).map_err(|source| ChecklistError::CreateDir {
        path: year_dir.clone(),
        source,
    })?;

    let line = next_line_for_day(cfg, repo_root, day)?;
    let key = EntryKey { date: day, line };
    let file_path = entry_file::entry_path(cfg, repo_root, key);
    let contents = entry_file::render(cfg, when, tags, text, Status::Open, &[]);
    std::fs::write(&file_path, contents).map_err(|source| ChecklistError::Write {
        path: file_path.clone(),
        source,
    })?;

    let relative = file_path
        .strip_prefix(repo_root)
        .unwrap_or(&file_path)
        .to_path_buf();
    Ok((key, relative))
}

/// Lists every entry across all checklist files matching `filter`, oldest
/// first -- merging both storage formats: a file whose stem parses as an
/// `EntryKey` is a new-format entry file (`entry_file::parse`); anything
/// else is a legacy month-file, walked with the original day-heading/counter
/// logic. Keys never collide across the two formats by construction
/// (`next_line_for_day` continues the counter across both), so the merge is
/// a plain concatenate + sort.
pub fn list_entries(
    cfg: &Config,
    repo_root: &Path,
    filter: StatusFilter,
) -> Result<Vec<Entry>, ChecklistError> {
    let mut entries = Vec::new();
    for file in checklist_files(cfg, repo_root) {
        let key_from_stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<EntryKey>().ok());

        match key_from_stem {
            Some(key) => {
                let contents = read_file(&file)?;
                let Some(parsed) = entry_file::parse(cfg, &contents) else {
                    continue;
                };
                let matches = match filter {
                    StatusFilter::Open => matches!(parsed.status, Status::Open),
                    StatusFilter::Closed => matches!(parsed.status, Status::Closed),
                };
                if matches {
                    entries.push(Entry {
                        key,
                        tags: parsed.tags,
                        text: parsed.text,
                        status: parsed.status,
                        links: parsed.links,
                    });
                }
            }
            None => {
                let contents = read_file(&file)?;
                let mut current_day: Option<NaiveDate> = None;
                let mut counter = 0u32;
                for line in contents.lines() {
                    if let Some(day) = line_format::parse_day_heading(line) {
                        current_day = Some(day);
                        counter = 0;
                        continue;
                    }
                    let Some(parsed) = line_format::parse_line(line) else {
                        continue;
                    };
                    let Some(day) = current_day else {
                        continue;
                    };
                    counter += 1;
                    let matches = match filter {
                        StatusFilter::Open => matches!(parsed.status, Status::Open),
                        StatusFilter::Closed => matches!(parsed.status, Status::Closed),
                    };
                    if matches {
                        entries.push(Entry {
                            key: EntryKey {
                                date: day,
                                line: counter,
                            },
                            tags: parsed.tags,
                            text: parsed.text,
                            status: parsed.status,
                            links: Vec::new(),
                        });
                    }
                }
            }
        }
    }
    entries.sort_by_key(|e| e.key);
    Ok(entries)
}

/// What `close_entry` found. Kept distinct from a plain `Result<PathBuf,
/// ChecklistError>` so the "already closed" case (a no-op: nothing to
/// write, nothing to commit) is a normal, expected outcome for the caller
/// -- not smuggled in as an `Err` that would tell the worker to report the
/// job as failed and skip straight to a git commit attempt on an unmodified
/// file.
pub enum CloseOutcome {
    Closed(PathBuf),
    AlreadyClosed,
}

/// What `open_entry` found -- mirrors `CloseOutcome` for the reverse
/// direction.
pub enum OpenOutcome {
    Opened(PathBuf),
    AlreadyOpen,
}

/// Flips the entry at `key` to `target` status, given a fully resolved
/// `EntryKey` (an `EntryReference::Shorthand` must already have been
/// resolved to a key by the caller, via `cache::ChecklistCache`). Tries the
/// new-format file first (direct addressing: `key` alone determines the one
/// file to open); falls back to the legacy month-file logic if no
/// new-format file exists at that key, so already-committed entries stay
/// fully operable. Returns `Ok(None)` if the entry was already in `target`
/// status, `Ok(Some(path))` if it changed, `Err(NotFound)` if neither format
/// has an entry at that key.
fn set_status(
    cfg: &Config,
    repo_root: &Path,
    key: EntryKey,
    target: Status,
) -> Result<Option<PathBuf>, ChecklistError> {
    let entry_path = entry_file::entry_path(cfg, repo_root, key);
    if entry_path.exists() {
        return match entry_file::set_status(&entry_path, target)? {
            Some(()) => {
                let relative = entry_path
                    .strip_prefix(repo_root)
                    .unwrap_or(&entry_path)
                    .to_path_buf();
                Ok(Some(relative))
            }
            None => Ok(None),
        };
    }
    set_status_legacy(cfg, repo_root, key, target)
}

/// The original month-file line-toggle logic, kept verbatim as the fallback
/// for entries that predate the per-entry-file format.
fn set_status_legacy(
    cfg: &Config,
    repo_root: &Path,
    key: EntryKey,
    target: Status,
) -> Result<Option<PathBuf>, ChecklistError> {
    let file_path = month_file_path(cfg, repo_root, key.date);
    if !file_path.exists() {
        return Err(ChecklistError::NotFound(key.to_string()));
    }
    let contents = read_file(&file_path)?;
    let mut lines: Vec<String> = contents.lines().map(String::from).collect();

    let heading = line_format::render_day_heading(key.date);
    let Some(heading_idx) = lines.iter().position(|l| l == &heading) else {
        return Err(ChecklistError::NotFound(key.to_string()));
    };

    let mut counter = 0u32;
    for idx in (heading_idx + 1)..lines.len() {
        if lines[idx].starts_with("## ") {
            break;
        }
        let Some(parsed) = line_format::parse_line(&lines[idx]) else {
            continue;
        };
        counter += 1;
        if counter != key.line {
            continue;
        }
        if parsed.status == target {
            return Ok(None);
        }
        lines[idx] = line_format::render_line(target, &parsed.tags, &parsed.text);
        let mut out = lines.join("\n");
        out.push('\n');
        std::fs::write(&file_path, out).map_err(|source| ChecklistError::Write {
            path: file_path.clone(),
            source,
        })?;
        let relative = file_path
            .strip_prefix(repo_root)
            .unwrap_or(&file_path)
            .to_path_buf();
        return Ok(Some(relative));
    }
    Err(ChecklistError::NotFound(key.to_string()))
}

pub fn close_entry(
    cfg: &Config,
    repo_root: &Path,
    key: EntryKey,
) -> Result<CloseOutcome, ChecklistError> {
    match set_status(cfg, repo_root, key, Status::Closed)? {
        Some(path) => Ok(CloseOutcome::Closed(path)),
        None => Ok(CloseOutcome::AlreadyClosed),
    }
}

pub fn open_entry(
    cfg: &Config,
    repo_root: &Path,
    key: EntryKey,
) -> Result<OpenOutcome, ChecklistError> {
    match set_status(cfg, repo_root, key, Status::Open)? {
        Some(path) => Ok(OpenOutcome::Opened(path)),
        None => Ok(OpenOutcome::AlreadyOpen),
    }
}

/// Whether `key` addresses an entry already in the new per-entry format.
/// `false` for a legacy-format entry (even if it exists) or a key with no
/// entry at all -- used by a domain that links *to* this one (today, only
/// `todo.rs` linking to `goal.rs`) to decide whether the target is a valid
/// link destination, since v1 only supports linking to an already-new-format
/// entry.
pub fn entry_exists(cfg: &Config, repo_root: &Path, key: EntryKey) -> bool {
    entry_file::entry_path(cfg, repo_root, key).exists()
}

/// The Obsidian wikilink target string for `key` in this domain (see
/// `entry_file::wikilink_target`) -- what a linking entry's frontmatter
/// should point at.
pub fn entry_wikilink_target(cfg: &Config, key: EntryKey) -> String {
    entry_file::wikilink_target(cfg, key)
}

/// Idempotently adds `target` (a wikilink target string from
/// `entry_wikilink_target`) to the entry at `key`'s link list -- `key` must
/// already be a new-format file (legacy entries can't be linked until
/// migrated). Returns `Ok(None)` if `target` was already linked (no write),
/// `Ok(Some(path))` if it was added, `path` relative to `repo_root` for
/// `git::commit_and_push`.
pub fn add_link(
    cfg: &Config,
    repo_root: &Path,
    key: EntryKey,
    target: &str,
) -> Result<Option<PathBuf>, ChecklistError> {
    let path = entry_file::entry_path(cfg, repo_root, key);
    if !path.exists() {
        return Err(ChecklistError::NotFound(key.to_string()));
    }
    match entry_file::add_link(cfg, &path, target)? {
        Some(()) => Ok(Some(
            path.strip_prefix(repo_root).unwrap_or(&path).to_path_buf(),
        )),
        None => Ok(None),
    }
}

/// Rewrites the tags/text of an already-existing entry at `key`, preserving
/// its status, links, and creation timestamp -- see `entry_file::edit`.
/// Only supported for the new per-entry file format (same boundary
/// `entry_exists` already draws for link targets): a legacy-format entry or
/// a missing key both surface as `ChecklistError::NotFound`, since there's
/// no single file to rewrite for a line inside a shared month-file.
/// Deliberately has no `checklist::Action`/`parse_command` counterpart --
/// unlike `add`/`close`/`reopen`, editing isn't a chat-typable subcommand
/// for any domain today (see TODO.md's "TODO management web UI" note); the
/// only caller is the web API.
pub fn edit_entry(
    cfg: &Config,
    repo_root: &Path,
    key: EntryKey,
    tags: &[String],
    text: &str,
) -> Result<PathBuf, ChecklistError> {
    let path = entry_file::entry_path(cfg, repo_root, key);
    if !path.exists() {
        return Err(ChecklistError::NotFound(format!(
            "{key} (not found, or predates the editable per-entry format)"
        )));
    }
    entry_file::edit(cfg, &path, tags, text)?;
    Ok(path.strip_prefix(repo_root).unwrap_or(&path).to_path_buf())
}

/// Permanently removes the entry file at `key` -- unlike `close_entry`,
/// this doesn't keep the entry around as a closed/resolved record; the file
/// is gone, git history is what's left of it (the caller is expected to
/// commit the removal). A deliberate deviation from the "nothing is ever
/// deleted, git history is the audit trail" precedent `close_entry`/
/// `open_entry` follow -- see TODO.md's "TODO management web UI" note for
/// why this exists anyway (explicit user ask, web API only, never a chat
/// subcommand). Same new-per-entry-format-only boundary as `edit_entry`.
pub fn delete_entry(
    cfg: &Config,
    repo_root: &Path,
    key: EntryKey,
) -> Result<PathBuf, ChecklistError> {
    let path = entry_file::entry_path(cfg, repo_root, key);
    if !path.exists() {
        return Err(ChecklistError::NotFound(format!(
            "{key} (not found, or predates the deletable per-entry format)"
        )));
    }
    std::fs::remove_file(&path).map_err(|source| ChecklistError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path.strip_prefix(repo_root).unwrap_or(&path).to_path_buf())
}

/// Turns whatever the user typed into a concrete `EntryKey`: a `Key` is
/// already one, no io needed; a `Shorthand` needs the most recently shown
/// `list` for `chat`, looked up in `cache`. `Ok(None)` means "nothing to
/// resolve this against" (no recent list, or the number's out of range) --
/// an expected outcome for the caller to turn into a friendly reply, not an
/// error.
pub fn resolve_reference(
    cfg: &Config,
    cache: &cache::ChecklistCache,
    chat: &crate::chat::ChatRef,
    reference: EntryReference,
) -> Result<Option<EntryKey>, ChecklistError> {
    match reference {
        EntryReference::Key(key) => Ok(Some(key)),
        EntryReference::Shorthand(n) => cache.resolve(cfg, chat, n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const TEST_CFG: Config = Config {
        command_name: "thing",
        folder: "thing",
        plural_noun: "things",
        close_subcommand: "finish",
        reopen_subcommand: "reopen",
        closed_verb: "finished",
        okf_type: "thing",
        link_field: Some("link"),
    };

    fn when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 17, 10, 30, 0).unwrap()
    }

    fn key(y: i32, m: u32, d: u32, line: u32) -> EntryKey {
        EntryKey {
            date: NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            line,
        }
    }

    #[test]
    fn entry_key_display_and_parse_round_trip() {
        let k = key(2026, 8, 3, 2);
        assert_eq!(k.to_string(), "2026-08-03-2");
        assert_eq!("2026-08-03-2".parse::<EntryKey>().unwrap(), k);
    }

    #[test]
    fn entry_key_rejects_garbage_and_zero_line() {
        assert!("not-a-key".parse::<EntryKey>().is_err());
        assert!("2026-08-03-0".parse::<EntryKey>().is_err());
    }

    #[test]
    fn entry_reference_parses_key_or_shorthand() {
        assert_eq!(
            "2026-08-03-2".parse::<EntryReference>().unwrap(),
            EntryReference::Key(key(2026, 8, 3, 2))
        );
        assert_eq!(
            "3".parse::<EntryReference>().unwrap(),
            EntryReference::Shorthand(3)
        );
        assert!("0".parse::<EntryReference>().is_err());
        assert!("bogus".parse::<EntryReference>().is_err());
    }

    #[test]
    fn parse_command_requires_a_subcommand() {
        assert!(parse_command(&TEST_CFG, &[]).is_err());
    }

    #[test]
    fn parse_command_rejects_unknown_subcommand() {
        assert!(parse_command(&TEST_CFG, &["bogus".to_string()]).is_err());
    }

    #[test]
    fn parse_command_add_keeps_tags_in_text() {
        let args: Vec<String> = ["add", "finish", "the", "report", "#work", "#urgent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Add { tags, text } => {
                assert_eq!(tags, vec!["work", "urgent"]);
                assert_eq!(text, "finish the report #work #urgent");
            }
            _ => panic!("expected Add"),
        }

        let args: Vec<String> = ["add", "#work", "finish", "the", "#urgent", "report"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Add { tags, text } => {
                assert_eq!(tags, vec!["work", "urgent"]);
                assert_eq!(text, "#work finish the #urgent report");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_add_without_tags() {
        let args: Vec<String> = ["add", "buy", "milk"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Add { tags, text } => {
                assert!(tags.is_empty());
                assert_eq!(text, "buy milk");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_add_treats_bare_number_hash_as_text_not_a_tag() {
        let args: Vec<String> = ["add", "fix", "#123", "today"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Add { tags, text } => {
                assert!(tags.is_empty());
                assert_eq!(text, "fix #123 today");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_add_allows_hyphen_and_underscore_in_tags() {
        let args: Vec<String> = ["add", "#work-item", "#code_review", "ship", "it"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Add { tags, text } => {
                assert_eq!(tags, vec!["work-item", "code_review"]);
                assert_eq!(text, "#work-item #code_review ship it");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_command_add_with_no_text_is_an_error() {
        let args: Vec<String> = ["add"].iter().map(|s| s.to_string()).collect();
        assert!(parse_command(&TEST_CFG, &args).is_err());
    }

    #[test]
    fn parse_command_list_defaults_to_open() {
        let args: Vec<String> = ["list".to_string()].to_vec();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::List { filter } => assert_eq!(filter, StatusFilter::Open),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_command_list_close() {
        let args: Vec<String> = ["list", "close"].iter().map(|s| s.to_string()).collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::List { filter } => assert_eq!(filter, StatusFilter::Closed),
            _ => panic!("expected List"),
        }
    }

    #[test]
    fn parse_command_list_rejects_bad_filter() {
        let args: Vec<String> = ["list", "bogus"].iter().map(|s| s.to_string()).collect();
        assert!(parse_command(&TEST_CFG, &args).is_err());
    }

    #[test]
    fn parse_command_close_accepts_key_or_shorthand() {
        let args: Vec<String> = ["finish", "not-a-number"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_command(&TEST_CFG, &args).is_err());

        let args: Vec<String> = ["finish", "7"].iter().map(|s| s.to_string()).collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Close {
                reference: EntryReference::Shorthand(n),
            } => assert_eq!(n, 7),
            _ => panic!("expected Close{{Shorthand}}"),
        }

        let args: Vec<String> = ["finish", "2026-08-03-2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Close {
                reference: EntryReference::Key(k),
            } => assert_eq!(k, key(2026, 8, 3, 2)),
            _ => panic!("expected Close{{Key}}"),
        }
    }

    #[test]
    fn parse_command_reopen_accepts_key_or_shorthand() {
        let args: Vec<String> = ["reopen", "2026-08-03-1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        match parse_command(&TEST_CFG, &args).unwrap() {
            Action::Reopen {
                reference: EntryReference::Key(k),
            } => assert_eq!(k, key(2026, 8, 3, 1)),
            _ => panic!("expected Reopen{{Key}}"),
        }
    }

    #[test]
    fn add_entry_writes_expected_path_and_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let (k, path) = add_entry(
            &TEST_CFG,
            dir.path(),
            when(),
            &["work".to_string(), "urgent".to_string()],
            "finish the report",
        )
        .unwrap();
        assert_eq!(k, key(2026, 7, 17, 1));
        assert_eq!(path, PathBuf::from("thing/2026/2026-07-17-1.md"));

        let contents = std::fs::read_to_string(dir.path().join(&path)).unwrap();
        assert_eq!(
            contents,
            "---\n\
             type: thing\n\
             title: \"finish the report\"\n\
             description: \"finish the report\"\n\
             tags: [\"work\", \"urgent\"]\n\
             timestamp: 2026-07-17T10:30:00Z\n\
             status: open\n\
             link: []\n\
             ---\n\
             \n\
             finish the report\n"
        );
    }

    #[test]
    fn add_entry_increments_line_within_the_same_day() {
        let dir = tempfile::tempdir().unwrap();
        let (first_key, first_path) =
            add_entry(&TEST_CFG, dir.path(), when(), &[], "first").unwrap();
        let (second_key, second_path) =
            add_entry(&TEST_CFG, dir.path(), when(), &[], "second").unwrap();
        assert_eq!(first_key, key(2026, 7, 17, 1));
        assert_eq!(second_key, key(2026, 7, 17, 2));
        // Each entry gets its own file now, not a second line in one file.
        assert_ne!(first_path, second_path);
        assert!(std::fs::read_to_string(dir.path().join(&first_path))
            .unwrap()
            .contains("first"));
        assert!(std::fs::read_to_string(dir.path().join(&second_path))
            .unwrap()
            .contains("second"));
    }

    #[test]
    fn add_entry_resets_line_counter_for_a_later_day() {
        let dir = tempfile::tempdir().unwrap();
        add_entry(&TEST_CFG, dir.path(), when(), &[], "day one").unwrap();
        let next_day = Utc.with_ymd_and_hms(2026, 7, 18, 9, 0, 0).unwrap();
        let (k, path) = add_entry(&TEST_CFG, dir.path(), next_day, &[], "day two").unwrap();

        // A new day resets the line counter back to 1 -- line numbers are
        // scoped per day, not per month or globally.
        assert_eq!(k, key(2026, 7, 18, 1));
        assert_eq!(path, PathBuf::from("thing/2026/2026-07-18-1.md"));
    }

    #[test]
    fn add_entry_writes_to_the_right_year_dir_across_months() {
        let dir = tempfile::tempdir().unwrap();
        let (first_key, first_path) =
            add_entry(&TEST_CFG, dir.path(), when(), &[], "july entry").unwrap();
        let august = Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap();
        let (second_key, second_path) =
            add_entry(&TEST_CFG, dir.path(), august, &[], "august entry").unwrap();
        assert_eq!(first_key, key(2026, 7, 17, 1));
        assert_eq!(second_key, key(2026, 8, 1, 1));
        assert_eq!(first_path, PathBuf::from("thing/2026/2026-07-17-1.md"));
        assert_eq!(second_path, PathBuf::from("thing/2026/2026-08-01-1.md"));
    }

    #[test]
    fn next_line_for_day_continues_past_existing_legacy_entries() {
        // A day that already has 2 legacy checklist lines must not let a new
        // add_entry mint a colliding key -- the new entry continues from 3,
        // not restart at 1 (see `next_line_for_day`'s doc comment).
        let dir = tempfile::tempdir().unwrap();
        let year_dir = dir.path().join("thing/2026");
        std::fs::create_dir_all(&year_dir).unwrap();
        std::fs::write(
            year_dir.join("07.md"),
            "---\ntype: thing-list\ntitle: \"x\"\ntimestamp: 2026-07-01T00:00:00Z\n---\n\n\
             ## 17-07-2026\n- [ ] [a] legacy one\n- [ ] [b] legacy two\n",
        )
        .unwrap();

        let (new_key, new_path) = add_entry(&TEST_CFG, dir.path(), when(), &[], "new one").unwrap();
        assert_eq!(new_key, key(2026, 7, 17, 3));
        assert_eq!(new_path, PathBuf::from("thing/2026/2026-07-17-3.md"));

        let open = list_entries(&TEST_CFG, dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open.len(), 3);
        assert_eq!(
            open.iter().map(|e| e.key).collect::<Vec<_>>(),
            vec![
                key(2026, 7, 17, 1),
                key(2026, 7, 17, 2),
                key(2026, 7, 17, 3)
            ]
        );
        assert_eq!(open[2].text, "new one");
    }

    #[test]
    fn list_entries_filters_by_status() {
        let dir = tempfile::tempdir().unwrap();
        add_entry(&TEST_CFG, dir.path(), when(), &[], "open one").unwrap();
        let (k, _) = add_entry(&TEST_CFG, dir.path(), when(), &[], "to be closed").unwrap();
        close_entry(&TEST_CFG, dir.path(), k).unwrap();

        let open = list_entries(&TEST_CFG, dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].text, "open one");
        assert_eq!(open[0].key, key(2026, 7, 17, 1));

        let closed = list_entries(&TEST_CFG, dir.path(), StatusFilter::Closed).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].text, "to be closed");
        assert_eq!(closed[0].key, k);
    }

    #[test]
    fn list_entries_empty_when_no_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_entries(&TEST_CFG, dir.path(), StatusFilter::Open)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn close_entry_only_flips_the_status_field_no_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let (k, path) = add_entry(&TEST_CFG, dir.path(), when(), &[], "fix the bug").unwrap();

        match close_entry(&TEST_CFG, dir.path(), k).unwrap() {
            CloseOutcome::Closed(p) => assert_eq!(p, path),
            CloseOutcome::AlreadyClosed => panic!("expected Closed"),
        }

        let contents = std::fs::read_to_string(dir.path().join(&path)).unwrap();
        assert!(contents.contains("status: closed\n"));
        assert!(!contents.contains("status: open\n"));
        assert!(!contents.contains("finished"));
        assert!(!contents.contains("closed_at"));
    }

    #[test]
    fn close_entry_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (k, _) = add_entry(&TEST_CFG, dir.path(), when(), &[], "fix the bug").unwrap();
        close_entry(&TEST_CFG, dir.path(), k).unwrap();

        match close_entry(&TEST_CFG, dir.path(), k).unwrap() {
            CloseOutcome::AlreadyClosed => {}
            CloseOutcome::Closed(_) => panic!("expected AlreadyClosed"),
        }
    }

    #[test]
    fn open_entry_reopens_a_closed_entry_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (k, path) = add_entry(&TEST_CFG, dir.path(), when(), &[], "fix the bug").unwrap();
        close_entry(&TEST_CFG, dir.path(), k).unwrap();

        match open_entry(&TEST_CFG, dir.path(), k).unwrap() {
            OpenOutcome::Opened(p) => assert_eq!(p, path),
            OpenOutcome::AlreadyOpen => panic!("expected Opened"),
        }
        let contents = std::fs::read_to_string(dir.path().join(&path)).unwrap();
        assert!(contents.contains("status: open\n"));

        match open_entry(&TEST_CFG, dir.path(), k).unwrap() {
            OpenOutcome::AlreadyOpen => {}
            OpenOutcome::Opened(_) => panic!("expected AlreadyOpen"),
        }
    }

    #[test]
    fn close_entry_not_found_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        add_entry(&TEST_CFG, dir.path(), when(), &[], "something").unwrap();
        assert!(matches!(
            close_entry(&TEST_CFG, dir.path(), key(2026, 7, 17, 999)),
            Err(ChecklistError::NotFound(_))
        ));
        // A date with no month-file at all is equally not found, and never
        // touches any other file trying to find it.
        assert!(matches!(
            close_entry(&TEST_CFG, dir.path(), key(1999, 1, 1, 1)),
            Err(ChecklistError::NotFound(_))
        ));
    }

    #[test]
    fn close_entry_addresses_the_right_months_file_directly() {
        let dir = tempfile::tempdir().unwrap();
        let (july_key, july_path) =
            add_entry(&TEST_CFG, dir.path(), when(), &[], "july entry").unwrap();
        let august = Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap();
        add_entry(&TEST_CFG, dir.path(), august, &[], "august entry").unwrap();

        match close_entry(&TEST_CFG, dir.path(), july_key).unwrap() {
            CloseOutcome::Closed(p) => assert_eq!(p, july_path),
            CloseOutcome::AlreadyClosed => panic!("expected Closed"),
        }
    }

    #[test]
    fn close_entry_accepts_a_legacy_hash_id_line() {
        // A pre-migration file with an old `#<id>` token should still be
        // addressable and closeable by its derived (date, line) key -- no
        // rewrite of existing files required.
        let dir = tempfile::tempdir().unwrap();
        let year_dir = dir.path().join("thing/2026");
        std::fs::create_dir_all(&year_dir).unwrap();
        std::fs::write(
            year_dir.join("07.md"),
            "---\ntype: thing-list\ntitle: \"x\"\ntimestamp: 2026-07-01T00:00:00Z\n---\n\n## 17-07-2026\n- [ ] #3 [work] legacy item\n",
        )
        .unwrap();

        let open = list_entries(&TEST_CFG, dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].key, key(2026, 7, 17, 1));
        assert_eq!(open[0].text, "legacy item");

        match close_entry(&TEST_CFG, dir.path(), key(2026, 7, 17, 1)).unwrap() {
            CloseOutcome::Closed(_) => {}
            CloseOutcome::AlreadyClosed => panic!("expected Closed"),
        }
        let contents = std::fs::read_to_string(year_dir.join("07.md")).unwrap();
        assert!(contents.contains("- [x] [work] legacy item\n"));
    }

    #[test]
    fn entry_exists_true_for_new_format_false_for_legacy_or_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (new_key, _) = add_entry(&TEST_CFG, dir.path(), when(), &[], "new one").unwrap();
        assert!(entry_exists(&TEST_CFG, dir.path(), new_key));

        let year_dir = dir.path().join("thing/2026");
        std::fs::write(
            year_dir.join("06.md"),
            "---\ntype: thing-list\ntitle: \"x\"\ntimestamp: 2026-06-01T00:00:00Z\n---\n\n## 01-06-2026\n- [ ] [] legacy item\n",
        )
        .unwrap();
        let legacy_key = key(2026, 6, 1, 1);
        assert!(!entry_exists(&TEST_CFG, dir.path(), legacy_key));
        assert!(!entry_exists(&TEST_CFG, dir.path(), key(1999, 1, 1, 1)));
    }

    #[test]
    fn add_link_writes_wikilink_and_is_readable_back() {
        let dir = tempfile::tempdir().unwrap();
        let (todo_key, _) = add_entry(&TEST_CFG, dir.path(), when(), &[], "a todo").unwrap();
        let goal_key = key(2026, 8, 2, 1);
        let target = entry_wikilink_target(&TEST_CFG, goal_key);
        assert_eq!(target, "thing/2026/2026-08-02-1");

        assert!(add_link(&TEST_CFG, dir.path(), todo_key, &target)
            .unwrap()
            .is_some());

        let open = list_entries(&TEST_CFG, dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].links, vec!["thing/2026/2026-08-02-1"]);
    }

    #[test]
    fn add_link_is_idempotent_per_target() {
        let dir = tempfile::tempdir().unwrap();
        let (todo_key, _) = add_entry(&TEST_CFG, dir.path(), when(), &[], "a todo").unwrap();
        let target = entry_wikilink_target(&TEST_CFG, key(2026, 8, 2, 1));

        assert!(add_link(&TEST_CFG, dir.path(), todo_key, &target)
            .unwrap()
            .is_some());
        assert!(add_link(&TEST_CFG, dir.path(), todo_key, &target)
            .unwrap()
            .is_none());

        let open = list_entries(&TEST_CFG, dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(open[0].links.len(), 1);
    }

    #[test]
    fn edit_entry_rewrites_new_format_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (k, path) =
            add_entry(&TEST_CFG, dir.path(), when(), &["a".to_string()], "old").unwrap();

        let edited_path = edit_entry(&TEST_CFG, dir.path(), k, &["b".to_string()], "new").unwrap();
        assert_eq!(edited_path, path);

        let entries = list_entries(&TEST_CFG, dir.path(), StatusFilter::Open).unwrap();
        assert_eq!(entries[0].text, "new");
        assert_eq!(entries[0].tags, vec!["b"]);
    }

    #[test]
    fn edit_entry_missing_key_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            edit_entry(&TEST_CFG, dir.path(), key(2026, 7, 17, 1), &[], "new"),
            Err(ChecklistError::NotFound(_))
        ));
    }

    #[test]
    fn edit_entry_rejects_legacy_format_entry() {
        let dir = tempfile::tempdir().unwrap();
        let year_dir = dir.path().join("thing/2026");
        std::fs::create_dir_all(&year_dir).unwrap();
        std::fs::write(
            year_dir.join("07.md"),
            "---\ntype: thing-list\ntitle: \"x\"\ntimestamp: 2026-07-01T00:00:00Z\n---\n\n## 17-07-2026\n- [ ] [a] legacy item\n",
        )
        .unwrap();

        assert!(matches!(
            edit_entry(&TEST_CFG, dir.path(), key(2026, 7, 17, 1), &[], "new"),
            Err(ChecklistError::NotFound(_))
        ));
    }

    #[test]
    fn delete_entry_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let (k, path) = add_entry(&TEST_CFG, dir.path(), when(), &[], "temporary").unwrap();
        assert!(dir.path().join(&path).exists());

        let deleted_path = delete_entry(&TEST_CFG, dir.path(), k).unwrap();
        assert_eq!(deleted_path, path);
        assert!(!dir.path().join(&path).exists());

        let entries = list_entries(&TEST_CFG, dir.path(), StatusFilter::Open).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn delete_entry_missing_key_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            delete_entry(&TEST_CFG, dir.path(), key(2026, 7, 17, 1)),
            Err(ChecklistError::NotFound(_))
        ));
    }

    #[test]
    fn entry_and_status_serialize_for_the_web_api() {
        let entry = Entry {
            key: key(2026, 8, 3, 2),
            tags: vec!["work".to_string()],
            text: "ship it".to_string(),
            status: Status::Closed,
            links: vec!["goals/2026/2026-08-02-1".to_string()],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"key\":\"2026-08-03-2\""));
        assert!(json.contains("\"status\":\"closed\""));
        assert!(json.contains("\"tags\":[\"work\"]"));
    }

    #[test]
    fn add_link_against_missing_key_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            add_link(
                &TEST_CFG,
                dir.path(),
                key(2026, 7, 17, 1),
                "thing/2026/2026-08-02-1"
            ),
            Err(ChecklistError::NotFound(_))
        ));
    }
}
