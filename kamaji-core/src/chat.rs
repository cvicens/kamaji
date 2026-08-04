use std::fmt;

use serde::{Deserialize, Serialize};

/// Transport-agnostic reference to a conversation a job originated from.
/// Telegram chat ids are integers; Matrix room ids are opaque strings like
/// `!abc:server.example` -- the two shapes don't fit a single field, so this
/// is a tagged enum rather than a discriminator alongside a bare int.
/// `Cli { request_id }` is a `kamaji` CLI invocation connected to `kamajid`
/// over a Unix socket, waiting on this id via the daemon's waiter registry
/// rather than a chat platform's send API -- see `kamajid::transport`.
/// `Rest { request_id }` is the same waiter-registry mechanism, but for a
/// `kamaji` CLI invocation connected over the REST API instead of the local
/// socket -- kept as a separate variant (rather than reusing `Cli`) because
/// a network-reachable REST client crosses a different trust boundary than
/// a local Unix-socket connection, worth being able to distinguish in logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "platform")]
pub enum ChatRef {
    Telegram { chat_id: i64 },
    Matrix { room_id: String },
    Cli { request_id: u64 },
    Rest { request_id: u64 },
}

impl fmt::Display for ChatRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatRef::Telegram { chat_id } => write!(f, "telegram:{chat_id}"),
            ChatRef::Matrix { room_id } => write!(f, "matrix:{room_id}"),
            ChatRef::Cli { request_id } => write!(f, "cli:{request_id}"),
            ChatRef::Rest { request_id } => write!(f, "rest:{request_id}"),
        }
    }
}

/// Transport-agnostic reference to the specific message a reply should be
/// threaded to. Telegram message ids are integers scoped to a chat; Matrix
/// event ids are opaque strings like `$xyz:server.example`. `Cli`/`Rest`
/// carry no meaningful "reply to" target -- the open connection already is
/// one -- but mirror `request_id` for symmetry with `ChatRef::Cli`/`ChatRef::Rest`
/// and useful correlation in logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "platform")]
pub enum MessageRef {
    Telegram { message_id: i32 },
    Matrix { event_id: String },
    Cli { request_id: u64 },
    Rest { request_id: u64 },
}

impl fmt::Display for MessageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MessageRef::Telegram { message_id } => write!(f, "telegram:{message_id}"),
            MessageRef::Matrix { event_id } => write!(f, "matrix:{event_id}"),
            MessageRef::Cli { request_id } => write!(f, "cli:{request_id}"),
            MessageRef::Rest { request_id } => write!(f, "rest:{request_id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_ref_telegram_round_trips() {
        let chat = ChatRef::Telegram { chat_id: 42 };
        let json = serde_json::to_string(&chat).unwrap();
        let back: ChatRef = serde_json::from_str(&json).unwrap();
        assert_eq!(chat, back);
    }

    #[test]
    fn chat_ref_matrix_round_trips() {
        let chat = ChatRef::Matrix {
            room_id: "!abc:matrix.pasoenfalso.com".to_string(),
        };
        let json = serde_json::to_string(&chat).unwrap();
        let back: ChatRef = serde_json::from_str(&json).unwrap();
        assert_eq!(chat, back);
    }

    #[test]
    fn message_ref_telegram_round_trips() {
        let msg = MessageRef::Telegram { message_id: 123 };
        let json = serde_json::to_string(&msg).unwrap();
        let back: MessageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn message_ref_matrix_round_trips() {
        let msg = MessageRef::Matrix {
            event_id: "$xyz:matrix.pasoenfalso.com".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: MessageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn chat_ref_display_disambiguates_platform() {
        assert_eq!(ChatRef::Telegram { chat_id: 1 }.to_string(), "telegram:1");
        assert_eq!(
            ChatRef::Matrix {
                room_id: "!r:s".to_string()
            }
            .to_string(),
            "matrix:!r:s"
        );
        assert_eq!(ChatRef::Cli { request_id: 7 }.to_string(), "cli:7");
    }

    #[test]
    fn chat_ref_cli_round_trips() {
        let chat = ChatRef::Cli { request_id: 7 };
        let json = serde_json::to_string(&chat).unwrap();
        let back: ChatRef = serde_json::from_str(&json).unwrap();
        assert_eq!(chat, back);
    }

    #[test]
    fn message_ref_cli_round_trips() {
        let msg = MessageRef::Cli { request_id: 7 };
        let json = serde_json::to_string(&msg).unwrap();
        let back: MessageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn chat_ref_rest_round_trips() {
        let chat = ChatRef::Rest { request_id: 7 };
        let json = serde_json::to_string(&chat).unwrap();
        let back: ChatRef = serde_json::from_str(&json).unwrap();
        assert_eq!(chat, back);
    }

    #[test]
    fn message_ref_rest_round_trips() {
        let msg = MessageRef::Rest { request_id: 7 };
        let json = serde_json::to_string(&msg).unwrap();
        let back: MessageRef = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }
}
