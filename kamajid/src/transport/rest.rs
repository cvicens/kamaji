use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use kamaji_core::chat::{ChatRef, MessageRef};
use kamaji_core::ipc::{CliRequest, CliResponse};
use serde::{Deserialize, Serialize};
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::SmartIpKeyExtractor;
use tower_governor::GovernorLayer;

use crate::state::DaemonState;
use crate::transport;

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

enum ApiError {
    InvalidTotp,
    Unauthorized,
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::InvalidTotp => (StatusCode::UNAUTHORIZED, "invalid totp code"),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token"),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
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
    let token = bearer_token(&headers).ok_or(ApiError::Unauthorized)?;
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
