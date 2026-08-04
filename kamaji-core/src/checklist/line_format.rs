use chrono::NaiveDate;
use once_cell::sync::Lazy;
use regex::Regex;

use super::{Config, Entry, Status, StatusFilter};

/// Matches one rendered checklist line, open or closed. The optional
/// `(?:\S+ )?` group is what makes this format-agnostic to the old
/// `#<id> [tags] text` lines this codebase used to write: it swallows
/// whatever single token (if any) sits between the checkbox and `[tags]`,
/// so a hand-edited or pre-migration `#7 [tags] text` line and a fresh
/// id-less `[tags] text` line both parse identically -- no rewrite of
/// existing files ever required. An entry's date/position aren't part of
/// this match at all: date comes from the enclosing `## DD-MM-YYYY`
/// heading (see `parse_day_heading`) and position comes from counting
/// matched lines under that heading, both tracked by the caller.
static LINE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^- \[(?P<mark>[ x])\] (?:\S+ )?\[(?P<tags>[^\]]*)\] (?P<text>.*)$")
        .expect("static regex is valid")
});

/// Matches a `## DD-MM-YYYY` day-section heading.
static DAY_HEADING_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^## (?P<d>\d{2})-(?P<m>\d{2})-(?P<y>\d{4})$").expect("static regex is valid")
});

const DAY_FMT: &str = "%d-%m-%Y";

/// One recognized checklist line's content, with no notion of *where* it
/// is -- that's what `super::EntryKey` (date + position, supplied by the
/// caller as it walks the file) adds on top of this.
pub(super) struct ParsedLine {
    pub tags: Vec<String>,
    pub text: String,
    pub status: Status,
}

/// Renders a `list` reply.
pub fn format_list(cfg: &Config, entries: &[Entry], filter: StatusFilter) -> String {
    let label = match filter {
        StatusFilter::Open => "Open",
        StatusFilter::Closed => "Closed",
    };
    if entries.is_empty() {
        return format!("No {} {}.", label.to_lowercase(), cfg.plural_noun);
    }
    let mut lines = vec![format!("{label} {} ({}):", cfg.plural_noun, entries.len())];
    for entry in entries {
        let tags = entry.tags.join(", ");
        let closed_suffix = match entry.status {
            Status::Open => String::new(),
            Status::Closed => format!(" ({})", cfg.closed_verb),
        };
        lines.push(format!(
            "{} [{}] {}{closed_suffix}",
            entry.key, tags, entry.text
        ));
    }
    lines.join("\n")
}

/// Renders one checklist line -- no id, no date (the enclosing `##
/// DD-MM-YYYY` heading carries that); closing an entry only flips the
/// checkbox, no timestamp appended (the git history is the audit trail for
/// when that happened).
pub(super) fn render_line(status: Status, tags: &[String], text: &str) -> String {
    let tags = tags.join(", ");
    let mark = match status {
        Status::Open => ' ',
        Status::Closed => 'x',
    };
    format!("- [{mark}] [{tags}] {text}")
}

/// Parses one checklist line, ignoring any legacy `#<id>` token. Returns
/// `None` for anything else (prose, headings, blank lines).
pub(super) fn parse_line(line: &str) -> Option<ParsedLine> {
    let caps = LINE_RE.captures(line)?;
    let status = if &caps["mark"] == "x" {
        Status::Closed
    } else {
        Status::Open
    };
    let tags: Vec<String> = caps["tags"]
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    Some(ParsedLine {
        tags,
        text: caps["text"].to_string(),
        status,
    })
}

/// Parses a `## DD-MM-YYYY` day-section heading into the date every
/// checklist line underneath it inherits.
pub(super) fn parse_day_heading(line: &str) -> Option<NaiveDate> {
    let caps = DAY_HEADING_RE.captures(line)?;
    NaiveDate::from_ymd_opt(
        caps["y"].parse().ok()?,
        caps["m"].parse().ok()?,
        caps["d"].parse().ok()?,
    )
}

/// Renders a `## DD-MM-YYYY` day-section heading for `date`.
pub(super) fn render_day_heading(date: NaiveDate) -> String {
    format!("## {}", date.format(DAY_FMT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checklist::EntryKey;
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

    fn day() -> NaiveDate {
        chrono::Utc
            .with_ymd_and_hms(2026, 7, 17, 10, 30, 0)
            .unwrap()
            .date_naive()
    }

    #[test]
    fn parse_line_round_trips_open_and_closed_entries() {
        let line = render_line(
            Status::Open,
            &["a".to_string(), "b".to_string()],
            "do the thing",
        );
        let parsed = parse_line(&line).unwrap();
        assert_eq!(parsed.tags, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(parsed.text, "do the thing");
        assert_eq!(parsed.status, Status::Open);

        let line = render_line(Status::Closed, &["a".to_string()], "do the thing");
        let parsed = parse_line(&line).unwrap();
        assert_eq!(parsed.status, Status::Closed);
    }

    #[test]
    fn parse_line_ignores_unrelated_lines() {
        assert!(parse_line("some prose").is_none());
        assert!(parse_line("# Some heading").is_none());
        assert!(parse_line("## 17-07-2026").is_none());
    }

    #[test]
    fn parse_line_accepts_legacy_hash_id_lines() {
        // Pre-migration lines had a `#<id> ` token between the checkbox and
        // `[tags]` -- these are never rewritten, so the parser must keep
        // accepting them forever.
        let parsed = parse_line("- [ ] #7 [work] legacy item").unwrap();
        assert_eq!(parsed.tags, vec!["work".to_string()]);
        assert_eq!(parsed.text, "legacy item");
        assert_eq!(parsed.status, Status::Open);

        let parsed = parse_line("- [x] #12 [] legacy closed item").unwrap();
        assert_eq!(parsed.text, "legacy closed item");
        assert_eq!(parsed.status, Status::Closed);
    }

    #[test]
    fn parse_day_heading_matches_dd_mm_yyyy() {
        assert_eq!(
            parse_day_heading("## 17-07-2026"),
            NaiveDate::from_ymd_opt(2026, 7, 17)
        );
        assert!(parse_day_heading("## not-a-date").is_none());
        assert!(parse_day_heading("- [ ] [] text").is_none());
    }

    #[test]
    fn render_day_heading_uses_dd_mm_yyyy() {
        assert_eq!(
            render_day_heading(NaiveDate::from_ymd_opt(2026, 7, 17).unwrap()),
            "## 17-07-2026"
        );
    }

    #[test]
    fn format_list_includes_key_tags_and_text() {
        let entries = vec![Entry {
            key: EntryKey {
                date: day(),
                line: 1,
            },
            tags: vec!["a".to_string()],
            text: "do it".to_string(),
            status: Status::Open,
            links: Vec::new(),
        }];
        let out = format_list(&TEST_CFG, &entries, StatusFilter::Open);
        assert!(out.contains("Open things (1):"));
        assert!(out.contains("2026-07-17-1 [a] do it"));
    }

    #[test]
    fn format_list_reports_none_when_empty() {
        assert_eq!(
            format_list(&TEST_CFG, &[], StatusFilter::Open),
            "No open things."
        );
    }

    #[test]
    fn format_list_closed_entry_shows_verb_without_a_timestamp() {
        let entries = vec![Entry {
            key: EntryKey {
                date: day(),
                line: 1,
            },
            tags: vec![],
            text: "do it".to_string(),
            status: Status::Closed,
            links: Vec::new(),
        }];
        let out = format_list(&TEST_CFG, &entries, StatusFilter::Closed);
        assert!(out.contains("2026-07-17-1 [] do it (finished)"));
    }
}
