use std::sync::Arc;

use kamaji_core::chat::ChatRef;
use kamaji_core::queue::{CommandAttachment, Job, JobKind};
use kamaji_core::worker::{process_job, FactAttachment};
use kamaji_core::{history, state::AppState};

use crate::attachment;
use crate::error::AttachmentError;
use crate::state::DaemonState;
use crate::transport::send_reply;

/// Sequential worker loop: dequeue, resolve any `/fact` attachment (the one
/// piece of job processing that needs a platform client, so it can't live
/// in core), process, reply, complete. Runs forever until the process
/// exits. A job-processing error (Claude failure, git failure, malformed
/// payload) logs and skips that job rather than crashing the daemon, per
/// the no-panic-after-startup convention.
pub async fn run(state: Arc<DaemonState>) {
    tracing::info!("worker loop started");
    loop {
        match state.core.queue.dequeue() {
            Ok(Some((job_id, job))) => {
                tracing::info!(job_id, chat = %job.chat, "processing job");
                let fact_attachment = resolve_fact_attachment(&state, &job).await;
                let (reply, job_result) =
                    process_job(&state.core, job_id, &job, fact_attachment).await;
                send_reply(&state, &job.chat, &job.reply_to, &reply).await;

                if let Err(err) = history::log_job(&state.core.queue.db, &job_result) {
                    tracing::error!(%err, job_id, "failed to write job history");
                }

                if let Err(err) = state.core.queue.complete(job_id) {
                    tracing::error!(%err, job_id, "failed to mark job complete");
                }
            }
            Ok(None) => {
                tokio::time::sleep(state.core.config.worker_poll_interval).await;
            }
            Err(err) => {
                tracing::error!(%err, "dequeue failed, retrying after poll interval");
                tokio::time::sleep(state.core.config.worker_poll_interval).await;
            }
        }
    }
}

/// Resolves a `/fact` job's attachment (if any) into the outcome
/// `kamaji_core::worker::process_job` expects. Every other job kind (and
/// every `/fact` job with no attachment at all -- true for anything
/// originated by the `kamaji` CLI today) is a no-op returning
/// `FactAttachment::None`.
async fn resolve_fact_attachment(state: &Arc<DaemonState>, job: &Job) -> FactAttachment {
    let JobKind::Command {
        name,
        attachment: Some(a),
        ..
    } = &job.kind
    else {
        return FactAttachment::None;
    };
    if name != "fact" {
        return FactAttachment::None;
    }

    match download_fact_attachment(state, &job.chat, a).await {
        Ok(resolved) => FactAttachment::Resolved(resolved),
        Err(err) => {
            tracing::warn!(%err, file_name = a.file_name, "failed to download /fact attachment, continuing without it");
            FactAttachment::Failed {
                file_name: a.file_name.clone(),
            }
        }
    }
}

/// Dispatches an attachment download to whichever platform the job came
/// from -- Telegram's `file_id` needs the two-step `getFile`-then-download
/// dance, Matrix's `file_id` is already an `mxc://` URI fetched in one call.
/// A CLI- or REST-originated job never reaches here: `kamaji fact` doesn't
/// populate `attachment` yet (see the workspace-split plan's "out of scope"
/// notes), over either transport.
async fn download_fact_attachment(
    state: &DaemonState,
    chat: &ChatRef,
    a: &CommandAttachment,
) -> Result<kamaji_core::attachment::ResolvedAttachment, AttachmentError> {
    let AppState { config, .. } = &*state.core;
    match chat {
        ChatRef::Telegram { .. } => {
            let telegram = state
                .telegram
                .as_ref()
                .ok_or(AttachmentError::ClientNotConfigured)?;
            attachment::download_attachment(
                &telegram.bot,
                &a.file_id,
                &a.file_name,
                config.telegram_file_timeout,
                config.max_attachment_bytes,
            )
            .await
        }
        ChatRef::Matrix { .. } => {
            let matrix = state
                .matrix
                .as_ref()
                .ok_or(AttachmentError::ClientNotConfigured)?;
            attachment::download_matrix_attachment(
                &matrix.client,
                &a.file_id,
                &a.file_name,
                config.matrix_media_timeout,
                config.max_attachment_bytes,
            )
            .await
        }
        ChatRef::Cli { .. } | ChatRef::Rest { .. } => Err(AttachmentError::ClientNotConfigured),
    }
}
