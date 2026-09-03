use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use kamaji_core::chat::{ChatRef, MessageRef};
use kamaji_core::checklist::{Entry, StatusFilter};
use kamaji_core::ipc::{CliRequest, CliResponse};
use kamaji_core::queue::{ChecklistApiOp, ChecklistDomain, FactApiOp, Job, JobKind};
use kamaji_core::{bitacora, checklist, goal, todo};
use serde::{Deserialize, Serialize};
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::SmartIpKeyExtractor;
use tower_governor::GovernorLayer;

use crate::state::DaemonState;
use crate::transport;

/// The static single-page web UI (see TODO.md's "TODO management web UI"
/// note) -- embedded at compile time rather than read from disk at startup,
/// matching how this daemon has no other runtime-loaded asset directory to
/// manage or get the deploy path wrong for. It serves all three domains
/// (todos, goals, facts) now; the filename predates the other two and is
/// kept so `include_str!`, CLAUDE.md and git history all still agree.
const TODO_APP_HTML: &str = include_str!("todo_app.html");

/// State handed to every REST handler. Built once in `run()`, after
/// confirming `state.core.config.rest_api` is `Some` -- captures the TOTP
/// secret and session TTL directly rather than re-checking the `Option` per
/// request, so handlers never need to `.expect()`/`.unwrap()` a config that
/// "should" be present.
#[derive(Clone)]
struct RestState {
    daemon: Arc<DaemonState>,
    totp_secret: String,
    session_ttl: Duration,
}

/// Binds and serves the REST API. No-ops if `REST_API_BIND` isn't
/// configured, so it's always safe to spawn unconditionally from
/// `main.rs`, mirroring `transport::matrix::run`. `kamajid` only ever binds
/// this to `127.0.0.1` -- a reverse proxy (Caddy) in front of it is what
/// terminates TLS and is actually reachable from the public internet.
pub async fn run(state: Arc<DaemonState>) {
    let Some(rest_cfg) = state.core.config.rest_api.as_ref() else {
        return;
    };
    let bind_addr = rest_cfg.bind_addr;
    let rest_state = RestState {
        daemon: Arc::clone(&state),
        totp_secret: rest_cfg.totp_secret.clone(),
        session_ttl: rest_cfg.session_ttl,
    };

    // A handful of attempts per minute per source IP -- brute-forcing a
    // 6-digit TOTP code needs far more attempts than this allows. Only
    // meaningful because this endpoint is reachable from the public
    // internet (via Caddy); it wouldn't be needed behind a private overlay.
    //
    // `SmartIpKeyExtractor`, not the default `PeerIpKeyExtractor`:
    // `kamajid` is always reached through Caddy (loopback-only bind), so
    // the raw TCP peer IP is always Caddy's own address, which either (a)
    // needs `axum::serve(..., into_make_service_with_connect_info)` just to
    // populate at all -- not wired up here -- or (b) once wired, would
    // still bucket every real caller together under one shared limit.
    // `SmartIpKeyExtractor` reads `X-Forwarded-For` (which Caddy's
    // `reverse_proxy` sets automatically) to key on the actual client IP
    // instead; trusting that header is safe here specifically because
    // Caddy is the only thing that can ever reach this port.
    let governor_conf = match GovernorConfigBuilder::default()
        .per_second(6)
        .burst_size(5)
        .key_extractor(SmartIpKeyExtractor)
        .finish()
    {
        Some(conf) => Arc::new(conf),
        None => {
            tracing::error!("failed to build rate-limit config, rest api disabled for this run");
            return;
        }
    };

    let router = Router::new()
        .route(
            "/auth/login",
            post(login_handler).layer(GovernorLayer {
                config: governor_conf,
            }),
        )
        .route("/api/cli", post(cli_handler))
        .route("/auth/logout", post(logout_handler))
        .route("/app", get(app_handler))
        // Todos and goals are the same `checklist` engine with a different
        // `Config`, so these two route groups differ only in the domain
        // constant threaded through and in the domain's own closing verb
        // (`resolve` vs `achieve`) -- kept as each domain's real vocabulary
        // rather than collapsed to a generic `/close`, which would read
        // wrong in both.
        .route("/api/todos", get(list_todos_handler).post(add_todo_handler))
        .route(
            "/api/todos/:key",
            patch(edit_todo_handler).delete(delete_todo_handler),
        )
        .route("/api/todos/:key/resolve", post(resolve_todo_handler))
        .route("/api/todos/:key/reopen", post(reopen_todo_handler))
        .route("/api/goals", get(list_goals_handler).post(add_goal_handler))
        .route(
            "/api/goals/:key",
            patch(edit_goal_handler).delete(delete_goal_handler),
        )
        .route("/api/goals/:key/achieve", post(achieve_goal_handler))
        .route("/api/goals/:key/reopen", post(reopen_goal_handler))
        // Facts address by path, not by `EntryKey`, so the target rides in
        // the query string -- see `FactTargetQuery`.
        .route(
            "/api/facts",
            get(list_facts_handler)
                .post(create_fact_handler)
                .patch(edit_fact_handler)
                .delete(delete_fact_handler),
        )
        .with_state(rest_state);

    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%err, %bind_addr, "failed to bind rest api listener, rest api disabled for this run");
            return;
        }
    };
    tracing::info!(%bind_addr, "listening for rest api connections");

    if let Err(err) = axum::serve(listener, router).await {
        tracing::error!(%err, "rest api server exited with error");
    }
}

#[derive(Debug)]
enum ApiError {
    InvalidTotp,
    Unauthorized,
    Internal,
    BadRequest(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::InvalidTotp => (StatusCode::UNAUTHORIZED, "invalid totp code".to_string()),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token".to_string(),
            ),
            ApiError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            ),
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

/// Shared bearer-token check for every `/api/*` route (`/api/cli` and every
/// `/api/todos/*` route) -- factored out once a third and fourth call site
/// appeared, rather than repeating `bearer_token(...).ok_or(...)` plus the
/// `validate_session` round trip in each handler.
async fn require_session(state: &RestState, headers: &HeaderMap) -> Result<(), ApiError> {
    let token = bearer_token(headers).ok_or(ApiError::Unauthorized)?;
    let valid = state
        .daemon
        .core
        .sessions
        .validate_session(token)
        .map_err(|err| {
            tracing::error!(%err, "failed to validate rest api session");
            ApiError::Internal
        })?;
    if !valid {
        return Err(ApiError::Unauthorized);
    }
    Ok(())
}

#[derive(Deserialize)]
struct LoginRequest {
    code: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    expires_at_unix: u64,
}

async fn login_handler(
    State(state): State<RestState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    if !state
        .daemon
        .core
        .sessions
        .verify_totp(&state.totp_secret, &body.code)
    {
        // Deliberately no code/secret in this log -- just enough to tell a
        // wrong/replayed code apart from "the request never reached here at
        // all" (e.g. the rate limiter answering first) when debugging a
        // login failure after the fact.
        tracing::warn!("rejected /auth/login: invalid or already-used totp code");
        return Err(ApiError::InvalidTotp);
    }

    let token = state
        .daemon
        .core
        .sessions
        .create_session(state.session_ttl)
        .map_err(|err| {
            tracing::error!(%err, "failed to create rest api session");
            ApiError::Internal
        })?;
    tracing::info!("accepted /auth/login, issued a new session");

    let expires_at_unix = now_unix() + state.session_ttl.as_secs();
    Ok(Json(LoginResponse {
        token,
        expires_at_unix,
    }))
}

async fn cli_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Json(request): Json<CliRequest>,
) -> Result<Json<CliResponse>, ApiError> {
    require_session(&state, &headers).await?;

    let response = transport::run_cli_style_request(&state.daemon, request, |request_id| {
        (
            ChatRef::Rest { request_id },
            MessageRef::Rest { request_id },
        )
    })
    .await;
    Ok(Json(response))
}

/// Invalidates the caller's bearer token server-side. Idempotent by design
/// (`SessionStore::revoke_session` doesn't error on an unknown/already-gone
/// token) -- `kamaji logout` always clears its local cache regardless of
/// whether this call even reaches the daemon, so this just needs to make a
/// still-valid token stop working, not report on prior state.
async fn logout_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Deliberately not `require_session` -- revoking an already-invalid
    // token must still succeed (see the doc comment above), whereas
    // `require_session` would reject the request before it ever reaches
    // `revoke_session`.
    let token = bearer_token(&headers).ok_or(ApiError::Unauthorized)?;
    state
        .daemon
        .core
        .sessions
        .revoke_session(token)
        .map_err(|err| {
            tracing::error!(%err, "failed to revoke rest api session");
            ApiError::Internal
        })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Serves the static todo web UI. Unauthenticated by design -- it's inert
/// HTML/CSS/JS with no embedded secret; the page itself calls `/auth/login`
/// and stores the resulting bearer token client-side (`localStorage`)
/// before any `/api/todos` call, the same TOTP-then-bearer flow `kamaji
/// login`/`/api/cli` already use.
async fn app_handler() -> Html<&'static str> {
    Html(TODO_APP_HTML)
}

#[derive(Deserialize)]
struct ListEntriesQuery {
    status: Option<String>,
}

/// Which `StatusFilter` reads a `?status=` value maps to. Pure and separate
/// from the handler so it can be unit-tested without a `DaemonState`, the
/// same split `bearer_token` already uses.
fn status_filters(param: Option<&str>) -> Result<&'static [StatusFilter], ApiError> {
    match param {
        None | Some("open") => Ok(&[StatusFilter::Open]),
        Some("closed") => Ok(&[StatusFilter::Closed]),
        Some("all") => Ok(&[StatusFilter::Open, StatusFilter::Closed]),
        Some(other) => Err(ApiError::BadRequest(format!(
            "status must be 'open', 'closed' or 'all', got '{other}'"
        ))),
    }
}

/// The `checklist::Config` behind each domain's routes. `rest.rs` needs it
/// only to *read* (writes go through the queue and are resolved again in
/// `worker::checklist_api_job`), so this stays a one-line lookup rather than
/// a shared type between the two crates.
fn domain_cfg(domain: ChecklistDomain) -> checklist::Config {
    match domain {
        ChecklistDomain::Todo => todo::CFG,
        ChecklistDomain::Goal => goal::CFG,
    }
}

/// Shared list handler for both checklist domains.
///
/// A direct filesystem read, not routed through the queue -- same
/// "reads don't need write-serialization" precedent `/status`/`/history`
/// already set for `CommandMode::Sync` commands (see commands.rs).
async fn list_entries(
    state: &RestState,
    headers: &HeaderMap,
    domain: ChecklistDomain,
    params: &ListEntriesQuery,
) -> Result<Json<Vec<Entry>>, ApiError> {
    require_session(state, headers).await?;
    // `all` is a REST-only concept, deliberately *not* a new
    // `StatusFilter` variant: `StatusFilter` is the parsed shape of a chat
    // `/todo list [open|close]` argument (see its doc comment), and there's
    // no "both" to type there. The web UI needs both halves in one response
    // to render the completion score (done / total for the selected day or
    // month), so it's composed here from the two existing reads instead of
    // pushing an unreachable variant down into `checklist.rs`.
    let repo = &state.daemon.core.config.notes_repo_path;
    let filters = status_filters(params.status.as_deref())?;
    let cfg = domain_cfg(domain);

    let mut entries = Vec::new();
    for filter in filters {
        entries.extend(checklist::list_entries(&cfg, repo, *filter).map_err(|err| {
            tracing::error!(%err, folder = cfg.folder, "failed to list entries for web api");
            ApiError::Internal
        })?);
    }
    // Concatenating two already-sorted halves leaves them interleaved
    // wrongly, so re-sort into the single chronological order every caller
    // of this endpoint already expects from `list_entries`.
    entries.sort_by_key(|e| e.key);
    Ok(Json(entries))
}

async fn list_todos_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Query(params): Query<ListEntriesQuery>,
) -> Result<Json<Vec<Entry>>, ApiError> {
    list_entries(&state, &headers, ChecklistDomain::Todo, &params).await
}

async fn list_goals_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Query(params): Query<ListEntriesQuery>,
) -> Result<Json<Vec<Entry>>, ApiError> {
    list_entries(&state, &headers, ChecklistDomain::Goal, &params).await
}

/// Enqueues a `JobKind::ChecklistApi` write and awaits its JSON reply --
/// deliberately bypasses `transport::dispatch_routed_job`/
/// `run_cli_style_request`: those require the job to name a
/// `commands::COMMANDS`-registered command, which `ChecklistApiOp`
/// intentionally isn't (see its doc comment -- `Edit`/`Delete` in particular
/// must stay unreachable from chat). Still goes through the exact same
/// `Queue::enqueue` + single sequential worker + `WaiterRegistry` machinery
/// `run_cli_style_request` uses, so the "no concurrent writes to the notes
/// repo" guardrail applies identically.
async fn run_checklist_api_op(
    state: &RestState,
    domain: ChecklistDomain,
    op: ChecklistApiOp,
) -> Result<Response, ApiError> {
    run_queued_write(state, |request_id| Job {
        chat: ChatRef::Rest { request_id },
        reply_to: MessageRef::Rest { request_id },
        kind: JobKind::ChecklistApi { domain, op },
    })
    .await
}

/// The enqueue-and-wait tail shared by every web write (checklist and fact
/// alike): register a waiter, enqueue a job built around its request id,
/// and hand the worker's JSON reply straight back to the browser.
async fn run_queued_write(
    state: &RestState,
    build: impl FnOnce(u64) -> Job,
) -> Result<Response, ApiError> {
    let (request_id, rx) = state.daemon.waiters.register();
    let job = build(request_id);
    if let Err(err) = state.daemon.core.queue.enqueue(&job) {
        tracing::error!(%err, "failed to enqueue web api write job");
        return Err(ApiError::Internal);
    }

    let timeout = state.daemon.core.config.git_timeout + Duration::from_secs(15);
    let body = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(json)) => json,
        Ok(Err(_)) => {
            tracing::error!("kamajid closed the web api reply channel before answering");
            return Err(ApiError::Internal);
        }
        Err(_) => {
            tracing::error!("timed out waiting for kamajid to process a web api request");
            return Err(ApiError::Internal);
        }
    };
    // Passed straight through rather than deserialized into a Rust type on
    // this side: the JSON was already produced by
    // `kamaji_core::worker::{checklist_api_job, fact_api_job}` for exactly
    // this purpose, and the browser is the only real consumer -- round-tripping
    // through a duplicate struct here would just be a second place the wire
    // shape could drift out of sync.
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

#[derive(Deserialize)]
struct AddEntryRequest {
    text: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct EditEntryRequest {
    text: String,
    #[serde(default)]
    tags: Vec<String>,
}

async fn add_entry_op(
    state: &RestState,
    headers: &HeaderMap,
    domain: ChecklistDomain,
    body: AddEntryRequest,
) -> Result<Response, ApiError> {
    require_session(state, headers).await?;
    run_checklist_api_op(
        state,
        domain,
        ChecklistApiOp::Add {
            text: body.text,
            tags: body.tags,
        },
    )
    .await
}

async fn edit_entry_op(
    state: &RestState,
    headers: &HeaderMap,
    domain: ChecklistDomain,
    key: String,
    body: EditEntryRequest,
) -> Result<Response, ApiError> {
    require_session(state, headers).await?;
    run_checklist_api_op(
        state,
        domain,
        ChecklistApiOp::Edit {
            key,
            text: body.text,
            tags: body.tags,
        },
    )
    .await
}

async fn status_op(
    state: &RestState,
    headers: &HeaderMap,
    domain: ChecklistDomain,
    key: String,
    close: bool,
) -> Result<Response, ApiError> {
    require_session(state, headers).await?;
    let op = if close {
        ChecklistApiOp::Resolve { key }
    } else {
        ChecklistApiOp::Reopen { key }
    };
    run_checklist_api_op(state, domain, op).await
}

/// Permanently deletes a checklist entry -- the one operation among these
/// that has no resolve/reopen-style undo (see `ChecklistApiOp::Delete`'s doc
/// comment). The warning-before-you-click responsibility lives in
/// `todo_app.html`; the *refusal* to delete a goal that todos still link to
/// lives in `worker::checklist_api_job`, so it holds regardless of which
/// client is calling. This handler enforces nothing extra beyond the same
/// session check every other write here requires.
async fn delete_entry_op(
    state: &RestState,
    headers: &HeaderMap,
    domain: ChecklistDomain,
    key: String,
) -> Result<Response, ApiError> {
    require_session(state, headers).await?;
    run_checklist_api_op(state, domain, ChecklistApiOp::Delete { key }).await
}

async fn add_todo_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Json(body): Json<AddEntryRequest>,
) -> Result<Response, ApiError> {
    add_entry_op(&state, &headers, ChecklistDomain::Todo, body).await
}

async fn add_goal_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Json(body): Json<AddEntryRequest>,
) -> Result<Response, ApiError> {
    add_entry_op(&state, &headers, ChecklistDomain::Goal, body).await
}

async fn edit_todo_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(body): Json<EditEntryRequest>,
) -> Result<Response, ApiError> {
    edit_entry_op(&state, &headers, ChecklistDomain::Todo, key, body).await
}

async fn edit_goal_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Json(body): Json<EditEntryRequest>,
) -> Result<Response, ApiError> {
    edit_entry_op(&state, &headers, ChecklistDomain::Goal, key, body).await
}

async fn resolve_todo_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    status_op(&state, &headers, ChecklistDomain::Todo, key, true).await
}

async fn achieve_goal_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    status_op(&state, &headers, ChecklistDomain::Goal, key, true).await
}

async fn reopen_todo_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    status_op(&state, &headers, ChecklistDomain::Todo, key, false).await
}

async fn reopen_goal_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    status_op(&state, &headers, ChecklistDomain::Goal, key, false).await
}

async fn delete_todo_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    delete_entry_op(&state, &headers, ChecklistDomain::Todo, key).await
}

async fn delete_goal_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    delete_entry_op(&state, &headers, ChecklistDomain::Goal, key).await
}

/// A fact's identity is its note path relative to the repo root
/// (`bitacora/2026/July/<stamp>-<slug>`), which contains `/`. That rides in
/// the query string rather than a path segment on purpose: percent-encoded
/// slashes inside a single route segment are a well-known source of
/// router-dependent surprise, and this value feeds a filesystem path
/// resolver -- not somewhere to accept ambiguity about what the server
/// actually received. `bitacora::fact_note_path` is still the boundary that
/// validates it; this just makes sure it arrives intact.
#[derive(Deserialize)]
struct FactTargetQuery {
    target: String,
}

/// Direct filesystem read, same precedent as the checklist list endpoints.
/// Returns every fact in full (`FactDetail`), not a summary: the window
/// filters in the UI are a client-side slice of one list, and an edit box
/// pre-filled from an already-loaded row costs no second round trip.
async fn list_facts_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
) -> Result<Json<Vec<bitacora::FactDetail>>, ApiError> {
    require_session(&state, &headers).await?;
    let repo = &state.daemon.core.config.notes_repo_path;
    let facts = bitacora::list_fact_details(repo).map_err(|err| {
        tracing::error!(%err, "failed to list facts for web api");
        ApiError::Internal
    })?;
    Ok(Json(facts))
}

#[derive(Deserialize)]
struct CreateFactRequest {
    text: String,
}

/// Creates a fact from raw text -- the one write on this page that invokes
/// the agent, and the reason it doesn't go through `run_queued_write` like
/// every other one: a fact's `title`/`summary`/`value`/`slug` come from the
/// agent, so this has to run the whole `/fact` pipeline, and
/// `run_cli_style_request` is the only helper that budgets
/// `agent_timeout` on top of `git_timeout` (`run_queued_write`'s
/// git-time-plus-15s would abandon a perfectly healthy agent call).
///
/// Deliberately *the same* `/fact` command path chat and the `kamaji` CLI
/// already take -- not a parallel implementation -- so the `.orig` written
/// here is the user's raw text verbatim, exactly as it is for a fact logged
/// from Telegram. That equivalence is the whole reason creating a fact is
/// offered at all; a web-only shortcut that hand-authored the fields would
/// have nothing to preserve in the `.orig` (see TODO.md).
///
/// One known rough edge, inherited from `/api/cli` rather than introduced
/// here: `CliResponse::ok` reports "a reply came back", not "the job
/// succeeded" (the waiter channel carries only text). A fact whose agent
/// call returned unparseable JSON therefore answers `ok: true` with a
/// message that says it failed -- which is why the UI shows this reply's
/// text verbatim instead of a generic "Added".
async fn create_fact_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Json(body): Json<CreateFactRequest>,
) -> Result<Response, ApiError> {
    require_session(&state, &headers).await?;
    if body.text.trim().is_empty() {
        return Err(ApiError::BadRequest("text must not be empty".to_string()));
    }

    let reply = transport::run_cli_style_request(
        &state.daemon,
        CliRequest::Fact { text: body.text },
        |request_id| {
            (
                ChatRef::Rest { request_id },
                MessageRef::Rest { request_id },
            )
        },
    )
    .await;

    // Reshaped into the `{ok, message}` envelope every other web write
    // returns, so the page has one response contract to handle rather than
    // a special case for this route.
    let envelope = serde_json::json!({ "ok": reply.ok, "message": reply.text });
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        envelope.to_string(),
    )
        .into_response())
}

#[derive(Deserialize)]
struct EditFactRequest {
    title: String,
    summary: String,
    value: i64,
    #[serde(default)]
    tags: Vec<String>,
}

/// `description` is deliberately not accepted from the client: it's derived
/// from `summary` in Rust (`okf::description_from_summary`) and regenerated
/// on every save, so there is nothing for a caller to set.
async fn edit_fact_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Query(query): Query<FactTargetQuery>,
    Json(body): Json<EditFactRequest>,
) -> Result<Response, ApiError> {
    require_session(&state, &headers).await?;
    run_queued_write(&state, |request_id| Job {
        chat: ChatRef::Rest { request_id },
        reply_to: MessageRef::Rest { request_id },
        kind: JobKind::FactApi(FactApiOp::Edit {
            target: query.target,
            title: body.title,
            summary: body.summary,
            value: body.value,
            tags: body.tags,
        }),
    })
    .await
}

/// Permanently deletes a fact: the note, the companion `.orig` and any
/// attachment bytes. Unlike a goal delete, this is *not* refused when
/// something links to it -- a goal's `demonstrated_by` wikilink can't be
/// partially rewritten the way a todo's can, so the accepted outcome is a
/// dangling link that the worker's reply names explicitly (see
/// `worker::fact_api_job`). The confirm dialog in `todo_app.html` says the
/// same thing before the click.
async fn delete_fact_handler(
    State(state): State<RestState>,
    headers: HeaderMap,
    Query(query): Query<FactTargetQuery>,
) -> Result<Response, ApiError> {
    require_session(&state, &headers).await?;
    run_queued_write(&state, |request_id| Job {
        chat: ChatRef::Rest { request_id },
        reply_to: MessageRef::Rest { request_id },
        kind: JobKind::FactApi(FactApiOp::Delete {
            target: query.target,
        }),
    })
    .await
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Full router-level tests (a real `/auth/login` + `/api/cli` round trip
    // via `tower::ServiceExt::oneshot`) would need a full `DaemonState` --
    // `Config`, `Queue`, `SessionStore`, all built from a redb-backed temp
    // dir -- which no other kamajid transport test does either (see
    // `transport::telegram`'s test module comment: exercising the full
    // async handler needs a real runtime and a mocked queue, so those tests
    // stick to the pure logic instead). Same approach here: unit-test
    // `bearer_token`/`ApiError` directly; the full request/response cycle
    // is covered by the manual curl smoke test in `docs/remote-api.md`.

    #[test]
    fn status_filters_maps_each_accepted_value() {
        assert_eq!(status_filters(None).unwrap(), &[StatusFilter::Open]);
        assert_eq!(status_filters(Some("open")).unwrap(), &[StatusFilter::Open]);
        assert_eq!(
            status_filters(Some("closed")).unwrap(),
            &[StatusFilter::Closed]
        );
        // `all` is what the web UI reads to compute its completion score --
        // both halves in one response, open first so the concatenation before
        // the re-sort is deterministic.
        assert_eq!(
            status_filters(Some("all")).unwrap(),
            &[StatusFilter::Open, StatusFilter::Closed]
        );
    }

    #[test]
    fn status_filters_rejects_an_unknown_value() {
        let err = status_filters(Some("bogus")).unwrap_err();
        assert_eq!(err.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn bearer_token_extracts_from_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("abc123"));
    }

    #[test]
    fn bearer_token_rejects_missing_header() {
        let headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn bearer_token_rejects_non_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic abc123".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn api_error_maps_to_expected_status_codes() {
        assert_eq!(
            ApiError::InvalidTotp.into_response().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::Unauthorized.into_response().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::Internal.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
