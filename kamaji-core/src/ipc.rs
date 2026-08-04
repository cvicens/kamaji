//! Wire protocol for the `kamaji` CLI <-> `kamajid` daemon Unix socket.
//! Newline-delimited JSON: one `CliRequest` per connection, one
//! `CliResponse` back. Chosen over a binary length-prefixed format for the
//! same reason the rest of the codebase already leans on `serde_json`
//! everywhere -- simple, debuggable with `socat`/`nc`, no new framing crate.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::IpcError;

/// A request from the `kamaji` CLI. Every variant maps onto an existing
/// named command (see `into_command`), so `kamajid::transport::socket` can
/// hand the result straight to the same `dispatch_routed_job` that
/// Telegram/Matrix-originated commands go through -- no separate dispatch
/// path for the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CliRequest {
    /// Maps to the `/ingest` command's semantics (link -> note, freeform
    /// text -> agent passthrough), not the raw no-prefix ingest path --
    /// there's no CLI equivalent of "just type a message with no command."
    Ingest {
        text: String,
    },
    Fact {
        text: String,
    },
    Todo {
        args: Vec<String>,
    },
    Status,
    History {
        limit: Option<usize>,
    },
    Help,
    /// Maps to the `/align` command's semantics: auto-links open TODOs to
    /// matching open goals by shared tag and reports the result --
    /// `CommandMode::Queued` since it writes to the notes repo. No args
    /// (v1, see TODO.md).
    Align,
    /// Maps to the `/demonstrate` command's semantics: auto-links bitacora
    /// facts to matching open goals they demonstrate and reports the
    /// result -- `CommandMode::Queued`, same as `Align`. `scope` is the
    /// optional single arg (`"all"`/`"YYYY-Q#"`); `None` defaults to the
    /// current quarter (see `demonstrate::parse_scope`).
    Demonstrate {
        scope: Option<String>,
    },
}

impl CliRequest {
    /// Translates this request into the `(name, args)` shape used by
    /// `commands::mode`/`JobKind::Command`, splitting free text the same
    /// way `routing::route_message` splits a `/command arg1 arg2` line.
    pub fn into_command(self) -> (&'static str, Vec<String>) {
        match self {
            CliRequest::Ingest { text } => ("ingest", split_args(&text)),
            CliRequest::Fact { text } => ("fact", split_args(&text)),
            CliRequest::Todo { args } => ("todo", args),
            CliRequest::Status => ("status", Vec::new()),
            CliRequest::History { limit } => (
                "history",
                limit.map(|l| vec![l.to_string()]).unwrap_or_default(),
            ),
            CliRequest::Help => ("help", Vec::new()),
            CliRequest::Align => ("align", Vec::new()),
            CliRequest::Demonstrate { scope } => {
                ("demonstrate", scope.map(|s| vec![s]).unwrap_or_default())
            }
        }
    }
}

fn split_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(|s| s.to_string()).collect()
}

/// A reply to a `CliRequest`. `ok = false` means the CLI should exit
/// non-zero -- e.g. an unknown command or a usage error, not a transport
/// failure (a transport failure means no `CliResponse` was ever read at
/// all).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliResponse {
    pub ok: bool,
    pub text: String,
}

/// Reads one newline-delimited JSON message. `Err(IpcError::UnexpectedEof)`
/// means the peer closed the connection before sending anything -- the
/// caller (either side) should treat that as "no message," not retry.
pub async fn read_message<T, R>(reader: &mut R) -> Result<T, IpcError>
where
    T: DeserializeOwned,
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(IpcError::UnexpectedEof);
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

/// Writes one newline-delimited JSON message and flushes.
pub async fn write_message<T, W>(writer: &mut W, value: &T) -> Result<(), IpcError>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let mut json = serde_json::to_string(value)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cli_request_round_trips_over_ndjson() {
        let mut buf = Vec::new();
        write_message(&mut buf, &CliRequest::Ingest { text: "x".into() })
            .await
            .unwrap();
        let mut reader = buf.as_slice();
        let back: CliRequest = read_message(&mut reader).await.unwrap();
        matches!(back, CliRequest::Ingest { text } if text == "x");
    }

    #[tokio::test]
    async fn cli_response_round_trips_over_ndjson() {
        let mut buf = Vec::new();
        let sent = CliResponse {
            ok: true,
            text: "done".to_string(),
        };
        write_message(&mut buf, &sent).await.unwrap();
        let mut reader = buf.as_slice();
        let back: CliResponse = read_message(&mut reader).await.unwrap();
        assert_eq!(back.ok, sent.ok);
        assert_eq!(back.text, sent.text);
    }

    #[tokio::test]
    async fn read_message_on_empty_input_is_unexpected_eof() {
        let mut reader: &[u8] = &[];
        let result: Result<CliResponse, _> = read_message(&mut reader).await;
        assert!(matches!(result, Err(IpcError::UnexpectedEof)));
    }

    #[test]
    fn ingest_maps_to_ingest_command_with_split_args() {
        let (name, args) = CliRequest::Ingest {
            text: "worth reading: https://example.com".to_string(),
        }
        .into_command();
        assert_eq!(name, "ingest");
        assert_eq!(args, vec!["worth", "reading:", "https://example.com"]);
    }

    #[test]
    fn history_with_limit_maps_to_single_arg() {
        let (name, args) = CliRequest::History { limit: Some(5) }.into_command();
        assert_eq!(name, "history");
        assert_eq!(args, vec!["5"]);
    }

    #[test]
    fn history_without_limit_maps_to_no_args() {
        let (name, args) = CliRequest::History { limit: None }.into_command();
        assert_eq!(name, "history");
        assert!(args.is_empty());
    }

    #[test]
    fn align_maps_to_no_args() {
        let (name, args) = CliRequest::Align.into_command();
        assert_eq!(name, "align");
        assert!(args.is_empty());
    }

    #[test]
    fn demonstrate_with_no_scope_maps_to_no_args() {
        let (name, args) = CliRequest::Demonstrate { scope: None }.into_command();
        assert_eq!(name, "demonstrate");
        assert!(args.is_empty());
    }

    #[test]
    fn demonstrate_with_scope_maps_to_single_arg() {
        let (name, args) = CliRequest::Demonstrate {
            scope: Some("2026-Q2".to_string()),
        }
        .into_command();
        assert_eq!(name, "demonstrate");
        assert_eq!(args, vec!["2026-Q2"]);
    }

    #[test]
    fn todo_args_pass_through_unsplit() {
        let (name, args) = CliRequest::Todo {
            args: vec!["add".to_string(), "buy".to_string(), "milk".to_string()],
        }
        .into_command();
        assert_eq!(name, "todo");
        assert_eq!(args, vec!["add", "buy", "milk"]);
    }
}
