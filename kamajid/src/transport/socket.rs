use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kamaji_core::chat::{ChatRef, MessageRef};
use kamaji_core::error::IpcError;
use kamaji_core::ipc::{self, CliRequest, CliResponse};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::error::SocketError;
use crate::state::DaemonState;
use crate::transport;

/// Correlates a CLI socket connection waiting on a queued job's reply with
/// the worker task that eventually produces it. Keyed by `request_id`, an
/// id namespace independent of `Queue`'s job ids -- registered before
/// `transport::dispatch_routed_job` enqueues the job, resolved by
/// `transport::send_reply`'s `ChatRef::Cli` arm once the worker calls it.
#[derive(Default)]
pub struct WaiterRegistry {
    inner: Mutex<HashMap<u64, oneshot::Sender<String>>>,
    next_id: AtomicU64,
}

impl WaiterRegistry {
    pub fn new() -> Self {
        WaiterRegistry {
            inner: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Registers a new waiter and returns its id plus the receiving half.
    pub fn register(&self) -> (u64, oneshot::Receiver<String>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        match self.inner.lock() {
            Ok(mut guard) => {
                guard.insert(id, tx);
            }
            Err(poisoned) => {
                tracing::warn!("waiter registry lock was poisoned, recovering");
                poisoned.into_inner().insert(id, tx);
            }
        }
        (id, rx)
    }

    /// Delivers `text` to the waiter registered under `id`, if it's still
    /// there. A missing id (or a send that fails because the receiver was
    /// dropped) means the CLI process that registered it is gone -- killed,
    /// or its own timeout already fired. That's a fine outcome: the job
    /// still completed durably and landed in `job_history`, there's just
    /// nobody left listening for the synchronous reply, so this only logs.
    pub fn deliver(&self, id: u64, text: String) {
        let sender = match self.inner.lock() {
            Ok(mut guard) => guard.remove(&id),
            Err(poisoned) => {
                tracing::warn!("waiter registry lock was poisoned, recovering");
                poisoned.into_inner().remove(&id)
            }
        };
        match sender {
            Some(tx) => {
                let _ = tx.send(text);
            }
            None => {
                tracing::debug!(
                    request_id = id,
                    "no waiter registered for cli reply, dropping"
                );
            }
        }
    }
}

/// Binds the CLI socket and runs the accept loop until the process exits.
/// A bind failure is logged and this simply returns rather than taking the
/// whole daemon down -- Telegram/Matrix keep working, only CLI support is
/// unavailable for this run.
pub async fn run(state: Arc<DaemonState>) {
    let socket_path = state.core.config.socket_path.clone();
    let listener = match bind_socket(&socket_path).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%err, "failed to bind cli socket, cli support disabled for this run");
            return;
        }
    };
    tracing::info!(path = %socket_path.display(), "listening for cli connections");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    handle_connection(stream, state).await;
                });
            }
            Err(err) => {
                tracing::error!(%err, "failed to accept cli connection");
            }
        }
    }
}

/// Binds a `UnixListener` at `path`. If a socket file already exists there,
/// first tries connecting to it: success means another `kamajid` is already
/// listening (fail fast, don't steal the socket); failure means it's a
/// stale file left over from an unclean shutdown, safe to remove and rebind.
async fn bind_socket(path: &Path) -> Result<UnixListener, SocketError> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(SocketError::AlreadyRunning(path.to_path_buf()));
        }
        std::fs::remove_file(path).map_err(|source| SocketError::RemoveStale {
            path: path.to_path_buf(),
            source,
        })?;
    }

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| SocketError::Bind {
            path: path.to_path_buf(),
            source,
        })?;
    }

    UnixListener::bind(path).map_err(|source| SocketError::Bind {
        path: path.to_path_buf(),
        source,
    })
}

/// Handles one CLI connection end to end: read a `CliRequest`, run it
/// through the shared `transport::run_cli_style_request` (same
/// register-waiter/dispatch/await-with-timeout sequence the REST API uses),
/// write the `CliResponse` back.
async fn handle_connection(stream: UnixStream, state: Arc<DaemonState>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let request: CliRequest = match ipc::read_message(&mut reader).await {
        Ok(request) => request,
        Err(IpcError::UnexpectedEof) => return,
        Err(err) => {
            tracing::warn!(%err, "failed to read cli request");
            let _ = ipc::write_message(
                &mut write_half,
                &CliResponse {
                    ok: false,
                    text: format!("malformed request: {err}"),
                },
            )
            .await;
            return;
        }
    };

    let response = transport::run_cli_style_request(&state, request, |request_id| {
        (ChatRef::Cli { request_id }, MessageRef::Cli { request_id })
    })
    .await;

    if let Err(err) = ipc::write_message(&mut write_half, &response).await {
        tracing::warn!(%err, "failed to write cli response");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[tokio::test]
    async fn waiter_registry_delivers_registered_reply() {
        let registry = WaiterRegistry::new();
        let (id, rx) = registry.register();
        registry.deliver(id, "hello".to_string());
        assert_eq!(rx.await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn waiter_registry_delivering_to_unknown_id_does_not_panic() {
        let registry = WaiterRegistry::new();
        registry.deliver(999, "orphaned".to_string());
    }

    #[tokio::test]
    async fn waiter_registry_assigns_unique_ids() {
        let registry = WaiterRegistry::new();
        let (id1, _rx1) = registry.register();
        let (id2, _rx2) = registry.register();
        assert_ne!(id1, id2);
    }

    #[test]
    fn bare_relative_socket_path_has_no_real_parent_directory() {
        // `bind_socket` skips `create_dir_all` when the parent component is
        // empty -- true for the default "kamaji.sock" (cwd-relative), so a
        // fresh checkout with no `KAMAJI_SOCKET_PATH` override never needs a
        // directory to exist first.
        let path = PathBuf::from("kamaji.sock");
        let real_parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        assert!(real_parent.is_none());
    }
}
