use std::sync::Arc;

use teloxide::types::UserId;
use teloxide::Bot;

use crate::matrix_client::MatrixClient;
use crate::transport::socket::WaiterRegistry;

/// The Telegram side of `DaemonState`, wrapping the client handle and its
/// own bot id (needed for the bot-self-filter guardrail in
/// `transport::telegram`).
pub struct TelegramClient {
    pub bot: Bot,
    pub bot_id: UserId,
}

/// Daemon-wide state: the platform-agnostic `kamaji_core::AppState` (config,
/// queue, last-note cache) plus everything that's inherently daemon-only --
/// the Telegram/Matrix client handles and the CLI socket's waiter registry.
/// `telegram`/`matrix` are both optional because either transport can run
/// without the other: `matrix` is `None` whenever `MATRIX_HOMESERVER_URL`
/// isn't configured, which keeps the daemon's behavior identical to before
/// Matrix support existed.
pub struct DaemonState {
    pub core: Arc<kamaji_core::state::AppState>,
    pub telegram: Option<TelegramClient>,
    pub matrix: Option<MatrixClient>,
    pub waiters: WaiterRegistry,
}
