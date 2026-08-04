use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Which agent binary `kamaji_core::prompt`'s entry points invoke through the
/// active `agent::Runner` (`Direct` or `OpenShell` -- orthogonal to this).
/// Selected by `AGENT_FLAVOR`; unset means `Claude`, byte-for-byte prior
/// behavior. Switching to `Codex` only makes sense alongside repointing
/// `OPENSHELL_SANDBOX_NAME` at a sandbox that actually has Codex configured
/// (e.g. `codex-deepseek-v2`, see `docs/openshell.md`) -- kamaji doesn't
/// validate that pairing, same as it doesn't validate `CLAUDE_BIN`/`CODEX_BIN`
/// point at something that actually exists in the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentFlavor {
    #[default]
    Claude,
    Codex,
}

/// Process-start configuration. `expect()` is acceptable here per project
/// convention: a malformed environment should fail fast before the daemon
/// starts, not be handled as a runtime error.
pub struct Config {
    pub redb_path: PathBuf,
    /// Path to the Unix domain socket `kamajid` listens on for `kamaji` CLI
    /// connections (see `kamaji_core::ipc`).
    pub socket_path: PathBuf,
    /// Git repo root; notes are written under `<notes_repo_path>/notes/`.
    pub notes_repo_path: PathBuf,
    /// Which agent binary/wire-format `kamaji_core::prompt`'s entry points
    /// invoke -- see `AgentFlavor`.
    pub agent_flavor: AgentFlavor,
    /// The binary path for whichever `agent_flavor` is active: `CLAUDE_BIN`
    /// (default `"claude"`) when `AgentFlavor::Claude`, `CODEX_BIN` (default
    /// `"codex"`) when `AgentFlavor::Codex` -- resolved once here so no
    /// downstream code needs to know the flavor-to-env-var mapping.
    pub agent_bin: String,
    /// Timeout for a single agent invocation (whichever `agent_flavor` is
    /// active), env `AGENT_TIMEOUT_SECS` -- flavor-agnostic, unlike
    /// `agent_bin`/`CLAUDE_BIN`/`CODEX_BIN`, since the timeout bounds the
    /// call regardless of which binary answers it.
    pub agent_timeout: Duration,
    pub git_timeout: Duration,
    pub git_push_retries: u32,
    pub job_lease_timeout: Duration,
    pub worker_poll_interval: Duration,
    /// Gates the debug log (prompt/payload per job) written to
    /// `debug_log_path`. Off by default so normal operation never writes it.
    pub debug: bool,
    pub debug_log_path: PathBuf,
    /// Upper bound on how long the Telegram update stream may go without
    /// producing an item (an update, or even an error) before main.rs tears
    /// down and rebuilds the long-poll listener. Needed because a TCP
    /// connection left half-open across a network drop or laptop sleep can
    /// sit forever producing neither data nor an I/O error, which no amount
    /// of teloxide's built-in error-backoff retries will ever detect.
    pub poll_watchdog_timeout: Duration,
    /// Cap on the combined size (in bytes) of fetched URL content for a
    /// single ingest job, across both the message's own links and the one
    /// level of links followed from them. `raw_text` itself isn't counted:
    /// Telegram already caps a message at 4096 characters, so the unbounded
    /// growth risk is entirely from fetched web content.
    pub max_fetched_text_bytes: usize,
    /// Timeout for a single Telegram file API call (`getFile` or the actual
    /// download) when fetching a `/fact` attachment. Separate from
    /// `agent_timeout`/`git_timeout` because it bounds a different external
    /// call, per the "timeout every external call" convention.
    pub telegram_file_timeout: Duration,
    /// Defensive cap on `/fact` attachment size, independent of Telegram's
    /// own ~20MB bot download limit.
    pub max_attachment_bytes: usize,
    /// Timeout for a single Matrix media-content fetch when downloading a
    /// `/fact` attachment. Separate from `telegram_file_timeout` because
    /// Matrix's media API is a different external call with a different
    /// protocol shape (one call, not `getFile`-then-download).
    pub matrix_media_timeout: Duration,
    /// For `/align`'s auto-linking pass: if a TODO's tag overlap would
    /// connect it to more than this many *not-yet-linked* open goals, that's
    /// treated as a noisy/too-generic tag rather than a real signal -- none
    /// of the candidates are auto-linked, and the TODO is surfaced in its
    /// own report section for manual `/todo link` instead. Env
    /// `ALIGN_NOISY_TAG_THRESHOLD`, default 3.
    pub align_noisy_tag_threshold: u32,
    /// For `/demonstrate`: whether the tag-overlap candidate facts for each
    /// open goal are additionally filtered by a Claude judgment call
    /// (`prompt::run_demonstrate_prompt`) before being auto-linked. Default
    /// on (tag overlap alone is noisier for facts than for todo/goal
    /// matching, since fact tags are Claude-inferred from prose rather than
    /// user-typed) -- `false` falls back to `/align`'s original pure
    /// tag-overlap mechanism, no Claude call. Env `DEMONSTRATE_SEMANTIC_MATCH`.
    pub demonstrate_semantic_match: bool,
    /// For `/demonstrate`'s candidate-generation pass: if a fact's tag
    /// overlap would connect it to more than this many *not-yet-linked*
    /// open goals, that's treated as a noisy/too-generic tag rather than a
    /// real signal -- the fact is skipped entirely this run (not linked to
    /// any of them) rather than guessing. Mirrors
    /// `align_noisy_tag_threshold`'s role, just on the fact side instead of
    /// the todo side, since facts are what generates candidates here. Env
    /// `DEMONSTRATE_NOISY_TAG_THRESHOLD`, default 3.
    pub demonstrate_noisy_tag_threshold: u32,
    /// Matrix support is opt-in: `None` (i.e. `MATRIX_HOMESERVER_URL` unset)
    /// means the daemon behaves exactly as it did before Matrix existed --
    /// no client is built, no sync loop runs.
    pub matrix: Option<MatrixConfig>,
    /// The REST API is opt-in, same as Matrix: `None` (i.e. `REST_API_BIND`
    /// unset) means no HTTP listener ever binds. `kamajid` is meant to bind
    /// this to `127.0.0.1` only -- a reverse proxy (Caddy) in front of it is
    /// what's actually reachable from the public internet, terminating TLS.
    pub rest_api: Option<RestApiConfig>,
    /// Telegram is opt-in, same as Matrix/REST: `None` (i.e.
    /// `TELEGRAM_BOT_TOKEN` unset) means no bot client is built and no
    /// long-poll loop runs. Bundled with `allowed_chats` in one struct/gate
    /// so `bot_token: Some` with no allow-list can't be represented, same
    /// reasoning as `MatrixConfig`.
    pub telegram: Option<TelegramConfig>,
    /// The OpenShell runner is opt-in, same convention as Matrix/REST/
    /// Telegram: `None` (i.e. `OPENSHELL_GATEWAY_URL` unset) means
    /// `agent::Runner::Direct` -- Claude invocations behave byte-for-byte as
    /// before this existed. This is an *agent-runner* axis, not a transport,
    /// so it's deliberately not folded into the "at least one transport"
    /// assert below.
    pub openshell: Option<OpenShellConfig>,
}

/// Bot credentials plus the Telegram-side allow-list. Gated on
/// `TELEGRAM_BOT_TOKEN` being set; see `telegram_config_from_env`.
pub struct TelegramConfig {
    pub bot_token: String,
    /// Plain `i64` rather than a platform-typed id: this is core, so it
    /// stays free of teloxide's `ChatId` -- the two Telegram call sites that
    /// compare against it (in `kamajid::transport::telegram`) convert with
    /// `msg.chat.id.0`.
    pub allowed_chats: Vec<i64>,
}

/// Bootstrap credentials for matrix-sdk, plus the Matrix-side allow-list.
/// `access_token`/`device_id` come from the one-time manual UIA registration
/// (see `docs/matrix.md` step 9) -- matrix-sdk's own session store persists
/// and refreshes from there, so no password ever needs to live in Kamaji's
/// config.
pub struct MatrixConfig {
    pub homeserver_url: String,
    pub user_id: String,
    pub access_token: String,
    pub device_id: String,
    pub store_path: PathBuf,
    pub allowed_rooms: Vec<String>,
}

/// Config for the optional REST API transport (see `kamajid::transport::rest`).
/// `totp_secret` is the base32 shared secret enrolled once into an
/// authenticator app via `kamajid --print-totp-setup`; `session_ttl` is how
/// long a bearer token issued by `/auth/login` stays valid.
pub struct RestApiConfig {
    pub bind_addr: SocketAddr,
    pub totp_secret: String,
    pub session_ttl: Duration,
}

/// Config for the pre-provisioned OpenShell sandbox `agent::Runner::OpenShell`
/// execs against. Gated on `OPENSHELL_GATEWAY_URL`; see
/// `openshell_config_from_env`. kamaji never creates or deletes this
/// sandbox -- it's provisioned out-of-band (see `docs/openshell.md`'s "Full
/// rebuild" for the pattern), and this struct only carries what's needed to
/// connect to it and confirm it's ready.
///
/// Two auth paths, both real SDK/gateway capabilities rather than narrowed
/// away by oversight: `mtls: None` is "anonymous TLS over HTTPS, or
/// plaintext when gateway is http://" (`openshell_sdk::ClientConfig::auth:
/// None`) -- fine for a gateway with `--enable-mtls-auth=false`. `mtls:
/// Some` matches this gateway's actual default posture (mTLS on for local
/// single-user Docker/Podman/VM gateways) by presenting a client
/// certificate via `agent::connect_mtls`, bypassing `openshell_sdk`'s own
/// `ClientConfig`/`connect()` (which has no client-cert support at all --
/// see the comment on `connect_mtls`) through `OpenShellClient::from_parts`.
/// `openshell_sdk::AuthConfig::EdgeJwt` (Cloudflare Access tunnel) and
/// `AuthConfig::Oidc` (bearer token + refresh) remain explicit future scope
/// -- see TODO.md -- for if kamaji's OpenShell gateway ever moves off a
/// trusted local network.
pub struct OpenShellConfig {
    pub gateway_url: String,
    pub sandbox_name: String,
    /// Only bounds the one-time startup `wait_ready` call in
    /// `agent::Runner::connect` -- separate from `agent_timeout`, which
    /// bounds each `exec` call once the runner is live, same "one timeout
    /// per distinct external call" convention as `telegram_file_timeout` vs
    /// `matrix_media_timeout`.
    pub ready_timeout: Duration,
    /// `None` when `OPENSHELL_MTLS_DIR` is unset -- anonymous TLS, see above.
    pub mtls: Option<OpenShellMtlsConfig>,
}

/// Client mTLS material for a gateway with `--enable-mtls-auth` on (this
/// gateway's actual default). Sourced from a directory containing `ca.crt`,
/// `tls.crt`, `tls.key` -- deliberately the same three filenames
/// `openshell-cli`/`openshell-bootstrap` already use under
/// `~/.config/openshell/gateways/<name>/mtls/`, so `OPENSHELL_MTLS_DIR`
/// reads as "the same kind of directory" to anyone who's used the CLI.
/// kamaji never mints this identity itself -- it's a copy of the gateway's
/// existing local client cert, provisioned out-of-band (see
/// `docs/openshell.md`).
pub struct OpenShellMtlsConfig {
    pub ca_cert_path: PathBuf,
    pub client_cert_path: PathBuf,
    pub client_key_path: PathBuf,
}

impl Config {
    pub fn from_env() -> Self {
        let redb_path = std::env::var("REDB_PATH")
            .unwrap_or_else(|_| "kamaji.redb".to_string())
            .into();

        let socket_path = std::env::var("KAMAJI_SOCKET_PATH")
            .unwrap_or_else(|_| "kamaji.sock".to_string())
            .into();

        let notes_repo_path =
            std::env::var("NOTES_REPO_PATH").expect("NOTES_REPO_PATH must be set");
        let notes_repo_path = PathBuf::from(notes_repo_path);
        assert!(
            notes_repo_path.is_dir(),
            "NOTES_REPO_PATH {notes_repo_path:?} does not exist or is not a directory"
        );

        let agent_flavor = match std::env::var("AGENT_FLAVOR") {
            Ok(v) if v == "claude" => AgentFlavor::Claude,
            Ok(v) if v == "codex" => AgentFlavor::Codex,
            Ok(v) => panic!("AGENT_FLAVOR: expected \"claude\" or \"codex\", got {v:?}"),
            Err(_) => AgentFlavor::default(),
        };
        let agent_bin = match agent_flavor {
            AgentFlavor::Claude => {
                std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
            }
            AgentFlavor::Codex => {
                std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
            }
        };

        let agent_timeout = Duration::from_secs(env_u64("AGENT_TIMEOUT_SECS", 120));
        let git_timeout = Duration::from_secs(env_u64("GIT_TIMEOUT_SECS", 30));
        let git_push_retries = env_u64("GIT_PUSH_RETRIES", 3) as u32;
        let job_lease_timeout = Duration::from_secs(env_u64("JOB_LEASE_TIMEOUT_SECS", 600));
        let worker_poll_interval = Duration::from_millis(env_u64("WORKER_POLL_INTERVAL_MS", 1000));

        let debug = std::env::var("DEBUG")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let debug_log_path = std::env::var("DEBUG_LOG_PATH")
            .unwrap_or_else(|_| "kamaji-debug.log".to_string())
            .into();

        let poll_watchdog_timeout = Duration::from_secs(env_u64("POLL_WATCHDOG_TIMEOUT_SECS", 60));

        let max_fetched_text_bytes = env_u64("MAX_FETCHED_TEXT_BYTES", 300_000) as usize;

        let telegram_file_timeout = Duration::from_secs(env_u64("TELEGRAM_FILE_TIMEOUT_SECS", 30));
        let max_attachment_bytes = env_u64("MAX_ATTACHMENT_BYTES", 20_000_000) as usize;
        let matrix_media_timeout = Duration::from_secs(env_u64("MATRIX_MEDIA_TIMEOUT_SECS", 30));

        let align_noisy_tag_threshold = env_u64("ALIGN_NOISY_TAG_THRESHOLD", 3) as u32;

        let demonstrate_semantic_match = std::env::var("DEMONSTRATE_SEMANTIC_MATCH")
            .map(|v| !matches!(v.trim().to_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);
        let demonstrate_noisy_tag_threshold = env_u64("DEMONSTRATE_NOISY_TAG_THRESHOLD", 3) as u32;

        let matrix = matrix_config_from_env();
        let rest_api = rest_api_config_from_env();
        let telegram = telegram_config_from_env();
        let openshell = openshell_config_from_env();

        assert!(
            telegram.is_some() || matrix.is_some() || rest_api.is_some(),
            "at least one of TELEGRAM_BOT_TOKEN, MATRIX_HOMESERVER_URL, or REST_API_BIND \
             must be set (kamajid with only the unix socket reachable is almost \
             certainly a misconfiguration)"
        );

        Config {
            redb_path,
            socket_path,
            notes_repo_path,
            agent_flavor,
            agent_bin,
            agent_timeout,
            git_timeout,
            git_push_retries,
            job_lease_timeout,
            worker_poll_interval,
            debug,
            debug_log_path,
            poll_watchdog_timeout,
            max_fetched_text_bytes,
            telegram_file_timeout,
            max_attachment_bytes,
            matrix_media_timeout,
            align_noisy_tag_threshold,
            demonstrate_semantic_match,
            demonstrate_noisy_tag_threshold,
            matrix,
            rest_api,
            telegram,
            openshell,
        }
    }
}

/// `TELEGRAM_BOT_TOKEN` unset means Telegram is disabled entirely -- this is
/// the one gate that decides whether `ALLOWED_CHAT_IDS` is even read, same
/// convention as `matrix_config_from_env` below.
fn telegram_config_from_env() -> Option<TelegramConfig> {
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").ok()?;

    let allowed_chats = std::env::var("ALLOWED_CHAT_IDS")
        .expect("ALLOWED_CHAT_IDS must be set (comma-separated chat ids) when TELEGRAM_BOT_TOKEN is set")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>()
                .unwrap_or_else(|_| panic!("ALLOWED_CHAT_IDS: invalid chat id: {s}"))
        })
        .collect::<Vec<_>>();
    assert!(
        !allowed_chats.is_empty(),
        "ALLOWED_CHAT_IDS must contain at least one chat id"
    );

    Some(TelegramConfig {
        bot_token,
        allowed_chats,
    })
}

/// `MATRIX_HOMESERVER_URL` unset means Matrix is disabled entirely -- this is
/// the one gate that decides whether the rest of the Matrix env vars are
/// even read. When set, the remaining vars are required (same
/// fail-fast-at-startup convention as `telegram_config_from_env` above).
fn matrix_config_from_env() -> Option<MatrixConfig> {
    let homeserver_url = std::env::var("MATRIX_HOMESERVER_URL").ok()?;

    let user_id = std::env::var("MATRIX_USER_ID")
        .expect("MATRIX_USER_ID must be set when MATRIX_HOMESERVER_URL is set");
    let access_token = std::env::var("MATRIX_ACCESS_TOKEN")
        .expect("MATRIX_ACCESS_TOKEN must be set when MATRIX_HOMESERVER_URL is set");
    let device_id = std::env::var("MATRIX_DEVICE_ID")
        .expect("MATRIX_DEVICE_ID must be set when MATRIX_HOMESERVER_URL is set");
    let store_path = std::env::var("MATRIX_STORE_PATH")
        .unwrap_or_else(|_| "kamaji-matrix-store".to_string())
        .into();

    let allowed_rooms = std::env::var("ALLOWED_MATRIX_ROOMS")
        .expect("ALLOWED_MATRIX_ROOMS must be set (comma-separated room ids) when MATRIX_HOMESERVER_URL is set")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    assert!(
        !allowed_rooms.is_empty(),
        "ALLOWED_MATRIX_ROOMS must contain at least one room id"
    );

    Some(MatrixConfig {
        homeserver_url,
        user_id,
        access_token,
        device_id,
        store_path,
        allowed_rooms,
    })
}

/// `REST_API_BIND` unset means the REST API is disabled entirely -- this is
/// the one gate that decides whether the rest of its env vars are even
/// read, same convention as `matrix_config_from_env` above.
fn rest_api_config_from_env() -> Option<RestApiConfig> {
    let bind_addr_raw = std::env::var("REST_API_BIND").ok()?;
    let bind_addr = bind_addr_raw
        .parse::<SocketAddr>()
        .unwrap_or_else(|_| panic!("REST_API_BIND: invalid socket address: {bind_addr_raw}"));

    let totp_secret = std::env::var("REST_API_TOTP_SECRET")
        .expect("REST_API_TOTP_SECRET must be set when REST_API_BIND is set");

    let session_ttl = Duration::from_secs(env_u64(
        "REST_API_SESSION_TTL_SECS",
        7 * 24 * 60 * 60, // 7 days -- a leaked/lost-laptop token has no
                          // revocation mechanism other than `kamaji logout`
                          // or `kamajid --revoke-all-sessions`, so the
                          // default exposure window is kept short.
    ));

    Some(RestApiConfig {
        bind_addr,
        totp_secret,
        session_ttl,
    })
}

/// `OPENSHELL_GATEWAY_URL` unset means the OpenShell runner is disabled
/// entirely -- `agent::Runner::Direct` behaves byte-for-byte as before this
/// existed. This is the one gate deciding whether `OPENSHELL_SANDBOX_NAME`
/// is even read, same convention as `matrix_config_from_env` above.
///
/// Deliberately no separate `AGENT_RUNNER=direct|openshell` selector:
/// presence of this gate var already *is* the selector, exactly like
/// Matrix/REST/Telegram -- none of those has a redundant enum flag next to
/// its own gate var either.
fn openshell_config_from_env() -> Option<OpenShellConfig> {
    let gateway_url = std::env::var("OPENSHELL_GATEWAY_URL").ok()?;

    let sandbox_name = std::env::var("OPENSHELL_SANDBOX_NAME")
        .expect("OPENSHELL_SANDBOX_NAME must be set when OPENSHELL_GATEWAY_URL is set");

    let ready_timeout = Duration::from_secs(env_u64("OPENSHELL_READY_TIMEOUT_SECS", 30));

    let mtls = std::env::var("OPENSHELL_MTLS_DIR").ok().map(|dir| {
        let dir = PathBuf::from(dir);
        OpenShellMtlsConfig {
            ca_cert_path: dir.join("ca.crt"),
            client_cert_path: dir.join("tls.crt"),
            client_key_path: dir.join("tls.key"),
        }
    });

    Some(OpenShellConfig {
        gateway_url,
        sandbox_name,
        ready_timeout,
        mtls,
    })
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .map(|s| {
            s.parse::<u64>()
                .unwrap_or_else(|_| panic!("{key}: invalid integer: {s}"))
        })
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// All of `from_env`'s inputs are process env vars, which `cargo test`'s
    /// default multi-threaded runner shares across every test in this
    /// binary -- without this, two tests in this module setting/clearing
    /// vars concurrently would flake into each other's state. Every test
    /// below takes this lock first and clears the vars it cares about
    /// before setting its own, so ordering between tests doesn't matter.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // Also covers `OPENSHELL_*` despite the name: those aren't a transport,
    // but they're env-var-gated the same way and shared by `from_env`, so
    // every test that exercises `from_env` needs them cleared too.
    const TRANSPORT_ENV_VARS: &[&str] = &[
        "TELEGRAM_BOT_TOKEN",
        "ALLOWED_CHAT_IDS",
        "MATRIX_HOMESERVER_URL",
        "MATRIX_USER_ID",
        "MATRIX_ACCESS_TOKEN",
        "MATRIX_DEVICE_ID",
        "MATRIX_STORE_PATH",
        "ALLOWED_MATRIX_ROOMS",
        "REST_API_BIND",
        "REST_API_TOTP_SECRET",
        "REST_API_SESSION_TTL_SECS",
        "OPENSHELL_GATEWAY_URL",
        "OPENSHELL_SANDBOX_NAME",
        "OPENSHELL_READY_TIMEOUT_SECS",
        "OPENSHELL_MTLS_DIR",
        "AGENT_FLAVOR",
        "CLAUDE_BIN",
        "CODEX_BIN",
    ];

    fn clear_transport_env() {
        for key in TRANSPORT_ENV_VARS {
            // SAFETY: serialized by ENV_LOCK, held by every test that calls this.
            unsafe { std::env::remove_var(key) };
        }
    }

    fn set_env(key: &str, value: &str) {
        // SAFETY: serialized by ENV_LOCK, held by every test that calls this.
        unsafe { std::env::set_var(key, value) };
    }

    #[test]
    fn telegram_config_from_env_none_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();

        assert!(telegram_config_from_env().is_none());
    }

    #[test]
    fn telegram_config_from_env_some_when_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");
        set_env("ALLOWED_CHAT_IDS", "123, 456");

        let cfg = telegram_config_from_env().expect("expected Some(TelegramConfig)");
        assert_eq!(cfg.bot_token, "test-token");
        assert_eq!(cfg.allowed_chats, vec![123, 456]);
    }

    #[test]
    #[should_panic(expected = "ALLOWED_CHAT_IDS must be set")]
    fn telegram_config_from_env_panics_without_allowed_chats() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");

        telegram_config_from_env();
    }

    /// Sets `NOTES_REPO_PATH` to a fresh tempdir so `Config::from_env`'s
    /// `is_dir()` assert passes; every test that exercises `from_env`
    /// itself (rather than a single `*_config_from_env` helper) needs this.
    fn set_valid_notes_repo_path() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        set_env("NOTES_REPO_PATH", dir.path().to_str().expect("utf-8 path"));
        dir
    }

    #[test]
    #[should_panic(expected = "at least one of TELEGRAM_BOT_TOKEN")]
    fn from_env_panics_when_no_transport_configured() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();

        Config::from_env();
    }

    #[test]
    fn from_env_telegram_only_leaves_matrix_and_rest_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");
        set_env("ALLOWED_CHAT_IDS", "123");

        let config = Config::from_env();
        assert!(config.telegram.is_some());
        assert!(config.matrix.is_none());
        assert!(config.rest_api.is_none());
    }

    #[test]
    fn from_env_rest_only_leaves_telegram_and_matrix_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();
        set_env("REST_API_BIND", "127.0.0.1:8080");
        set_env("REST_API_TOTP_SECRET", "test-secret");

        let config = Config::from_env();
        assert!(config.rest_api.is_some());
        assert!(config.telegram.is_none());
        assert!(config.matrix.is_none());
    }

    #[test]
    fn openshell_config_from_env_none_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();

        assert!(openshell_config_from_env().is_none());
    }

    #[test]
    fn openshell_config_from_env_some_when_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        set_env("OPENSHELL_GATEWAY_URL", "http://127.0.0.1:8080");
        set_env("OPENSHELL_SANDBOX_NAME", "kamaji-claude");

        let cfg = openshell_config_from_env().expect("expected Some(OpenShellConfig)");
        assert_eq!(cfg.gateway_url, "http://127.0.0.1:8080");
        assert_eq!(cfg.sandbox_name, "kamaji-claude");
        assert_eq!(cfg.ready_timeout, Duration::from_secs(30));
        assert!(cfg.mtls.is_none());
    }

    #[test]
    #[should_panic(expected = "OPENSHELL_SANDBOX_NAME must be set")]
    fn openshell_config_from_env_panics_without_sandbox_name() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        set_env("OPENSHELL_GATEWAY_URL", "http://127.0.0.1:8080");

        openshell_config_from_env();
    }

    #[test]
    fn openshell_config_from_env_mtls_none_when_dir_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        set_env("OPENSHELL_GATEWAY_URL", "https://127.0.0.1:17670");
        set_env("OPENSHELL_SANDBOX_NAME", "kamaji-claude");

        let cfg = openshell_config_from_env().expect("expected Some(OpenShellConfig)");
        assert!(cfg.mtls.is_none());
    }

    #[test]
    fn openshell_config_from_env_mtls_some_when_dir_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        set_env("OPENSHELL_GATEWAY_URL", "https://127.0.0.1:17670");
        set_env("OPENSHELL_SANDBOX_NAME", "kamaji-claude");
        set_env("OPENSHELL_MTLS_DIR", "/etc/kamaji/openshell-mtls");

        let cfg = openshell_config_from_env().expect("expected Some(OpenShellConfig)");
        let mtls = cfg.mtls.expect("expected Some(OpenShellMtlsConfig)");
        assert_eq!(
            mtls.ca_cert_path,
            PathBuf::from("/etc/kamaji/openshell-mtls/ca.crt")
        );
        assert_eq!(
            mtls.client_cert_path,
            PathBuf::from("/etc/kamaji/openshell-mtls/tls.crt")
        );
        assert_eq!(
            mtls.client_key_path,
            PathBuf::from("/etc/kamaji/openshell-mtls/tls.key")
        );
    }

    #[test]
    fn from_env_openshell_configured_alongside_telegram_leaves_openshell_some() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");
        set_env("ALLOWED_CHAT_IDS", "123");
        set_env("OPENSHELL_GATEWAY_URL", "http://127.0.0.1:8080");
        set_env("OPENSHELL_SANDBOX_NAME", "kamaji-claude");

        let config = Config::from_env();
        assert!(config.telegram.is_some());
        assert!(config.openshell.is_some());
    }

    #[test]
    fn from_env_agent_flavor_defaults_to_claude_with_claude_bin() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");
        set_env("ALLOWED_CHAT_IDS", "123");

        let config = Config::from_env();
        assert_eq!(config.agent_flavor, AgentFlavor::Claude);
        assert_eq!(config.agent_bin, "claude");
    }

    #[test]
    fn from_env_agent_flavor_codex_resolves_codex_bin() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");
        set_env("ALLOWED_CHAT_IDS", "123");
        set_env("AGENT_FLAVOR", "codex");

        let config = Config::from_env();
        assert_eq!(config.agent_flavor, AgentFlavor::Codex);
        assert_eq!(config.agent_bin, "codex");
    }

    #[test]
    fn from_env_agent_flavor_codex_honors_codex_bin_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");
        set_env("ALLOWED_CHAT_IDS", "123");
        set_env("AGENT_FLAVOR", "codex");
        set_env("CODEX_BIN", "/opt/codex/bin/codex");

        let config = Config::from_env();
        assert_eq!(config.agent_bin, "/opt/codex/bin/codex");
    }

    #[test]
    #[should_panic(expected = "AGENT_FLAVOR: expected \"claude\" or \"codex\"")]
    fn from_env_agent_flavor_panics_on_invalid_value() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_transport_env();
        let _tmp = set_valid_notes_repo_path();
        set_env("TELEGRAM_BOT_TOKEN", "test-token");
        set_env("ALLOWED_CHAT_IDS", "123");
        set_env("AGENT_FLAVOR", "gpt5");

        Config::from_env();
    }
}
