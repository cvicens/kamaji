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
    let contents = render_markdown(when, result, attachment_file.as_deref());
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
fn render_markdown(when: DateTime<Utc>, result: &FactResult, attachment: Option<&str>) -> String {
    let mut fm = String::new();
    fm.push_str("---\n");
    fm.push_str(&format!("type: {}\n", okf::TYPE_FACT));
    fm.push_str(&format!("title: {}\n", okf::yaml_quote(&result.title)));
    fm.push_str(&format!(
        "description: {}\n",
        okf::yaml_quote(&okf::description_from_summary(&result.summary))
    ));
    let tags = result
        .tags
        .iter()
        .map(|t| okf::yaml_quote(t))
        .collect::<Vec<_>>()
        .join(", ");
    fm.push_str(&format!("tags: [{tags}]\n"));
    // OKF `timestamp` is ISO 8601; facts carry a full clock time.
    fm.push_str(&format!(
        "timestamp: {}\n",
        when.format("%Y-%m-%dT%H:%M:%SZ")
    ));
    // Custom fields, kept alongside the OKF core:
    fm.push_str(&format!("value: {}\n", result.value));
    if let Some(file) = attachment {
        fm.push_str(&format!("attachment: {}\n", okf::yaml_quote(file)));
    }
    fm.push_str("---\n\n");
    fm.push_str(result.summary.trim());
    fm.push('\n');
    fm
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
}

fn parse_fact(contents: &str) -> Option<ParsedFact> {
    let rest = contents.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];

    let mut title = String::new();
    let mut description = String::new();
    let mut tags = Vec::new();
    for line in frontmatter.lines() {
        if let Some(value) = line.strip_prefix("title: ") {
            title = okf::yaml_unquote(value);
        } else if let Some(value) = line.strip_prefix("description: ") {
            description = okf::yaml_unquote(value);
        } else if let Some(value) = line
            .strip_prefix("tags: [")
            .and_then(|s| s.strip_suffix(']'))
        {
            tags = value
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
                return Ok(Vec::new());
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

    let mut records = Vec::new();
    for month_dir in month_dirs {
        let Ok(entries) = std::fs::read_dir(&month_dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort();

        for file in files {
            let contents = std::fs::read_to_string(&file).map_err(|source| NoteError::Read {
                path: file.clone(),
                source,
            })?;
            let Some(parsed) = parse_fact(&contents) else {
                continue;
            };
            let relative = file.strip_prefix(repo_root).unwrap_or(&file);
            let wikilink_target = relative
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            records.push(FactRecord {
                wikilink_target,
                title: parsed.title,
                description: parsed.description,
                tags: parsed.tags,
            });
        }
    }
    Ok(records)
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

    #[test]
    fn list_facts_empty_when_no_bitacora_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_facts(dir.path(), None).unwrap().is_empty());
        assert!(list_facts(dir.path(), Some(&[(2026, 7)]))
            .unwrap()
            .is_empty());
    }
}
