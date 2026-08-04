use std::time::Duration;

use serde::Deserialize;

use crate::agent;
use crate::error::PromptError;
use crate::prompt::{AgentEnvelope, UsageInfo};

/// Runs `codex exec --skip-git-repo-check --json -` via the generic
/// `agent::invoke`, feeding `prompt` via stdin -- Codex's `-` positional arg
/// reads the prompt from stdin instead of argv, same "never argv for a large
/// payload" reasoning as `claude::invoke_claude`. Unlike Claude's single JSON
/// envelope, Codex's `--json` flag streams one JSON object per line (JSONL);
/// `parse_codex_jsonl` extracts the final answer and token usage from that
/// stream into the same `AgentEnvelope` shape `claude::invoke_claude`
/// produces, so `prompt.rs`'s entry points don't need to know which flavor
/// produced it. `prompt.rs` is the only caller, chosen via
/// `AgentFlavor::Codex`.
pub(crate) async fn invoke_codex(
    runner: &agent::Runner,
    codex_bin: &str,
    timeout: Duration,
    prompt: &str,
) -> Result<AgentEnvelope, PromptError> {
    let output = agent::invoke(
        runner,
        codex_bin,
        &["exec", "--skip-git-repo-check", "--json", "-"],
        prompt,
        timeout,
    )
    .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_codex_jsonl(&stdout)
}

/// One line of Codex's `--json` (JSONL) event stream. Only the shapes we
/// actually consume are modeled here.
#[derive(Debug, Deserialize)]
struct CodexEvent {
    #[serde(rename = "type")]
    kind: String,
    item: Option<CodexItem>,
    usage: Option<CodexUsage>,
}

#[derive(Debug, Deserialize)]
struct CodexItem {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

/// Pure, unit-testable without a live gateway -- same "no I/O" pattern as
/// `agent.rs`'s `map_exec_success`/`map_exec_result`. Lines that don't parse
/// as `CodexEvent`, or whose `type` we don't recognize, are skipped rather
/// than failing the whole stream -- Codex may emit event types we don't
/// model, and a stray malformed line shouldn't fail a job that still has a
/// valid answer elsewhere in the stream. Takes the **last** `item.completed`
/// event whose `item.type == "agent_message"` as the answer (in case of
/// multiple turns; kamaji's own prompts are single-turn, so there should
/// only ever be one) and the `turn.completed` event's `usage` for token
/// accounting. Errors with `PromptError::CodexNoAgentMessage` if no such
/// event appears anywhere in the stream.
fn parse_codex_jsonl(stdout: &str) -> Result<AgentEnvelope, PromptError> {
    let mut result = None;
    let mut usage = None;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<CodexEvent>(line) else {
            tracing::debug!(line, "skipping unparseable codex JSONL line");
            continue;
        };

        match event.kind.as_str() {
            "item.completed" => {
                if let Some(item) = event.item {
                    if item.kind == "agent_message" {
                        if let Some(text) = item.text {
                            result = Some(text);
                        }
                    }
                }
            }
            "turn.completed" => usage = event.usage,
            _ => {}
        }
    }

    let result = result.ok_or(PromptError::CodexNoAgentMessage)?;

    // Codex's {input_tokens, cached_input_tokens, output_tokens} doesn't
    // line up 1:1 with the shared `UsageInfo` breakdown (base/cache_creation/
    // cache_read) that `prompt::extract_tokens` sums back into a total --
    // map `cached_input_tokens` onto `cache_read_input_tokens` (tokens served
    // from cache), `cache_creation_input_tokens` to 0 (Codex doesn't report
    // creation separately), and subtract the cached count out of the base so
    // the sum still equals Codex's own reported total. Note: Codex/DeepSeek
    // jobs won't carry a dollar cost the way Claude's do -- `total_cost_usd`
    // is absent from Codex's own JSON entirely.
    let usage = usage.map(|u| {
        let cached = u.cached_input_tokens.unwrap_or(0);
        let total_input = u.input_tokens.unwrap_or(0);
        UsageInfo {
            input_tokens: Some(total_input.saturating_sub(cached)),
            output_tokens: u.output_tokens,
            cache_creation_input_tokens: Some(0),
            cache_read_input_tokens: Some(cached),
        }
    });

    Ok(AgentEnvelope {
        result: Some(result),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codex_jsonl_extracts_agent_message_and_usage() {
        let stdout = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n",
            "{\"type\":\"turn.started\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"hello\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"cached_input_tokens\":40,\"output_tokens\":10}}\n",
        );
        let envelope = parse_codex_jsonl(stdout).expect("should parse");
        assert_eq!(envelope.result.as_deref(), Some("hello"));
        let usage = envelope.usage.expect("usage present");
        assert_eq!(usage.input_tokens, Some(60));
        assert_eq!(usage.cache_read_input_tokens, Some(40));
        assert_eq!(usage.cache_creation_input_tokens, Some(0));
        assert_eq!(usage.output_tokens, Some(10));
    }

    #[test]
    fn parse_codex_jsonl_ignores_unrelated_event_types() {
        let stdout = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n",
            "{\"type\":\"some.future.event\",\"whatever\":123}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"hi\"}}\n",
        );
        let envelope = parse_codex_jsonl(stdout).expect("should parse");
        assert_eq!(envelope.result.as_deref(), Some("hi"));
    }

    #[test]
    fn parse_codex_jsonl_skips_malformed_lines() {
        let stdout = concat!(
            "not even json\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"hi\"}}\n",
        );
        let envelope = parse_codex_jsonl(stdout).expect("should parse despite one malformed line");
        assert_eq!(envelope.result.as_deref(), Some("hi"));
    }

    #[test]
    fn parse_codex_jsonl_errors_when_no_agent_message_found() {
        let stdout = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"t1\"}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}\n",
        );
        let result = parse_codex_jsonl(stdout);
        assert!(matches!(result, Err(PromptError::CodexNoAgentMessage)));
    }

    #[test]
    fn parse_codex_jsonl_takes_last_agent_message_when_multiple_turns() {
        let stdout = concat!(
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"first\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"agent_message\",\"text\":\"second\"}}\n",
        );
        let envelope = parse_codex_jsonl(stdout).expect("should parse");
        assert_eq!(envelope.result.as_deref(), Some("second"));
    }
}
