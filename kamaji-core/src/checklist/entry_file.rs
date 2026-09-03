//! Per-entry file format: one markdown file per checklist entry, OKF
//! frontmatter + plain-text body -- introduced alongside the legacy
//! month-aggregate format `line_format.rs` still reads/writes. New entries
//! always land here (see `mod.rs::add_entry`); `line_format.rs`'s format is
//! kept only as a fallback for already-committed entries. See `mod.rs` for
//! how the two formats are dispatched between.

use std::path::{Path, PathBuf};

use chrono::DateTime;
use chrono::Utc;

use crate::error::ChecklistError;
use crate::okf;

use super::{Config, EntryKey, Status};

/// `<repo_root>/<cfg.folder>/<YYYY>/<key>.md` -- distinguishable from a
/// legacy month file (`<MM>.md`) purely by filename shape, since `EntryKey`'s
/// `FromStr` requires a `%Y-%m-%d` prefix a bare `08` never satisfies.
pub(super) fn entry_path(cfg: &Config, repo_root: &Path, key: EntryKey) -> PathBuf {
    repo_root
        .join(cfg.folder)
        .join(key.date.format("%Y").to_string())
        .join(format!("{key}.md"))
}

/// The Obsidian wikilink target for `key` in this domain, e.g.
/// `goals/2026/2026-08-03-2`. Includes `folder`/`year`, not just the bare
/// key, because `EntryKey` is only unique *within* one domain -- a todo and
/// a goal can independently produce the same `{date, line}`, so a bare
/// `[[2026-08-03-2]]` wikilink would be ambiguous (or resolve to the wrong
/// file) once both domains use per-entry files.
pub(super) fn wikilink_target(cfg: &Config, key: EntryKey) -> String {
    format!("{}/{}/{key}", cfg.folder, key.date.format("%Y"))
}

pub(super) struct ParsedEntry {
    pub tags: Vec<String>,
    pub text: String,
    pub status: Status,
    pub links: Vec<String>,
    /// The entry's original `timestamp:` frontmatter value -- only needed so
    /// `edit` can preserve creation time across a content rewrite; `None` if
    /// the line is missing or unparsable (never true for a file this module
    /// wrote itself, but a hand-edited file shouldn't panic over it).
    pub timestamp: Option<DateTime<Utc>>,
}

/// Renders a `link: [...]` frontmatter line's value from wikilink targets,
/// matching the same flow-list style `tags` already uses.
fn render_links_value(links: &[String]) -> String {
    links
        .iter()
        .map(|target| okf::yaml_quote(&format!("[[{target}]]")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders a brand-new entry file from scratch: OKF frontmatter (core fields
/// plus `status` and, for a domain with `cfg.link_field` set, a `links`
/// list -- always present for such a domain, even when empty, mirroring how
/// `tags: []` is always rendered rather than omitted) followed by a blank
/// line and the plain entry text as the body. There's no Claude call in this
/// path (unlike notes/facts), so `title` is the entry text verbatim and
/// `description` is derived from it the same way
/// (`okf::description_from_summary`) -- both are write-only OKF-conformance
/// fields; kamaji itself never reads them back, only the body and the other
/// frontmatter fields below.
pub(super) fn render(
    cfg: &Config,
    when: DateTime<Utc>,
    tags: &[String],
    text: &str,
    status: Status,
    links: &[String],
) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("type: {}\n", cfg.okf_type));
    out.push_str(&format!("title: {}\n", okf::yaml_quote(text)));
    out.push_str(&format!(
        "description: {}\n",
        okf::yaml_quote(&okf::description_from_summary(text))
    ));
    let tags_rendered = tags
        .iter()
        .map(|t| okf::yaml_quote(t))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("tags: [{tags_rendered}]\n"));
    out.push_str(&format!(
        "timestamp: {}\n",
        when.format("%Y-%m-%dT%H:%M:%SZ")
    ));
    out.push_str(&format!(
        "status: {}\n",
        match status {
            Status::Open => "open",
            Status::Closed => "closed",
        }
    ));
    if let Some(field) = cfg.link_field {
        out.push_str(&format!("{field}: [{}]\n", render_links_value(links)));
    }
    out.push_str("---\n\n");
    out.push_str(text.trim());
    out.push('\n');
    out
}

/// Parses one already-unquoted `link:` value into wikilink targets, brackets
/// stripped. Accepts both the current flow-list shape (`["[[a]]", "[[b]]"]`)
/// and the old singular scalar shape (`"[[a]]"`, only ever committed in a
/// pre-multi-link version of this code, never deployed) -- distinguished by
/// whether the value starts with `[` (list) or `"` (scalar), so a stray old
/// file doesn't just silently lose its link.
fn parse_links_value(value: &str) -> Vec<String> {
    let value = value.trim();
    let items: Vec<&str> =
        if let Some(inner) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            inner
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect()
        } else if value.is_empty() {
            Vec::new()
        } else {
            vec![value]
        };
    items
        .into_iter()
        .map(okf::yaml_unquote)
        .filter_map(|unquoted| {
            unquoted
                .strip_prefix("[[")
                .and_then(|s| s.strip_suffix("]]"))
                .map(String::from)
        })
        .collect()
}

/// Parses a new-format entry file's frontmatter + body. Returns `None` if
/// the file doesn't look like one (no `---` frontmatter delimiters) --
/// conservative, mirrors `line_format::parse_line`'s stance of skipping
/// anything it doesn't recognize rather than guessing.
pub(super) fn parse(cfg: &Config, contents: &str) -> Option<ParsedEntry> {
    let rest = contents.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let after_close = &rest[end + "\n---".len()..];
    let body = after_close.strip_prefix('\n').unwrap_or(after_close).trim();

    let mut tags = Vec::new();
    let mut status = Status::Open;
    let mut links = Vec::new();
    let mut timestamp = None;
    for line in frontmatter.lines() {
        if let Some(value) = line
            .strip_prefix("tags: [")
            .and_then(|s| s.strip_suffix(']'))
        {
            tags = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(okf::yaml_unquote)
                .collect();
        } else if let Some(value) = line.strip_prefix("status: ") {
            status = if value.trim() == "closed" {
                Status::Closed
            } else {
                Status::Open
            };
        } else if let Some(value) = line.strip_prefix("timestamp: ") {
            timestamp = chrono::NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%dT%H:%M:%SZ")
                .ok()
                .map(|naive| naive.and_utc());
        } else if let Some(field) = cfg.link_field {
            let prefix = format!("{field}: ");
            if let Some(value) = line.strip_prefix(&prefix) {
                links = parse_links_value(value);
            }
        }
    }

    Some(ParsedEntry {
        tags,
        text: body.to_string(),
        status,
        links,
        timestamp,
    })
}

/// Surgically flips the `status: ` frontmatter line in an already-existing
/// entry file, leaving everything else -- including hand-edited body content
/// -- untouched. Returns `Ok(None)` if the file was already at `target`
/// (idempotent no-op, mirrors the legacy path's contract), `Ok(Some(()))` if
/// it changed.
pub(super) fn set_status(path: &Path, target: Status) -> Result<Option<()>, ChecklistError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ChecklistError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let (target_line, other_line) = match target {
        Status::Open => ("status: open", "status: closed"),
        Status::Closed => ("status: closed", "status: open"),
    };
    if contents.lines().any(|l| l == target_line) {
        return Ok(None);
    }
    let new_contents: String = contents
        .lines()
        .map(|l| if l == other_line { target_line } else { l })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(path, new_contents).map_err(|source| ChecklistError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(()))
}

/// Idempotently appends `target` to `cfg.link_field`'s wikilink list --
/// a no-op (`Ok(None)`) if `target` is already linked, matching
/// `set_status`'s idempotency contract. Every file written by the current
/// `render` already has a `link: [...]` line (even if empty) for a domain
/// with `link_field` configured, so this is normally a pure line rewrite;
/// only a file predating this feature (no `link:` line at all) hits the
/// "insert a fresh line" fallback.
pub(super) fn add_link(
    cfg: &Config,
    path: &Path,
    target: &str,
) -> Result<Option<()>, ChecklistError> {
    let field = cfg
        .link_field
        .expect("add_link only called for a domain with link_field configured");
    let contents = std::fs::read_to_string(path).map_err(|source| ChecklistError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let prefix = format!("{field}: ");

    let mut lines: Vec<String> = contents.lines().map(String::from).collect();
    if let Some(idx) = lines.iter().position(|l| l.starts_with(&prefix)) {
        let existing = parse_links_value(&lines[idx][prefix.len()..]);
        if existing.iter().any(|t| t == target) {
            return Ok(None);
        }
        let mut updated = existing;
        updated.push(target.to_string());
        lines[idx] = format!("{field}: [{}]", render_links_value(&updated));
    } else {
        let new_line = format!("{field}: [{}]", render_links_value(&[target.to_string()]));
        let mut seen_first_delimiter = false;
        let insert_at = lines.iter().position(|l| {
            if l == "---" {
                if seen_first_delimiter {
                    return true;
                }
                seen_first_delimiter = true;
            }
            false
        });
        match insert_at {
            Some(idx) => lines.insert(idx, new_line),
            None => lines.push(new_line),
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(path, out).map_err(|source| ChecklistError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(()))
}

/// Rewrites an already-existing entry file's tags/text in place, preserving
/// its original creation `timestamp:`, `status:`, and `links:` -- everything
/// that isn't the content the user is editing. Re-derives `title`/
/// `description` from the new text the same way `render` does for a
/// brand-new entry, so the frontmatter never drifts from the body it
/// describes. Returns `Err(ChecklistError::NotFound)` if `path` doesn't
/// parse as a per-entry file at all (a legacy month-file line has no single
/// file to rewrite -- editing that format isn't supported).
pub(super) fn edit(
    cfg: &Config,
    path: &Path,
    tags: &[String],
    text: &str,
) -> Result<(), ChecklistError> {
    let contents = std::fs::read_to_string(path).map_err(|source| ChecklistError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let Some(parsed) = parse(cfg, &contents) else {
        return Err(ChecklistError::NotFound(format!(
            "{} (not a per-entry file, can't be edited)",
            path.display()
        )));
    };
    let when = parsed.timestamp.unwrap_or_else(Utc::now);
    let rendered = render(cfg, when, tags, text, parsed.status, &parsed.links);
    std::fs::write(path, rendered).map_err(|source| ChecklistError::Write {
        path: path.to_path_buf(),
        source,
    })
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

    const NO_LINK_CFG: Config = Config {
        link_field: None,
        ..TEST_CFG
    };

    fn when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 17, 10, 30, 0).unwrap()
    }

    #[test]
    fn render_parse_round_trip_open_no_links() {
        let rendered = render(
            &TEST_CFG,
            when(),
            &["work".to_string(), "urgent".to_string()],
            "finish the report",
            Status::Open,
            &[],
        );
        let parsed = parse(&TEST_CFG, &rendered).unwrap();
        assert_eq!(parsed.tags, vec!["work", "urgent"]);
        assert_eq!(parsed.text, "finish the report");
        assert_eq!(parsed.status, Status::Open);
        assert_eq!(parsed.links, Vec::<String>::new());
        // Always rendered (even empty) for a domain with link_field set.
        assert!(rendered.contains("link: []"));
    }

    #[test]
    fn render_parse_round_trip_closed_with_links() {
        let links = vec![
            "goals/2026/2026-08-03-2".to_string(),
            "goals/2026/2026-08-02-1".to_string(),
        ];
        let rendered = render(&TEST_CFG, when(), &[], "ship it", Status::Closed, &links);
        let parsed = parse(&TEST_CFG, &rendered).unwrap();
        assert_eq!(parsed.status, Status::Closed);
        assert_eq!(parsed.links, links);
    }

    #[test]
    fn render_omits_link_field_when_cfg_has_none() {
        let rendered = render(&NO_LINK_CFG, when(), &[], "a goal", Status::Open, &[]);
        assert!(!rendered.contains("link:"));
    }

    #[test]
    fn parse_rejects_non_frontmatter_content() {
        assert!(parse(&TEST_CFG, "not a frontmatter file").is_none());
    }

    #[test]
    fn set_status_flips_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.md");
        std::fs::write(
            &path,
            render(&TEST_CFG, when(), &[], "text", Status::Open, &[]),
        )
        .unwrap();

        assert!(set_status(&path, Status::Closed).unwrap().is_some());
        let parsed = parse(&TEST_CFG, &std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.status, Status::Closed);

        // Idempotent: already closed, no-op.
        assert!(set_status(&path, Status::Closed).unwrap().is_none());
    }

    #[test]
    fn add_link_appends_and_is_idempotent_per_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.md");
        std::fs::write(
            &path,
            render(&TEST_CFG, when(), &[], "text", Status::Open, &[]),
        )
        .unwrap();

        assert!(add_link(&TEST_CFG, &path, "goals/2026/2026-08-03-1")
            .unwrap()
            .is_some());
        let parsed = parse(&TEST_CFG, &std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.links, vec!["goals/2026/2026-08-03-1"]);

        // A second, different target is appended, not replaced -- multiple
        // goals can share one supporting todo.
        assert!(add_link(&TEST_CFG, &path, "goals/2026/2026-08-03-2")
            .unwrap()
            .is_some());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("link:").count(), 1);
        let parsed = parse(&TEST_CFG, &contents).unwrap();
        assert_eq!(
            parsed.links,
            vec!["goals/2026/2026-08-03-1", "goals/2026/2026-08-03-2"]
        );

        // Re-adding the same target is a no-op.
        assert!(add_link(&TEST_CFG, &path, "goals/2026/2026-08-03-1")
            .unwrap()
            .is_none());
        let parsed = parse(&TEST_CFG, &std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.links.len(), 2);
    }

    #[test]
    fn parse_accepts_legacy_singular_link_scalar() {
        // A file committed under the pre-multi-link scalar shape (never
        // actually deployed, but cheap to keep readable) should still parse
        // as a one-element list rather than silently losing the link.
        let contents = "---\ntype: thing\ntitle: \"x\"\ndescription: \"x\"\ntags: []\ntimestamp: 2026-07-17T10:30:00Z\nstatus: open\nlink: \"[[goals/2026/2026-08-03-1]]\"\n---\n\nx\n";
        let parsed = parse(&TEST_CFG, contents).unwrap();
        assert_eq!(parsed.links, vec!["goals/2026/2026-08-03-1"]);
    }

    #[test]
    fn edit_rewrites_text_and_tags_preserving_status_links_and_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.md");
        let links = vec!["goals/2026/2026-08-02-1".to_string()];
        std::fs::write(
            &path,
            render(
                &TEST_CFG,
                when(),
                &["work".to_string()],
                "original text",
                Status::Closed,
                &links,
            ),
        )
        .unwrap();

        edit(&TEST_CFG, &path, &["urgent".to_string()], "new text").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let parsed = parse(&TEST_CFG, &contents).unwrap();
        assert_eq!(parsed.text, "new text");
        assert_eq!(parsed.tags, vec!["urgent"]);
        // Status and links are untouched by an edit -- only content changes.
        assert_eq!(parsed.status, Status::Closed);
        assert_eq!(parsed.links, links);
        assert_eq!(parsed.timestamp, Some(when()));
        assert!(contents.contains("title: \"new text\""));
    }

    #[test]
    fn edit_rejects_non_frontmatter_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("entry.md");
        std::fs::write(&path, "not a frontmatter file").unwrap();

        assert!(matches!(
            edit(&TEST_CFG, &path, &[], "new text"),
            Err(ChecklistError::NotFound(_))
        ));
    }

    #[test]
    fn entry_path_and_wikilink_target_disambiguate_by_folder() {
        let key = EntryKey {
            date: chrono::NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(),
            line: 2,
        };
        let todo_cfg = Config {
            folder: "todo",
            ..TEST_CFG
        };
        let goal_cfg = Config {
            folder: "goals",
            ..TEST_CFG
        };
        assert_ne!(
            entry_path(&todo_cfg, Path::new("/repo"), key),
            entry_path(&goal_cfg, Path::new("/repo"), key)
        );
        assert_ne!(
            wikilink_target(&todo_cfg, key),
            wikilink_target(&goal_cfg, key)
        );
        assert_eq!(wikilink_target(&goal_cfg, key), "goals/2026/2026-08-03-2");
    }
}
