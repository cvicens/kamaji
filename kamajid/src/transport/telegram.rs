use std::sync::Arc;

use futures::StreamExt;
use kamaji_core::chat::{ChatRef, MessageRef};
use kamaji_core::queue::CommandAttachment;
use kamaji_core::routing::route_message;
use teloxide::prelude::*;
use teloxide::types::UpdateKind;
use teloxide::update_listeners::AsUpdateStream;

use crate::state::DaemonState;
use crate::transport::dispatch_routed_job;

/// Long-polls Telegram for updates and routes each one. No-ops if Telegram
/// isn't configured, so `main.rs` can spawn this unconditionally alongside
/// `transport::matrix::run` and `transport::socket::run`. Otherwise runs
/// forever until the process exits.
///
/// The outer loop rebuilds the long-poll listener from scratch on any
/// stall. teloxide's own backoff only covers requests that resolve to an
/// `Err`; a connection left half-open (e.g. across a laptop sleep/wake or a
/// network drop that produces neither data nor a socket error) can leave
/// `stream.next()` pending forever, which no amount of internal retry logic
/// will ever observe. `poll_watchdog_timeout` bounds that wait, and
/// reconnecting from offset 0 is safe because `seen_updates` in redb
/// already dedupes replayed updates.
pub async fn run(state: Arc<DaemonState>) {
    let Some(telegram) = &state.telegram else {
        return;
    };
    let bot = telegram.bot.clone();
    let poll_watchdog_timeout = state.core.config.poll_watchdog_timeout;

    loop {
        let mut listener = teloxide::update_listeners::polling_default(bot.clone()).await;
        let stream = listener.as_stream();
        futures::pin_mut!(stream);

        loop {
            let update = match tokio::time::timeout(poll_watchdog_timeout, stream.next()).await {
                Ok(Some(Ok(upd))) => upd,
                Ok(Some(Err(err))) => {
                    tracing::error!(%err, "update listener error");
                    continue;
                }
                Ok(None) => {
                    tracing::warn!("update stream ended, reconnecting to telegram");
                    break;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = poll_watchdog_timeout.as_secs(),
                        "no telegram activity within watchdog window, reconnecting"
                    );
                    break;
                }
            };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                handle_update(update, state).await;
            });
        }
    }
}

/// The Telegram message handler. Critical guardrail: this MUST filter the
/// bot's own messages FIRST, before any other routing logic, to prevent the
/// infinite-loop scenario where the bot's reply re-triggers ingestion.
pub async fn handle_update(upd: Update, state: Arc<DaemonState>) {
    // Extract the message from the update, or drop the update if it's not a
    // message (we only process messages for v1, no inline queries, etc.).
    let msg = match upd.kind {
        UpdateKind::Message(m) => m,
        UpdateKind::ChannelPost(m) => m,
        _ => return,
    };

    let Some(telegram) = &state.telegram else {
        return;
    };
    let Some(telegram_config) = &state.core.config.telegram else {
        return;
    };

    // Bot-self filter (the load-bearing guardrail): drop any message from the
    // bot itself, because with no-command-prefix -> ingest as the default
    // path, a bot reply landing back in the trigger chat would be re-ingested
    // as a new note, which replies again -> infinite loop + infinite commits.
    let Some(from) = msg.from.as_ref() else {
        return;
    };
    if from.id == telegram.bot_id {
        return;
    }

    // Allow-list: silently drop messages from chats not on the list.
    if !is_allowed_telegram_chat(msg.chat.id.0, &telegram_config.allowed_chats) {
        tracing::debug!(chat_id = %msg.chat.id, "dropped message from unlisted chat");
        return;
    }

    // Dedupe: if we've already seen this update_id (e.g. on restart replay),
    // skip it.
    let is_new = match state.core.queue.mark_seen(upd.id.0 as i64) {
        Ok(is_new) => is_new,
        Err(err) => {
            tracing::error!(%err, update_id = upd.id.0, "failed to mark update as seen");
            return;
        }
    };
    if !is_new {
        tracing::debug!(
            update_id = upd.id.0,
            "dedupe: already processed this update"
        );
        return;
    }

    // Text extraction: a plain text message, or the caption on a document
    // (that's how `/fact <description>` reaches us when sent together with
    // an attached file -- Telegram puts the caption in `.caption()`, not
    // `.text()`, for a message that also carries a document). A document
    // with no caption at all carries no command and no ingest text, so
    // there's nothing routable in it; it's silently dropped for v1, same as
    // any other non-text message (photos with no caption, stickers, etc.).
    let document = msg.document();
    let Some(text) = msg.text().or_else(|| msg.caption()) else {
        tracing::warn!(chat_id = %msg.chat.id, message_id = msg.id.0, has_attachment = document.is_some(), "dropped message with no text/caption");
        return;
    };

    let attachment = document.map(command_attachment_from_document);
    let chat = ChatRef::Telegram {
        chat_id: msg.chat.id.0,
    };
    let reply_to = MessageRef::Telegram {
        message_id: msg.id.0,
    };

    let job = route_message(text, attachment, chat.clone(), reply_to.clone());
    dispatch_routed_job(&state, chat, reply_to, job).await;
}

/// The Telegram-side allow-list guardrail, mirroring
/// `matrix::is_allowed_matrix_room`, factored out for the same
/// unit-testability reason.
fn is_allowed_telegram_chat(chat_id: i64, allowed: &[i64]) -> bool {
    allowed.contains(&chat_id)
}

fn command_attachment_from_document(doc: &teloxide::types::Document) -> CommandAttachment {
    CommandAttachment {
        file_id: doc.file.id.0.clone(),
        file_name: doc
            .file_name
            .clone()
            .unwrap_or_else(|| "attachment".to_string()),
        mime_type: doc.mime_type.as_ref().map(|m| m.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kamaji_core::queue::JobKind;
    use serde_json::json;

    fn sample_update(from_id: u64, text: &str) -> Update {
        let json = json!({
            "update_id": 1,
            "message": {
                "message_id": 123,
                "date": 1625000000,
                "chat": {"id": 1, "type": "private"},
                "from": {"id": from_id, "is_bot": false, "first_name": "Test"},
                "text": text,
            }
        });
        serde_json::from_value(json).unwrap()
    }

    fn sample_document(file_name: &str) -> teloxide::types::Document {
        use teloxide::types::{FileId, FileMeta, FileUniqueId};
        teloxide::types::Document {
            file: FileMeta {
                id: FileId("file-id-123".to_string()),
                unique_id: FileUniqueId("unique-123".to_string()),
                size: 42,
            },
            thumbnail: None,
            file_name: Some(file_name.to_string()),
            mime_type: None,
        }
    }

    #[test]
    fn allow_list_drops_unlisted_chats() {
        let allowed = vec![111, 222];
        assert!(is_allowed_telegram_chat(111, &allowed));
        assert!(!is_allowed_telegram_chat(333, &allowed));
    }

    #[test]
    fn command_attachment_from_document_carries_file_metadata() {
        let doc = sample_document("report.pdf");
        let attachment = command_attachment_from_document(&doc);
        assert_eq!(attachment.file_id, "file-id-123");
        assert_eq!(attachment.file_name, "report.pdf");
    }

    #[test]
    fn bot_self_filter_test() {
        // This test simulates the critical guardrail: a message from the bot
        // itself (bot_id == from.id) must be dropped before any routing logic.
        // We can't easily test the full async handler without spinning up a
        // real tokio runtime and mocking the queue, but we can at least
        // deserialize an Update with the bot as sender and verify route_message
        // would see it as an ingest job (no leading `/`), which is what makes
        // the bot-self filter load-bearing: without it, the bot's own reply
        // "Note written: title, importance 4, tags [...]" would be re-ingested.
        let update = sample_update(999, "Note written: title, importance 4, tags [a, b]");
        if let UpdateKind::Message(msg) = update.kind {
            assert_eq!(msg.from.as_ref().unwrap().id.0, 999);
            let job = route_message(
                msg.text().unwrap(),
                None,
                ChatRef::Telegram {
                    chat_id: msg.chat.id.0,
                },
                MessageRef::Telegram {
                    message_id: msg.id.0,
                },
            );
            // If bot_id were 999, this job would be an infinite-loop trigger.
            match job.kind {
                JobKind::Ingest { .. } => {} // expected
                _ => panic!("bot reply should route as Ingest if not filtered"),
            }
        }
    }
}
