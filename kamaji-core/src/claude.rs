use std::time::Duration;

use crate::agent;
use crate::error::PromptError;
use crate::prompt::AgentEnvelope;

/// Runs `claude -p --output-format json` via the generic `agent::invoke`
/// (spawn/stdin/timeout mechanics live there now, shared with any future
/// agent binary), feeding `prompt` via stdin -- see `agent::invoke`'s doc
/// comment for why stdin rather than argv -- and parses the resulting
/// stdout directly into `AgentEnvelope` (Claude's JSON envelope already has
/// the exact `result`/`usage` shape `AgentEnvelope`/`UsageInfo` expect, so no
/// mapping is needed here -- unlike `codex::invoke_codex`, which has to
/// translate Codex's differently-shaped JSONL stream into the same type).
/// `prompt.rs`'s entry points are the only callers, chosen via
/// `AgentFlavor::Claude`.
pub(crate) async fn invoke_claude(
    runner: &agent::Runner,
    claude_bin: &str,
    timeout: Duration,
    prompt: &str,
) -> Result<AgentEnvelope, PromptError> {
    // Never add --dangerously-skip-permissions here. Sandbox isolation (when
    // `runner` is `agent::Runner::OpenShell`) is an additive, orthogonal
    // control -- it does not make this flag safe. This argv is shared
    // unchanged by both runners (`agent::invoke` forwards it as-is into the
    // sandbox exec command when wrapped), so the guardrail holds regardless
    // of which runner is wired up.
    let output = agent::invoke(
        runner,
        claude_bin,
        &["-p", "--output-format", "json"],
        prompt,
        timeout,
    )
    .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).map_err(PromptError::EnvelopeParse)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for E2BIG ("Argument list too long"), covering the layer
    /// this module owns: a fake `claude` binary that emits a JSON envelope
    /// after reading an oversized prompt from stdin. The spawn/stdin/timeout
    /// mechanics that make this possible now live in `agent.rs` and have
    /// their own dedicated test (`agent::tests::invoke_pipes_oversized_stdin`);
    /// this one instead confirms `invoke_claude` still parses the envelope
    /// correctly end to end when a large prompt is involved.
    #[tokio::test]
    async fn invoke_claude_pipes_oversized_prompt_via_stdin() {
        // A shell script standing in for `claude`: it ignores the -p/--output-format
        // args, drains stdin, and prints a JSON envelope echoing the byte count.
        let dir = std::env::temp_dir().join(format!("kamaji-fake-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-claude.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nbytes=$(wc -c)\nprintf '{\"result\":\"%s\"}' \"$bytes\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 200 KiB -- comfortably past MAX_ARG_STRLEN, where argv would fail.
        let prompt = "x".repeat(200 * 1024);
        let envelope = invoke_claude(
            &agent::Runner::Direct,
            script.to_str().unwrap(),
            Duration::from_secs(30),
            &prompt,
        )
        .await
        .expect("oversized prompt should spawn and run via stdin");
        let byte_count: usize = envelope
            .result
            .expect("result present")
            .trim()
            .parse()
            .expect("byte count is numeric");
        assert_eq!(byte_count, prompt.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn envelope_parses_result_field() {
        let raw = r#"{"type":"result","result":"{\"title\":\"x\"}","cost_usd":0.01}"#;
        let envelope: AgentEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(envelope.result.as_deref(), Some("{\"title\":\"x\"}"));
    }

    #[test]
    fn envelope_parses_usage_with_cache_breakdown() {
        let raw = r#"{
            "type": "result",
            "result": "{\"title\":\"x\"}",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_creation_input_tokens": 30,
                "cache_read_input_tokens": 40
            }
        }"#;
        let envelope: AgentEnvelope = serde_json::from_str(raw).unwrap();
        let usage = envelope.usage.expect("usage present");
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cache_creation_input_tokens, Some(30));
        assert_eq!(usage.cache_read_input_tokens, Some(40));
    }
}
