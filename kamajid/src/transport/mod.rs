pub mod matrix;
pub mod rest;
pub mod socket;
pub mod telegram;

use std::sync::Arc;
use std::time::Duration;

use kamaji_core::chat::{ChatRef, MessageRef};
use kamaji_core::commands::{self, CommandMode};
use kamaji_core::ipc::{CliRequest, CliResponse};
use kamaji_core::queue::{Job, JobKind};
use kamaji_core::{demonstrate, goal, todo, worker};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::RoomId;
use teloxide::prelude::*;
use teloxide::types::{ChatId, MessageId, ReplyParameters};

use crate::state::DaemonState;

/// The single reply chokepoint for every transport. Previously there were
/// two independent places that sent a reply; collapsing them here means
/// every reply -- worker-processed job or an immediate usage/unknown-command
/// error -- goes through one function that knows how to reach whichever
/// origin the job came from, including a `kamaji` CLI connection waiting on
/// `state.waiters` (over either the Unix socket or the REST API).
pub async fn send_reply(state: &DaemonState, chat: &ChatRef, reply_to: &MessageRef, text: &str) {
    match chat {
        ChatRef::Telegram { chat_id } => send_telegram_reply(state, *chat_id, reply_to, text).await,
        ChatRef::Matrix { room_id } => send_matrix_reply(state, room_id, text).await,
        ChatRef::Cli { request_id } => state.waiters.deliver(*request_id, text.to_string()),
        ChatRef::Rest { request_id } => state.waiters.deliver(*request_id, text.to_string()),
    }
}

async fn send_telegram_reply(state: &DaemonState, chat_id: i64, reply_to: &MessageRef, text: &str) {
    let Some(telegram) = &state.telegram else {
        tracing::error!(
            chat_id,
            "telegram reply requested but telegram is not configured"
        );
        return;
    };
    let MessageRef::Telegram { message_id } = reply_to else {
        tracing::error!(
            chat_id,
            "telegram chat paired with a non-telegram message ref"
        );
        return;
    };
    let chat_id = ChatId(chat_id);
    let reply_params = ReplyParameters::new(MessageId(*message_id));
    let req = telegram
        .bot
        .send_message(chat_id, text)
        .reply_parameters(reply_params);
    if let Err(err) = req.await {
        tracing::error!(%err, chat_id = %chat_id, message_id, "failed to send telegram reply");
    }
}

/// Unlike Telegram, this doesn't thread the reply to the originating event
/// (no `m.in_reply_to` relation) -- doing that properly per the Matrix rich
/// reply spec needs the original message's sender alongside its event id,
/// which `MessageRef::Matrix` doesn't carry, and a plain message in the room
/// already satisfies every functional requirement of the pipeline. Visual
/// threading is a follow-up nicety, not wired up in this pass.
async fn send_matrix_reply(state: &DaemonState, room_id: &str, text: &str) {
    let Some(matrix) = &state.matrix else {
        tracing::error!(
            room_id,
            "matrix reply requested but matrix is not configured"
        );
        return;
    };
    let room_id = match RoomId::parse(room_id) {
        Ok(id) => id,
        Err(err) => {
            tracing::error!(%err, room_id, "invalid matrix room id");
            return;
        }
    };
    let Some(room) = matrix.client.get_room(&room_id) else {
        tracing::error!(%room_id, "matrix room not found (bot not joined?)");
        return;
    };
    if let Err(err) = room.send(RoomMessageEventContent::text_plain(text)).await {
        tracing::error!(%err, %room_id, "failed to send matrix reply");
    }
}

/// Shared, transport-agnostic tail of every transport's receive path
/// (`transport::telegram::handle_update`, `transport::matrix::handle_message`,
/// `transport::socket::handle_connection`): interprets an already-routed
/// `Job` according to its command's registered `CommandMode` (see
/// `commands::CommandMode`). Unknown commands get an immediate error reply
/// and are never enqueued; `Sync` commands run right here, bypassing the
/// queue and job_history entirely; `Queued` commands (and all ingest jobs)
/// fall through to the shared queue, processed one at a time by the single
/// worker. Keeping this in one place -- rather than duplicated per
/// transport -- is what keeps the routing rule (no leading `/` -> ingest,
/// unrecognized command -> error, never silent ingestion) identical
/// regardless of where the message came from.
pub async fn dispatch_routed_job(
    state: &Arc<DaemonState>,
    chat: ChatRef,
    reply_to: MessageRef,
    job: Job,
) {
    if let JobKind::Command {
        ref name,
        ref args,
        ref attachment,
    } = job.kind
    {
        match commands::mode(name) {
            None => {
                let reply = commands::unknown_command_reply(name);
                send_reply(state, &chat, &reply_to, &reply).await;
                return;
            }
            Some(CommandMode::Sync) => {
                let reply = worker::dispatch_sync_command(&state.core, name, args).await;
                send_reply(state, &chat, &reply_to, &reply).await;
                return;
            }
            Some(CommandMode::Queued) => {
                // `/ingest` with no argument is a usage error, not an empty
                // job -- reply immediately and skip the queue entirely,
                // mirroring the unknown-command reply just above.
                if name == "ingest" && args.is_empty() {
                    send_reply(state, &chat, &reply_to, commands::INGEST_USAGE).await;
                    return;
                }
                // `/fact` needs either a description or an attachment --
                // neither means there's nothing to log, mirroring the same
                // "usage error, don't enqueue" treatment as `/ingest` above.
                if name == "fact" && args.is_empty() && attachment.is_none() {
                    send_reply(state, &chat, &reply_to, commands::FACT_USAGE).await;
                    return;
                }
                // `/todo` has three subcommands (add/list/resolve) each with
                // their own usage shape, so validation is delegated to
                // `todo::parse_command` rather than a single flag check like
                // `/ingest`/`/fact` above -- same "usage error, don't
                // enqueue" treatment either way.
                if name == "todo" {
                    if let Err(usage) = todo::parse_command(args) {
                        send_reply(state, &chat, &reply_to, &usage).await;
                        return;
                    }
                }
                // `/goal` mirrors `/todo`: same subcommand shape (add/list/
                // achieve), same "usage error, don't enqueue" treatment,
                // delegated to `goal::parse_command` rather than a single
                // flag check like `/ingest`/`/fact` above.
                if name == "goal" {
                    if let Err(usage) = goal::parse_command(args) {
                        send_reply(state, &chat, &reply_to, &usage).await;
                        return;
                    }
                }
                // `/demonstrate` mirrors `/todo`/`/goal`: its one optional
                // arg (scope: `all`/`YYYY-Q#`) is validated via
                // `demonstrate::parse_scope` before enqueueing, same
                // "usage error, don't enqueue" treatment.
                if name == "demonstrate" {
                    if let Err(usage) = demonstrate::parse_scope(args) {
                        send_reply(state, &chat, &reply_to, &usage).await;
                        return;
                    }
                }
            }
        }
    }

    match state.core.queue.enqueue(&job) {
        Ok(job_id) => {
            tracing::info!(job_id, chat = %chat, "enqueued job");
        }
        Err(err) => {
            tracing::error!(%err, "failed to enqueue job");
        }
    }
}

/// Shared tail for both `kamaji` CLI transports (the Unix socket in
/// `transport::socket` and the REST API in `transport::rest`): translate a
/// `CliRequest` to the same `(name, args)` shape a `/command` produces,
/// register a waiter, hand it to `dispatch_routed_job`, and wait for the
/// reply (bounded by a timeout). `make_refs` supplies the platform-tagged
/// `ChatRef`/`MessageRef` pair for the given `request_id` -- `ChatRef::Cli`
/// for the socket, `ChatRef::Rest` for the REST API -- since the two
/// transports are worth distinguishing in logs even though the dispatch
/// logic itself is identical. `ok` reflects whether this round trip
/// completed -- not whether the underlying command itself succeeded, which
/// is already conveyed in `text` (e.g. "Fact logging failed: ..." is still
/// `ok: true`, since kamajid did answer); `ok: false` means the caller never
/// got a real answer at all.
pub async fn run_cli_style_request(
    state: &Arc<DaemonState>,
    request: CliRequest,
    make_refs: impl FnOnce(u64) -> (ChatRef, MessageRef),
) -> CliResponse {
    let (name, args) = request.into_command();
    let (request_id, rx) = state.waiters.register();
    let (chat, reply_to) = make_refs(request_id);
    let job = Job {
        chat: chat.clone(),
        reply_to: reply_to.clone(),
        kind: JobKind::Command {
            name: name.to_string(),
            args,
            attachment: None,
        },
    };

    dispatch_routed_job(state, chat, reply_to, job).await;

    let timeout =
        state.core.config.agent_timeout + state.core.config.git_timeout + Duration::from_secs(30);
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(text)) => CliResponse { ok: true, text },
        Ok(Err(_)) => CliResponse {
            ok: false,
            text: "kamajid closed the reply channel before answering".to_string(),
        },
        Err(_) => CliResponse {
            ok: false,
            text: "timed out waiting for kamajid to process the request".to_string(),
        },
    }
}
