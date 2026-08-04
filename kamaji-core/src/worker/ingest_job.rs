use std::collections::HashSet;

use chrono::Utc;

use crate::git;
use crate::notes;
use crate::prompt::{self, FetchedContent, TokenUsage};
use crate::state::AppState;
use crate::tags;
use crate::urls;

use super::{describe_push_outcome, fetch_one};

/// Deliberate breadth cap on link-following: a fetched page can contain far
/// more links than a message ever could, so this bounds worst-case fetch
/// count (and thus job latency, since the worker is single-threaded and
/// sequential) independent of `max_fetched_text_bytes`.
const MAX_LEVEL1_URLS: usize = 5;

/// Which of `/ingest`'s two behaviors an argument selects. Split out from
/// `process_ingest_command` as a plain function so the branching decision is
/// unit-testable without a full `AppState`. Assumes `args` is non-empty --
/// `kamajid::transport::dispatch_routed_job` replies with a usage error and
/// never enqueues an `/ingest` job with no argument, the same way it never
/// enqueues an unknown command.
enum IngestCommandRoute {
    /// Same note-filing pipeline as the no-command default path.
    Link { raw_text: String, urls: Vec<String> },
    /// Not filed as a note -- handed to the agent as-is (see TODO.md).
    AgentQuery(String),
}

fn route_ingest_command(args: &[String]) -> IngestCommandRoute {
    let text = args.join(" ");
    let found_urls = urls::extract_urls(&text);
    if !found_urls.is_empty() {
        IngestCommandRoute::Link {
            raw_text: text,
            urls: found_urls,
        }
    } else {
        IngestCommandRoute::AgentQuery(text)
    }
}

/// `/ingest <link or text>`: a link takes the same note-filing path as the
/// no-command default (reusing `process_ingest` unchanged); freeform text is
/// not filed as a note at all -- it's passed straight to the agent.
pub(super) async fn process_ingest_command(
    state: &AppState,
    args: &[String],
) -> (String, Option<TokenUsage>, bool, Option<String>, String) {
    match route_ingest_command(args) {
        IngestCommandRoute::Link { raw_text, urls } => {
            process_ingest(state, &raw_text, &urls).await
        }
        IngestCommandRoute::AgentQuery(text) => process_agent_query(state, &text).await,
    }
}

/// The non-link `/ingest <text>` branch: hands the text to the agent and
/// relays its reply verbatim, with no note/commit/push involved.
async fn process_agent_query(
    state: &AppState,
    text: &str,
) -> (String, Option<TokenUsage>, bool, Option<String>, String) {
    let debug_prompt = prompt::build_agent_prompt(text);
    match prompt::run_agent_query(
        state.config.agent_flavor,
        &state.runner,
        &state.config.agent_bin,
        state.config.agent_timeout,
        text,
    )
    .await
    {
        Ok((reply, tokens)) => (reply, tokens, true, None, debug_prompt),
        Err(err) => {
            tracing::error!(%err, "agent query failed");
            let msg = format!("Agent query failed: {err}");
            (msg.clone(), None, false, Some(msg), debug_prompt)
        }
    }
}

pub(super) async fn process_ingest(
    state: &AppState,
    raw_text: &str,
    urls: &[String],
) -> (String, Option<TokenUsage>, bool, Option<String>, String) {
    // Tags are read from the user's own message only, before anything is
    // fetched -- fetched article bodies are full of #-prefixed tokens that
    // aren't user intent (hashtags in copy, #anchor fragments, #define,
    // #!/bin/sh, CSS selectors), and pulling tags from those would junk up
    // every note's frontmatter with garbage the user never typed, and
    // non-deterministically (re-fetch later, different tags). See TODO.md
    // "User #tags on ingest".
    let user_tags = tags::extract_tags(raw_text);

    // A message that's only tags and carries no URL has nothing to
    // summarize -- mirrors the all-fetches-failed-and-no-standalone-text
    // guard below rather than adding a second, differently-shaped
    // empty-content path.
    if urls.is_empty() && !has_standalone_text(raw_text, urls) {
        let debug_prompt = prompt::build_prompt(raw_text, &[], &[]);
        let msg = "Nothing to ingest: message contains only tags.".to_string();
        return (msg.clone(), None, false, Some(msg), debug_prompt);
    }

    // Pre-fetch all URLs before calling Claude (level 0: the message's own links).
    // Real fetch failures (not auth-required, which gets a useful placeholder)
    // are tracked separately rather than fed to Claude as content -- see the
    // no-content check below.
    let mut fetched_urls = Vec::new();
    let mut failed_urls = Vec::new();
    for url in urls {
        fetch_one(
            url,
            state.config.agent_timeout,
            &mut fetched_urls,
            &mut failed_urls,
        )
        .await;
    }

    // Follow one level of links found inside the fetched content (e.g. a
    // tweet linking out to the article it's about) -- but never deeper than
    // that, and skip it entirely if level 0 already blew the size budget,
    // since level 1 would only be discarded anyway.
    let level0_bytes: usize = fetched_urls.iter().map(|f| f.text.len()).sum();
    if level0_bytes <= state.config.max_fetched_text_bytes {
        for url in level1_targets(urls, &fetched_urls, MAX_LEVEL1_URLS) {
            fetch_one(
                &url,
                state.config.agent_timeout,
                &mut fetched_urls,
                &mut failed_urls,
            )
            .await;
        }
    }

    // If every URL fetch that failed left us with nothing else to work with
    // (no successfully fetched content, and the message itself was just the
    // link(s)), there's nothing for Claude to summarize -- calling it would
    // only spend tokens to produce a placeholder "Unable to process" note.
    // Skip the call and tell the user directly instead of filing a note.
    if !failed_urls.is_empty() && fetched_urls.is_empty() && !has_standalone_text(raw_text, urls) {
        let debug_prompt = prompt::build_prompt(raw_text, &fetched_urls, &[]);
        let detail = failed_urls
            .iter()
            .map(|(url, err)| format!("{url}: {err}"))
            .collect::<Vec<_>>()
            .join("\n");
        tracing::warn!(
            detail,
            "all URL fetches failed with no other content, skipping ingestion"
        );
        let msg = format!("Ingestion skipped: could not fetch content.\n{detail}");
        return (msg.clone(), None, false, Some(msg), debug_prompt);
    }

    // Existing category folders are trivial for Rust to enumerate up front,
    // so they're gathered here (unioned with a seed list for a fresh repo)
    // and handed to the prompt as context rather than making Claude discover
    // them itself (see TODO.md).
    let existing_categories = notes::categories_for_prompt(&state.config.notes_repo_path);

    // Built once here (rather than only inside `run_ingest_prompt`) so the
    // exact text sent to Claude -- including fetched URL content -- is
    // available for debug logging regardless of how this function returns.
    let debug_prompt = prompt::build_prompt(raw_text, &fetched_urls, &existing_categories);

    let total_fetched_bytes: usize = fetched_urls.iter().map(|f| f.text.len()).sum();
    if total_fetched_bytes > state.config.max_fetched_text_bytes {
        tracing::warn!(
            total_fetched_bytes,
            limit = state.config.max_fetched_text_bytes,
            "fetched content exceeds size limit, skipping ingestion"
        );
        let msg = format!(
            "Ingestion skipped: fetched content is {total_fetched_bytes} bytes, over the {}-byte limit.",
            state.config.max_fetched_text_bytes
        );
        return (msg.clone(), None, false, Some(msg), debug_prompt);
    }

    let result = prompt::run_ingest_prompt(
        state.config.agent_flavor,
        &state.runner,
        &state.config.agent_bin,
        state.config.agent_timeout,
        raw_text,
        &fetched_urls,
        &existing_categories,
    )
    .await;

    let (mut ingest_result, tokens) = match result {
        Ok((r, t)) => {
            // Warn if token parsing failed, but continue processing
            if t.is_none() {
                tracing::warn!("failed to parse token usage from Claude CLI stderr");
            }
            (r, t)
        }
        Err(err) => {
            tracing::error!(%err, "claude invocation failed");
            let msg = format!("Ingestion failed: {err}");
            return (msg.clone(), None, false, Some(msg), debug_prompt);
        }
    };

    // Union the user's inline tags with Claude's inferred ones -- done here
    // in Rust, after the call, rather than touching the ingest prompt, so
    // the strict-JSON ingest contract stays untouched (same precedent as
    // OKF `description`, see CLAUDE.md). Everything downstream (frontmatter,
    // OKF tags, the confirmation reply, `record_last_note`) reads
    // `ingest_result.tags`, so mutating it here is enough for the merge to
    // reach all of them.
    ingest_result.tags = tags::merge_user_tags(&user_tags, &ingest_result.tags);

    let date = Utc::now().date_naive();
    let note_path = match notes::write_note(&state.config.notes_repo_path, date, &ingest_result) {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(%err, "failed to write note");
            let msg = format!("Ingestion failed: could not write note: {err}");
            return (msg.clone(), tokens, false, Some(msg), debug_prompt);
        }
    };

    let push_outcome = match git::commit_and_push(
        &state.config.notes_repo_path,
        std::slice::from_ref(&note_path),
        &format!("Add note: {}", ingest_result.title),
        state.config.git_timeout,
        state.config.git_push_retries,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::error!(%err, "git commit failed");
            let msg = format!(
                "Note written to disk, but commit failed: {err}\nPath: {}",
                note_path.display()
            );
            return (msg.clone(), tokens, false, Some(msg), debug_prompt);
        }
    };

    let tags_display = ingest_result.tags.join(", ");
    let mut confirmation = format!(
        "Note written: {}\nCategory: {}\nImportance: {}\nTags: [{}]",
        ingest_result.title, ingest_result.category, ingest_result.importance, tags_display
    );

    // Add token usage to the reply if available
    if let Some(ref t) = tokens {
        confirmation.push_str(&format!("\n\nTokens: {} in, {} out", t.input, t.output));
    }

    state.record_last_note(format!(
        "{}, category {}, importance {}, tags [{}]",
        ingest_result.title, ingest_result.category, ingest_result.importance, tags_display
    ));

    let reply = describe_push_outcome(confirmation, push_outcome);

    (reply, tokens, true, None, debug_prompt)
}

/// Picks which links found inside already-fetched content to follow next,
/// deduped against `already_fetched` (so a level-1 link back to a level-0
/// URL, or one repeated across multiple fetched pages, isn't fetched twice)
/// and capped at `max`.
fn level1_targets(
    already_fetched: &[String],
    fetched_urls: &[FetchedContent],
    max: usize,
) -> Vec<String> {
    let mut seen: HashSet<String> = already_fetched.iter().cloned().collect();
    let mut targets = Vec::new();
    for fetched in fetched_urls {
        for link in urls::extract_urls(&fetched.text) {
            if seen.insert(link.clone()) {
                targets.push(link);
            }
        }
    }
    targets.truncate(max);
    targets
}

/// True if `raw_text` has any whitespace token that is neither one of
/// `urls` nor a recognized `#tag` -- i.e. the message carries content beyond
/// bare links and inline tags. Used both to decide whether a failed fetch
/// still leaves something for Claude to work with, and to detect a
/// tags-only message (no URL, nothing else) before any fetch is attempted.
fn has_standalone_text(raw_text: &str, urls: &[String]) -> bool {
    raw_text
        .split_whitespace()
        .any(|token| !urls.iter().any(|url| url == token) && tags::parse_tag_token(token).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetched(url: &str, text: &str) -> FetchedContent {
        FetchedContent {
            url: url.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn route_ingest_command_takes_link_path_for_a_url() {
        let args = vec!["https://example.com/article".to_string()];
        match route_ingest_command(&args) {
            IngestCommandRoute::Link { raw_text, urls } => {
                assert_eq!(raw_text, "https://example.com/article");
                assert_eq!(urls, vec!["https://example.com/article"]);
            }
            IngestCommandRoute::AgentQuery(_) => panic!("expected Link route"),
        }
    }

    #[test]
    fn route_ingest_command_takes_link_path_for_text_with_a_url() {
        let args = vec![
            "worth".to_string(),
            "reading:".to_string(),
            "https://example.com".to_string(),
        ];
        match route_ingest_command(&args) {
            IngestCommandRoute::Link { raw_text, urls } => {
                assert_eq!(raw_text, "worth reading: https://example.com");
                assert_eq!(urls, vec!["https://example.com"]);
            }
            IngestCommandRoute::AgentQuery(_) => panic!("expected Link route"),
        }
    }

    #[test]
    fn route_ingest_command_takes_agent_path_for_plain_text() {
        let args = vec![
            "what".to_string(),
            "time".to_string(),
            "is".to_string(),
            "it".to_string(),
        ];
        match route_ingest_command(&args) {
            IngestCommandRoute::AgentQuery(text) => assert_eq!(text, "what time is it"),
            IngestCommandRoute::Link { .. } => panic!("expected AgentQuery route"),
        }
    }

    #[test]
    fn has_standalone_text_false_for_bare_url() {
        let urls = vec!["https://x.com/i/article/123".to_string()];
        assert!(!has_standalone_text("https://x.com/i/article/123", &urls));
    }

    #[test]
    fn has_standalone_text_true_when_message_has_commentary() {
        let urls = vec!["https://x.com/i/article/123".to_string()];
        assert!(has_standalone_text(
            "worth reading: https://x.com/i/article/123",
            &urls
        ));
    }

    #[test]
    fn has_standalone_text_true_when_no_urls() {
        assert!(has_standalone_text("just plain text", &[]));
    }

    #[test]
    fn has_standalone_text_false_for_tags_only_message() {
        assert!(!has_standalone_text("#work #urgent", &[]));
    }

    #[test]
    fn has_standalone_text_false_for_url_plus_tag_only() {
        let urls = vec!["https://example.com".to_string()];
        assert!(!has_standalone_text("#rust https://example.com", &urls));
    }

    #[test]
    fn has_standalone_text_true_when_tag_plus_commentary() {
        assert!(has_standalone_text("#rust worth a read", &[]));
    }

    #[test]
    fn has_standalone_text_true_for_bare_number_hash() {
        // #2026 isn't a recognized tag (no alphabetic char), so it still
        // counts as standalone content rather than being silently dropped.
        assert!(has_standalone_text("#2026", &[]));
    }

    #[test]
    fn follows_links_found_in_fetched_content() {
        let already_fetched = vec!["https://a.com".to_string()];
        let fetched_urls = vec![fetched("https://a.com", "see https://b.com for more")];

        let targets = level1_targets(&already_fetched, &fetched_urls, 5);
        assert_eq!(targets, vec!["https://b.com"]);
    }

    #[test]
    fn does_not_refetch_an_already_fetched_url() {
        let already_fetched = vec!["https://a.com".to_string()];
        // The fetched content links back to the URL that produced it.
        let fetched_urls = vec![fetched(
            "https://a.com",
            "originally posted at https://a.com",
        )];

        let targets = level1_targets(&already_fetched, &fetched_urls, 5);
        assert!(targets.is_empty());
    }

    #[test]
    fn dedupes_the_same_link_across_multiple_fetched_pages() {
        let already_fetched = vec!["https://a.com".to_string(), "https://b.com".to_string()];
        let fetched_urls = vec![
            fetched("https://a.com", "see https://c.com"),
            fetched("https://b.com", "also see https://c.com"),
        ];

        let targets = level1_targets(&already_fetched, &fetched_urls, 5);
        assert_eq!(targets, vec!["https://c.com"]);
    }

    #[test]
    fn caps_at_max_level1_urls() {
        let already_fetched = vec!["https://a.com".to_string()];
        let fetched_urls = vec![fetched(
            "https://a.com",
            "https://c1.com https://c2.com https://c3.com",
        )];

        let targets = level1_targets(&already_fetched, &fetched_urls, 2);
        assert_eq!(targets.len(), 2);
    }
}
