use std::path::{Path, PathBuf};

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::{AuthSession, SessionTokens};
use matrix_sdk::ruma::{OwnedDeviceId, OwnedUserId, UserId};
use matrix_sdk::{Client, SessionChange, SessionMeta};
use serde::{Deserialize, Serialize};

use kamaji_core::config::MatrixConfig;

use crate::error::MatrixClientError;

pub struct MatrixClient {
    pub client: Client,
    pub user_id: OwnedUserId,
}

/// Session tokens matrix-sdk's own sqlite store doesn't durably own on its
/// own -- the SDK expects the host application to persist and restore these
/// itself (see `Client::session`/`Client::restore_session`), which is what
/// docs/matrix.md step 9 means by "wire up a session store, not a bare
/// access token": without this file, a restart would fall back to the
/// original bootstrap `access_token`, discarding any refresh-token rotation
/// that happened since.
#[derive(Serialize, Deserialize)]
struct PersistedSession {
    user_id: String,
    device_id: String,
    access_token: String,
    refresh_token: Option<String>,
}

/// Builds (or restores) the matrix-sdk client. First run: bootstraps from
/// the `access_token`/`device_id` obtained via the manual UIA registration
/// (docs/matrix.md step 9). Every run after that: restores from the session
/// file this function itself maintains, which stays current across
/// refresh-token rotations via `subscribe_to_session_changes`.
pub async fn build(config: &MatrixConfig) -> Result<MatrixClient, MatrixClientError> {
    std::fs::create_dir_all(&config.store_path).map_err(|source| {
        MatrixClientError::CreateStoreDir {
            path: config.store_path.clone(),
            source,
        }
    })?;
    let session_file = config.store_path.join("session.json");

    let client = Client::builder()
        .homeserver_url(&config.homeserver_url)
        .sqlite_store(&config.store_path, None)
        .handle_refresh_tokens()
        .build()
        .await
        .map_err(|err| MatrixClientError::Build(Box::new(err)))?;

    let persisted = load_persisted_session(&session_file).unwrap_or(PersistedSession {
        user_id: config.user_id.clone(),
        device_id: config.device_id.clone(),
        access_token: config.access_token.clone(),
        refresh_token: None,
    });

    let user_id = UserId::parse(persisted.user_id.as_str())
        .map_err(MatrixClientError::ParseUserId)?
        .to_owned();
    let device_id: OwnedDeviceId = persisted.device_id.as_str().into();

    let session = MatrixSession {
        meta: SessionMeta {
            user_id: user_id.clone(),
            device_id,
        },
        tokens: SessionTokens {
            access_token: persisted.access_token,
            refresh_token: persisted.refresh_token,
        },
    };
    client
        .restore_session(session)
        .await
        .map_err(MatrixClientError::RestoreSession)?;
    persist_current_session(&client, &session_file)?;

    spawn_session_change_listener(client.clone(), session_file);

    Ok(MatrixClient { client, user_id })
}

/// Rewrites the session file whenever matrix-sdk rotates the access/refresh
/// tokens, so the next restart picks up the latest ones instead of the
/// original bootstrap token. Runs for the lifetime of the process; a failure
/// here only means the *next* restart re-uses a stale (but still currently
/// valid) token, not a reason to take the daemon down, so it logs and
/// continues rather than propagating.
fn spawn_session_change_listener(client: Client, session_file: PathBuf) {
    let mut changes = client.subscribe_to_session_changes();
    tokio::spawn(async move {
        while let Ok(change) = changes.recv().await {
            if matches!(change, SessionChange::TokensRefreshed) {
                if let Err(err) = persist_current_session(&client, &session_file) {
                    tracing::error!(%err, "failed to persist refreshed matrix session");
                }
            }
        }
    });
}

fn load_persisted_session(path: &Path) -> Option<PersistedSession> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn persist_current_session(client: &Client, path: &Path) -> Result<(), MatrixClientError> {
    let Some(AuthSession::Matrix(session)) = client.session() else {
        return Ok(());
    };
    let persisted = PersistedSession {
        user_id: session.meta.user_id.to_string(),
        device_id: session.meta.device_id.to_string(),
        access_token: session.tokens.access_token,
        refresh_token: session.tokens.refresh_token,
    };
    let json = serde_json::to_vec_pretty(&persisted).map_err(MatrixClientError::Serialize)?;
    std::fs::write(path, json).map_err(|source| MatrixClientError::WriteSessionFile {
        path: path.to_path_buf(),
        source,
    })
}
