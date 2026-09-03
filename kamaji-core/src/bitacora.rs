use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};

use crate::error::NoteError;
use crate::okf;
use crate::prompt::FactResult;

/// Every path `write_fact` produced, relative to the notes repo root --
/// handed straight to `git::commit_and_push`, which now takes a slice of
/// paths (a fact is 2-3 files, not the single file an ingest note is).
pub struct FactPaths {
    pub note: PathBuf,
    pub orig: PathBuf,
    pub attachment: Option<PathBuf>,
}

impl FactPaths {
    pub fn all(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.note.clone(), self.orig.clone()];
        if let Some(attachment) = &self.attachment {
            paths.push(attachment.clone());
        }
        paths
    }
}

/// Writes a `/fact` bitacora entry under
/// `<repo_root>/bitacora/<YYYY>/<Month>/`: the rendered markdown note (with
/// YAML frontmatter), the raw message text saved verbatim as `.orig` (so
/// nothing Claude summarized away is ever lost), and -- if a document
/// was attached -- its raw bytes under a sanitized,
/// timestamp-prefixed filename in the same folder.
///
/// Layout is year/month (not year/quarter/month): quarter is trivially
/// derivable from month when generating the quarterly report later, so
/// there's no redundant quarter folder to keep in sync.
pub fn write_fact(
    repo_root: &Path,
    when: DateTime<Utc>,
    raw_text: &str,
    result: &FactResult,
    attachment: Option<(&str, &[u8])>,
) -> Result<FactPaths, NoteError> {
    let month_dir = repo_root
        .join("bitacora")
        .join(when.format("%Y").to_string())
        .join(when.format("%B").to_string());
    std::fs::create_dir_all(&month_dir).map_err(|source| NoteError::CreateDir {
        path: month_dir.clone(),
        source,
    })?;

    let stamp = when.format("%Y%m%d-%H%M%S").to_string();
    let base = unique_base(&month_dir, &stamp, &result.slug);

    let note_file = format!("{base}.md");
    let orig_file = format!("{base}.orig");
    // The attachment keeps its original name but lives under a subfolder
    // named after the entry (`<base>/<original-name>`) rather than being
    // renamed with the base as a prefix -- keeps the on-disk name identical
    // to what was sent, while still scoping it unambiguously to this entry.
    let attachment_file = attachment.map(|(name, _)| format!("{base}/{}", sanitize_filename(name)));

    let note_path = month_dir.join(&note_file);
    let contents = render_markdown(
        &format_timestamp(when),
        &result.title,
        &result.summary,
        result.value,
        &result.tags,
        attachment_file.as_deref(),
    );
    std::fs::write(&note_path, contents).map_err(|source| NoteError::Write {
        path: note_path.clone(),
        source,
    })?;

    let orig_path = month_dir.join(&orig_file);
    std::fs::write(&orig_path, raw_text).map_err(|source| NoteError::Write {
        path: orig_path.clone(),
        source,
    })?;

    let attachment_path = if let (Some(file), Some((_, bytes))) = (&attachment_file, attachment) {
        let path = month_dir.join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| NoteError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&path, bytes).map_err(|source| NoteError::Write {
            path: path.clone(),
            source,
        })?;
        Some(path)
    } else {
        None
    };

    let relative = |p: &Path| p.strip_prefix(repo_root).unwrap_or(p).to_path_buf();
    Ok(FactPaths {
        note: relative(&note_path),
        orig: relative(&orig_path),
        attachment: attachment_path.as_deref().map(relative),
    })
}

/// Appends a numeric suffix if `<stamp>-<slug>` already exists in `dir`, so
/// two facts logged in the same second don't clobber each other -- mirrors
/// `notes::unique_path`'s same purpose for ingest notes.
fn unique_base(dir: &Path, stamp: &str, slug: &str) -> String {
    let base = format!("{stamp}-{slug}");
    if !dir.join(format!("{base}.md")).exists() {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{stamp}-{slug}-{n}");
        if !dir.join(format!("{candidate}.md")).exists() {
            return candidate;
        }
        n += 1;
    }
}

/// User-supplied filenames are untrusted input (unlike `slug`, which
/// comes from Claude under an explicit filename-safe instruction). Taking
/// `Path::file_name()` strips any directory components structurally --
/// including `..` and an absolute leading `/` -- so a crafted name like
/// `../../.ssh/authorized_keys` can never escape the bitacora month
/// directory, rather than relying on pattern-matching every way a path
/// separator or `..` could appear. What's left still might start with a dot
/// (hidden file) or, on the empty/`..`-only input, have no file name at
/// all, so those are handled explicitly afterward.
fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("attachment");
    let cleaned = base.trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Renders OKF-conformant frontmatter (see `okf.rs`). The OKF core fields
/// (`type`, `title`, `description`, `tags`, `timestamp`) come first; `value`
/// and `attachment` ride along as producer-defined custom fields, which OKF
/// consumers must preserve. There's no `resource`: a bitacora entry's
/// attachment is a repo-local file, not a canonical external URI.
fn render_markdown(
    timestamp: &str,
    title: &str,
    summary: &str,
    value: i64,
    tags: &[String],
    attachment: Option<&str>,
) -> String {
    let mut fm = String::new();
    fm.push_str("---\n");
    fm.push_str(&format!("type: {}\n", okf::TYPE_FACT));
    fm.push_str(&format!("title: {}\n", okf::yaml_quote(title)));
    fm.push_str(&format!(
        "description: {}\n",
        okf::yaml_quote(&okf::description_from_summary(summary))
    ));
    let tags = tags
        .iter()
        .map(|t| okf::yaml_quote(t))
        .collect::<Vec<_>>()
        .join(", ");
    fm.push_str(&format!("tags: [{tags}]\n"));
    // OKF `timestamp` is ISO 8601; facts carry a full clock time. Passed in
    // already formatted rather than as a `DateTime` so `edit_fact` can carry
    // the original value through verbatim -- a hand-edited frontmatter value
    // that doesn't round-trip through `chrono` still has to survive an edit
    // untouched, since the filename encodes that same instant.
    fm.push_str(&format!("timestamp: {timestamp}\n"));
    // Custom fields, kept alongside the OKF core:
    fm.push_str(&format!("value: {value}\n"));
    if let Some(file) = attachment {
        fm.push_str(&format!("attachment: {}\n", okf::yaml_quote(file)));
    }
    fm.push_str("---\n\n");
    fm.push_str(summary.trim());
    fm.push('\n');
    fm
}

/// The `%Y-%m-%dT%H:%M:%SZ` form OKF `timestamp` uses throughout kamaji.
fn format_timestamp(when: DateTime<Utc>) -> String {
    when.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// A read-back view of one `/fact` entry -- used by `/demonstrate` to
/// tag-match facts against open goals. `bitacora.rs` was write-only before
/// this (nothing ever read a fact back); this is deliberately a thin
/// projection (no body/value/attachment) rather than a full round-trip
/// struct, since matching only needs identity + a short description + tags.
pub struct FactRecord {
    /// Obsidian wikilink target, e.g.
    /// `bitacora/2026/July/20260714-153045-fixed-prod-outage` -- the note's
    /// path relative to the repo root, `.md` stripped.
    pub wikilink_target: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// The same `%B` full-month-name format `write_fact` names folders with --
/// kept as its own helper so `list_facts` stays self-consistent with the
/// write path without hardcoding a month-name table.
fn month_dir_name(year: i32, month: u32) -> Option<String> {
    NaiveDate::from_ymd_opt(year, month, 1).map(|d| d.format("%B").to_string())
}

/// Parses a fact `.md` file's frontmatter, extracting just the fields
/// `/demonstrate` needs. Returns `None` if the file doesn't look like OKF
/// frontmatter -- conservative, mirrors
/// `checklist::entry_file::parse`'s stance of skipping anything it doesn't
/// recognize rather than guessing.
struct ParsedFact {
    title: String,
    description: String,
    tags: Vec<String>,
    /// The remaining fields are only needed by the full read-back
    /// (`read_fact`/`edit_fact`), not by `/demonstrate`'s `FactRecord`
    /// projection -- but they're parsed here regardless so the frontmatter
    /// grammar lives in exactly one function. Each projection takes what it
    /// needs; neither widens the other's public shape.
    summary: String,
    value: i64,
    timestamp: String,
    attachment: Option<String>,
}

fn parse_fact(contents: &str) -> Option<ParsedFact> {
    let rest = contents.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let after_close = &rest[end + "\n---".len()..];
    let body = after_close.strip_prefix('\n').unwrap_or(after_close).trim();

    let mut title = String::new();
    let mut description = String::new();
    let mut tags = Vec::new();
    let mut value = 0i64;
    let mut timestamp = String::new();
    let mut attachment = None;
    for line in frontmatter.lines() {
        if let Some(v) = line.strip_prefix("title: ") {
            title = okf::yaml_unquote(v);
        } else if let Some(v) = line.strip_prefix("description: ") {
            description = okf::yaml_unquote(v);
        } else if let Some(v) = line.strip_prefix("timestamp: ") {
            timestamp = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("value: ") {
            value = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("attachment: ") {
            attachment = Some(okf::yaml_unquote(v.trim()));
        } else if let Some(v) = line
            .strip_prefix("tags: [")
            .and_then(|s| s.strip_suffix(']'))
        {
            tags = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(okf::yaml_unquote)
                .collect();
        }
    }

    Some(ParsedFact {
        title,
        description,
        tags,
        summary: body.to_string(),
        value,
        timestamp,
        attachment,
    })
}

/// Lists every `/fact` entry under `<repo_root>/bitacora/`, in no particular
/// order. `months: None` walks every `<year>/<month-name>` dir present;
/// `Some(pairs)` only reads those specific (year, month) dirs -- missing
/// dirs are silently skipped (same convention as
/// `checklist::checklist_files` for a missing year folder). Each month dir
/// is read non-recursively, filtered to `*.md`, which naturally excludes a
/// fact's companion `.orig` file and its attachment subfolder (no `.md`
/// extension on either).
pub fn list_facts(
    repo_root: &Path,
    months: Option<&[(i32, u32)]>,
) -> Result<Vec<FactRecord>, NoteError> {
    let mut records = Vec::new();
    for file in fact_note_files(repo_root, months) {
        let contents = read_note(&file)?;
        let Some(parsed) = parse_fact(&contents) else {
            continue;
        };
        records.push(FactRecord {
            wikilink_target: wikilink_target_of(repo_root, &file),
            title: parsed.title,
            description: parsed.description,
            tags: parsed.tags,
        });
    }
    Ok(records)
}

/// Every fact `.md` file under `<repo_root>/bitacora/`, month directory by
/// month directory -- the shared walk behind both `list_facts` and
/// `list_fact_details`, which differ only in what they project each file
/// into. Each month dir is read non-recursively, filtered to `*.md`, which
/// naturally excludes a fact's companion `.orig` file and its attachment
/// subfolder (no `.md` extension on either).
fn fact_note_files(repo_root: &Path, months: Option<&[(i32, u32)]>) -> Vec<PathBuf> {
    let bitacora_root = repo_root.join("bitacora");
    let month_dirs: Vec<PathBuf> = match months {
        Some(pairs) => pairs
            .iter()
            .filter_map(|(year, month)| {
                let name = month_dir_name(*year, *month)?;
                Some(bitacora_root.join(year.to_string()).join(name))
            })
            .collect(),
        None => {
            let Ok(year_entries) = std::fs::read_dir(&bitacora_root) else {
                return Vec::new();
            };
            let mut year_dirs: Vec<PathBuf> = year_entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            year_dirs.sort();

            let mut dirs = Vec::new();
            for year_dir in year_dirs {
                let Ok(month_entries) = std::fs::read_dir(&year_dir) else {
                    continue;
                };
                let mut months: Vec<PathBuf> = month_entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect();
                months.sort();
                dirs.extend(months);
            }
            dirs
        }
    };

    let mut files = Vec::new();
    for month_dir in month_dirs {
        let Ok(entries) = std::fs::read_dir(&month_dir) else {
            continue;
        };
        let mut month_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect();
        month_files.sort();
        files.extend(month_files);
    }
    files
}

fn read_note(path: &Path) -> Result<String, NoteError> {
    std::fs::read_to_string(path).map_err(|source| NoteError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// A fact note's identity: its path relative to the repo root with the `.md`
/// stripped, which is exactly what a goal's `demonstrated_by` wikilink points
/// at. The one place that shape is derived, so the read side and the write
/// side can never disagree about what identifies a fact.
fn wikilink_target_of(repo_root: &Path, note_path: &Path) -> String {
    note_path
        .strip_prefix(repo_root)
        .unwrap_or(note_path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

/// The full read-back of one fact -- everything a human might want to
/// correct, plus the derived/structural fields the UI shows but never edits.
/// Deliberately a *second* projection alongside `FactRecord` rather than a
/// widening of it: `/demonstrate` matches on identity + description + tags
/// and would just be carrying a body and an attachment path it never looks
/// at.
///
/// `description` is derived in Rust (`okf::description_from_summary`) and
/// regenerated on every save, so it's read-only here -- an editor that
/// offered it as its own field would immediately drift from the summary it
/// is supposed to preview. `timestamp` is carried as the raw frontmatter
/// string, not a `DateTime`: it's what the web UI dates entries off, and
/// `edit_fact` writes it straight back, so parsing and re-formatting it
/// would only add a way to lose it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FactDetail {
    pub wikilink_target: String,
    pub title: String,
    pub description: String,
    /// The note body -- the editable narrative `description` is derived from.
    pub summary: String,
    pub value: i64,
    pub tags: Vec<String>,
    pub timestamp: String,
    /// `<base>/<original-name>`, relative to the fact's month directory, or
    /// `None` when the fact was logged without a document.
    pub attachment: Option<String>,
    /// Whether the companion `.orig` (the raw message, saved verbatim) is
    /// present. Surfaced so the UI can say the original is preserved; there
    /// is deliberately no API that reads or writes its contents.
    pub has_orig: bool,
}

/// The 12 `%B` month-directory names `write_fact` can produce. Derived from
/// `chrono` rather than typed out, so it can't drift from the write path.
fn month_dir_names() -> Vec<String> {
    (1..=12).filter_map(|m| month_dir_name(2000, m)).collect()
}

/// Resolves an untrusted fact identifier (`FactDetail::wikilink_target`, as
/// it arrives from a web request) to the one `.md` file it may address.
///
/// This is a security boundary, so it validates *structurally* rather than
/// by pattern-matching known-bad input -- the same stance `sanitize_filename`
/// takes for attachment names. A target must be exactly
/// `bitacora/<4-digit year>/<month name>/<stem>`, and the stem must be a
/// single path component that `Path::file_name` returns verbatim, which
/// rules out `..`, `.`, absolute paths and embedded separators without
/// enumerating them. Anything else is `InvalidTarget`; a well-formed target
/// with no file behind it is `NotFound`.
///
/// The returned path always ends in `.md`. That is what makes the `.orig`
/// immutability guarantee structural rather than conventional: no caller can
/// steer a write at a fact's `.orig` (or at its attachment bytes) through
/// this function, because it cannot produce such a path at all.
fn fact_note_path(repo_root: &Path, target: &str) -> Result<PathBuf, NoteError> {
    let invalid = || NoteError::InvalidTarget(target.to_string());
    let segments: Vec<&str> = target.split('/').collect();
    let [root, year, month, stem] = segments[..] else {
        return Err(invalid());
    };
    if root != "bitacora" {
        return Err(invalid());
    }
    if year.len() != 4 || !year.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    if !month_dir_names().iter().any(|m| m == month) {
        return Err(invalid());
    }
    // One real path component, nothing that could climb out of the month
    // directory or start a hidden file.
    if stem.is_empty()
        || stem.starts_with('.')
        || stem.contains('\\')
        || Path::new(stem).file_name().and_then(|f| f.to_str()) != Some(stem)
    {
        return Err(invalid());
    }

    let path = repo_root
        .join(root)
        .join(year)
        .join(month)
        .join(format!("{stem}.md"));
    if !path.is_file() {
        return Err(NoteError::NotFound(target.to_string()));
    }
    Ok(path)
}

fn detail_from(repo_root: &Path, note_path: &Path, parsed: ParsedFact) -> FactDetail {
    FactDetail {
        wikilink_target: wikilink_target_of(repo_root, note_path),
        title: parsed.title,
        description: parsed.description,
        summary: parsed.summary,
        value: parsed.value,
        tags: parsed.tags,
        timestamp: parsed.timestamp,
        attachment: parsed.attachment,
        has_orig: note_path.with_extension("orig").is_file(),
    }
}

/// Reads one fact back in full, addressed by its wikilink target.
pub fn read_fact(repo_root: &Path, target: &str) -> Result<FactDetail, NoteError> {
    let path = fact_note_path(repo_root, target)?;
    let contents = read_note(&path)?;
    let parsed = parse_fact(&contents)
        .ok_or_else(|| NoteError::NotFound(format!("{target} (not a fact note)")))?;
    Ok(detail_from(repo_root, &path, parsed))
}

/// Every fact under `<repo_root>/bitacora/`, read back in full -- what
/// `GET /api/facts` serves. Same walk as `list_facts`, different projection.
pub fn list_fact_details(repo_root: &Path) -> Result<Vec<FactDetail>, NoteError> {
    let mut details = Vec::new();
    for file in fact_note_files(repo_root, None) {
        let contents = read_note(&file)?;
        let Some(parsed) = parse_fact(&contents) else {
            continue;
        };
        details.push(detail_from(repo_root, &file, parsed));
    }
    Ok(details)
}

/// Rewrites a fact's editable fields in place, returning the single path
/// written (relative to `repo_root`) for `git::commit_and_push`.
///
/// Three guarantees, all of them load-bearing:
///
/// * **The `.orig` is never touched.** It exists so nothing the agent
///   summarised away is ever lost (see `write_fact`), which is only worth
///   anything if editing can't quietly rewrite it. Enforced by construction:
///   `fact_note_path` can only ever yield a `.md`, and this is the only
///   write here.
/// * **The file is never renamed.** The filename encodes the entry's
///   timestamp and the agent's slug, and goals point at it by that path via
///   `demonstrated_by` wikilinks -- retitling a fact must not break every
///   inbound link. So `title` changes the frontmatter only.
/// * **`timestamp` and `attachment` survive.** Both are read back from the
///   file being edited and written straight through, not re-derived: the
///   timestamp is the same instant the filename encodes, and the attachment
///   names bytes on disk this function has no business relocating.
///
/// `description` is *not* an input -- it's regenerated from `summary` exactly
/// as `render_markdown` does for a fresh fact, so the two can't drift.
pub fn edit_fact(
    repo_root: &Path,
    target: &str,
    title: &str,
    summary: &str,
    value: i64,
    tags: &[String],
) -> Result<PathBuf, NoteError> {
    let path = fact_note_path(repo_root, target)?;
    let contents = read_note(&path)?;
    let parsed = parse_fact(&contents)
        .ok_or_else(|| NoteError::NotFound(format!("{target} (not a fact note)")))?;

    let rendered = render_markdown(
        &parsed.timestamp,
        title,
        summary,
        value,
        tags,
        parsed.attachment.as_deref(),
    );
    std::fs::write(&path, rendered).map_err(|source| NoteError::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path.strip_prefix(repo_root).unwrap_or(&path).to_path_buf())
}

/// Permanently removes a fact: its `.md`, the companion `.orig`, and the
/// attachment bytes if it has any. Returns every removed path relative to
/// `repo_root` so they land in one `git::commit_and_push` (`git add` stages
/// a deletion for a tracked path just as it stages a modification).
///
/// This deletes the `.orig` too, which is the one place kamaji ever discards
/// a raw message -- deliberately, and only from this explicitly-confirmed
/// web action: keeping an orphaned `.orig` behind would leave a file nothing
/// can ever address again, since a fact's identity *is* its note path.
///
/// Nothing here rewrites goals whose `demonstrated_by` points at the deleted
/// fact -- facts can't be addressed for a partial link update the way a
/// checklist entry can, and Obsidian tolerates a dangling wikilink. The
/// caller (`worker::fact_api_job`) is responsible for counting those goals
/// and saying so in the reply, so the orphaning is reported rather than
/// silent.
pub fn delete_fact(repo_root: &Path, target: &str) -> Result<Vec<PathBuf>, NoteError> {
    let note_path = fact_note_path(repo_root, target)?;
    let contents = read_note(&note_path)?;
    let attachment = parse_fact(&contents).and_then(|parsed| parsed.attachment);
    let month_dir = note_path.parent().map(Path::to_path_buf);

    let mut removed = Vec::new();
    let remove = |path: &Path, removed: &mut Vec<PathBuf>| -> Result<(), NoteError> {
        if !path.exists() {
            return Ok(());
        }
        std::fs::remove_file(path).map_err(|source| NoteError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        removed.push(path.strip_prefix(repo_root).unwrap_or(path).to_path_buf());
        Ok(())
    };

    remove(&note_path, &mut removed)?;
    remove(&note_path.with_extension("orig"), &mut removed)?;

    if let (Some(month_dir), Some(file)) = (month_dir, attachment) {
        // The attachment path in frontmatter is `<base>/<name>`, relative to
        // the month directory -- resolved through the same
        // `Path::file_name`-based structural check `fact_note_path` uses, so
        // a hand-edited `attachment:` value can't point the removal at an
        // arbitrary file.
        if let Some((base, name)) = file.split_once('/') {
            let safe = Path::new(base).file_name().and_then(|f| f.to_str()) == Some(base)
                && Path::new(name).file_name().and_then(|f| f.to_str()) == Some(name);
            if safe {
                let dir = month_dir.join(base);
                remove(&dir.join(name), &mut removed)?;
                // Best-effort: only succeeds once the folder is empty, which
                // is the only case where removing it is right.
                let _ = std::fs::remove_dir(&dir);
            } else {
                tracing::warn!(
                    target_fact = target,
                    attachment = file,
                    "skipped removing a fact attachment with an unexpected path shape"
                );
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> FactResult {
        FactResult {
            title: "Fixed the prod outage".to_string(),
            summary: "Diagnosed and rolled back the bad deploy.".to_string(),
            value: 4,
            tags: vec!["ops".to_string(), "incident".to_string()],
            slug: "fixed-prod-outage".to_string(),
        }
    }

    fn sample_when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 14, 15, 30, 45).unwrap()
    }

    #[test]
    fn writes_fact_under_year_month_folder_with_orig_and_md() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fact(
            dir.path(),
            sample_when(),
            "fixed it, big outage",
            &sample(),
            None,
        )
        .unwrap();

        assert_eq!(
            paths.note,
            PathBuf::from("bitacora/2026/July/20260714-153045-fixed-prod-outage.md")
        );
        assert_eq!(
            paths.orig,
            PathBuf::from("bitacora/2026/July/20260714-153045-fixed-prod-outage.orig")
        );
        assert!(paths.attachment.is_none());

        let note = std::fs::read_to_string(dir.path().join(&paths.note)).unwrap();
        assert!(note.contains("type: fact\n"));
        assert!(note.contains("title: \"Fixed the prod outage\"\n"));
        assert!(note.contains("description: \"Diagnosed and rolled back the bad deploy.\"\n"));
        assert!(note.contains("timestamp: 2026-07-14T15:30:45Z\n"));
        assert!(note.contains("value: 4\n"));
        assert!(note.contains("tags: [\"ops\", \"incident\"]\n"));
        assert!(note.contains("Diagnosed and rolled back"));
        assert!(!note.contains("attachment:"));

        let orig = std::fs::read_to_string(dir.path().join(&paths.orig)).unwrap();
        assert_eq!(orig, "fixed it, big outage");
    }

    #[test]
    fn writes_attachment_bytes_alongside_the_note() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fact(
            dir.path(),
            sample_when(),
            "see report",
            &sample(),
            Some(("report.pdf", b"%PDF-fake-bytes")),
        )
        .unwrap();

        let attachment = paths.attachment.expect("attachment path expected");
        assert_eq!(
            attachment,
            PathBuf::from("bitacora/2026/July/20260714-153045-fixed-prod-outage/report.pdf")
        );
        let bytes = std::fs::read(dir.path().join(&attachment)).unwrap();
        assert_eq!(bytes, b"%PDF-fake-bytes");

        let note = std::fs::read_to_string(dir.path().join(&paths.note)).unwrap();
        assert!(note.contains("attachment: \"20260714-153045-fixed-prod-outage/report.pdf\"\n"));
    }

    #[test]
    fn sanitizes_path_traversal_in_attachment_filename() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fact(
            dir.path(),
            sample_when(),
            "malicious upload",
            &sample(),
            Some(("../../../.ssh/authorized_keys", b"pwned")),
        )
        .unwrap();

        let attachment = paths.attachment.expect("attachment path expected");
        // Must stay inside the bitacora month directory, not escape it.
        assert!(attachment.starts_with("bitacora/2026/July"));
        assert!(!attachment.to_string_lossy().contains(".."));
        assert!(dir.path().join(&attachment).exists());
    }

    #[test]
    fn dedupes_same_second_same_slug_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let first = write_fact(dir.path(), sample_when(), "first", &sample(), None).unwrap();
        let second = write_fact(dir.path(), sample_when(), "second", &sample(), None).unwrap();
        assert_ne!(first.note, second.note);
        assert_eq!(
            second.note,
            PathBuf::from("bitacora/2026/July/20260714-153045-fixed-prod-outage-2.md")
        );
    }

    #[test]
    fn all_lists_note_orig_and_attachment_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fact(
            dir.path(),
            sample_when(),
            "see report",
            &sample(),
            Some(("report.pdf", b"bytes")),
        )
        .unwrap();
        assert_eq!(paths.all().len(), 3);
    }

    #[test]
    fn all_lists_only_note_and_orig_without_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let paths =
            write_fact(dir.path(), sample_when(), "no attachment", &sample(), None).unwrap();
        assert_eq!(paths.all().len(), 2);
    }

    #[test]
    fn list_facts_round_trips_a_written_fact() {
        let dir = tempfile::tempdir().unwrap();
        write_fact(
            dir.path(),
            sample_when(),
            "fixed it, big outage",
            &sample(),
            None,
        )
        .unwrap();

        let facts = list_facts(dir.path(), None).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].wikilink_target,
            "bitacora/2026/July/20260714-153045-fixed-prod-outage"
        );
        assert_eq!(facts[0].title, "Fixed the prod outage");
        assert_eq!(
            facts[0].description,
            "Diagnosed and rolled back the bad deploy."
        );
        assert_eq!(facts[0].tags, vec!["ops", "incident"]);
    }

    #[test]
    fn list_facts_ignores_orig_and_attachment_files() {
        let dir = tempfile::tempdir().unwrap();
        write_fact(
            dir.path(),
            sample_when(),
            "see report",
            &sample(),
            Some(("report.pdf", b"%PDF-fake-bytes")),
        )
        .unwrap();

        let facts = list_facts(dir.path(), None).unwrap();
        assert_eq!(facts.len(), 1);
    }

    #[test]
    fn list_facts_scoped_to_months_excludes_others() {
        let dir = tempfile::tempdir().unwrap();
        write_fact(dir.path(), sample_when(), "july fact", &sample(), None).unwrap();
        let august = Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap();
        write_fact(dir.path(), august, "august fact", &sample(), None).unwrap();

        let july_only = list_facts(dir.path(), Some(&[(2026, 7)])).unwrap();
        assert_eq!(july_only.len(), 1);

        let both = list_facts(dir.path(), Some(&[(2026, 7), (2026, 8)])).unwrap();
        assert_eq!(both.len(), 2);

        let neither = list_facts(dir.path(), Some(&[(2025, 1)])).unwrap();
        assert!(neither.is_empty());
    }

    const TARGET: &str = "bitacora/2026/July/20260714-153045-fixed-prod-outage";

    #[test]
    fn read_fact_round_trips_every_editable_field() {
        let dir = tempfile::tempdir().unwrap();
        write_fact(
            dir.path(),
            sample_when(),
            "fixed it, big outage",
            &sample(),
            Some(("report.pdf", b"%PDF-fake-bytes")),
        )
        .unwrap();

        let fact = read_fact(dir.path(), TARGET).unwrap();
        assert_eq!(fact.wikilink_target, TARGET);
        assert_eq!(fact.title, "Fixed the prod outage");
        assert_eq!(fact.summary, "Diagnosed and rolled back the bad deploy.");
        assert_eq!(
            fact.description,
            "Diagnosed and rolled back the bad deploy."
        );
        assert_eq!(fact.value, 4);
        assert_eq!(fact.tags, vec!["ops", "incident"]);
        assert_eq!(fact.timestamp, "2026-07-14T15:30:45Z");
        assert_eq!(
            fact.attachment.as_deref(),
            Some("20260714-153045-fixed-prod-outage/report.pdf")
        );
        assert!(fact.has_orig);
    }

    #[test]
    fn list_fact_details_reads_every_fact_in_full() {
        let dir = tempfile::tempdir().unwrap();
        write_fact(dir.path(), sample_when(), "july", &sample(), None).unwrap();
        let august = Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap();
        write_fact(dir.path(), august, "august", &sample(), None).unwrap();

        let details = list_fact_details(dir.path()).unwrap();
        assert_eq!(details.len(), 2);
        assert!(details
            .iter()
            .all(|d| !d.summary.is_empty() && d.value == 4));
    }

    #[test]
    fn fact_note_path_rejects_anything_that_is_not_one_bitacora_note() {
        let dir = tempfile::tempdir().unwrap();
        write_fact(dir.path(), sample_when(), "x", &sample(), None).unwrap();

        // A well-formed, existing target resolves.
        assert!(fact_note_path(dir.path(), TARGET).is_ok());

        // Traversal and separator smuggling: structurally impossible, since
        // the stem must be a single component `Path::file_name` returns
        // verbatim.
        for bad in [
            "bitacora/2026/July/../../../.ssh/authorized_keys",
            "bitacora/2026/July/..",
            "bitacora/2026/July/.",
            "bitacora/2026/July/.hidden",
            "bitacora/2026/July/sub/dir",
            "bitacora/2026/July",
            "bitacora/2026/July/a/b/c",
            // Not the bitacora tree, not a 4-digit year, not a month name.
            "notes/2026/July/something",
            "bitacora/26/July/something",
            "bitacora/2026/Jul/something",
            "/etc/passwd",
            "",
        ] {
            assert!(
                matches!(
                    fact_note_path(dir.path(), bad),
                    Err(NoteError::InvalidTarget(_))
                ),
                "expected {bad:?} to be rejected as an invalid target"
            );
        }

        // Well-formed but nothing behind it -- including the companion
        // `.orig`, which can never be addressed as a note.
        for missing in [
            "bitacora/2026/July/20260714-153045-fixed-prod-outage.orig",
            "bitacora/2026/July/nothing-here",
            "bitacora/1999/January/nothing-here",
        ] {
            assert!(
                matches!(
                    fact_note_path(dir.path(), missing),
                    Err(NoteError::NotFound(_))
                ),
                "expected {missing:?} to be not found"
            );
        }
    }

    #[test]
    fn edit_fact_never_touches_the_orig() {
        // The `.orig` is why `/fact` can summarize at all: it's the raw
        // message, kept verbatim. An edit rewrites the note's frontmatter and
        // body; it must leave the original message byte-identical, and must
        // not even report it as a path to commit.
        let dir = tempfile::tempdir().unwrap();
        let raw = "fixed it, big outage -- rolled back deploy 41ab, 20 min downtime";
        let paths = write_fact(dir.path(), sample_when(), raw, &sample(), None).unwrap();
        let orig_before = std::fs::read(dir.path().join(&paths.orig)).unwrap();

        let written = edit_fact(
            dir.path(),
            TARGET,
            "Rolled back the bad deploy",
            "Rolled back deploy 41ab. Downtime was 20 minutes.",
            5,
            &["ops".to_string()],
        )
        .unwrap();

        assert_eq!(written, paths.note);
        assert_ne!(written, paths.orig);
        let orig_after = std::fs::read(dir.path().join(&paths.orig)).unwrap();
        assert_eq!(orig_before, orig_after);
        assert_eq!(String::from_utf8(orig_after).unwrap(), raw);
    }

    #[test]
    fn edit_fact_keeps_the_path_timestamp_and_attachment() {
        // Retitling must not rename the file: the name encodes the entry's
        // timestamp and slug, and goals point at it by that exact path via
        // `demonstrated_by`.
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fact(
            dir.path(),
            sample_when(),
            "see report",
            &sample(),
            Some(("report.pdf", b"%PDF-fake-bytes")),
        )
        .unwrap();

        edit_fact(
            dir.path(),
            TARGET,
            "A completely different title",
            "First sentence. Second sentence.",
            2,
            &["ops".to_string(), "postmortem".to_string()],
        )
        .unwrap();

        let fact = read_fact(dir.path(), TARGET).unwrap();
        assert_eq!(fact.wikilink_target, TARGET);
        assert_eq!(fact.title, "A completely different title");
        assert_eq!(fact.value, 2);
        assert_eq!(fact.tags, vec!["ops", "postmortem"]);
        // Carried straight through from the file being edited, not re-derived.
        assert_eq!(fact.timestamp, "2026-07-14T15:30:45Z");
        assert_eq!(
            fact.attachment.as_deref(),
            Some("20260714-153045-fixed-prod-outage/report.pdf")
        );
        // The attachment bytes are still where the frontmatter says.
        assert!(dir.path().join(paths.attachment.unwrap()).exists());
        // `description` is regenerated from the new summary, never carried
        // over from the old one.
        assert_eq!(fact.description, "First sentence.");
    }

    #[test]
    fn edit_fact_rejects_an_unknown_target() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            edit_fact(dir.path(), TARGET, "t", "s", 3, &[]),
            Err(NoteError::NotFound(_))
        ));
        assert!(matches!(
            edit_fact(dir.path(), "../etc/passwd", "t", "s", 3, &[]),
            Err(NoteError::InvalidTarget(_))
        ));
    }

    #[test]
    fn delete_fact_removes_note_orig_and_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_fact(
            dir.path(),
            sample_when(),
            "see report",
            &sample(),
            Some(("report.pdf", b"%PDF-fake-bytes")),
        )
        .unwrap();
        let attachment = paths.attachment.clone().unwrap();

        let removed = delete_fact(dir.path(), TARGET).unwrap();
        assert_eq!(removed.len(), 3);
        assert!(removed.contains(&paths.note));
        assert!(removed.contains(&paths.orig));
        assert!(removed.contains(&attachment));

        for path in [&paths.note, &paths.orig, &attachment] {
            assert!(!dir.path().join(path).exists(), "{path:?} should be gone");
        }
        // The per-entry attachment folder goes too, once it's empty.
        assert!(!dir
            .path()
            .join("bitacora/2026/July/20260714-153045-fixed-prod-outage")
            .exists());
        assert!(list_fact_details(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn delete_fact_without_an_attachment_removes_two_files() {
        let dir = tempfile::tempdir().unwrap();
        write_fact(dir.path(), sample_when(), "no attachment", &sample(), None).unwrap();
        assert_eq!(delete_fact(dir.path(), TARGET).unwrap().len(), 2);
    }

    #[test]
    fn delete_fact_rejects_an_unknown_target() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            delete_fact(dir.path(), TARGET),
            Err(NoteError::NotFound(_))
        ));
    }

    #[test]
    fn delete_fact_ignores_a_hand_edited_attachment_path_that_escapes() {
        // `attachment:` is frontmatter a human can edit in Obsidian, so the
        // removal path validates it the same structural way
        // `fact_note_path` validates a target -- an escaping value is skipped,
        // never followed.
        let dir = tempfile::tempdir().unwrap();
        write_fact(dir.path(), sample_when(), "x", &sample(), None).unwrap();
        let outside = dir.path().join("bitacora/2026/secrets.txt");
        std::fs::write(&outside, b"do not delete me").unwrap();

        let note = dir
            .path()
            .join("bitacora/2026/July/20260714-153045-fixed-prod-outage.md");
        let contents = std::fs::read_to_string(&note).unwrap();
        let tampered = contents.replace("value: 4\n", "value: 4\nattachment: \"../secrets.txt\"\n");
        std::fs::write(&note, tampered).unwrap();

        let removed = delete_fact(dir.path(), TARGET).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(
            outside.exists(),
            "a path outside the entry must be untouched"
        );
    }

    #[test]
    fn list_facts_empty_when_no_bitacora_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_facts(dir.path(), None).unwrap().is_empty());
        assert!(list_facts(dir.path(), Some(&[(2026, 7)]))
            .unwrap()
            .is_empty());
    }
}
