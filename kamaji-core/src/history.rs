use chrono::{DateTime, Utc};
use redb::{ReadableDatabase, ReadableTable};
use serde::{Deserialize, Serialize};

use crate::prompt::TokenUsage;

/// A completed job record stored in the `job_history` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobHistoryRecord {
    pub job_id: u64,
    pub kind: JobKindSummary,
    pub completed_at: DateTime<Utc>,
    pub status: JobStatus,
    pub error_message: Option<String>,
    pub tokens: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JobKindSummary {
    Ingest,
    Command { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobStatus {
    Success,
    Failed,
}

impl JobHistoryRecord {
    pub fn new_success(job_id: u64, kind: JobKindSummary, tokens: Option<TokenUsage>) -> Self {
        Self {
            job_id,
            kind,
            completed_at: Utc::now(),
            status: JobStatus::Success,
            error_message: None,
            tokens,
        }
    }

    pub fn new_failure(
        job_id: u64,
        kind: JobKindSummary,
        error: String,
        tokens: Option<TokenUsage>,
    ) -> Self {
        Self {
            job_id,
            kind,
            completed_at: Utc::now(),
            status: JobStatus::Failed,
            error_message: Some(error),
            tokens,
        }
    }
}

/// Write a job history record to the database.
pub fn log_job(db: &redb::Database, record: &JobHistoryRecord) -> Result<(), redb::Error> {
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(crate::db::JOB_HISTORY)?;
        let json = serde_json::to_string(record).expect("JobHistoryRecord serializes");
        table.insert(record.job_id, json.as_str())?;
    }
    txn.commit()?;
    Ok(())
}

/// Query the most recent job history records, up to `limit` entries.
/// Returns records sorted by job_id descending (newest first).
pub fn query_recent(
    db: &redb::Database,
    limit: usize,
) -> Result<Vec<JobHistoryRecord>, redb::Error> {
    let txn = db.begin_read()?;
    let table = txn.open_table(crate::db::JOB_HISTORY)?;

    let mut records: Vec<JobHistoryRecord> = table
        .iter()?
        .filter_map(|entry| entry.ok())
        .filter_map(|(_, json_value)| {
            let json = json_value.value();
            serde_json::from_str(json).ok()
        })
        .collect();

    // Sort by job_id descending (newest first)
    records.sort_by_key(|r| std::cmp::Reverse(r.job_id));
    records.truncate(limit);

    Ok(records)
}
