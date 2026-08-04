use crate::chat::{ChatRef, MessageRef};
use crate::queue::{CommandAttachment, Job, JobKind};
use crate::urls;

/// Decides whether a message is a command or an ingest job. Leading `/` ->
/// command; otherwise -> ingest. This is the routing rule from the spec, and
/// the distinction is enforced here so unknown commands can reply with an
/// error rather than silently falling through to ingestion. Shared by every
/// transport (Telegram, Matrix, the `kamaji` CLI over its Unix socket) --
/// each extracts its own platform-native text/attachment/chat/reply-to down
/// to these generic types before calling this.
pub fn route_message(
    text: &str,
    attachment: Option<CommandAttachment>,
    chat: ChatRef,
    reply_to: MessageRef,
) -> Job {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix('/') {
        let mut parts = rest.split_whitespace();
        let name = parts.next().unwrap_or("").to_string();
        let args = parts.map(|s| s.to_string()).collect();
        Job {
            chat,
            reply_to,
            kind: JobKind::Command {
                name,
                args,
                attachment,
            },
        }
    } else {
        Job {
            chat,
            reply_to,
            kind: JobKind::Ingest {
                raw_text: text.to_string(),
                urls: urls::extract_urls(text),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telegram_chat(id: i64) -> ChatRef {
        ChatRef::Telegram { chat_id: id }
    }

    fn telegram_reply(id: i32) -> MessageRef {
        MessageRef::Telegram { message_id: id }
    }

    #[test]
    fn route_message_ingest_path() {
        let job = route_message("hello world", None, telegram_chat(1), telegram_reply(123));
        match job.kind {
            JobKind::Ingest { raw_text, .. } => assert_eq!(raw_text, "hello world"),
            _ => panic!("expected Ingest"),
        }
    }

    #[test]
    fn route_message_command_path() {
        let job = route_message("/status", None, telegram_chat(1), telegram_reply(123));
        match job.kind {
            JobKind::Command {
                name,
                args,
                attachment,
            } => {
                assert_eq!(name, "status");
                assert!(args.is_empty());
                assert!(attachment.is_none());
            }
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn route_message_command_with_args() {
        let job = route_message(
            "/search foo bar",
            None,
            telegram_chat(1),
            telegram_reply(123),
        );
        match job.kind {
            JobKind::Command { name, args, .. } => {
                assert_eq!(name, "search");
                assert_eq!(args, vec!["foo", "bar"]);
            }
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn route_message_command_with_attachment_carries_file_metadata() {
        let attachment = CommandAttachment {
            file_id: "file-id-123".to_string(),
            file_name: "report.pdf".to_string(),
            mime_type: None,
        };
        let job = route_message(
            "/fact wrote the quarterly report",
            Some(attachment),
            telegram_chat(1),
            telegram_reply(123),
        );
        match job.kind {
            JobKind::Command {
                name,
                args,
                attachment,
            } => {
                assert_eq!(name, "fact");
                assert_eq!(args, vec!["wrote", "the", "quarterly", "report"]);
                let attachment = attachment.expect("attachment should be carried through");
                assert_eq!(attachment.file_id, "file-id-123");
                assert_eq!(attachment.file_name, "report.pdf");
            }
            _ => panic!("expected Command"),
        }
    }

    #[test]
    fn route_message_ingest_path_ignores_attachment() {
        // A document caption with no leading `/` still ingests as plain
        // text -- the default path is deliberately unchanged, so any
        // attachment on it is dropped rather than silently mis-filed.
        let attachment = CommandAttachment {
            file_id: "file-id-456".to_string(),
            file_name: "photo.jpg".to_string(),
            mime_type: None,
        };
        let job = route_message(
            "just a caption, no command",
            Some(attachment),
            telegram_chat(1),
            telegram_reply(123),
        );
        match job.kind {
            JobKind::Ingest { raw_text, .. } => assert_eq!(raw_text, "just a caption, no command"),
            _ => panic!("expected Ingest"),
        }
    }

    #[test]
    fn route_message_matrix_chat_round_trips() {
        let chat = ChatRef::Matrix {
            room_id: "!room:matrix.pasoenfalso.com".to_string(),
        };
        let reply_to = MessageRef::Matrix {
            event_id: "$event:matrix.pasoenfalso.com".to_string(),
        };
        let job = route_message("hello from matrix", None, chat.clone(), reply_to.clone());
        assert_eq!(job.chat, chat);
        assert_eq!(job.reply_to, reply_to);
        match job.kind {
            JobKind::Ingest { raw_text, .. } => assert_eq!(raw_text, "hello from matrix"),
            _ => panic!("expected Ingest"),
        }
    }

    #[test]
    fn route_message_cli_chat_round_trips() {
        let chat = ChatRef::Cli { request_id: 7 };
        let reply_to = MessageRef::Cli { request_id: 7 };
        let job = route_message("/status", None, chat.clone(), reply_to.clone());
        assert_eq!(job.chat, chat);
        assert_eq!(job.reply_to, reply_to);
    }
}
