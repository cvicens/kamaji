use std::collections::HashSet;

/// Recognizes a `#tag` token anywhere among an `add` command's args (start,
/// middle, or end -- order doesn't matter) rather than requiring a single
/// bracketed block up front. Each tag is self-delimiting -- it starts with
/// `#` and ends at the next whitespace, which routing already split on --
/// so there's no closing delimiter to forget or mistype, unlike an earlier
/// `[a,b]` syntax. Same sigil pattern Todoist uses for `#project`/`@label`
/// in its mobile quick-add, chosen for the same reason: it has to survive
/// being thumbed out on a phone keyboard.
///
/// Requires at least one ASCII letter in the token so a bare `#123` (e.g.
/// referencing an issue number) is left as ordinary text instead of being
/// misread as a tag; a token that's just digits/punctuation after the `#`
/// returns `None`.
///
/// A leading `(`/quote and trailing prose punctuation (`,.;:!?)]}` plus a
/// trailing quote) are trimmed before recognition -- ingest/fact text is
/// prose, not pre-split command args, so `an article about #rust, really`
/// must still tag `rust` rather than silently dropping it because of the
/// comma. Only the outer edges are touched, never the interior, so `#a#b`
/// stays ambiguous rather than getting rewritten.
///
/// `pub(crate)` (rather than `pub`) since this is an internal building
/// block shared by `checklist::parse_command`'s "add" branch and
/// `worker::process_ingest`/`process_fact_command`'s inline-tag support --
/// one recognizer, no drift between call sites.
pub(crate) fn parse_tag_token(token: &str) -> Option<String> {
    let token = token.trim_start_matches(['(', '\'', '"']);
    let token = token.trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
    let rest = token.strip_prefix('#')?;
    if rest.is_empty() {
        return None;
    }
    if !rest
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    if !rest.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(rest.to_string())
}

/// Every recognized `#tag` token in `text`, in the order they appear,
/// duplicates kept (callers needing dedup do it themselves). Splits on
/// whitespace and delegates to `parse_tag_token`, so this is the one place
/// `checklist::parse_command`'s `add` branch and `/ingest`/`/fact`'s
/// inline-tag support (`worker::process_ingest`,
/// `worker::process_fact_command`) get their tags from.
pub(crate) fn extract_tags(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(parse_tag_token)
        .collect()
}

/// Unions `user_tags` (typed inline in the message, explicit intent) with
/// `other_tags` (inferred by Claude, for `/ingest`/`/fact`): user tags
/// lead, since they're explicit; neither list overrides the other, since
/// the freeform-tags-no-taxonomy rule (CLAUDE.md) treats both as additive
/// signal. Deduped case-insensitively so `#Rust` from the user and `rust`
/// from Claude collapse to one tag, keeping whichever spelling was seen
/// first (the user's, since they're iterated first).
pub(crate) fn merge_user_tags(user_tags: &[String], other_tags: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    user_tags
        .iter()
        .chain(other_tags.iter())
        .filter(|tag| seen.insert(tag.to_lowercase()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_token_trims_trailing_prose_punctuation() {
        assert_eq!(parse_tag_token("#rust,"), Some("rust".to_string()));
        assert_eq!(parse_tag_token("#rust."), Some("rust".to_string()));
        assert_eq!(parse_tag_token("#rust!"), Some("rust".to_string()));
        assert_eq!(parse_tag_token("#rust?"), Some("rust".to_string()));
    }

    #[test]
    fn parse_tag_token_trims_surrounding_parens_and_quotes() {
        assert_eq!(parse_tag_token("(#rust)"), Some("rust".to_string()));
        assert_eq!(parse_tag_token("\"#rust\""), Some("rust".to_string()));
    }

    #[test]
    fn parse_tag_token_bare_number_after_trim_is_still_not_a_tag() {
        assert_eq!(parse_tag_token("#2026,"), None);
    }

    #[test]
    fn extract_tags_reads_prose_with_punctuation() {
        let tags = extract_tags("an article about #rust, really");
        assert_eq!(tags, vec!["rust".to_string()]);
    }

    #[test]
    fn extract_tags_never_sees_fetched_body_noise() {
        // The boundary this whole feature exists for: `process_ingest` and
        // `process_fact_command` compute `user_tags` from `raw_text` alone,
        // never from `FetchedContent.text`. This test pins why that
        // matters -- the very same recognizer, if it were ever fed a
        // fetched article body instead, would misread hashtags-in-copy,
        // `#define`, and CSS selectors as user-intended tags. A message
        // with no tags of its own must yield none, regardless of what any
        // fetched content looked like.
        let raw_text = "an article worth filing, no hashtags here";
        assert!(extract_tags(raw_text).is_empty());

        let fetched_body_noise =
            "social hashtag #hashtag in the copy\n# Heading\ncode: #define FOO\nshebang: #!/bin/sh\ncss: #selector { color: red; }";
        let noise_tags = extract_tags(fetched_body_noise);
        assert_eq!(
            noise_tags,
            vec!["hashtag", "define", "selector"],
            "sanity check: this is exactly the class of noise raw_text-only extraction avoids"
        );
    }

    #[test]
    fn merge_user_tags_puts_user_tags_first_in_typed_order() {
        let merged = merge_user_tags(
            &["urgent".to_string(), "work".to_string()],
            &["rust".to_string()],
        );
        assert_eq!(merged, vec!["urgent", "work", "rust"]);
    }

    #[test]
    fn merge_user_tags_dedupes_case_insensitively_keeping_user_spelling() {
        let merged = merge_user_tags(&["Rust".to_string()], &["rust".to_string()]);
        assert_eq!(merged, vec!["Rust"]);
    }

    #[test]
    fn merge_user_tags_keeps_non_overlapping_tags_from_both_sides() {
        let merged = merge_user_tags(
            &["work".to_string()],
            &["rust".to_string(), "async".to_string()],
        );
        assert_eq!(merged, vec!["work", "rust", "async"]);
    }
}
