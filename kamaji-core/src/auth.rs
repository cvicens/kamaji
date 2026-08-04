use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableTable};
use serde::{Deserialize, Serialize};
use totp_rs::{Algorithm, Secret, TOTP};

use crate::db::SESSIONS;
use crate::error::AuthError;

const TOTP_STEP_SECS: u64 = 30;
const TOTP_SKEW_STEPS: u8 = 1;
const TOTP_DIGITS: usize = 6;

#[derive(Debug, Serialize, Deserialize)]
struct SessionRecord {
    issued_at: u64,
    expires_at: u64,
}

/// Backs the REST API's TOTP login + bearer-session auth (see
/// `kamajid::transport::rest`). Structured like `Queue`: owns a shared
/// `Arc<Database>`, exposes typed methods, no HTTP framing concerns.
pub struct SessionStore {
    db: Arc<Database>,
    /// Guards against replaying an already-accepted TOTP code: the last
    /// time-step a login succeeded for, so a captured code can't be
    /// resubmitted even within its own skew window. Resets on daemon
    /// restart -- fine for a single-user tool, not worth persisting.
    last_totp_step: AtomicU64,
}

impl SessionStore {
    pub fn new(db: Arc<Database>) -> Self {
        SessionStore {
            db,
            last_totp_step: AtomicU64::new(0),
        }
    }

    /// Checks `code` against `secret` (base32-encoded) for the current time
    /// step, allowing `TOTP_SKEW_STEPS` of clock drift either side. Rejects
    /// a code if its step is at or before the last one that already
    /// succeeded, so a captured code is single-use.
    pub fn verify_totp(&self, secret: &str, code: &str) -> bool {
        let current_step = now_unix() / TOTP_STEP_SECS;
        if current_step <= self.last_totp_step.load(Ordering::SeqCst) {
            return false;
        }

        let Ok(secret_bytes) = Secret::Encoded(secret.to_string()).to_bytes() else {
            tracing::error!("REST_API_TOTP_SECRET is not valid base32");
            return false;
        };
        let Ok(totp) = TOTP::new(
            Algorithm::SHA1,
            TOTP_DIGITS,
            TOTP_SKEW_STEPS,
            TOTP_STEP_SECS,
            secret_bytes,
            None,
            String::new(),
        ) else {
            tracing::error!("failed to construct TOTP validator from REST_API_TOTP_SECRET");
            return false;
        };

        match totp.check_current(code) {
            Ok(true) => {
                self.last_totp_step.store(current_step, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// Issues a fresh bearer token valid for `ttl`.
    pub fn create_session(&self, ttl: Duration) -> Result<String, AuthError> {
        let token = generate_token();
        let now = now_unix();
        let record = SessionRecord {
            issued_at: now,
            expires_at: now + ttl.as_secs(),
        };
        let payload = serde_json::to_string(&record)?;
        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            sessions.insert(token.as_str(), payload.as_str())?;
        }
        txn.commit()?;
        Ok(token)
    }

    /// Validates `token`, deleting it if it has expired. Lazy pruning --
    /// no background sweep needed at this scale.
    pub fn validate_session(&self, token: &str) -> Result<bool, AuthError> {
        let txn = self.db.begin_write()?;
        let valid = {
            let mut sessions = txn.open_table(SESSIONS)?;
            let existing = sessions.get(token)?.map(|guard| guard.value().to_string());
            match existing {
                Some(payload) => {
                    let record: SessionRecord = serde_json::from_str(&payload)?;
                    if record.expires_at <= now_unix() {
                        sessions.remove(token)?;
                        false
                    } else {
                        true
                    }
                }
                None => false,
            }
        };
        txn.commit()?;
        Ok(valid)
    }

    /// Deletes `token` if present. Idempotent -- deleting an
    /// already-missing or already-expired token isn't an error, matching
    /// `kamaji logout`'s "make sure this token can't be used again"
    /// semantics regardless of whether it already expired on its own.
    pub fn revoke_session(&self, token: &str) -> Result<(), AuthError> {
        let txn = self.db.begin_write()?;
        {
            let mut sessions = txn.open_table(SESSIONS)?;
            sessions.remove(token)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Deletes every session. The fallback for "I lost the device, kill
    /// every remote session now" -- there's no way to tell which token is
    /// "the leaked one" from the daemon side, so this is all-or-nothing.
    /// Returns how many were removed.
    pub fn revoke_all_sessions(&self) -> Result<usize, AuthError> {
        let txn = self.db.begin_write()?;
        let removed = {
            let mut sessions = txn.open_table(SESSIONS)?;
            let tokens: Vec<String> = sessions
                .iter()?
                .filter_map(|entry| entry.ok())
                .map(|(k, _)| k.value().to_string())
                .collect();
            for token in &tokens {
                sessions.remove(token.as_str())?;
            }
            tokens.len()
        };
        txn.commit()?;
        Ok(removed)
    }
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("test.redb"));
        let store = SessionStore::new(Arc::new(db));
        (store, dir)
    }

    fn test_secret() -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 20];
        rand::thread_rng().fill_bytes(&mut bytes);
        match Secret::Raw(bytes.to_vec()).to_encoded() {
            Secret::Encoded(s) => s,
            Secret::Raw(_) => unreachable!("to_encoded always returns Secret::Encoded"),
        }
    }

    fn code_for(secret: &str) -> String {
        let bytes = Secret::Encoded(secret.to_string()).to_bytes().unwrap();
        let totp = TOTP::new(
            Algorithm::SHA1,
            TOTP_DIGITS,
            TOTP_SKEW_STEPS,
            TOTP_STEP_SECS,
            bytes,
            None,
            String::new(),
        )
        .unwrap();
        totp.generate_current().unwrap()
    }

    #[test]
    fn session_create_and_validate_roundtrip() {
        let (store, _dir) = test_store();
        let token = store.create_session(Duration::from_secs(60)).unwrap();
        assert!(store.validate_session(&token).unwrap());
    }

    #[test]
    fn session_unknown_token_is_invalid() {
        let (store, _dir) = test_store();
        assert!(!store.validate_session("nonexistent").unwrap());
    }

    #[test]
    fn session_expired_token_is_invalid_and_pruned() {
        let (store, _dir) = test_store();
        let token = store.create_session(Duration::from_secs(0)).unwrap();
        assert!(!store.validate_session(&token).unwrap());
        // Pruned on the failed check -- a second lookup must not resurrect it.
        assert!(!store.validate_session(&token).unwrap());
    }

    #[test]
    fn totp_accepts_valid_code() {
        let (store, _dir) = test_store();
        let secret = test_secret();
        let code = code_for(&secret);
        assert!(store.verify_totp(&secret, &code));
    }

    #[test]
    fn totp_rejects_wrong_code() {
        let (store, _dir) = test_store();
        let secret = test_secret();
        assert!(!store.verify_totp(&secret, "000000"));
    }

    #[test]
    fn totp_rejects_replayed_code() {
        let (store, _dir) = test_store();
        let secret = test_secret();
        let code = code_for(&secret);
        assert!(store.verify_totp(&secret, &code));
        assert!(!store.verify_totp(&secret, &code));
    }

    #[test]
    fn revoke_session_invalidates_a_valid_token() {
        let (store, _dir) = test_store();
        let token = store.create_session(Duration::from_secs(60)).unwrap();
        store.revoke_session(&token).unwrap();
        assert!(!store.validate_session(&token).unwrap());
    }

    #[test]
    fn revoke_session_on_unknown_token_is_not_an_error() {
        let (store, _dir) = test_store();
        store.revoke_session("nonexistent").unwrap();
    }

    #[test]
    fn revoke_all_sessions_removes_every_token_and_reports_count() {
        let (store, _dir) = test_store();
        let a = store.create_session(Duration::from_secs(60)).unwrap();
        let b = store.create_session(Duration::from_secs(60)).unwrap();

        let removed = store.revoke_all_sessions().unwrap();

        assert_eq!(removed, 2);
        assert!(!store.validate_session(&a).unwrap());
        assert!(!store.validate_session(&b).unwrap());
    }

    #[test]
    fn revoke_all_sessions_on_empty_store_removes_nothing() {
        let (store, _dir) = test_store();
        assert_eq!(store.revoke_all_sessions().unwrap(), 0);
    }
}
