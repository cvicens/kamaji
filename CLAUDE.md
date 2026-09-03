# CLAUDE.md

## Project
A Rust daemon on a Hetzner VM. Messages (from Telegram, and optionally Matrix — see Stack below) route to one of two paths: no command → ingest (summarize, score, tag, write a markdown note, commit, push); `/command` → dispatch to a named handler or reply with an error. Single VM, systemd-managed, no external services besides the two chat platforms (and the self-hosted Matrix homeserver, if enabled).

## Stack — do not swap without an explicit request
- Telegram: `teloxide`, long polling (no webhook).
- Matrix (added, not a swap): `matrix-sdk`, self-hosted Tuwunel homeserver (see `docs/matrix.md`). Runs alongside Telegram, both feeding the same `Queue`/worker. Opt-in via `MATRIX_HOMESERVER_URL`; unset means Matrix is fully off and behavior is Telegram-only, byte-for-byte as before.
- Persistence: `redb` (pure-Rust embedded KV, ACID/WAL). Never introduce sqlx, rusqlite, or Turso — deliberately avoided for an unattended daemon (C-wrapped or beta). **Exception, scoped narrowly:** `matrix-sdk`'s `sqlite` feature pulls in `rusqlite` transitively for its own session/crypto store — accepted because matrix-sdk has no non-WASM persistent store backend other than sqlite, and losing session persistence breaks refresh-token handling (Tuwunel's `access_token_ttl` is 7 days). This is contained to matrix-sdk's internals; kamaji's own queue/job_history/dedupe persistence remains 100% redb, untouched.
- Runtime: tokio.
- Claude invocation: `tokio::process::Command` (or, when the OpenShell runner is active, a gRPC `exec` call — see below) → `claude -p "..." --output-format json`, parsed with `serde_json`. Never use `--dangerously-skip-permissions`. **Landed:** a pluggable runner layer in `kamaji-core/src/agent.rs` — `agent::Runner::Direct` | `agent::Runner::OpenShell`, orthogonal to the *agent* axis. The agent axis itself is now multi-flavor: `kamaji-core/src/prompt.rs` owns the prompt/schema contract (ingest/fact/query prompts, strict-JSON parsing, `IngestResult`/`FactResult`/`TokenUsage`) and is the only module that knows more than one agent flavor exists; `claude.rs` and `codex.rs` are peers, each knowing only how to invoke its own binary and parse its own wire format into the shared `AgentEnvelope` shape `prompt.rs` consumes (`claude::invoke_claude` parses Claude's single JSON envelope directly; `codex::invoke_codex` parses Codex's `--json` JSONL event stream, extracting the `item.completed`/`agent_message` event). Selected by `AGENT_FLAVOR` (`claude` default, or `codex`); `CLAUDE_BIN`/`CODEX_BIN` (defaults `"claude"`/`"codex"`) resolve to one `Config::agent_bin` at startup based on the active flavor. Switching to `codex` only makes sense alongside repointing `OPENSHELL_SANDBOX_NAME` at a sandbox that actually has Codex/DeepSeek configured (e.g. `codex-deepseek-v2`, see `docs/openshell.md`) — kamaji doesn't validate that pairing, same as it never validated `CLAUDE_BIN` pointing at something real. `Runner::OpenShell` execs against a **pre-provisioned** [OpenShell](https://docs.nvidia.com/openshell/latest/about/overview.md) sandbox via NVIDIA's `openshell-sdk` gRPC client (not the `openshell` CLI as a subprocess — this is how OpenShell's own TUI talks to a gateway, and it resolves what would otherwise be an open question about whether a CLI's stdin/stdout/exit-code passthrough is transparent, since stdin here is a typed RPC field, not an OS pipe). kamaji never creates or deletes the sandbox — only `exec`s into it, plus one startup `wait_ready` check — provisioning is out-of-band (see `docs/openshell.md`'s "Full rebuild" pattern; Claude Code has full default OpenShell policy coverage for `api.anthropic.com`, and Claude's own invocation never touches the filesystem or git, so no custom policy/provider is needed). Opt-in via `OPENSHELL_GATEWAY_URL` (+ `OPENSHELL_SANDBOX_NAME`, `OPENSHELL_READY_TIMEOUT_SECS`); unset means `Runner::Direct`, byte-for-byte prior behavior — same opt-in convention as Matrix/REST/Telegram. **Exception, scoped narrowly:** `openshell-sdk` is not published to crates.io (confirmed 404 there) — pinned via a `git`/`rev` dependency with no SemVer safety net (see the `Cargo.toml` comment above the dependency for the bump process). It's scoped to `kamaji-core` only (not `kamajid`/`kamaji`), mirroring how the matrix-sdk/sqlite exception below is contained. It also pulls in `prost`/`protobuf-src` (vendors and compiles protobuf's C++ source at build time — needs a C/C++ toolchain, not a system `protoc` package) and transitively enables `openshell-core`'s default `telemetry` feature (anonymous usage reporting to an NVIDIA endpoint, un-disableable from kamaji-core's `Cargo.toml` due to Cargo feature unification); reading `openshell-sdk`'s own source found no call site that invokes that emission path, so it's inert dead weight in the dependency tree for kamaji's usage, not a live behavior. **Auth:** two paths, gated on the optional `OPENSHELL_MTLS_DIR`. Unset (`OpenShellConfig::mtls: None`) is `openshell_sdk::ClientConfig`'s anonymous TLS/plaintext path, the original v1 design for a gateway with `--enable-mtls-auth=false`. Set (pointing at a directory with `ca.crt`/`tls.crt`/`tls.key`, the same three filenames `openshell-cli` itself uses) matches this gateway's actual default posture (mTLS on for local single-user Docker/Podman/VM gateways) — `openshell_sdk`'s own `ClientConfig`/`connect()` has no client-cert support at all (`transport.rs`: *"mTLS is intentionally out of scope here... handled by `openshell-cli`'s legacy path"*), so `agent::connect_mtls` hand-builds a `tonic::transport::Channel` with a client `Identity` (same approach as `openshell-cli/src/tls.rs`) and hands it to `OpenShellClient::from_parts`, the SDK's documented escape hatch for exactly this. `tonic` is therefore a **direct** `kamaji-core` dependency too (previously only transitive via `openshell-sdk`), pinned to the version `openshell-sdk` already resolves, `tls-aws-lc` feature (kamaji-core always pins its own CA explicitly, never system roots; matches the `aws-lc-rs` `CryptoProvider` installed as process default in `kamajid::main()`). kamaji never mints its own client cert — it's a copy of the gateway's existing local identity, provisioned out-of-band (see `docs/openshell.md`). `openshell_sdk::AuthConfig::EdgeJwt`/`Oidc` remain explicit future scope (see TODO.md) for if the gateway ever moves off a trusted local network.
- Notes: plain markdown, git-committed. Frontmatter is [OKF](https://github.com/GoogleCloudPlatform/knowledge-catalog/tree/main/okf)-conformant (rendered via `kamaji-core/src/okf.rs`): the OKF core fields (`type`, `title`, `description`, `resource`, `tags`, `timestamp`) plus kamaji's own fields carried as OKF custom fields (`category`/`importance` for notes, `value`/`attachment` for facts). `type` is the only OKF-required field; keep it non-empty. `description` is derived in Rust (first sentence of `summary`) specifically so the strict-JSON ingest/fact contract below stays untouched — don't add it to the Claude prompt without an explicit request. Still Obsidian-compatible; no vault-specific folder structure — don't add one without being asked.
- Supervision: systemd, `Restart=always`.
- Web UI (`/app`): one self-contained `kamajid/src/transport/todo_app.html`,
  `include_str!`-embedded (no runtime asset directory to deploy or get the path
  wrong for). Rendered with **Preact + htm**, vendored inline as the
  `htm@3.1.1/preact/standalone.umd.js` UMD bundle (~13KB, `htmPreact` global) --
  deliberately *not* CDN-loaded, so the page keeps working with zero browser
  egress to anything but this origin, and *not* a Vite/React SPA, which would
  put node in the build and break the single-binary deploy for a single-user
  todo list. htm is tagged-template JSX-equivalent, so there is **no build
  step**: edit the HTML, rebuild the crate, done. To bump the bundle, re-fetch
  that exact pinned URL and update the sha256 in the comment above it. Keep the
  component tree keyed by the item's stable id (`itemId`: `EntryKey` for a
  todo/goal, `wikilink_target` for a fact) -- that keyed diffing is the whole
  reason the layer exists (an open edit box must survive a list re-render with
  its focus and caret intact; the focus half needs `useAutoFocus`, since
  `autofocus` is only honoured at parse time and every inline editor here is
  inserted into an already-rendered list).
  One page, **three domains** (todos, goals, facts), tab-switched. Filename
  predates the other two and is kept deliberately. Two rules hold across all
  three: **reads go straight to the filesystem, writes go through
  `Queue::enqueue` + the single sequential worker** (that split is the "no
  concurrent writes to the notes repo" guardrail, not an optimization); and
  every item dates off one accessor (`itemDate` -- `EntryKey` for a checklist
  entry, `timestamp` frontmatter for a fact), never a per-domain parser. Facts
  have no open/closed status, so they get no checkbox and no completion ring --
  don't render either as a disabled or 0/N placeholder. Creating a fact is the
  one web write that invokes the agent: it reuses the real `/fact` pipeline via
  `run_cli_style_request` (the only helper budgeting `agent_timeout`), never a
  web-only shortcut that hand-authors the fields -- the `.orig` has to keep
  holding the raw message verbatim.

## Data model (redb tables)
- `pending<u64, &str>` — job_id → JSON payload (`Job { chat: ChatRef, reply_to: MessageRef, kind }`), tagged `JobKind::Ingest { raw_text, urls }` or `JobKind::Command { name, args }`. `ChatRef`/`MessageRef` (`src/chat.rs`) are platform-tagged enums (`Telegram { chat_id: i64 }` / `Matrix { room_id: String }`, etc.) — Matrix room/event ids are opaque strings, not integers, so this isn't a bare int field.
- `running<u64, u64>` — job_id → leased_at (unix ts)
- `seen_updates<i64, ()>` — Telegram update_id, dedupe on restart replay
- `seen_matrix_events<&str, ()>` — Matrix event_id, same purpose as `seen_updates` but string-keyed since Matrix has no numeric update-id equivalent

## Routing rule
No leading `/` → ingest job. Leading `/` → look up in the command registry; unknown command replies with an error and does **not** fall through to ingest. Don't blur this line — an unrecognized command silently being treated as "information to file away" is a bug, not graceful degradation. This rule is defined exactly once (`routing::route_message`) and applies identically regardless of which platform the message came from.

## Critical guardrail — this is the load-bearing one
The default (no-command) path is what triggers ingestion, so **the bot-self-filter is the primary loop guardrail**, not a secondary check, **on every platform**: Telegram compares `from.id == bot_id`; Matrix compares the event's `sender` against the bot's own Matrix user id. If the bot's own reply ever lands back in the trigger chat/room unfiltered, it has no command prefix, gets re-ingested as a new note, replies again — infinite loop, infinite commits. Any change that touches message routing (`telegram::handle_update`, `matrix::handle_message`) must keep this filter first in the chain for that platform, and the test for it must stay green.

## Ingest path contract
- Claude's ingest prompt must return strict JSON only: `title`, `summary`, `importance` (integer 1-5), `tags` (freeform array, no fixed taxonomy — don't introduce one), `source_url`, `slug`.
- On JSON parse failure: log the raw output, don't write a note, tell the user (on whichever platform the message came from) that ingestion failed for that message. Don't fail silently.
- `git push` is a fallible network operation: bounded retry with backoff, and if it still fails, the note stays committed locally and the user is told it didn't push — never lose the note itself.

## Other non-negotiables
- One worker, sequential dequeue, shared across both platforms. No concurrent Claude runs against the same working directory without discussing it first.
- Startup recovery: stale `running` jobs (leased_at past timeout) move back to `pending`.
- Dedupe every `update_id` (Telegram) / `event_id` (Matrix) before enqueueing.
- Chat/room allow-list enforced before enqueue; unlisted chats/rooms are dropped silently.

## Rust conventions
- **No `unwrap()` / `expect()` / `panic!()` in any path that runs after startup.** Config/env parsing at process start may `expect()` with a descriptive message — everything in the message/job lifecycle after that returns `Result` and propagates with `?`. A malformed payload, a failed Claude call, or a git error should log and skip that job, not take the daemon down.
- `thiserror` for typed error enums at module boundaries (`queue.rs`, the Claude-invocation module, the git module); `anyhow::Result` only in `main.rs` and top-level orchestration. Don't let `anyhow::Error` leak out of library-ish modules.
- No `unsafe` — nothing here needs it.
- `tracing`, not `println!`/`eprintln!`, structured with fields (`job_id`, `chat_id`) rather than interpolated into the message string.
- Prefer borrowing (`&str`, `&[T]`) over owned types in signatures; `.clone()` only when a second owner is genuinely needed.
- No `.unwrap()` on lock acquisition — decide explicitly what a poisoned lock means for this daemon (log-and-skip vs propagate).
- Timeouts on every external call: the Claude subprocess and the `git push` both need `tokio::time::timeout` — a hang in either must not wedge the single worker forever.
- Tests for queue atomicity (concurrent enqueue/dequeue, crash-recovery simulation) and the bot-self-filter are not optional — these are the two places a bug means silently dropped/duplicated jobs or an infinite loop.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean before any change is considered done.

## Style
- Keep functions small enough that error handling stays legible — split if a function has more than 2-3 unrelated `?` chains.
- Comment *why*, not *what* — especially around the guardrails above; if you touch that code, explain the reason the check exists, not what the line does.
