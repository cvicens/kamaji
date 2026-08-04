//! OKF (Open Knowledge Format) frontmatter helpers, shared by the ingest
//! notes (`notes.rs`) and the `/fact` bitacora entries (`bitacora.rs`).
//!
//! kamaji writes its notes so the git-committed notes repo doubles as an OKF
//! knowledge bundle that downstream agents can consume:
//! <https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf>.
//!
//! OKF requires exactly one field -- a non-empty `type` -- and *recommends*
//! `title`, `description`, `resource`, `tags`, and `timestamp` (ISO 8601).
//! Every other key is a producer-defined custom field that conformant
//! consumers must preserve untouched. That's the reason kamaji keeps its
//! domain-specific fields (`category`/`importance` for notes, `value`/
//! `attachment` for facts) alongside the OKF core rather than folding them
//! away -- they ride along as custom fields, fully within spec.

/// OKF `type` for an ingest note. This is the *kind* of knowledge, orthogonal
/// to the note's `category` folder (which is the topic/domain); consumers
/// filter notes vs. bitacora entries on this field.
pub const TYPE_NOTE: &str = "note";

/// OKF `type` for a `/fact` bitacora (bio-log) entry.
pub const TYPE_FACT: &str = "fact";

/// Quote a string as a double-quoted YAML scalar, escaping backslashes and
/// quotes. Shared so the escaping rule lives in exactly one place.
pub fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Reverse of `yaml_quote`: strip one layer of surrounding `"..."` and
/// unescape `\\`->`\`, `\"`->`"`. If `s` isn't a quoted scalar (no
/// surrounding quotes), it's returned unchanged -- callers that only ever
/// feed it something `yaml_quote` produced won't hit that path, but a
/// hand-edited frontmatter value (e.g. someone editing a note in Obsidian)
/// shouldn't panic or silently corrupt the value either.
pub fn yaml_unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Derive OKF's one-sentence `description` from the multi-sentence `summary`
/// kamaji already stores.
///
/// OKF's `description` is a single-line preview/search snippet, distinct from
/// the note body. Rather than add a field to the load-bearing strict-JSON
/// Claude contract (see `CLAUDE.md`), we take the first sentence of the
/// summary Claude already returns -- it works uniformly for notes and facts
/// (both carry `summary`) and keeps the prompt untouched. Falls back to the
/// whole trimmed summary when there's no sentence terminator to split on.
///
/// This is a preview-snippet heuristic, not a full sentence tokenizer: a
/// terminator only ends the description if it sits at the end of the text or
/// is followed by whitespace, so a decimal like `3.5` inside the first
/// sentence won't cut it short.
pub fn description_from_summary(summary: &str) -> String {
    let trimmed = summary.trim();
    for (i, ch) in trimmed.char_indices() {
        if matches!(ch, '.' | '!' | '?') {
            let after = trimmed[i + ch.len_utf8()..].chars().next();
            if after.is_none_or(|c| c.is_whitespace()) {
                return trimmed[..i + ch.len_utf8()].trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_takes_first_sentence() {
        assert_eq!(
            description_from_summary("Fixed the outage. Rolled back the deploy. Paged the team."),
            "Fixed the outage."
        );
    }

    #[test]
    fn description_falls_back_to_whole_summary_without_terminator() {
        assert_eq!(
            description_from_summary("a fragment with no period"),
            "a fragment with no period"
        );
    }

    #[test]
    fn description_handles_single_sentence() {
        assert_eq!(
            description_from_summary("A short summary."),
            "A short summary."
        );
    }

    #[test]
    fn description_does_not_split_on_a_decimal_point() {
        assert_eq!(
            description_from_summary("Bumped the model to 3.5 Sonnet. Ran the eval."),
            "Bumped the model to 3.5 Sonnet."
        );
    }

    #[test]
    fn description_trims_surrounding_whitespace() {
        assert_eq!(
            description_from_summary("  Leading and trailing.  next"),
            "Leading and trailing."
        );
    }

    #[test]
    fn yaml_quote_escapes_quotes_and_backslashes() {
        assert_eq!(
            yaml_quote(r#"a "quote" and \slash"#),
            r#""a \"quote\" and \\slash""#
        );
    }

    #[test]
    fn yaml_unquote_reverses_yaml_quote() {
        let original = r#"a "quote" and \slash"#;
        assert_eq!(yaml_unquote(&yaml_quote(original)), original);
    }

    #[test]
    fn yaml_unquote_plain_string_round_trips() {
        assert_eq!(
            yaml_unquote(&yaml_quote("finish the report")),
            "finish the report"
        );
    }

    #[test]
    fn yaml_unquote_leaves_unquoted_input_unchanged() {
        assert_eq!(yaml_unquote("open"), "open");
    }
}
