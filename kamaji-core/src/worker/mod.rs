use std::time::Duration;

use crate::attachment::ResolvedAttachment;
use crate::commands;
use crate::fetch;
use crate::git::PushOutcome;
use crate::history::{JobHistoryRecord, JobKindSummary, JobStatus};
use crate::prompt::FetchedContent;
use crate::queue::{ChecklistDomain, Job, JobKind};
use crate::state::AppState;

mod align_job;
mod checklist_api_job;
mod demonstrate_job;
mod fact_api_job;
mod fact_job;
mod goal_job;
mod ingest_job;
mod todo_job;

/// Appends a note about the git push outcome to a confirmation message, if
/// there's anything to say -- shared by every command that writes to the
/// notes repo (`/ingest`, `/fact`, `/todo add|resolve`, `/goal add|achieve`).
fn describe_push_outcome(confirmation: String, push_outcome: PushOutcome) -> String {
    match push_outcome {
        PushOutcome::Pushed => confirmation,
        PushOutcome::PushedAfterRebase => {
            format!("{confirmation}\n\n(Note: remote had new commits; rebased and pushed)")
        }
        PushOutcome::CommittedNotPushed { reason } => {
            format!("{confirmation}\n\n(Note: {reason})")
        }
    }
}

/// The outcome of attempting to resolve a `/fact` command's attachment (if
/// the original request had one at all). Computed by the caller *before*
/// calling `process_job` -- core has no platform client (Telegram/Matrix)
/// to do a `file_id`/`mxc://` download itself, and the `kamaji` CLI has no
/// attachment support yet (see the workspace-split plan's "out of scope"
/// notes), so this is always constructed by `kamajid`'s worker loop.
pub enum FactAttachment {
    /// The request had no attachment.
    None,
    /// The request had an attachment named `file_name`, but the caller
    /// couldn't resolve it to bytes (network/timeout/size-cap failure) --
    /// still worth mentioning in the confirmation, just not saved.
    Failed { file_name: String },
    /// The request had an attachment and it resolved successfully.
    Resolved(ResolvedAttachment),
}

/// Processes one dequeued job and returns the reply text plus the record to
/// write to `job_history`. Pure with respect to transports: the caller
/// (`kamajid`'s worker loop) is responsible for delivering `reply` to
/// wherever `job.chat` points, and for resolving any `/fact` attachment
/// into `fact_attachment` beforehand. A job-processing error (Claude
/// failure, git failure, malformed payload) is captured in the returned
/// `JobHistoryRecord` rather than propagated, per the no-panic-after-startup
/// convention -- one bad job must never take the daemon down.
pub async fn process_job(
    state: &AppState,
    job_id: u64,
    job: &Job,
    fact_attachment: FactAttachment,
) -> (String, JobHistoryRecord) {
    let (reply, tokens, status, error, prompt) = match &job.kind {
        JobKind::Ingest { raw_text, urls } => {
            let (reply, tokens, is_success, err_msg, debug_prompt) =
                ingest_job::process_ingest(state, raw_text, urls).await;
            (
                reply,
                tokens,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                err_msg,
                debug_prompt,
            )
        }
        // `ingest` is special-cased ahead of the generic dispatch below: like
        // `JobKind::Ingest`, it invokes Claude (and, on the link path, the
        // notes git repo), so it needs token tracking and a real
        // success/failure status rather than the always-Success, no-tokens
        // shape every other (Claude-free) command gets.
        JobKind::Command { name, args, .. } if name == "ingest" => {
            let (reply, tokens, is_success, err_msg, debug_prompt) =
                ingest_job::process_ingest_command(state, args).await;
            (
                reply,
                tokens,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                err_msg,
                debug_prompt,
            )
        }
        // `fact` is special-cased the same way as `ingest`: it invokes
        // Claude and always touches the notes git repo (unlike `ingest`,
        // there's no agent-passthrough branch -- every `/fact` call writes
        // a bitacora entry), so it needs the same token-tracking and
        // real-status treatment.
        JobKind::Command { name, args, .. } if name == "fact" => {
            let (reply, tokens, is_success, err_msg, debug_prompt) =
                fact_job::process_fact_command(state, args, fact_attachment).await;
            (
                reply,
                tokens,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                err_msg,
                debug_prompt,
            )
        }
        // `todo` is special-cased the same way as `ingest`/`fact`: it touches
        // the notes git repo (via commit+push on add/resolve), so it needs a
        // real success/failure status rather than the always-Success generic
        // dispatch path below. Unlike `ingest`/`fact` it never invokes
        // Claude, so there are no tokens to track.
        JobKind::Command { name, args, .. } if name == "todo" => {
            let (reply, is_success, err_msg, debug_prompt) =
                todo_job::process_todo_command(state, &job.chat, args).await;
            (
                reply,
                None,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                err_msg,
                debug_prompt,
            )
        }
        // `goal` is special-cased the same way as `todo`: it touches the
        // notes git repo (via commit+push on add/achieve), so it needs a
        // real success/failure status rather than the always-Success
        // generic dispatch path below. Never invokes Claude, so no tokens.
        JobKind::Command { name, args, .. } if name == "goal" => {
            let (reply, is_success, err_msg, debug_prompt) =
                goal_job::process_goal_command(state, &job.chat, args).await;
            (
                reply,
                None,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                err_msg,
                debug_prompt,
            )
        }
        // `align` is special-cased the same way as `todo`/`goal`: since it
        // auto-links TODOs to matching goals, it touches the notes git repo
        // (via a batched commit+push when it finds anything new to link),
        // so it needs a real success/failure status rather than the
        // always-Success generic dispatch path below. Never invokes Claude,
        // so no tokens.
        JobKind::Command { name, .. } if name == "align" => {
            let (reply, is_success, err_msg, debug_prompt) =
                align_job::process_align_command(state).await;
            (
                reply,
                None,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                err_msg,
                debug_prompt,
            )
        }
        // `demonstrate` is special-cased the same way as `ingest`/`fact`
        // (not `align`/`todo`/`goal`): it touches the notes git repo (via a
        // batched commit+push when it finds anything new to link), *and*,
        // unlike `align`, it can invoke Claude (the semantic-match pass,
        // on by default -- see `Config::demonstrate_semantic_match`), so it
        // needs real token tracking alongside the real success/failure
        // status.
        JobKind::Command { name, args, .. } if name == "demonstrate" => {
            let (reply, tokens, is_success, err_msg, debug_prompt) =
                demonstrate_job::process_demonstrate_command(state, args).await;
            (
                reply,
                tokens,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                err_msg,
                debug_prompt,
            )
        }
        JobKind::Command { name, args, .. } => {
            // Only `CommandMode::Queued` commands are ever enqueued (see
            // `kamajid::transport::dispatch_routed_job`), so this only runs
            // for commands that are meant to be recorded in job_history.
            let reply = commands::dispatch(name, args, state).await;
            // Commands don't use Claude, so no tokens to track
            (
                reply,
                None,
                JobStatus::Success,
                None,
                command_prompt(name, args),
            )
        }
        // The web UI's structured checklist API (see `queue::ChecklistApiOp`'s
        // doc comment) -- never invokes Claude, so no tokens; the reply is a
        // JSON string rather than chat-formatted text, and there's no
        // `command_prompt`-shaped debug entry since it didn't come from a
        // `/command` line, so the op's `Debug` form stands in for one.
        //
        // The legacy `TodoApi` variant only ever arrives from a payload
        // queued before goals shared this path, so it can only have meant the
        // todo domain.
        JobKind::TodoApi(op) => {
            let (reply, is_success) =
                checklist_api_job::process(state, ChecklistDomain::Todo, op).await;
            (
                reply,
                None,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                None,
                format!("{op:?}"),
            )
        }
        JobKind::ChecklistApi { domain, op } => {
            let (reply, is_success) = checklist_api_job::process(state, *domain, op).await;
            (
                reply,
                None,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                None,
                format!("{domain:?} {op:?}"),
            )
        }
        // The web UI's structured fact API (see `queue::FactApiOp`) -- same
        // shape as the checklist API above, different domain.
        JobKind::FactApi(op) => {
            let (reply, is_success) = fact_api_job::process(state, op).await;
            (
                reply,
                None,
                if is_success {
                    JobStatus::Success
                } else {
                    JobStatus::Failed
                },
                None,
                format!("{op:?}"),
            )
        }
    };

    let kind_summary = match &job.kind {
        JobKind::Ingest { .. } => JobKindSummary::Ingest,
        JobKind::Command { name, .. } => JobKindSummary::Command { name: name.clone() },
        JobKind::TodoApi(_) => JobKindSummary::Command {
            name: "todo_api".to_string(),
        },
        JobKind::ChecklistApi { domain, .. } => JobKindSummary::Command {
            name: format!("{}_api", domain_name(*domain)),
        },
        JobKind::FactApi(_) => JobKindSummary::Command {
            name: "fact_api".to_string(),
        },
    };

    // Debug-only: record what triggered this job (the full Claude prompt --
    // raw text plus any fetched URL content -- for an ingest, the command
    // line for a /command) alongside the reply it produced. Gated on DEBUG
    // so normal operation never writes this file.
    if let Err(err) = crate::debug_log::log_job(
        &state.config.debug_log_path,
        state.config.debug,
        job_id,
        &prompt,
        &reply,
    ) {
        tracing::error!(%err, job_id, "failed to write debug log entry");
    }

    let record = match status {
        JobStatus::Success => JobHistoryRecord::new_success(job_id, kind_summary, tokens),
        JobStatus::Failed => {
            JobHistoryRecord::new_failure(job_id, kind_summary, error.unwrap_or_default(), tokens)
        }
    };

    (reply, record)
}

/// Runs a `CommandMode::Sync` command's dispatch + debug logging. Pulled out
/// as a small transport-agnostic helper so `kamajid::transport`'s
/// `run_sync_command` (which also needs to deliver the reply, a daemon-only
/// concern) doesn't duplicate the debug-log-then-dispatch shape used by
/// `process_job` above. A job_id is still minted (via `Queue::next_id`,
/// which writes nothing) purely so the debug log entry has the same shape
/// as a queued job's; `Sync` commands are deliberately never recorded in
/// `job_history` -- that table exists to audit ingest/queued work, and a
/// durable record for every `/status` call would just be noise.
pub async fn dispatch_sync_command(state: &AppState, name: &str, args: &[String]) -> String {
    let job_id = state.queue.next_id();
    let reply = commands::dispatch(name, args, state).await;

    if let Err(err) = crate::debug_log::log_job(
        &state.config.debug_log_path,
        state.config.debug,
        job_id,
        &command_prompt(name, args),
        &reply,
    ) {
        tracing::error!(%err, job_id, "failed to write debug log entry");
    }

    reply
}

/// `job_history`'s name for a web checklist write, kept domain-specific
/// ("todo_api"/"goal_api") so `/history` reads the same way it did before
/// goals shared this path.
fn domain_name(domain: ChecklistDomain) -> &'static str {
    match domain {
        ChecklistDomain::Todo => "todo",
        ChecklistDomain::Goal => "goal",
    }
}

fn command_prompt(name: &str, args: &[String]) -> String {
    if args.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {}", args.join(" "))
    }
}

/// Fetches one URL and records the outcome: successful or auth-required
/// content goes to `fetched_urls` (both are useful context for Claude, the
/// auth-required case with a placeholder explaining why); a genuine fetch
/// error goes to `failed_urls` instead of a placeholder, since there's
/// nothing there worth spending a Claude call on. Shared between level-0
/// fetches (the message's own links, in `ingest_job`/`fact_job`) and
/// level-1 fetches (links found inside level-0 content, in `ingest_job`
/// only) so both go through identical handling.
async fn fetch_one(
    url: &str,
    timeout: Duration,
    fetched_urls: &mut Vec<FetchedContent>,
    failed_urls: &mut Vec<(String, String)>,
) {
    match fetch::fetch_url_content(url, timeout).await {
        Ok(content) => {
            tracing::info!(url, "successfully fetched URL content");
            fetched_urls.push(FetchedContent {
                url: url.to_string(),
                text: content,
            });
        }
        Err(fetch::FetchError::RequiresAuth) => {
            tracing::info!(
                url,
                "URL requires authentication, creating note with URL reference"
            );
            // For auth-required sites, provide context about what it is
            let placeholder = if url.contains("x.com") || url.contains("twitter.com") {
                format!("[X/Twitter post: {}]\nNote: Content requires authentication and could not be fetched automatically. The URL has been preserved for manual review.", url)
            } else if url.contains("instagram.com") {
                format!("[Instagram content: {}]\nNote: Content requires authentication and could not be fetched automatically.", url)
            } else {
                format!("[Authentication-required content: {}]", url)
            };
            fetched_urls.push(FetchedContent {
                url: url.to_string(),
                text: placeholder,
            });
        }
        Err(err) => {
            tracing::warn!(%err, url, "failed to fetch URL");
            failed_urls.push((url.to_string(), err.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_prompt_without_args() {
        assert_eq!(command_prompt("status", &[]), "/status");
    }

    #[test]
    fn command_prompt_with_args() {
        let args = vec!["10".to_string()];
        assert_eq!(command_prompt("history", &args), "/history 10");
    }
}
