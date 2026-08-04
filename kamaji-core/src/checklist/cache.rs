use std::sync::Arc;

use redb::{Database, ReadableDatabase};

use crate::chat::ChatRef;
use crate::db::CHECKLIST_LIST_CACHE;
use crate::error::ChecklistError;

use super::{Config, EntryKey};

/// Backs the short-number shorthand for `/todo resolve|reopen <n>` (and the
/// `/goal` equivalents): the most recently shown `list` for a given domain
/// (todo/goal) and chat/room, so a plain small number can stand in for the
/// full `EntryKey`. Structured like `auth::SessionStore` -- owns a shared
/// `Arc<Database>`, no chat-platform concerns of its own. A stale/missing
/// cache entry is not an error: it just means "no recent list to resolve
/// against", surfaced as `resolve` returning `None` rather than an `Err`.
pub struct ChecklistCache {
    db: Arc<Database>,
}

impl ChecklistCache {
    pub fn new(db: Arc<Database>) -> Self {
        ChecklistCache { db }
    }

    /// Namespaced by domain (`cfg.command_name`, e.g. "todo"/"goal") so a
    /// `/goal list` doesn't clobber the shorthand numbers a preceding
    /// `/todo list` set up for the same chat, and by `chat` (via `ChatRef`'s
    /// existing `Display`) so different chats/rooms never see each other's
    /// shown lists.
    fn cache_key(cfg: &Config, chat: &ChatRef) -> String {
        format!("{}:{chat}", cfg.command_name)
    }

    /// Records the order `keys` were just shown in for `chat`, replacing
    /// whatever was cached before for this domain+chat. Position 0 becomes
    /// shorthand `1`, and so on.
    pub fn remember(
        &self,
        cfg: &Config,
        chat: &ChatRef,
        keys: &[EntryKey],
    ) -> Result<(), ChecklistError> {
        let rendered: Vec<String> = keys.iter().map(EntryKey::to_string).collect();
        let payload = serde_json::to_string(&rendered)?;
        let cache_key = Self::cache_key(cfg, chat);
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CHECKLIST_LIST_CACHE)?;
            table.insert(cache_key.as_str(), payload.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Resolves shorthand number `n` (1-based) against the most recently
    /// shown list for `chat`. `Ok(None)` covers both "nothing has been
    /// listed yet" and "n is out of range" -- both are the same "there's
    /// nothing to resolve this against" outcome as far as the caller is
    /// concerned.
    pub fn resolve(
        &self,
        cfg: &Config,
        chat: &ChatRef,
        n: u32,
    ) -> Result<Option<EntryKey>, ChecklistError> {
        let cache_key = Self::cache_key(cfg, chat);
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHECKLIST_LIST_CACHE)?;
        let Some(payload) = table
            .get(cache_key.as_str())?
            .map(|guard| guard.value().to_string())
        else {
            return Ok(None);
        };
        let rendered: Vec<String> = serde_json::from_str(&payload)?;
        let Some(index) = (n as usize).checked_sub(1) else {
            return Ok(None);
        };
        Ok(rendered.get(index).and_then(|s| s.parse::<EntryKey>().ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CFG: Config = Config {
        command_name: "thing",
        folder: "thing",
        plural_noun: "things",
        close_subcommand: "finish",
        reopen_subcommand: "reopen",
        closed_verb: "finished",
        okf_type: "thing",
        link_field: Some("link"),
    };

    fn test_cache() -> (ChecklistCache, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(&dir.path().join("test.redb"));
        (ChecklistCache::new(Arc::new(db)), dir)
    }

    fn chat() -> ChatRef {
        ChatRef::Telegram { chat_id: 42 }
    }

    fn key(y: i32, m: u32, d: u32, line: u32) -> EntryKey {
        EntryKey {
            date: chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            line,
        }
    }

    #[test]
    fn remember_then_resolve_by_position() {
        let (cache, _dir) = test_cache();
        let keys = vec![key(2026, 8, 3, 1), key(2026, 8, 3, 2)];
        cache.remember(&TEST_CFG, &chat(), &keys).unwrap();

        assert_eq!(cache.resolve(&TEST_CFG, &chat(), 1).unwrap(), Some(keys[0]));
        assert_eq!(cache.resolve(&TEST_CFG, &chat(), 2).unwrap(), Some(keys[1]));
    }

    #[test]
    fn resolve_out_of_range_or_zero_is_none_not_an_error() {
        let (cache, _dir) = test_cache();
        cache
            .remember(&TEST_CFG, &chat(), &[key(2026, 8, 3, 1)])
            .unwrap();

        assert_eq!(cache.resolve(&TEST_CFG, &chat(), 5).unwrap(), None);
        assert_eq!(cache.resolve(&TEST_CFG, &chat(), 0).unwrap(), None);
    }

    #[test]
    fn resolve_with_no_prior_list_is_none() {
        let (cache, _dir) = test_cache();
        assert_eq!(cache.resolve(&TEST_CFG, &chat(), 1).unwrap(), None);
    }

    #[test]
    fn remember_overwrites_the_previous_list_for_the_same_chat_and_domain() {
        let (cache, _dir) = test_cache();
        cache
            .remember(&TEST_CFG, &chat(), &[key(2026, 8, 3, 1)])
            .unwrap();
        cache
            .remember(&TEST_CFG, &chat(), &[key(2026, 8, 4, 1)])
            .unwrap();

        assert_eq!(
            cache.resolve(&TEST_CFG, &chat(), 1).unwrap(),
            Some(key(2026, 8, 4, 1))
        );
    }

    #[test]
    fn different_chats_get_independent_caches() {
        let (cache, _dir) = test_cache();
        cache
            .remember(&TEST_CFG, &chat(), &[key(2026, 8, 3, 1)])
            .unwrap();

        let other = ChatRef::Matrix {
            room_id: "!abc:server".to_string(),
        };
        assert_eq!(cache.resolve(&TEST_CFG, &other, 1).unwrap(), None);
    }
}
