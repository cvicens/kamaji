use std::sync::Arc;

use kamaji_core::chat::{ChatRef, MessageRef};
use kamaji_core::queue::CommandAttachment;
use kamaji_core::routing::route_message;
use matrix_sdk::room::Room;
use matrix_sdk::ruma::events::room::member::{MembershipState, StrippedRoomMemberEvent};
use matrix_sdk::ruma::events::room::message::{MessageType, SyncRoomMessageEvent};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::UserId;

use crate::state::DaemonState;
use crate::transport::dispatch_routed_job;

/// Registers the message/invite event handlers and runs the sync loop until
/// it exits for good. No-ops if Matrix isn't configured, so `main.rs` can
/// spawn this unconditionally alongside `transport::telegram::run` and
/// `transport::socket::run`. matrix-sdk's `sync` already retries internally
/// on transient errors; if it returns at all, something ended the loop for
/// good (e.g. an unrecoverable auth failure), which is worth logging loudly
/// since it silently stops all Matrix routing.
pub async fn run(state: Arc<DaemonState>) {
    let Some(matrix) = &state.matrix else {
        return;
    };
    let client = matrix.client.clone();

    let message_state = Arc::clone(&state);
    client.add_event_handler(move |ev, room| {
        let state = Arc::clone(&message_state);
        async move {
            handle_message(ev, room, state).await;
        }
    });

    let invite_state = Arc::clone(&state);
    client.add_event_handler(move |ev, room| {
        let state = Arc::clone(&invite_state);
        async move {
            handle_invite(ev, room, state).await;
        }
    });

    if let Err(err) = client.sync(matrix_sdk::config::SyncSettings::new()).await {
        tracing::error!(%err, "matrix sync loop exited");
    }
}

/// The Matrix event handler. Runs the same guardrail chain as
/// `transport::telegram::handle_update`, in the same order (self-filter,
/// then allow-list, then dedupe) -- per CLAUDE.md, the bot-self-filter must
/// come first on every platform, not just Telegram.
pub async fn handle_message(event: SyncRoomMessageEvent, room: Room, state: Arc<DaemonState>) {
    let Some(matrix) = &state.matrix else {
        return;
    };
    let Some(matrix_config) = &state.core.config.matrix else {
        return;
    };

    let (sender, event_id) = match &event {
        SyncRoomMessageEvent::Original(e) => (e.sender.clone(), e.event_id.clone()),
        SyncRoomMessageEvent::Redacted(e) => (e.sender.clone(), e.event_id.clone()),
    };

    if is_matrix_self(&sender, &matrix.user_id) {
        return;
    }

    let room_id = room.room_id().to_string();
    if !is_allowed_matrix_room(&room_id, &matrix_config.allowed_rooms) {
        tracing::debug!(room_id, "dropped matrix message from unlisted room");
        return;
    }

    match state.core.queue.mark_seen_matrix(event_id.as_str()) {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                event_id = event_id.as_str(),
                "dedupe: already processed this matrix event"
            );
            return;
        }
        Err(err) => {
            tracing::error!(%err, event_id = event_id.as_str(), "failed to mark matrix event as seen");
            return;
        }
    }

    // Redacted events carry no routable content (body/attachment are gone),
    // same treatment as a Telegram message with no text/caption.
    let SyncRoomMessageEvent::Original(original) = event else {
        tracing::debug!(room_id, "dropped redacted matrix event");
        return;
    };

    let (text, attachment) = match &original.content.msgtype {
        MessageType::Text(t) => (t.body.clone(), None),
        MessageType::File(f) => {
            let file_name = f.filename().to_string();
            let attachment = match &f.source {
                MediaSource::Plain(mxc) => Some(CommandAttachment {
                    file_id: mxc.to_string(),
                    file_name: file_name.clone(),
                    mime_type: f.info.as_deref().and_then(|i| i.mimetype.clone()),
                }),
                MediaSource::Encrypted(_) => {
                    // E2EE room attachments aren't supported yet (out of
                    // scope until the bot is actually put in an encrypted
                    // room) -- degrade to filing without the attachment
                    // rather than dropping the whole message.
                    tracing::warn!(
                        room_id,
                        "encrypted matrix attachments are not supported yet, dropping attachment"
                    );
                    None
                }
            };
            (f.body.clone(), attachment)
        }
        _ => {
            tracing::debug!(room_id, "dropped matrix message of unroutable type");
            return;
        }
    };

    let chat = ChatRef::Matrix {
        room_id: room_id.clone(),
    };
    let reply_to = MessageRef::Matrix {
        event_id: event_id.to_string(),
    };

    let job = route_message(&text, attachment, chat.clone(), reply_to.clone());
    dispatch_routed_job(&state, chat, reply_to, job).await;
}

/// Matrix invites don't auto-accept -- a room invite arrives as a stripped
/// `m.room.member` state event, and the bot stays in the "invited" state
/// (seeing no timeline events at all for that room) until it explicitly
/// calls `Room::join`. Only auto-joins rooms already on
/// `ALLOWED_MATRIX_ROOMS` -- joining is a one-way trust decision (unlike
/// message routing, which can silently drop per-message), so it gets the
/// same allow-list gate rather than accepting every invite from anyone who
/// can reach the homeserver.
pub async fn handle_invite(event: StrippedRoomMemberEvent, room: Room, state: Arc<DaemonState>) {
    let Some(matrix) = &state.matrix else {
        return;
    };
    let Some(matrix_config) = &state.core.config.matrix else {
        return;
    };

    // Only care about invites addressed to the bot itself, not membership
    // changes for other users in rooms it's already in.
    if event.state_key != matrix.user_id || event.content.membership != MembershipState::Invite {
        return;
    }

    let room_id = room.room_id().to_string();
    if !is_allowed_matrix_room(&room_id, &matrix_config.allowed_rooms) {
        tracing::warn!(room_id, sender = %event.sender, "declined invite to unlisted matrix room");
        return;
    }

    tracing::info!(room_id, sender = %event.sender, "accepting invite to allowed matrix room");
    if let Err(err) = room.join().await {
        tracing::error!(%err, room_id, "failed to join matrix room");
    }
}

/// The bot-self-filter guardrail, factored out as a pure function so it's
/// unit-testable without a live `matrix_sdk::Client` -- see CLAUDE.md's
/// guardrail note: without this check, the bot's own reply would re-enter
/// as a new message with no command prefix and be re-ingested.
fn is_matrix_self(sender: &UserId, bot_user_id: &UserId) -> bool {
    sender == bot_user_id
}

/// The Matrix-side allow-list guardrail, mirroring `allowed_chats.contains`
/// for Telegram, factored out for the same unit-testability reason.
fn is_allowed_matrix_room(room_id: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|r| r == room_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_bot_self_filter_drops_own_messages() {
        let bot_id = UserId::parse("@kamaji:matrix.pasoenfalso.com").unwrap();
        let same = UserId::parse("@kamaji:matrix.pasoenfalso.com").unwrap();
        let other = UserId::parse("@someone:matrix.pasoenfalso.com").unwrap();
        assert!(is_matrix_self(&same, &bot_id));
        assert!(!is_matrix_self(&other, &bot_id));
    }

    #[test]
    fn matrix_allow_list_drops_unlisted_rooms() {
        let allowed = vec!["!allowed:matrix.pasoenfalso.com".to_string()];
        assert!(is_allowed_matrix_room(
            "!allowed:matrix.pasoenfalso.com",
            &allowed
        ));
        assert!(!is_allowed_matrix_room(
            "!other:matrix.pasoenfalso.com",
            &allowed
        ));
    }
}
