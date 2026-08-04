use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::agent;
use crate::claude;
use crate::codex;
use crate::config::AgentFlavor;
use crate::error::PromptError;

/// The fixed schema the agent is instructed to return for an ingest job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResult {
    pub title: String,
    pub summary: String,
    pub importance: i64,
    pub tags: Vec<String>,
    pub source_url: Option<String>,
    pub slug: String,
    /// Folder the note is filed under (`notes/<category>/`). Reuses one of
    /// the existing category names handed to the prompt when one fits, or
    /// names a new one otherwise -- no fixed taxonomy, no "uncategorized"
    /// catch-all.
    pub category: String,
}

/// The fixed schema the agent is instructed to return for a `/fact` bitacora
/// entry. Deliberately a separate shape from `IngestResult`: facts are
/// framed as accomplishments for a personal activity log, not triaged notes
/// -- there's no `category` (bitacora entries are filed by date, not
/// topic), and `value` replaces `importance` as the field name to match
/// that framing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactResult {
    pub title: String,
    pub summary: String,
    pub value: i64,
    pub tags: Vec<String>,
    pub slug: String,
}

/// One tag-matched bitacora fact handed to the `/demonstrate` semantic-match
/// prompt as a judgment candidate. `id` is the fact's wikilink target
/// (`bitacora::FactRecord::wikilink_target`) -- opaque to Claude, just an
/// identifier to echo back in `DemonstrateResult`.
#[derive(Debug, Clone)]
pub struct DemonstrateCandidate {
    pub id: String,
    pub title: String,
    pub description: String,
}

/// The fixed schema the agent is instructed to return for `/demonstrate`'s
/// semantic-match pass: which of the candidate facts handed in actually
/// demonstrate progress toward the goal. Always a subset of the candidate
/// ids handed in -- `run_demonstrate_prompt` filters out anything else
/// before returning, so callers never see a hallucinated id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DemonstrateResult {
    pub demonstrating: Vec<String>,
}

/// Token usage from an agent CLI invocation.
///
/// `input` is the full input total (base + cache_creation + cache_read),
/// kept for backward compatibility with existing history entries and
/// displays. `cache_creation`/`cache_read` break that total down so job
/// history isn't just the aggregate. `#[serde(default)]` lets old
/// `job_history` records (written before these fields existed) still
/// deserialize, defaulting the breakdown to 0.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_creation: u64,
    #[serde(default)]
    pub cache_read: u64,
}

/// Normalized shape both `claude::invoke_claude` and `codex::invoke_codex`
/// produce, regardless of each binary's own wire format -- `claude.rs`
/// deserializes Claude's single JSON envelope directly into this shape
/// (its fields already line up); `codex.rs` builds one from Codex's JSONL
/// event stream instead. Everything below this point only ever touches
/// `.result`/`.usage`, never either binary's raw output, which is what
/// keeps this module agent-agnostic.
#[derive(Debug, Deserialize)]
pub(crate) struct AgentEnvelope {
    pub(crate) result: Option<String>,
    pub(crate) usage: Option<UsageInfo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UsageInfo {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) cache_creation_input_tokens: Option<u64>,
    pub(crate) cache_read_input_tokens: Option<u64>,
}

/// Content fetched from a URL.
#[derive(Debug, Clone)]
pub struct FetchedContent {
    pub url: String,
    pub text: String,
}

/// The one place that knows both agent flavors exist -- `claude.rs`/`codex.rs`
/// are otherwise peers, each only knowing how to invoke its own binary and
/// parse its own wire format into `AgentEnvelope`.
async fn invoke_agent(
    flavor: AgentFlavor,
    runner: &agent::Runner,
    bin: &str,
    timeout: Duration,
    prompt: &str,
) -> Result<AgentEnvelope, PromptError> {
    match flavor {
        AgentFlavor::Claude => claude::invoke_claude(runner, bin, timeout, prompt).await,
        AgentFlavor::Codex => codex::invoke_codex(runner, bin, timeout, prompt).await,
    }
}

pub async fn run_ingest_prompt(
    flavor: AgentFlavor,
    runner: &agent::Runner,
    bin: &str,
    timeout: Duration,
    raw_text: &str,
    fetched_urls: &[FetchedContent],
    existing_categories: &[String],
) -> Result<(IngestResult, Option<TokenUsage>), PromptError> {
    let prompt = build_prompt(raw_text, fetched_urls, existing_categories);

    let envelope = invoke_agent(flavor, runner, bin, timeout, &prompt).await?;
    let result_text = envelope.result.ok_or(PromptError::MissingResult)?;

    let cleaned = strip_code_fence(&result_text);
    let parsed: IngestResult =
        serde_json::from_str(cleaned).map_err(|source| PromptError::SchemaParse {
            source,
            raw: result_text.clone(),
        })?;

    if !(1..=5).contains(&parsed.importance) {
        return Err(PromptError::ImportanceOutOfRange(parsed.importance));
    }

    let tokens = extract_tokens(envelope.usage);

    Ok((parsed, tokens))
}

/// Runs the `/fact` bitacora prompt: same envelope/timeout/retry shape as
/// `run_ingest_prompt`, but a different strict-JSON schema (`FactResult`
/// instead of `IngestResult`) and validated field (`value` instead of
/// `importance`).
pub async fn run_fact_prompt(
    flavor: AgentFlavor,
    runner: &agent::Runner,
    bin: &str,
    timeout: Duration,
    raw_text: &str,
    fetched_urls: &[FetchedContent],
    attachment_name: Option<&str>,
) -> Result<(FactResult, Option<TokenUsage>), PromptError> {
    let prompt = build_fact_prompt(raw_text, fetched_urls, attachment_name);

    let envelope = invoke_agent(flavor, runner, bin, timeout, &prompt).await?;
    let result_text = envelope.result.ok_or(PromptError::MissingResult)?;

    let cleaned = strip_code_fence(&result_text);
    let parsed: FactResult =
        serde_json::from_str(cleaned).map_err(|source| PromptError::SchemaParse {
            source,
            raw: result_text.clone(),
        })?;

    if !(1..=5).contains(&parsed.value) {
        return Err(PromptError::ValueOutOfRange(parsed.value));
    }

    let tokens = extract_tokens(envelope.usage);

    Ok((parsed, tokens))
}

/// Runs `/demonstrate`'s semantic-match prompt: given one open goal and its
/// tag-matched candidate facts, asks the agent which candidates actually
/// demonstrate progress toward it. Unlike `run_ingest_prompt`/
/// `run_fact_prompt` there's no numeric-range validation -- instead the
/// returned `demonstrating` list is filtered down to the intersection with
/// `candidates`' own ids, since an agent could echo back a malformed or
/// invented id; anything outside that set is dropped and logged rather than
/// failing the whole run (same log-and-tolerate posture as Codex's
/// unrecognized-JSONL-event skipping).
pub async fn run_demonstrate_prompt(
    flavor: AgentFlavor,
    runner: &agent::Runner,
    bin: &str,
    timeout: Duration,
    goal_text: &str,
    candidates: &[DemonstrateCandidate],
) -> Result<(DemonstrateResult, Option<TokenUsage>), PromptError> {
    let prompt = build_demonstrate_prompt(goal_text, candidates);

    let envelope = invoke_agent(flavor, runner, bin, timeout, &prompt).await?;
    let result_text = envelope.result.ok_or(PromptError::MissingResult)?;

    let cleaned = strip_code_fence(&result_text);
    let parsed: DemonstrateResult =
        serde_json::from_str(cleaned).map_err(|source| PromptError::SchemaParse {
            source,
            raw: result_text.clone(),
        })?;

    let demonstrating = filter_known_ids(candidates, parsed.demonstrating);
    let tokens = extract_tokens(envelope.usage);

    Ok((DemonstrateResult { demonstrating }, tokens))
}

/// Keeps only the ids that actually appear in `candidates`, dropping (and
/// logging) anything else -- guards against a hallucinated or malformed id
/// coming back from `run_demonstrate_prompt`'s agent call. Factored out as
/// its own pure function so it's unit-testable without a live agent call.
fn filter_known_ids(candidates: &[DemonstrateCandidate], ids: Vec<String>) -> Vec<String> {
    let valid_ids: std::collections::HashSet<&str> =
        candidates.iter().map(|c| c.id.as_str()).collect();
    let mut kept = Vec::new();
    for id in ids {
        if valid_ids.contains(id.as_str()) {
            kept.push(id);
        } else {
            tracing::warn!(
                id,
                "agent returned a demonstrate candidate id not in the candidate set; dropping it"
            );
        }
    }
    kept
}

/// A freeform query passed straight through to the agent, distinct from
/// `run_ingest_prompt`'s strict-JSON note-taking contract -- used by
/// `/ingest <text>` (see TODO.md), where non-link text is meant to reach the
/// agent directly rather than be filed away as a note. Returns whatever
/// prose the agent replies with, unparsed beyond the envelope.
pub async fn run_agent_query(
    flavor: AgentFlavor,
    runner: &agent::Runner,
    bin: &str,
    timeout: Duration,
    query: &str,
) -> Result<(String, Option<TokenUsage>), PromptError> {
    let prompt = build_agent_prompt(query);

    let envelope = invoke_agent(flavor, runner, bin, timeout, &prompt).await?;
    let result_text = envelope.result.ok_or(PromptError::MissingResult)?;

    let tokens = extract_tokens(envelope.usage);

    Ok((result_text, tokens))
}

/// Shared between `run_ingest_prompt` and `run_agent_query`: total input is
/// base + cache_creation + cache_read, and a usage field present without
/// any token counts is treated the same as no usage field at all.
fn extract_tokens(usage: Option<UsageInfo>) -> Option<TokenUsage> {
    usage.and_then(|u| {
        let input_base = u.input_tokens.unwrap_or(0);
        let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);
        let cache_read = u.cache_read_input_tokens.unwrap_or(0);
        let total_input = input_base + cache_creation + cache_read;

        match (total_input, u.output_tokens) {
            (input, Some(output)) if input > 0 => {
                tracing::debug!(
                    input,
                    output,
                    input_new = input_base,
                    cache_creation,
                    cache_read,
                    "extracted token usage from JSON envelope"
                );
                Some(TokenUsage {
                    input,
                    output,
                    cache_creation,
                    cache_read,
                })
            }
            _ => {
                tracing::warn!("usage field present but missing token counts");
                None
            }
        }
    })
}

/// `pub(crate)` so the worker can build the same string for debug logging
/// without duplicating it. Deliberately not a JSON-schema prompt like
/// `build_prompt` -- `/ingest <text>` hands the message to the agent as-is,
/// asking it to respond directly rather than file a note.
pub(crate) fn build_agent_prompt(query: &str) -> String {
    format!(
        "A user sent this message to kamaji via the /ingest command, as a direct \
         query for you to answer -- not a note to file away:\n\n---\n{query}\n---\n\n\
         Respond directly and helpfully. Keep the reply concise and suitable for a chat \
         message."
    )
}

/// `pub(crate)` so the worker can build the same string for debug logging
/// without duplicating the fetched-URL formatting logic.
/// Shared between `build_prompt` and `build_fact_prompt`: renders fetched
/// URL content into the same "--- Content from X ---" block either prompt
/// interpolates, so the format only needs to change in one place.
fn format_fetched_urls(fetched_urls: &[FetchedContent]) -> String {
    if fetched_urls.is_empty() {
        return String::new();
    }
    let mut sections = vec!["\n\nFetched URL content:\n".to_string()];
    for fetched in fetched_urls {
        sections.push(format!(
            "\n--- Content from {} ---\n{}\n--- End of {} ---\n",
            fetched.url, fetched.text, fetched.url
        ));
    }
    sections.join("")
}

pub(crate) fn build_prompt(
    raw_text: &str,
    fetched_urls: &[FetchedContent],
    existing_categories: &[String],
) -> String {
    let url_section = format_fetched_urls(fetched_urls);

    let category_section = if existing_categories.is_empty() {
        "No category folders exist yet -- name whatever category best fits this note.".to_string()
    } else {
        format!(
            "Existing category folders: {}. Reuse one of these if it fits, or name a new \
             category if none do.",
            existing_categories.join(", ")
        )
    };

    format!(
        "You are a note-taking assistant. A user sent this message to kamaji, an \
         information-triage bot:\n\n---\n{raw_text}\n---{url_section}\n\n\
         CRITICAL: You MUST respond with valid JSON even if the content is unclear \
         or incomplete. NEVER write explanatory prose. NEVER explain why you cannot \
         process something. If the content is insufficient, use title=\"Unable to process\" \
         with a brief summary.\n\n\
         Respond with STRICT JSON ONLY, no markdown code fence, no prose before or \
         after, matching exactly this shape:\n\
         {{\n\
         \x20\"title\": \"string\",\n\
         \x20\"summary\": \"2-4 sentences\",\n\
         \x20\"importance\": 1,\n\
         \x20\"tags\": [\"freeform\", \"tags\"],\n\
         \x20\"source_url\": \"string or null\",\n\
         \x20\"slug\": \"url-safe-filename-fragment\",\n\
         \x20\"category\": \"string\"\n\
         }}\n\n\
         \"importance\" must be an integer from 1 (trivial) to 5 (critical). \"tags\" are \
         freeform, invent whatever tags best describe this note -- there is no fixed \
         taxonomy. \"slug\" must be lowercase, ASCII, words separated by hyphens, safe to \
         use directly in a filename. \"source_url\" should be the original URL if one was \
         provided, or null otherwise. \"category\" is the folder this note is filed under: \
         lowercase, ASCII, words separated by hyphens, no fixed taxonomy and no cap on how \
         many exist -- pick whatever fits best, there is no \"uncategorized\" catch-all to \
         fall back on. Categorize by the core subject matter of the content, not by the \
         medium, format, or type of the source it came from -- \"tech-news\" or \"article\" \
         describe what kind of text this is, not what it's about, so never use those or \
         similarly generic umbrella labels. Instead name the actual topic or domain: an \
         article about a new AI coding agent is \"agentic-ai\", a post about Kubernetes \
         deployment automation is \"gitops\", one about ML models running in production is \
         \"aiops\", one about compute moving to the network edge is \"edge-computing\". \
         {category_section}"
    )
}

/// `pub(crate)` so the worker can build the same string for debug logging
/// without duplicating it. Deliberately not `build_prompt`'s note-triage
/// framing: `/fact` entries are accomplishments for a personal bitacora
/// (bio log), not information to categorize, so there's no category
/// negotiation and the fields are named to match that framing (`value`
/// instead of `importance`, no `category`/`source_url`).
pub(crate) fn build_fact_prompt(
    raw_text: &str,
    fetched_urls: &[FetchedContent],
    attachment_name: Option<&str>,
) -> String {
    let url_section = format_fetched_urls(fetched_urls);

    // Attachment *content* extraction isn't wired up yet (see TODO.md) --
    // the file is downloaded and saved alongside the note, but the agent
    // only ever sees its name here, so the prompt must be explicit that it
    // should reference the attachment, not invent what's in it.
    let attachment_section = match attachment_name {
        Some(name) => format!(
            "\n\nAn attachment named \"{name}\" was included with this entry. Its contents \
             were not extracted for this prompt -- reference it by filename in the summary \
             if useful, but do not invent or guess what it contains."
        ),
        None => String::new(),
    };

    format!(
        "You are helping maintain a personal bitacora (an accomplishment/activity log) for \
         a user. The user just logged this fact via the /fact command:\n\n---\n\
         {raw_text}\n---{url_section}{attachment_section}\n\n\
         CRITICAL: You MUST respond with valid JSON even if the content is unclear or \
         incomplete. NEVER write explanatory prose. NEVER explain why you cannot process \
         something. If the content is insufficient, use title=\"Unable to process\" with a \
         brief summary.\n\n\
         Respond with STRICT JSON ONLY, no markdown code fence, no prose before or after, \
         matching exactly this shape:\n\
         {{\n\
         \x20\"title\": \"string\",\n\
         \x20\"summary\": \"2-4 sentences describing what was done and why it mattered\",\n\
         \x20\"value\": 1,\n\
         \x20\"tags\": [\"freeform\", \"tags\"],\n\
         \x20\"slug\": \"url-safe-filename-fragment\"\n\
         }}\n\n\
         \"value\" must be an integer from 1 (minor/routine) to 5 (major accomplishment), \
         reflecting how significant this entry is likely to be for a future quarterly \
         self-review. \"tags\" are freeform, invent whatever tags best describe this entry \
         -- there is no fixed taxonomy. \"slug\" must be lowercase, ASCII, words separated \
         by hyphens, safe to use directly in a filename."
    )
}

/// `pub(crate)` so the worker can build the same string for debug logging
/// without duplicating it. Unlike `build_prompt`/`build_fact_prompt`, this
/// isn't a note-taking contract -- it's a judgment call over a fixed
/// candidate set, so the schema is a plain id list rather than a note's
/// title/summary/tags shape.
pub(crate) fn build_demonstrate_prompt(
    goal_text: &str,
    candidates: &[DemonstrateCandidate],
) -> String {
    let candidates_section = candidates
        .iter()
        .map(|c| {
            format!(
                "- id: \"{}\"\n  title: \"{}\"\n  description: \"{}\"",
                c.id, c.title, c.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are helping maintain a personal goal-tracking system. A user has this open \
         goal:\n\n---\n{goal_text}\n---\n\n\
         Here are candidate log entries (\"facts\") from their activity log that share a tag \
         with this goal, and so might be evidence of progress toward it:\n\n\
         {candidates_section}\n\n\
         CRITICAL: You MUST respond with valid JSON even if none of the candidates apply. \
         NEVER write explanatory prose.\n\n\
         Respond with STRICT JSON ONLY, no markdown code fence, no prose before or after, \
         matching exactly this shape:\n\
         {{\n\
         \x20\"demonstrating\": [\"id\", \"id\"]\n\
         }}\n\n\
         \"demonstrating\" must be the subset of the candidate \"id\" values above (copied \
         exactly, do not invent new ones) whose entry genuinely shows progress toward the \
         goal -- not just a shared tag or topic. An empty list is a valid answer if none of \
         the candidates actually demonstrate the goal."
    )
}

/// Defensive: models sometimes wrap JSON in a ```json fence even when told
/// not to. Strip it if present; otherwise return the input unchanged.
fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let after_lang = after_open.trim_start_matches(|c: char| c.is_alphanumeric());
    let after_lang = after_lang.strip_prefix('\n').unwrap_or(after_lang);
    after_lang.strip_suffix("```").unwrap_or(after_lang).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fact_prompt_mentions_attachment_by_name_without_claiming_its_contents() {
        let prompt = build_fact_prompt("fixed the outage", &[], Some("report.pdf"));
        assert!(prompt.contains("report.pdf"));
        assert!(prompt.contains("were not extracted"));
    }

    #[test]
    fn fact_prompt_omits_attachment_section_when_none() {
        let prompt = build_fact_prompt("fixed the outage", &[], None);
        assert!(!prompt.contains("attachment"));
    }

    #[test]
    fn fact_prompt_includes_fetched_url_content() {
        let fetched = vec![FetchedContent {
            url: "https://example.com".to_string(),
            text: "page body".to_string(),
        }];
        let prompt = build_fact_prompt("shipped the release notes", &fetched, None);
        assert!(prompt.contains("page body"));
        assert!(prompt.contains("https://example.com"));
    }

    #[test]
    fn demonstrate_prompt_lists_candidate_ids_and_asks_for_a_subset() {
        let candidates = vec![
            DemonstrateCandidate {
                id: "bitacora/2026/July/fact-a".to_string(),
                title: "Fixed the outage".to_string(),
                description: "Diagnosed and rolled back the bad deploy.".to_string(),
            },
            DemonstrateCandidate {
                id: "bitacora/2026/July/fact-b".to_string(),
                title: "Unrelated errand".to_string(),
                description: "Bought milk.".to_string(),
            },
        ];
        let prompt = build_demonstrate_prompt("run a marathon", &candidates);
        assert!(prompt.contains("run a marathon"));
        assert!(prompt.contains("bitacora/2026/July/fact-a"));
        assert!(prompt.contains("bitacora/2026/July/fact-b"));
        assert!(prompt.contains("\"demonstrating\""));
    }

    #[test]
    fn filter_known_ids_drops_ids_outside_the_candidate_set() {
        let candidates = vec![DemonstrateCandidate {
            id: "bitacora/2026/July/fact-a".to_string(),
            title: "t".to_string(),
            description: "d".to_string(),
        }];
        let kept = filter_known_ids(
            &candidates,
            vec![
                "bitacora/2026/July/fact-a".to_string(),
                "bitacora/2026/July/hallucinated".to_string(),
            ],
        );
        assert_eq!(kept, vec!["bitacora/2026/July/fact-a".to_string()]);
    }

    #[test]
    fn filter_known_ids_empty_input_stays_empty() {
        let candidates = vec![DemonstrateCandidate {
            id: "a".to_string(),
            title: "t".to_string(),
            description: "d".to_string(),
        }];
        assert!(filter_known_ids(&candidates, vec![]).is_empty());
    }

    #[test]
    fn strip_code_fence_removes_json_fence() {
        let input = "```json\n{\"a\": 1}\n```";
        assert_eq!(strip_code_fence(input), "{\"a\": 1}");
    }

    #[test]
    fn strip_code_fence_passes_through_plain_json() {
        let input = "{\"a\": 1}";
        assert_eq!(strip_code_fence(input), "{\"a\": 1}");
    }

    /// Existing `job_history` entries were written before `cache_creation`/
    /// `cache_read` existed on `TokenUsage`; they must keep deserializing
    /// (defaulting the new fields to 0) instead of breaking `/history`.
    #[test]
    fn token_usage_deserializes_old_record_missing_cache_fields() {
        let raw = r#"{"input": 100, "output": 50}"#;
        let usage: TokenUsage = serde_json::from_str(raw).unwrap();
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.cache_creation, 0);
        assert_eq!(usage.cache_read, 0);
    }

    /// Manual verification only, same reasoning as
    /// `agent::tests::openshell_smoke_test_mtls` -- separate
    /// `OPENSHELL_SMOKE_CODEX_*` env vars so this can never accidentally
    /// point at production config. `OPENSHELL_SMOKE_CODEX_MTLS_DIR` is
    /// optional -- most local gateways run with `--enable-mtls-auth` on (see
    /// `docs/openshell.md`), so it's needed in practice, but left optional
    /// here for a genuinely anonymous-TLS gateway. Run explicitly with:
    ///   cargo test -p kamaji-core --ignored -- codex_smoke_test_agent_query
    #[tokio::test]
    #[ignore = "requires a real OpenShell gateway + a sandbox with Codex/DeepSeek configured"]
    async fn codex_smoke_test_agent_query() {
        let gateway_url =
            std::env::var("OPENSHELL_SMOKE_CODEX_GATEWAY_URL").expect("set for manual runs");
        let sandbox_name =
            std::env::var("OPENSHELL_SMOKE_CODEX_SANDBOX_NAME").expect("set for manual runs");
        let codex_bin =
            std::env::var("OPENSHELL_SMOKE_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        let mtls = std::env::var("OPENSHELL_SMOKE_CODEX_MTLS_DIR")
            .ok()
            .map(|dir| {
                let dir = std::path::PathBuf::from(dir);
                crate::config::OpenShellMtlsConfig {
                    ca_cert_path: dir.join("ca.crt"),
                    client_cert_path: dir.join("tls.crt"),
                    client_key_path: dir.join("tls.key"),
                }
            });

        let runner = agent::Runner::connect(Some(&crate::config::OpenShellConfig {
            gateway_url,
            sandbox_name,
            ready_timeout: Duration::from_secs(30),
            mtls,
        }))
        .await
        .expect("gateway should be reachable and sandbox should become ready");

        let (reply, _tokens) = run_agent_query(
            AgentFlavor::Codex,
            &runner,
            &codex_bin,
            Duration::from_secs(60),
            "say hello",
        )
        .await
        .expect("codex query should round-trip through the gateway");
        assert!(!reply.trim().is_empty());
    }
}
