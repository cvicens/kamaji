#![recursion_limit = "256"]

mod attachment;
mod error;
mod matrix_client;
mod state;
mod transport;
mod worker;

use std::sync::{Arc, Mutex};

use kamaji_core::config::Config;
use kamaji_core::queue::Queue;
use kamaji_core::{commands, db};
use teloxide::prelude::*;

use state::{DaemonState, TelegramClient};
use transport::socket::WaiterRegistry;

/// One-time TOTP enrollment for the REST API (`transport::rest`). Generates
/// a fresh secret, prints the `otpauth://` URI plus a terminal QR code and
/// the raw base32 secret (for apps that only take manual entry), then
/// exits -- never touches `Config::from_env()`/the redb database/any
/// listener, so it works before the rest of the daemon's env vars are even
/// set up.
fn print_totp_setup() {
    use rand::RngCore;
    use totp_rs::{Algorithm, Secret, TOTP};

    let mut raw = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut raw);
    let secret_base32 = match Secret::Raw(raw.to_vec()).to_encoded() {
        Secret::Encoded(s) => s,
        Secret::Raw(_) => unreachable!("to_encoded always returns Secret::Encoded"),
    };

    let totp = match TOTP::new(
        Algorithm::SHA1,
        6,
        1,
        30,
        raw.to_vec(),
        Some("kamaji".to_string()),
        "kamaji".to_string(),
    ) {
        Ok(totp) => totp,
        Err(err) => {
            eprintln!("failed to build totp url: {err}");
            return;
        }
    };
    let url = totp.get_url();

    println!("Scan this QR code with your authenticator app:\n");
    match qrcode::QrCode::new(&url) {
        Ok(code) => {
            // `qrcode`'s default 4-module quiet zone renders as ~2 blank
            // text rows top/bottom with the Dense1x2 (2-modules-per-row)
            // pixel type -- purely cosmetic in a terminal, which already
            // has its own surrounding whitespace, so it's turned off here.
            let ascii = code
                .render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(false)
                .build();
            println!("{ascii}");
        }
        Err(err) => eprintln!("failed to render qr code: {err}"),
    }
    println!("Or enter this secret manually: {secret_base32}");
    println!("\nSet this in kamajid's environment (e.g. the systemd EnvironmentFile):");
    println!("REST_API_TOTP_SECRET={secret_base32}");
}

/// Admin escape hatch for "I lost the device, kill every remote REST
/// session now" -- there's no way to identify a specific token as "the
/// leaked one" from the daemon side (see `kamaji logout` for revoking just
/// one), so this is all-or-nothing. Only needs `REDB_PATH` (same default as
/// `Config::from_env()`), read directly here rather than requiring the rest
/// of the daemon's env vars (bot token, notes repo, etc.) to be set for what
/// is otherwise an unrelated one-off operation. Doesn't require the daemon
/// to be running.
fn revoke_all_sessions() {
    let redb_path = std::env::var("REDB_PATH").unwrap_or_else(|_| "kamaji.redb".to_string());
    let database = Arc::new(db::open(std::path::Path::new(&redb_path)));
    let sessions = kamaji_core::auth::SessionStore::new(database);
    match sessions.revoke_all_sessions() {
        Ok(count) => println!("revoked {count} session(s)"),
        Err(err) => eprintln!("failed to revoke sessions: {err}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--print-totp-setup") {
        print_totp_setup();
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--revoke-all-sessions") {
        revoke_all_sessions();
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("kamaji=info,warn")),
        )
        .init();

    // Must run before anything opens a TLS connection (Telegram, Matrix, and
    // -- the case that actually surfaced this -- `agent::Runner::connect`'s
    // OpenShell gateway channel). Cargo feature unification pulls in both
    // the `aws-lc-rs` and `ring` rustls crypto backends across this
    // dependency tree (reqwest/teloxide/matrix-sdk/openshell-sdk each ask
    // for their own), so rustls can no longer auto-select a default and
    // panics on the first connection that doesn't set one explicitly. Every
    // other TLS-using client here happens to configure its own provider
    // internally; `openshell-sdk`'s tonic-based gRPC channel does not, so it
    // was the first to hit the ambiguity.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls default CryptoProvider (aws-lc-rs)");

    tracing::info!("starting kamajid");

    let config = Config::from_env();
    // `%` formats via Display (PathBuf has no Display, hence `.display()`).
    tracing::info!(
        redb_path = %config.redb_path.display(),
        notes_repo = %config.notes_repo_path.display(),
        socket_path = %config.socket_path.display(),
        telegram_configured = config.telegram.is_some(),
        matrix_configured = config.matrix.is_some(),
        rest_api_configured = config.rest_api.is_some(),
        "loaded config"
    );

    let database = db::open(&config.redb_path);
    let database = Arc::new(database);

    let queue = Queue::new(Arc::clone(&database))?;
    let recovered = queue.recover_stale(config.job_lease_timeout.as_secs())?;
    if recovered > 0 {
        tracing::info!(recovered, "recovered stale jobs from previous run");
    }

    let sessions = kamaji_core::auth::SessionStore::new(Arc::clone(&database));

    let checklist_cache = kamaji_core::checklist::cache::ChecklistCache::new(Arc::clone(&database));

    let telegram = match &config.telegram {
        Some(telegram_config) => {
            let bot = Bot::new(&telegram_config.bot_token);
            let me = bot.get_me().await?;
            tracing::info!(bot_username = %me.username(), bot_id = me.id.0, "connected to telegram");

            // Registers the command list with Telegram so clients show it as
            // a tappable "/" menu. Best-effort: a failure here just means
            // the menu is stale/missing, not a reason to abort startup.
            let bot_commands = commands::COMMANDS
                .iter()
                .map(|c| teloxide::types::BotCommand::new(c.name, c.description))
                .collect::<Vec<_>>();
            if let Err(err) = bot.set_my_commands(bot_commands).await {
                tracing::warn!(%err, "failed to register command menu with telegram");
            }

            Some(TelegramClient { bot, bot_id: me.id })
        }
        None => None,
    };

    let matrix = match &config.matrix {
        Some(matrix_config) => {
            let client = matrix_client::build(matrix_config).await?;
            tracing::info!(user_id = %client.user_id, "connected to matrix");
            Some(client)
        }
        None => None,
    };

    // `None` (i.e. `OPENSHELL_GATEWAY_URL` unset) resolves to `Runner::Direct`
    // immediately, byte-for-byte prior behavior. `Some` connects to the
    // gateway and blocks until the pre-provisioned sandbox reports `Ready` --
    // a bad `OPENSHELL_SANDBOX_NAME` or unreachable gateway is a fatal
    // startup error here, same severity class as every other startup
    // `.expect()`/`?` in this function (systemd `Restart=always` recovers).
    let runner = kamaji_core::agent::Runner::connect(config.openshell.as_ref()).await?;
    tracing::info!(
        openshell_configured = config.openshell.is_some(),
        "agent runner ready"
    );

    let core = Arc::new(kamaji_core::state::AppState {
        config,
        queue,
        sessions,
        checklist_cache,
        last_note: Mutex::new(None),
        runner,
    });

    let state = Arc::new(DaemonState {
        core,
        telegram,
        matrix,
        waiters: WaiterRegistry::new(),
    });

    // Process-lifetime anchor: every task spawned into this JoinSet is one
    // that's supposed to run forever, so any of them returning or panicking
    // is fatal for the whole process -- systemd's `Restart=always` is the
    // intended recovery path, not a daemon quietly running half-alive with a
    // dead subsystem. Spawns are gated per-transport rather than relying on
    // each `run()`'s internal no-op: `transport::matrix::run`/
    // `transport::rest::run` legitimately return almost instantly when
    // unconfigured, and watching an unconfigured transport unconditionally
    // would make the daemon exit within milliseconds of starting whenever
    // that transport is off.
    let mut tasks = tokio::task::JoinSet::new();

    let worker_state = Arc::clone(&state);
    tasks.spawn(async move {
        worker::run(worker_state).await;
        "worker"
    });

    let socket_state = Arc::clone(&state);
    tasks.spawn(async move {
        transport::socket::run(socket_state).await;
        "socket"
    });

    if state.telegram.is_some() {
        let telegram_state = Arc::clone(&state);
        tasks.spawn(async move {
            transport::telegram::run(telegram_state).await;
            "telegram"
        });
    }

    if state.matrix.is_some() {
        let matrix_state = Arc::clone(&state);
        tasks.spawn(async move {
            transport::matrix::run(matrix_state).await;
            "matrix"
        });
    }

    if state.core.config.rest_api.is_some() {
        let rest_state = Arc::clone(&state);
        tasks.spawn(async move {
            transport::rest::run(rest_state).await;
            "rest"
        });
    }

    match tasks.join_next().await {
        Some(Ok(name)) => Err(anyhow::anyhow!(
            "daemon subsystem '{name}' exited unexpectedly"
        )),
        Some(Err(join_err)) => Err(anyhow::anyhow!("daemon subsystem panicked: {join_err}")),
        None => Err(anyhow::anyhow!("no daemon subsystems were spawned")),
    }
}
