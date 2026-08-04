use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use openshell_sdk::{
    ClientConfig, EdgeAuthInterceptor, ExecOptions, ExecResult, OpenShellClient, SdkError,
};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

use crate::config::{OpenShellConfig, OpenShellMtlsConfig};
use crate::error::AgentError;

/// Raw output from a spawned agent process. Envelope/schema parsing is a
/// per-agent concern (`claude.rs`'s job today, a hypothetical `opencode.rs`'s
/// later) -- this layer only knows how to launch a process and pipe input to
/// it, not what the output means.
#[derive(Debug)]
pub struct RawOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub success: bool,
}

/// Which mechanism launches the agent process -- orthogonal to *which* agent
/// answers the prompt (`claude.rs`'s job, unchanged either way). See
/// TODO.md's "Agent backend abstraction" section. `Direct` carries no state
/// (the binary path/timeout are per-call args, same as before this existed);
/// `OpenShell` carries a connected gateway client plus the name of a
/// pre-provisioned sandbox -- kamaji never creates or deletes that sandbox,
/// only execs into it (see `Runner::connect` and CLAUDE.md).
pub enum Runner {
    Direct,
    OpenShell {
        // Boxed so `Direct` (zero-sized) doesn't force every `Runner` value
        // to reserve `OpenShellClient`'s size (its tonic `Channel` handle);
        // `Runner` is built once at startup and stored in `AppState`, so the
        // extra indirection costs nothing per-job.
        client: Box<OpenShellClient>,
        sandbox_name: String,
    },
}

impl Runner {
    /// Builds the runner for `Config::openshell`. `None` (the gate var
    /// `OPENSHELL_GATEWAY_URL` unset) means `Runner::Direct`, byte-for-byte
    /// prior behavior -- same "gate var absent -> fully off" convention as
    /// Matrix/REST/Telegram. `Some` connects to the gateway and blocks until
    /// the pre-provisioned sandbox reports `Ready` (or fails fast: the
    /// gateway's own `wait_for` poll loop propagates a `get_sandbox` error
    /// immediately rather than retrying past it, so a nonexistent sandbox or
    /// an unreachable gateway fails this call right away, not after the full
    /// `ready_timeout`).
    ///
    /// Async and fallible, unlike `Config::from_env` -- call once from
    /// `kamajid`'s `main()` after `Config::from_env()`, same pattern as
    /// `matrix_client::build`.
    pub async fn connect(config: Option<&OpenShellConfig>) -> Result<Self, AgentError> {
        let Some(config) = config else {
            return Ok(Runner::Direct);
        };

        let client = match &config.mtls {
            Some(mtls) => connect_mtls(&config.gateway_url, mtls).await?,
            None => {
                let client_config = ClientConfig::new(config.gateway_url.clone());
                OpenShellClient::connect(client_config)
                    .await
                    .map_err(AgentError::SandboxRejected)?
            }
        };
        client
            .wait_ready(&config.sandbox_name, config.ready_timeout)
            .await
            .map_err(AgentError::SandboxRejected)?;

        Ok(Runner::OpenShell {
            client: Box::new(client),
            sandbox_name: config.sandbox_name.clone(),
        })
    }
}

/// Builds an `OpenShellClient` presenting a client certificate, for gateways
/// with `--enable-mtls-auth` on (this gateway's actual default posture --
/// see the doc comment on `OpenShellMtlsConfig`). `openshell_sdk`'s own
/// `ClientConfig`/`OpenShellClient::connect` has no client-cert support at
/// all -- confirmed from its source, `transport.rs`: *"mTLS is intentionally
/// out of scope here. Gateways that require client certificates are handled
/// by `openshell-cli`'s legacy path until the auth method is retired."* This
/// mirrors that legacy path (`openshell-cli/src/tls.rs`'s
/// `build_tonic_tls_config`) by hand-building the `tonic` `Channel` and
/// handing it to `OpenShellClient::from_parts` -- a constructor the SDK
/// documents as existing exactly for callers who need to customize channel
/// construction beyond what `ClientConfig` exposes.
///
/// `EdgeAuthInterceptor::noop()` is used because auth happens entirely at
/// the transport layer (the mTLS handshake itself) -- no bearer header is
/// needed on top of it.
async fn connect_mtls(
    gateway_url: &str,
    mtls: &OpenShellMtlsConfig,
) -> Result<OpenShellClient, AgentError> {
    let read = |path: &std::path::Path| {
        let path = path.to_path_buf();
        async move {
            tokio::fs::read(&path)
                .await
                .map_err(|source| AgentError::MtlsMaterial {
                    path: path.clone(),
                    source,
                })
        }
    };
    let ca = read(&mtls.ca_cert_path).await?;
    let cert = read(&mtls.client_cert_path).await?;
    let key = read(&mtls.client_key_path).await?;

    let tls_config = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(cert, key));

    let channel: Channel = Endpoint::from_shared(gateway_url.to_string())
        .map_err(AgentError::MtlsChannel)?
        .tls_config(tls_config)
        .map_err(AgentError::MtlsChannel)?
        .connect()
        .await
        .map_err(AgentError::MtlsChannel)?;

    Ok(OpenShellClient::from_parts(
        channel,
        EdgeAuthInterceptor::noop(),
    ))
}

/// Spawns `bin args...` via `runner`, feeds `stdin` to it, and returns its
/// raw output. `stdin` is used rather than an argv argument so callers with
/// large payloads (e.g. `claude.rs`'s fetched-URL-laden prompts) don't hit
/// the kernel's per-argument limit (`MAX_ARG_STRLEN`, 128 KiB) -- see
/// `invoke_pipes_oversized_stdin` below.
pub async fn invoke(
    runner: &Runner,
    bin: &str,
    args: &[&str],
    stdin: &str,
    timeout: Duration,
) -> Result<RawOutput, AgentError> {
    match runner {
        Runner::Direct => invoke_direct(bin, args, stdin, timeout).await,
        Runner::OpenShell {
            client,
            sandbox_name,
        } => invoke_openshell(client, sandbox_name, bin, args, stdin, timeout).await,
    }
}

async fn invoke_direct(
    bin: &str,
    args: &[&str],
    stdin: &str,
    timeout: Duration,
) -> Result<RawOutput, AgentError> {
    let output = tokio::time::timeout(timeout, async {
        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(AgentError::Spawn)?;

        let mut child_stdin = child.stdin.take().ok_or(AgentError::StdinUnavailable)?;
        child_stdin
            .write_all(stdin.as_bytes())
            .await
            .map_err(AgentError::StdinWrite)?;
        // Drop stdin to send EOF; the child is expected to read the whole
        // input before emitting output, so writing then waiting can't
        // deadlock here.
        drop(child_stdin);

        child.wait_with_output().await.map_err(AgentError::Spawn)
    })
    .await
    .map_err(|_| AgentError::Timeout)??;

    if !output.status.success() {
        return Err(AgentError::NonZeroExit {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(RawOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        success: output.status.success(),
    })
}

async fn invoke_openshell(
    client: &OpenShellClient,
    sandbox_name: &str,
    bin: &str,
    args: &[&str],
    stdin: &str,
    timeout: Duration,
) -> Result<RawOutput, AgentError> {
    let cmd: Vec<String> = std::iter::once(bin.to_string())
        .chain(args.iter().map(|a| a.to_string()))
        .collect();
    let opts = ExecOptions {
        workdir: None,
        environment: HashMap::new(),
        timeout: Some(timeout),
        stdin: Some(stdin.as_bytes().to_vec()),
    };

    // `ExecOptions::timeout` only asks the *gateway* to enforce a limit --
    // `OpenShellClient::exec` has no client-side timeout of its own. Wrap it
    // here too so a hung gateway/network partition can't defeat kamaji's own
    // "timeout every external call" rule (CLAUDE.md), the same backstop
    // `invoke_direct` already gets from `tokio::time::timeout`.
    let result = tokio::time::timeout(timeout, client.exec(sandbox_name, &cmd, opts))
        .await
        .map_err(|_| AgentError::Timeout)?;

    map_exec_result(result)
}

/// Pure mapping, no I/O -- unit-testable without a live gateway.
/// `OpenShellClient::exec` never returns `Err` for the wrapped binary's own
/// non-zero exit (that's `ExecResult::exit_code`); any `Err` here is the
/// sandbox/gateway boundary itself rejecting the request (missing sandbox,
/// auth, transport), a different failure class from the agent running
/// inside the sandbox and failing on its own terms -- see
/// `AgentError::SandboxRejected`.
fn map_exec_result(result: Result<ExecResult, SdkError>) -> Result<RawOutput, AgentError> {
    let exec = result.map_err(AgentError::SandboxRejected)?;
    map_exec_success(exec.exit_code, exec.stdout, exec.stderr)
}

/// Split out from `map_exec_result` so tests can exercise the exit-code
/// logic directly: `ExecResult` is `#[non_exhaustive]` upstream, so it can't
/// be struct-literal-constructed outside `openshell-sdk` itself -- taking
/// the three fields as plain values here keeps this half of the mapping
/// unit-testable without a live gateway.
fn map_exec_success(
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<RawOutput, AgentError> {
    if exit_code != 0 {
        return Err(AgentError::NonZeroExit {
            status: format!("exit code {exit_code}"),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }
    Ok(RawOutput {
        stdout,
        stderr,
        success: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for E2BIG ("Argument list too long"): a stdin payload
    /// larger than the kernel's 128 KiB per-argument limit (`MAX_ARG_STRLEN`)
    /// must spawn fine since it never touches argv. The fake binary reads
    /// stdin and echoes the byte count, exercising the real
    /// spawn/stdin/timeout path without a paid Claude call.
    #[tokio::test]
    async fn invoke_pipes_oversized_stdin() {
        // A dedicated `tempfile::tempdir()` per test, not a
        // `std::process::id()`-keyed path shared across every test in this
        // file: two tests using the same directory raced on teardown (one
        // test's `remove_dir_all` could yank the other's still-running
        // script out from under it, e.g. the E2BIG regression here observed
        // as a spurious `StdinWrite`/`BrokenPipe`), since cargo test runs
        // tests in the same binary concurrently by default. `TempDir` is
        // unique per call and cleans itself up on drop, so there's no
        // shared path to race on.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-agent.sh");
        std::fs::write(&script, "#!/bin/sh\nwc -c\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 200 KiB -- comfortably past MAX_ARG_STRLEN, where argv would fail.
        let stdin = "x".repeat(200 * 1024);
        let output = invoke(
            &Runner::Direct,
            script.to_str().unwrap(),
            &[],
            &stdin,
            Duration::from_secs(30),
        )
        .await
        .expect("oversized stdin should spawn and run fine");
        let byte_count: usize = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("byte count is numeric");
        assert_eq!(byte_count, stdin.len());
    }

    #[tokio::test]
    async fn invoke_reports_non_zero_exit_with_stderr() {
        // See `invoke_pipes_oversized_stdin` above for why this is a
        // per-test `tempfile::tempdir()` rather than a shared, pid-keyed path.
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-fail.sh");
        std::fs::write(&script, "#!/bin/sh\necho 'boom' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let err = invoke(
            &Runner::Direct,
            script.to_str().unwrap(),
            &[],
            "",
            Duration::from_secs(30),
        )
        .await
        .expect_err("non-zero exit should error");
        match err {
            AgentError::NonZeroExit { stderr, .. } => assert!(stderr.contains("boom")),
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[test]
    fn map_exec_success_zero_exit_returns_raw_output() {
        let result = map_exec_success(0, b"hello".to_vec(), Vec::new());
        let output = result.expect("zero exit code should be Ok");
        assert!(output.success);
        assert_eq!(output.stdout, b"hello");
    }

    #[test]
    fn map_exec_success_nonzero_exit_is_non_zero_exit_not_sandbox_rejected() {
        let result = map_exec_success(1, Vec::new(), b"boom".to_vec());
        match result {
            Err(AgentError::NonZeroExit { stderr, .. }) => assert!(stderr.contains("boom")),
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[test]
    fn map_exec_result_sdk_error_is_sandbox_rejected() {
        let result = map_exec_result(Err(SdkError::NotFound {
            message: "sandbox 'kamaji-claude' not found".to_string(),
        }));
        match result {
            Err(AgentError::SandboxRejected(SdkError::NotFound { .. })) => {}
            other => panic!("expected SandboxRejected(NotFound), got {other:?}"),
        }
    }

    /// Manual verification only -- needs a real OpenShell gateway and a
    /// pre-provisioned sandbox reachable via the env vars below (not
    /// `OPENSHELL_GATEWAY_URL`/`OPENSHELL_SANDBOX_NAME`, which are read by
    /// `Config` for the daemon's own runtime configuration -- kept separate
    /// so running this test can never accidentally point at a production
    /// config). Run explicitly with:
    ///   cargo test -p kamaji-core --ignored -- openshell_smoke_test
    ///
    /// Mirrors `invoke_pipes_oversized_stdin`'s oversized-stdin regression,
    /// but through a real gateway, since the gRPC stdin path can't be faked
    /// with a shell script the way `tokio::process::Command`'s stdin pipe
    /// can. This is the concrete verification TODO.md's "confirm OpenShell
    /// passes stdin/stdout/exit code through transparently before relying on
    /// it" item asked for.
    #[tokio::test]
    #[ignore = "requires a real OpenShell gateway + pre-provisioned sandbox"]
    async fn openshell_smoke_test_oversized_stdin() {
        let gateway_url =
            std::env::var("OPENSHELL_SMOKE_GATEWAY_URL").expect("set for manual runs");
        let sandbox_name =
            std::env::var("OPENSHELL_SMOKE_SANDBOX_NAME").expect("set for manual runs");

        let runner = Runner::connect(Some(&OpenShellConfig {
            gateway_url,
            sandbox_name,
            ready_timeout: Duration::from_secs(30),
            mtls: None,
        }))
        .await
        .expect("gateway should be reachable and sandbox should become ready");

        let stdin = "x".repeat(200 * 1024);
        let output = invoke(&runner, "wc", &["-c"], &stdin, Duration::from_secs(30))
            .await
            .expect("oversized stdin should round-trip through the gateway");
        let byte_count: usize = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("byte count is numeric");
        assert_eq!(byte_count, stdin.len());
    }

    /// Manual verification only, same reasoning as
    /// `openshell_smoke_test_oversized_stdin` above -- separate
    /// `OPENSHELL_SMOKE_MTLS_*` env vars so this can never accidentally
    /// point at production config. Run explicitly with:
    ///   cargo test -p kamaji-core --ignored -- openshell_smoke_test_mtls
    ///
    /// This is the one real proof point for `connect_mtls`: `kamajid`'s
    /// binary installs the `aws-lc-rs` rustls `CryptoProvider` as process
    /// default (see the comment above that `install_default()` call in
    /// `kamajid::main()`), because Cargo feature unification pulls in both
    /// `aws-lc-rs` and `ring` across this dependency tree. Building a
    /// `ClientTlsConfig` with a client identity, inside a binary where the
    /// global default is `aws-lc-rs`, against a gateway whose own stack was
    /// built and tested with `ring` (`openshell-cli`'s workspace pins
    /// `rustls` with the `ring` feature), is a code path that has to be
    /// checked against the real gateway -- not assumed safe just because
    /// `openshell-cli` (a different binary, different crypto-provider
    /// resolution) does the analogous thing successfully.
    #[tokio::test]
    #[ignore = "requires a real OpenShell gateway with --enable-mtls-auth + pre-provisioned sandbox"]
    async fn openshell_smoke_test_mtls() {
        let gateway_url =
            std::env::var("OPENSHELL_SMOKE_MTLS_GATEWAY_URL").expect("set for manual runs");
        let sandbox_name =
            std::env::var("OPENSHELL_SMOKE_MTLS_SANDBOX_NAME").expect("set for manual runs");
        let mtls_dir = std::env::var("OPENSHELL_SMOKE_MTLS_DIR").expect("set for manual runs");
        let mtls_dir = std::path::PathBuf::from(mtls_dir);

        let runner = Runner::connect(Some(&OpenShellConfig {
            gateway_url,
            sandbox_name,
            ready_timeout: Duration::from_secs(30),
            mtls: Some(OpenShellMtlsConfig {
                ca_cert_path: mtls_dir.join("ca.crt"),
                client_cert_path: mtls_dir.join("tls.crt"),
                client_key_path: mtls_dir.join("tls.key"),
            }),
        }))
        .await
        .expect("mTLS gateway should be reachable and sandbox should become ready");

        let output = invoke(&runner, "echo", &["mtls-ok"], "", Duration::from_secs(30))
            .await
            .expect("exec should round-trip through the mTLS-authenticated gateway");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "mtls-ok");
    }
}
