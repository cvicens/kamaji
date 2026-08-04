use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kamaji_core::ipc::{self, CliRequest, CliResponse};
use tokio::io::BufReader;
use tokio::net::UnixStream;

const DEFAULT_SOCKET_PATH: &str = "kamaji.sock";

#[derive(Parser)]
#[command(
    name = "kamaji",
    about = "CLI client for the kamajid daemon",
    disable_help_subcommand = true
)]
struct Cli {
    /// Path to kamajid's Unix domain socket. Falls back to
    /// KAMAJI_SOCKET_PATH, then the daemon's own default. An explicit
    /// --remote flag still wins over this, but typing --socket suppresses
    /// the ambient KAMAJI_REMOTE_URL fallback -- an explicitly typed flag
    /// is a stronger signal of intent than inherited environment.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// Base URL of a kamajid REST API to talk to instead of the local
    /// socket (e.g. https://kamaji.example.com). Falls back to
    /// KAMAJI_REMOTE_URL, but only when --socket is not also given.
    /// Requires a cached session token from `kamaji login` first.
    #[arg(long, global = true)]
    remote: Option<String>,

    /// Force the local Unix socket for this call, ignoring both --remote
    /// and KAMAJI_REMOTE_URL -- the way to run one command locally without
    /// unsetting an exported env var. Combine with --socket to also pick a
    /// non-default path.
    #[arg(long, global = true)]
    local: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ingest a link or freeform text (same as /ingest).
    Ingest {
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    /// Log a fact (same as /fact).
    Fact {
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    /// Manage TODOs: add <text> #tag1 #tag2 | list [open|close] | resolve <id>.
    Todo {
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,
    },
    /// Show queue/worker status.
    Status,
    /// Show recent job history.
    History {
        /// Max number of entries to show.
        limit: Option<usize>,
    },
    /// Show the command list.
    Help,
    /// Show open goals/TODOs grouped by shared tag (same as /align).
    Align,
    /// Link bitacora facts to open goals they demonstrate (same as
    /// /demonstrate). Scope: all | YYYY-Q1..4 (default: current quarter).
    Demonstrate { scope: Option<String> },
    /// Authenticate against a remote kamajid REST API (requires --remote),
    /// caching the resulting bearer session token for later commands.
    Login,
    /// Clear the cached local session token and invalidate it on the
    /// remote kamajid too (requires --remote), e.g. after a lost/stolen
    /// device.
    Logout,
}

impl Command {
    /// `None` only for `Login`/`Logout`, which `main` always handles before
    /// calling this -- they talk to `/auth/login`/`/auth/logout`, not
    /// `/api/cli`, so neither has a `CliRequest` equivalent.
    fn into_request(self) -> Option<CliRequest> {
        Some(match self {
            Command::Ingest { text } => CliRequest::Ingest {
                text: text.join(" "),
            },
            Command::Fact { text } => CliRequest::Fact {
                text: text.join(" "),
            },
            Command::Todo { args } => CliRequest::Todo { args },
            Command::Status => CliRequest::Status,
            Command::History { limit } => CliRequest::History { limit },
            Command::Help => CliRequest::Help,
            Command::Align => CliRequest::Align,
            Command::Demonstrate { scope } => CliRequest::Demonstrate { scope },
            Command::Login | Command::Logout => return None,
        })
    }
}

/// Trims whitespace and treats an empty result as unset -- `std::env::var`
/// hands back `Ok("")` for `KEY=` and a trailing newline from
/// `export KEY="$(cat url)"` is a common way that value gets there by
/// accident, and both should behave like the var was never set rather than
/// becoming a real (and broken) value downstream.
fn normalize(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn socket_path(cli_flag: Option<PathBuf>) -> PathBuf {
    cli_flag
        .or_else(|| normalize(std::env::var("KAMAJI_SOCKET_PATH").ok()).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH))
}

/// Where a resolved `--remote` value came from, so a scheme-validation error
/// can name the actual source instead of leaving the user to guess whether
/// it was the flag or the env var.
#[derive(Debug, PartialEq, Eq)]
enum RemoteSource {
    Flag,
    Env,
}

impl RemoteSource {
    fn label(&self) -> &'static str {
        match self {
            RemoteSource::Flag => "--remote",
            RemoteSource::Env => "KAMAJI_REMOTE_URL",
        }
    }
}

/// Resolves `--remote` against its `KAMAJI_REMOTE_URL` env fallback.
///
/// `use_env_fallback` must be `false` whenever `--socket` was explicitly
/// typed: an explicit `--remote` flag still wins over `--socket` (flag beats
/// flag), but the *ambient* env var must not -- otherwise a
/// `KAMAJI_REMOTE_URL` exported in a shared shell profile silently sends an
/// explicitly-local command over the network with no error or hint. This is
/// the regression this function exists to pin down.
fn remote_url(cli_flag: Option<String>, use_env_fallback: bool) -> Option<(String, RemoteSource)> {
    if let Some(url) = normalize(cli_flag) {
        return Some((url, RemoteSource::Flag));
    }
    if !use_env_fallback {
        return None;
    }
    normalize(std::env::var("KAMAJI_REMOTE_URL").ok()).map(|url| (url, RemoteSource::Env))
}

enum SchemeCheck {
    Ok,
    /// Plain `http`: not rejected, since loopback testing against
    /// `http://127.0.0.1` is legitimate, but the bearer token would cross a
    /// real network in cleartext, so it's worth a warning.
    Warn(String),
    Invalid(String),
}

fn check_scheme(url: &str, source: &RemoteSource) -> SchemeCheck {
    if url.starts_with("https://") {
        SchemeCheck::Ok
    } else if url.starts_with("http://") {
        SchemeCheck::Warn(format!(
            "{} is {url}, which is plain http -- the session token will cross the network in cleartext unless this is loopback/testing",
            source.label()
        ))
    } else {
        SchemeCheck::Invalid(format!(
            "{} is {url:?}, which is not a valid http(s) URL",
            source.label()
        ))
    }
}

/// Resolves and validates `--remote`/`KAMAJI_REMOTE_URL` once, printing any
/// scheme warning immediately. `Err` means the value is unusable and the
/// caller should bail; `Ok(None)` means no remote applies (either `--local`
/// was given, or nothing resolved and the local socket should be used).
fn resolve_remote(cli: &Cli) -> Result<Option<String>, String> {
    if cli.local {
        return Ok(None);
    }
    let use_env_fallback = cli.socket.is_none();
    match remote_url(cli.remote.clone(), use_env_fallback) {
        None => Ok(None),
        Some((url, source)) => match check_scheme(&url, &source) {
            SchemeCheck::Ok => Ok(Some(url)),
            SchemeCheck::Warn(msg) => {
                eprintln!("warning: {msg}");
                Ok(Some(url))
            }
            SchemeCheck::Invalid(msg) => Err(msg),
        },
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let remote = match resolve_remote(&cli) {
        Ok(remote) => remote,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    if let Command::Login = cli.command {
        return match remote {
            Some(remote) => login(&remote).await,
            None => {
                eprintln!("kamaji login requires --remote <url> or KAMAJI_REMOTE_URL");
                ExitCode::FAILURE
            }
        };
    }
    if let Command::Logout = cli.command {
        return match remote {
            Some(remote) => logout(&remote).await,
            None => {
                eprintln!("kamaji logout requires --remote <url> or KAMAJI_REMOTE_URL");
                ExitCode::FAILURE
            }
        };
    }

    let request = cli
        .command
        .into_request()
        .expect("Command::Login already handled above");

    match remote {
        Some(remote) => run_remote(&remote, request).await,
        None => run_local(socket_path(cli.socket), request).await,
    }
}

async fn run_local(path: PathBuf, request: CliRequest) -> ExitCode {
    let stream = match UnixStream::connect(&path).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("kamajid is not running at {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    if let Err(err) = ipc::write_message(&mut write_half, &request).await {
        eprintln!("failed to send request to kamajid: {err}");
        return ExitCode::FAILURE;
    }

    let response: CliResponse = match ipc::read_message(&mut reader).await {
        Ok(response) => response,
        Err(err) => {
            eprintln!("failed to read response from kamajid: {err}");
            return ExitCode::FAILURE;
        }
    };

    print_response(&response)
}

async fn run_remote(remote: &str, request: CliRequest) -> ExitCode {
    let token = match read_token() {
        Ok(Some(token)) => token,
        Ok(None) => {
            eprintln!("no cached session token -- run `kamaji --remote {remote} login` first");
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("failed to read cached session token: {err}");
            return ExitCode::FAILURE;
        }
    };

    let client = reqwest::Client::new();
    let sent = client
        .post(format!("{}/api/cli", remote.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&request)
        .send()
        .await;

    let response = match sent {
        Ok(response) => response,
        Err(err) => {
            eprintln!("failed to reach {remote}: {err}");
            return ExitCode::FAILURE;
        }
    };

    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        eprintln!("session expired or invalid -- run `kamaji --remote {remote} login` again");
        return ExitCode::FAILURE;
    }

    match response.json::<CliResponse>().await {
        Ok(body) => print_response(&body),
        Err(err) => {
            eprintln!("malformed response from {remote}: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn login(remote: &str) -> ExitCode {
    use std::io::Write as _;

    print!("TOTP code: ");
    if std::io::stdout().flush().is_err() {
        eprintln!("failed to write to stdout");
        return ExitCode::FAILURE;
    }
    let mut code = String::new();
    if std::io::stdin().read_line(&mut code).is_err() {
        eprintln!("failed to read totp code from stdin");
        return ExitCode::FAILURE;
    }
    let code = code.trim().to_string();

    let client = reqwest::Client::new();
    let sent = client
        .post(format!("{}/auth/login", remote.trim_end_matches('/')))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await;

    let response = match sent {
        Ok(response) => response,
        Err(err) => {
            eprintln!("failed to reach {remote}: {err}");
            return ExitCode::FAILURE;
        }
    };

    if !response.status().is_success() {
        eprintln!("login failed: http {}", response.status());
        return ExitCode::FAILURE;
    }

    #[derive(serde::Deserialize)]
    struct LoginResponse {
        token: String,
    }

    let body: LoginResponse = match response.json().await {
        Ok(body) => body,
        Err(err) => {
            eprintln!("malformed login response from {remote}: {err}");
            return ExitCode::FAILURE;
        }
    };

    match write_token(&body.token) {
        Ok(path) => {
            println!("logged in, session token cached at {}", path.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("failed to cache session token: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Clears the local token unconditionally, even if the remote revoke call
/// never reaches `kamajid` (offline, expired token, etc.) -- "log me out"
/// should always succeed locally; the remote call is best-effort cleanup on
/// top of that, not a precondition for it.
async fn logout(remote: &str) -> ExitCode {
    let token = match read_token() {
        Ok(token) => token,
        Err(err) => {
            eprintln!("failed to read cached session token: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(token) = &token {
        let client = reqwest::Client::new();
        match client
            .post(format!("{}/auth/logout", remote.trim_end_matches('/')))
            .bearer_auth(token)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                eprintln!(
                    "kamajid did not confirm logout (http {}), clearing local token anyway",
                    response.status()
                );
            }
            Err(err) => {
                eprintln!("failed to reach {remote}: {err} -- clearing local token anyway");
            }
        }
    }

    match remove_token() {
        Ok(()) => {
            println!("logged out, local session token cleared");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("failed to clear cached session token: {err}");
            ExitCode::FAILURE
        }
    }
}

fn print_response(response: &CliResponse) -> ExitCode {
    println!("{}", response.text);
    if response.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn token_dir() -> Result<PathBuf, std::io::Error> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".kamaji"))
}

fn read_token() -> Result<Option<String>, std::io::Error> {
    match std::fs::read_to_string(token_dir()?.join("token")) {
        Ok(contents) => Ok(Some(contents.trim().to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Writes the session token with `0600` permissions -- it's a bearer
/// credential for the remote daemon, same sensitivity class as an SSH key.
fn write_token(token: &str) -> Result<PathBuf, std::io::Error> {
    let dir = token_dir()?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("token");
    std::fs::write(&path, token)?;
    let mut perms = std::fs::metadata(&path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(&path, perms)?;
    Ok(path)
}

fn remove_token() -> Result<(), std::io::Error> {
    match std::fs::remove_file(token_dir()?.join("token")) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `remote_url`/`socket_path` read process env vars, which `cargo
    /// test`'s default multi-threaded runner shares across every test in
    /// this binary -- without this, two tests setting/clearing vars
    /// concurrently would flake into each other's state. Every test below
    /// takes this lock first and clears both vars before setting its own,
    /// so ordering between tests doesn't matter.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const ENV_VARS: &[&str] = &["KAMAJI_REMOTE_URL", "KAMAJI_SOCKET_PATH"];

    fn clear_env() {
        for key in ENV_VARS {
            // SAFETY: serialized by ENV_LOCK, held by every test that calls this.
            unsafe { std::env::remove_var(key) };
        }
    }

    fn set_env(key: &str, value: &str) {
        // SAFETY: serialized by ENV_LOCK, held by every test that calls this.
        unsafe { std::env::set_var(key, value) };
    }

    #[test]
    fn remote_url_flag_beats_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "https://env.example.com");

        let (url, source) =
            remote_url(Some("https://flag.example.com".to_string()), true).expect("expected Some");
        assert_eq!(url, "https://flag.example.com");
        assert_eq!(source, RemoteSource::Flag);
    }

    #[test]
    fn remote_url_env_used_when_no_flag() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "https://env.example.com");

        let (url, source) = remote_url(None, true).expect("expected Some");
        assert_eq!(url, "https://env.example.com");
        assert_eq!(source, RemoteSource::Env);
    }

    /// The regression test this whole change exists for: an explicit
    /// `--socket` must not be overridden by an ambient `KAMAJI_REMOTE_URL`.
    /// `use_env_fallback: false` is what the caller passes whenever
    /// `cli.socket` is `Some`.
    #[test]
    fn remote_url_explicit_socket_suppresses_env_fallback() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "https://env.example.com");

        assert_eq!(remote_url(None, false), None);
    }

    #[test]
    fn remote_url_empty_env_is_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "");

        assert_eq!(remote_url(None, true), None);
    }

    #[test]
    fn remote_url_whitespace_only_env_is_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "   \t  ");

        assert_eq!(remote_url(None, true), None);
    }

    #[test]
    fn remote_url_trims_trailing_newline() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "https://env.example.com\n");

        let (url, _) = remote_url(None, true).expect("expected Some");
        assert_eq!(url, "https://env.example.com");
    }

    #[test]
    fn remote_url_empty_flag_falls_back_to_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "https://env.example.com");

        let (url, source) = remote_url(Some("   ".to_string()), true).expect("expected Some");
        assert_eq!(url, "https://env.example.com");
        assert_eq!(source, RemoteSource::Env);
    }

    #[test]
    fn socket_path_flag_beats_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_SOCKET_PATH", "/env/kamaji.sock");

        let path = socket_path(Some(PathBuf::from("/flag/kamaji.sock")));
        assert_eq!(path, PathBuf::from("/flag/kamaji.sock"));
    }

    #[test]
    fn socket_path_env_used_when_no_flag() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_SOCKET_PATH", "/env/kamaji.sock");

        let path = socket_path(None);
        assert_eq!(path, PathBuf::from("/env/kamaji.sock"));
    }

    #[test]
    fn socket_path_default_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();

        let path = socket_path(None);
        assert_eq!(path, PathBuf::from(DEFAULT_SOCKET_PATH));
    }

    #[test]
    fn socket_path_empty_env_falls_back_to_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_SOCKET_PATH", "");

        let path = socket_path(None);
        assert_eq!(path, PathBuf::from(DEFAULT_SOCKET_PATH));
    }

    #[test]
    fn check_scheme_https_is_ok() {
        assert!(matches!(
            check_scheme("https://kamaji.example.com", &RemoteSource::Flag),
            SchemeCheck::Ok
        ));
    }

    #[test]
    fn check_scheme_http_warns() {
        assert!(matches!(
            check_scheme("http://127.0.0.1:8080", &RemoteSource::Flag),
            SchemeCheck::Warn(_)
        ));
    }

    #[test]
    fn check_scheme_no_scheme_is_invalid() {
        match check_scheme("kamaji.example.com", &RemoteSource::Env) {
            SchemeCheck::Invalid(msg) => assert!(msg.contains("KAMAJI_REMOTE_URL")),
            _ => panic!("expected Invalid"),
        }
    }

    #[test]
    fn resolve_remote_local_flag_ignores_everything() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "https://env.example.com");

        let cli = Cli {
            socket: None,
            remote: Some("https://flag.example.com".to_string()),
            local: true,
            command: Command::Status,
        };
        assert_eq!(resolve_remote(&cli), Ok(None));
    }

    #[test]
    fn resolve_remote_socket_flag_resolves_local_despite_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        set_env("KAMAJI_REMOTE_URL", "https://env.example.com");

        let cli = Cli {
            socket: Some(PathBuf::from("/run/kamaji/kamaji.sock")),
            remote: None,
            local: false,
            command: Command::Status,
        };
        assert_eq!(resolve_remote(&cli), Ok(None));
    }

    #[test]
    fn resolve_remote_invalid_scheme_errors() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();

        let cli = Cli {
            socket: None,
            remote: Some("kamaji.example.com".to_string()),
            local: false,
            command: Command::Status,
        };
        assert!(resolve_remote(&cli).is_err());
    }
}
