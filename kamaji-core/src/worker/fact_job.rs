use chrono::Utc;

use crate::bitacora;
use crate::git;
use crate::prompt::{self, TokenUsage};
use crate::state::AppState;
use crate::tags;
use crate::urls;

use super::{describe_push_outcome, fetch_one, FactAttachment};

/// `/fact <description>` [+ optional attached file]: logs a bitacora entry
/// under `bitacora/<year>/<month>/`. Unlike `/ingest`, there's no
/// agent-passthrough branch -- every call writes an entry. Any URLs found in
/// the text are fetched the same way `process_ingest` does (reusing
/// `fetch_one`); the attachment (if any) has already been resolved -- or its
/// resolution attempted -- by the caller (see `FactAttachment`), since core
/// has no platform client to do that itself.
pub(super) async fn process_fact_command(
    state: &AppState,
    args: &[String],
    attachment: FactAttachment,
) -> (String, Option<TokenUsage>, bool, Option<String>, String) {
    let raw_text = args.join(" ");
    let urls = urls::extract_urls(&raw_text);

    // Same rationale as `process_ingest`: read tags from the user's own
    // text before any URL is fetched, never from the fetched body, so a
    // linked article's own hashtags/anchors/CSS selectors can't leak into
    // this fact's frontmatter.
    let user_tags = tags::extract_tags(&raw_text);

    let mut fetched_urls = Vec::new();
    let mut failed_urls = Vec::new();
    for url in &urls {
        fetch_one(
            url,
            state.config.agent_timeout,
            &mut fetched_urls,
            &mut failed_urls,
        )
        .await;
    }

    let attachment_name = match &attachment {
        FactAttachment::None => None,
        FactAttachment::Failed { file_name } => Some(file_name.as_str()),
        FactAttachment::Resolved(resolved) => Some(resolved.file_name.as_str()),
    };
    let debug_prompt = prompt::build_fact_prompt(&raw_text, &fetched_urls, attachment_name);

    let result = prompt::run_fact_prompt(
        state.config.agent_flavor,
        &state.runner,
        &state.config.agent_bin,
        state.config.agent_timeout,
        &raw_text,
        &fetched_urls,
        attachment_name,
    )
    .await;

    let (mut fact_result, tokens) = match result {
        Ok((r, t)) => {
            if t.is_none() {
                tracing::warn!("failed to parse token usage from Claude CLI stderr");
            }
            (r, t)
        }
        Err(err) => {
            tracing::error!(%err, "claude invocation failed for /fact");
            let msg = format!("Fact logging failed: {err}");
            return (msg.clone(), None, false, Some(msg), debug_prompt);
        }
    };

    // Union the user's inline tags with Claude's inferred ones, same as
    // `process_ingest` -- done here in Rust rather than touching the fact
    // prompt, so the strict-JSON fact contract stays untouched.
    fact_result.tags = tags::merge_user_tags(&user_tags, &fact_result.tags);

    let attachment_bytes = match &attachment {
        FactAttachment::Resolved(resolved) => {
            Some((resolved.file_name.as_str(), resolved.bytes.as_slice()))
        }
        FactAttachment::None | FactAttachment::Failed { .. } => None,
    };

    let paths = match bitacora::write_fact(
        &state.config.notes_repo_path,
        Utc::now(),
        &raw_text,
        &fact_result,
        attachment_bytes,
    ) {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(%err, "failed to write fact");
            let msg = format!("Fact logging failed: could not write entry: {err}");
            return (msg.clone(), tokens, false, Some(msg), debug_prompt);
        }
    };

    let push_outcome = match git::commit_and_push(
        &state.config.notes_repo_path,
        &paths.all(),
        &format!("Add fact: {}", fact_result.title),
        state.config.git_timeout,
        state.config.git_push_retries,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::error!(%err, "git commit failed for /fact");
            let msg = format!(
                "Fact written to disk, but commit failed: {err}\nPath: {}",
                paths.note.display()
            );
            return (msg.clone(), tokens, false, Some(msg), debug_prompt);
        }
    };

    let tags_display = fact_result.tags.join(", ");
    let mut confirmation = format!(
        "Fact logged: {}\nValue: {}/5\nTags: [{}]",
        fact_result.title, fact_result.value, tags_display
    );

    match &attachment {
        FactAttachment::None => {}
        FactAttachment::Resolved(resolved) => {
            confirmation.push_str(&format!("\nAttachment: {} (saved)", resolved.file_name));
        }
        FactAttachment::Failed { file_name } => {
            confirmation.push_str(&format!(
                "\nAttachment: {file_name} (download failed, entry saved without it)"
            ));
        }
    }
    if !failed_urls.is_empty() {
        confirmation.push_str(&format!(
            "\n(Note: {} link(s) could not be fetched)",
            failed_urls.len()
        ));
    }
    if let Some(ref t) = tokens {
        confirmation.push_str(&format!("\n\nTokens: {} in, {} out", t.input, t.output));
    }

    state.record_last_note(format!(
        "{}, value {}, tags [{}]",
        fact_result.title, fact_result.value, tags_display
    ));

    let reply = describe_push_outcome(confirmation, push_outcome);

    (reply, tokens, true, None, debug_prompt)
}
