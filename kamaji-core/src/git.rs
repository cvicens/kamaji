use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::error::GitError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,
    /// Remote had commits we didn't; we pulled with `--rebase`, replayed our
    /// commit on top, and the retried push succeeded.
    PushedAfterRebase,
    CommittedNotPushed {
        reason: NotPushedReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotPushedReason {
    /// Today's original failure mode: transient (network/auth) errors, blind
    /// retry with backoff exhausted without success.
    ExhaustedRetries,
    /// Remote was ahead (push rejected as non-fast-forward), but the working
    /// tree had uncommitted changes so `git pull --rebase` was skipped rather
    /// than risking an autostash pop conflicting on top of a mid-rebase repo.
    RemoteAheadDirtyTree,
    /// Remote was ahead; `git pull --rebase` was attempted and conflicted.
    /// The rebase was aborted (guardrail: never leave the shared working
    /// directory mid-rebase), so the local commit is intact and unpushed.
    RebaseConflict,
}

impl fmt::Display for NotPushedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotPushedReason::ExhaustedRetries => {
                write!(f, "committed locally, but git push failed after retries")
            }
            NotPushedReason::RemoteAheadDirtyTree => write!(
                f,
                "committed locally, but not pushed: remote has new commits and the \
                 working tree has other uncommitted changes -- pull and rebase by hand"
            ),
            NotPushedReason::RebaseConflict => write!(
                f,
                "committed locally, but not pushed: remote had new commits and \
                 rebasing onto them conflicted -- resolve by hand in the notes repo"
            ),
        }
    }
}

/// Outcome of classifying a rejected push and, if warranted, acting on it.
enum ClassifyOutcome {
    /// Not a non-fast-forward rejection (or we couldn't tell, e.g. no
    /// upstream configured, fetch failed) -- caller should fall back to the
    /// ordinary blind-retry-with-backoff path, unchanged from before this
    /// existed.
    NotRemoteAhead,
    RebasedCleanly,
    DirtyWorkingTree,
    Conflict,
}

/// `git add`, `git commit`, then `git push` with bounded retry + backoff.
///
/// Takes a slice of paths (not one): an ingest note is a single file, but a
/// `/fact` entry is 2-3 (the rendered `.md`, the verbatim `.orig`, and an
/// optional attachment), and they all belong in one commit rather than one
/// commit per file.
///
/// A commit failure (e.g. `git add`/`git commit` erroring) is returned as an
/// `Err` -- the files still exist on disk either way, so nothing is lost,
/// but the worker needs to know to tell the user commit itself failed
/// rather than just "didn't push". A push failure is NOT an error: it's
/// reported as `CommittedNotPushed` so the caller can reply "written and
/// committed locally, but not pushed" instead of discarding a perfectly
/// good commit.
///
/// If the first push attempt is rejected, we classify *why* before blindly
/// retrying: a non-fast-forward rejection (remote moved because the notes
/// repo was edited from elsewhere -- laptop, GitHub web) can never be fixed
/// by retrying the identical push, so we integrate the remote commits via
/// `git pull --rebase` (at most once) and push again instead of burning the
/// whole backoff budget on a push that can't succeed.
pub async fn commit_and_push(
    repo_root: &Path,
    relative_paths: &[PathBuf],
    commit_message: &str,
    timeout: Duration,
    push_retries: u32,
) -> Result<PushOutcome, GitError> {
    let mut add_args: Vec<String> = vec!["add".to_string()];
    add_args.extend(
        relative_paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned()),
    );
    let add_args: Vec<&str> = add_args.iter().map(String::as_str).collect();
    run_git(repo_root, &add_args, timeout, "add").await?;
    run_git(
        repo_root,
        &["commit", "-m", commit_message],
        timeout,
        "commit",
    )
    .await?;

    let mut attempt = 0u32;
    let mut rebased = false;

    loop {
        match run_git(repo_root, &["push"], timeout, "push").await {
            Ok(()) => {
                return Ok(if rebased {
                    PushOutcome::PushedAfterRebase
                } else {
                    PushOutcome::Pushed
                });
            }
            Err(err) => {
                if !rebased {
                    match classify_and_maybe_rebase(repo_root, timeout).await {
                        ClassifyOutcome::RebasedCleanly => {
                            rebased = true;
                            // Real progress was made (remote commits integrated); retry
                            // the push right away rather than sleeping a backoff meant
                            // for transient network/auth errors.
                            continue;
                        }
                        ClassifyOutcome::DirtyWorkingTree => {
                            tracing::warn!(
                                "push rejected, remote ahead, but working tree dirty; skipping rebase"
                            );
                            return Ok(PushOutcome::CommittedNotPushed {
                                reason: NotPushedReason::RemoteAheadDirtyTree,
                            });
                        }
                        ClassifyOutcome::Conflict => {
                            tracing::warn!(
                                "push rejected, remote ahead, rebase conflicted; aborted"
                            );
                            return Ok(PushOutcome::CommittedNotPushed {
                                reason: NotPushedReason::RebaseConflict,
                            });
                        }
                        ClassifyOutcome::NotRemoteAhead => {
                            // Not a non-fast-forward rejection (or couldn't classify);
                            // fall through to the ordinary blind-retry path below.
                        }
                    }
                }

                if attempt < push_retries {
                    attempt += 1;
                    tracing::warn!(attempt, %err, "git push failed, retrying with backoff");
                    let backoff = Duration::from_secs(2u64.saturating_pow(attempt));
                    tokio::time::sleep(backoff).await;
                } else {
                    tracing::error!(%err, "git push failed after all retries; note stays committed locally");
                    return Ok(PushOutcome::CommittedNotPushed {
                        reason: NotPushedReason::ExhaustedRetries,
                    });
                }
            }
        }
    }
}

/// Classifies a rejected push (is the remote actually ahead of us?) and, if
/// so and the working tree is clean, integrates it via `git pull --rebase`.
/// Classification failures (no upstream configured, fetch unreachable) are
/// swallowed and reported as `NotRemoteAhead` -- they mean "we can't tell",
/// which is exactly the case that should fall back to the pre-existing
/// blind-retry behavior rather than surfacing a confusing secondary error
/// for what is still fundamentally a push failure.
async fn classify_and_maybe_rebase(repo_root: &Path, timeout: Duration) -> ClassifyOutcome {
    if let Err(err) = run_git(repo_root, &["fetch"], timeout, "fetch").await {
        tracing::warn!(%err, "git fetch failed while classifying a rejected push");
        return ClassifyOutcome::NotRemoteAhead;
    }

    let ahead = match run_git_stdout(
        repo_root,
        &["rev-list", "--count", "HEAD..@{u}"],
        timeout,
        "rev_list",
    )
    .await
    {
        Ok(stdout) => stdout.trim().parse::<u64>().unwrap_or(0) > 0,
        Err(err) => {
            tracing::warn!(%err, "could not determine upstream ahead-count; no upstream configured?");
            false
        }
    };
    if !ahead {
        return ClassifyOutcome::NotRemoteAhead;
    }

    // `--untracked-files=no` is load-bearing, not tidiness: untracked files do
    // not stop `git pull --rebase`, but they *do* show up in a plain
    // `--porcelain` listing. Counting them as dirty would mean one stray
    // untracked file -- e.g. a note left behind by a job whose `git commit`
    // errored -- silently disables this whole rebase path forever, sending
    // every rejected push to "pull by hand". Only uncommitted changes to
    // tracked files actually make rebase refuse.
    let dirty = match run_git_stdout(
        repo_root,
        &["status", "--porcelain", "--untracked-files=no"],
        timeout,
        "status",
    )
    .await
    {
        Ok(stdout) => !stdout.trim().is_empty(),
        Err(err) => {
            tracing::warn!(%err, "git status failed while classifying a rejected push");
            return ClassifyOutcome::NotRemoteAhead;
        }
    };
    if dirty {
        return ClassifyOutcome::DirtyWorkingTree;
    }

    match run_git(repo_root, &["pull", "--rebase"], timeout, "pull_rebase").await {
        Ok(()) => ClassifyOutcome::RebasedCleanly,
        Err(err) => {
            tracing::warn!(%err, "git pull --rebase conflicted; aborting rebase");
            // Guardrail: kamaji is a long-lived daemon reusing one working
            // directory, so a rebase left stopped at a conflict would poison
            // every subsequent job (the next `git add`/`git commit` would run
            // inside an unfinished rebase). Always abort, unconditionally.
            if let Err(abort_err) =
                run_git(repo_root, &["rebase", "--abort"], timeout, "rebase_abort").await
            {
                tracing::error!(%abort_err, "git rebase --abort itself failed");
            }
            ClassifyOutcome::Conflict
        }
    }
}

async fn run_git(
    repo_root: &Path,
    args: &[&str],
    timeout: Duration,
    subcommand: &'static str,
) -> Result<(), GitError> {
    run_git_stdout(repo_root, args, timeout, subcommand)
        .await
        .map(|_| ())
}

async fn run_git_stdout(
    repo_root: &Path,
    args: &[&str],
    timeout: Duration,
    subcommand: &'static str,
) -> Result<String, GitError> {
    let output = tokio::time::timeout(
        timeout,
        Command::new("git")
            .current_dir(repo_root)
            .args(args)
            .output(),
    )
    .await
    .map_err(|_| GitError::Timeout { subcommand })?
    .map_err(|source| GitError::Spawn { subcommand, source })?;

    if !output.status.success() {
        return Err(GitError::CommandFailed {
            subcommand,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .await
            .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn init_bare_origin(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "--bare", "-q", "-b", "main"]).await;
    }

    async fn init_local_clone(origin: &Path, local: &Path) {
        git(
            local.parent().unwrap(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                local.file_name().unwrap().to_str().unwrap(),
            ],
        )
        .await;
        git(local, &["config", "user.email", "kamaji@example.com"]).await;
        git(local, &["config", "user.name", "kamaji"]).await;
    }

    async fn write_and_commit(repo: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(repo.join(file), contents).unwrap();
        git(repo, &["add", file]).await;
        git(repo, &["commit", "-q", "-m", message]).await;
    }

    async fn head_commit(repo: &Path) -> String {
        let output = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn timeout() -> Duration {
        Duration::from_secs(10)
    }

    /// Sets up: bare `origin`, a `local` clone with one commit already pushed
    /// (establishing upstream tracking, as a real cloned notes repo would
    /// have), and a second `other` clone used to simulate "someone else
    /// pushed from elsewhere" before `local` tries to push again.
    async fn setup() -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let root = TempDir::new().unwrap();
        let origin = root.path().join("origin.git");
        let local = root.path().join("local");
        let other = root.path().join("other");

        init_bare_origin(&origin).await;
        init_local_clone(&origin, &local).await;
        write_and_commit(&local, "seed.md", "seed\n", "seed").await;
        git(&local, &["push", "-q", "-u", "origin", "main"]).await;

        init_local_clone(&origin, &other).await;

        (root, origin, local, other)
    }

    #[tokio::test]
    async fn remote_ahead_non_conflicting_rebases_and_pushes() {
        let (_root, origin, local, other) = setup().await;

        // Someone else pushes a commit touching a different file.
        write_and_commit(&other, "other.md", "from elsewhere\n", "other's note").await;
        git(&other, &["push", "-q"]).await;

        std::fs::write(local.join("mine.md"), "mine\n").unwrap();
        let outcome = commit_and_push(&local, &[PathBuf::from("mine.md")], "my note", timeout(), 0)
            .await
            .unwrap();

        assert_eq!(outcome, PushOutcome::PushedAfterRebase);

        // Both commits must be present on the bare remote.
        let log = Command::new("git")
            .current_dir(&origin)
            .args(["log", "--oneline", "main"])
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("other's note"));
        assert!(log.contains("my note"));
    }

    /// Regression guard: untracked files don't stop `git pull --rebase`, so
    /// they must not count as a dirty tree. Treating them as dirty meant one
    /// stray file (e.g. a note left behind by a failed commit) permanently
    /// disabled the rebase path and sent every rejected push to "pull by hand".
    #[tokio::test]
    async fn untracked_file_does_not_count_as_dirty() {
        let (_root, origin, local, other) = setup().await;

        write_and_commit(&other, "other.md", "from elsewhere\n", "other's note").await;
        git(&other, &["push", "-q"]).await;

        std::fs::write(local.join("stray.tmp"), "not tracked, not staged\n").unwrap();
        std::fs::write(local.join("mine.md"), "mine\n").unwrap();
        let outcome = commit_and_push(&local, &[PathBuf::from("mine.md")], "my note", timeout(), 0)
            .await
            .unwrap();

        assert_eq!(outcome, PushOutcome::PushedAfterRebase);

        let log = Command::new("git")
            .current_dir(&origin)
            .args(["log", "--oneline", "main"])
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("other's note"));
        assert!(log.contains("my note"));

        // The rebase must not have swept the user's untracked file away.
        assert!(local.join("stray.tmp").exists());
    }

    /// The flip side of the above: uncommitted edits to a *tracked* file are
    /// what genuinely make rebase refuse, so those still skip the rebase and
    /// report `RemoteAheadDirtyTree` rather than being autostashed.
    #[tokio::test]
    async fn uncommitted_tracked_change_skips_rebase() {
        let (_root, _origin, local, other) = setup().await;

        write_and_commit(&other, "other.md", "from elsewhere\n", "other's note").await;
        git(&other, &["push", "-q"]).await;

        // Edited by hand and left uncommitted -- not part of the note we commit.
        std::fs::write(local.join("seed.md"), "edited by hand\n").unwrap();
        std::fs::write(local.join("mine.md"), "mine\n").unwrap();

        let outcome = commit_and_push(&local, &[PathBuf::from("mine.md")], "my note", timeout(), 0)
            .await
            .unwrap();

        match outcome {
            PushOutcome::CommittedNotPushed {
                reason: NotPushedReason::RemoteAheadDirtyTree,
            } => {}
            other => panic!("expected RemoteAheadDirtyTree, got {other:?}"),
        }

        // The hand edit is still there, untouched.
        assert_eq!(
            std::fs::read_to_string(local.join("seed.md")).unwrap(),
            "edited by hand\n"
        );
    }

    #[tokio::test]
    async fn remote_ahead_conflicting_change_leaves_clean_worktree() {
        let (_root, _origin, local, other) = setup().await;

        // Someone else pushes a conflicting change to the same file we'll edit.
        write_and_commit(&other, "seed.md", "changed elsewhere\n", "elsewhere edit").await;
        git(&other, &["push", "-q"]).await;

        std::fs::write(local.join("seed.md"), "changed locally\n").unwrap();
        let our_head_before = head_commit(&local).await;

        let outcome = commit_and_push(
            &local,
            &[PathBuf::from("seed.md")],
            "my conflicting edit",
            timeout(),
            0,
        )
        .await
        .unwrap();

        match outcome {
            PushOutcome::CommittedNotPushed {
                reason: NotPushedReason::RebaseConflict,
            } => {}
            other => panic!("expected RebaseConflict, got {other:?}"),
        }

        // Regression guard for the poisoned-worktree bug: no leftover rebase
        // state, and HEAD moved to our new commit rather than sitting in some
        // partial rebase artifact.
        assert!(!local.join(".git/rebase-merge").exists());
        assert!(!local.join(".git/rebase-apply").exists());
        let head_after = head_commit(&local).await;
        assert_ne!(head_after, our_head_before);

        let status = Command::new("git")
            .current_dir(&local)
            .args(["status", "--porcelain"])
            .output()
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "working tree should be clean after rebase --abort"
        );
    }

    #[tokio::test]
    async fn non_rebase_related_push_failure_behaves_as_before() {
        let (_root, _origin, local, _other) = setup().await;

        // Point at a remote that can never be reached -- push (and the
        // classify step's `git fetch`) both fail for an unrelated reason, so
        // no rebase should ever be attempted.
        git(
            &local,
            &["remote", "set-url", "origin", "/nonexistent/bogus/path.git"],
        )
        .await;

        std::fs::write(local.join("mine.md"), "mine\n").unwrap();
        let outcome = commit_and_push(&local, &[PathBuf::from("mine.md")], "my note", timeout(), 0)
            .await
            .unwrap();

        match outcome {
            PushOutcome::CommittedNotPushed {
                reason: NotPushedReason::ExhaustedRetries,
            } => {}
            other => panic!("expected ExhaustedRetries, got {other:?}"),
        }
    }
}
