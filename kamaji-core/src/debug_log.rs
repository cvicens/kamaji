use std::io::Write;
use std::path::Path;

use chrono::Utc;

use crate::error::DebugLogError;

/// Appends a debug-only record of a job's prompt and payload to `path`.
/// A no-op unless `enabled` (the `DEBUG` env var) -- this exists to inspect
/// what triggered a job and what it produced while developing, not for
/// normal operation, so a write failure here must never fail the job
/// itself: callers log-and-skip, matching the treatment of every other
/// non-essential side effect in the worker.
pub fn log_job(
    path: &Path,
    enabled: bool,
    job_id: u64,
    prompt: &str,
    payload: &str,
) -> Result<(), DebugLogError> {
    if !enabled {
        return Ok(());
    }

    let entry = format!(
        "---\n[{}] job_id={job_id}\nprompt: {prompt}\npayload: {payload}\n",
        Utc::now().to_rfc3339()
    );

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| DebugLogError::Open {
            path: path.to_path_buf(),
            source,
        })?;

    file.write_all(entry.as_bytes())
        .map_err(|source| DebugLogError::Write {
            path: path.to_path_buf(),
            source,
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_prompt_and_payload_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");

        log_job(&path, true, 1, "/status", "Queue depth: 0").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("job_id=1"));
        assert!(contents.contains("prompt: /status"));
        assert!(contents.contains("payload: Queue depth: 0"));
    }

    #[test]
    fn appends_multiple_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");

        log_job(&path, true, 1, "first prompt", "first payload").unwrap();
        log_job(&path, true, 2, "second prompt", "second payload").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("job_id=1"));
        assert!(contents.contains("job_id=2"));
    }

    #[test]
    fn does_nothing_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");

        log_job(&path, false, 1, "/status", "Queue depth: 0").unwrap();

        assert!(!path.exists());
    }
}
