//! The local database (`contextai.db`, SQLite): history and words to learn; chats later.
//!
//! Rows carry UUIDv7 ids, `updated_at` and a `deleted_at` tombstone instead of being removed at
//! once, so that a later sync between devices can merge them. Schema changes are numbered
//! migrations tracked in `PRAGMA user_version`.

mod cards;
mod history;

use std::{
    path::Path,
    sync::{
        atomic::{AtomicI64, Ordering},
        Mutex, MutexGuard,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};

pub use cards::{Card, CardEdit, CardQuery, NewCard, SavedCard};
pub use history::{HistoryEntry, HistoryKind, HistoryQuery, HistorySettings, NewHistoryEntry};

/// Each step runs once, in order, inside one transaction with the version bump.
const MIGRATIONS: &[&str] = &[
    // 1: history with full-text search, and app settings that travel with the data.
    r#"
    CREATE TABLE history (
        seq INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        kind TEXT NOT NULL,
        query TEXT NOT NULL,
        result TEXT NOT NULL,
        provider TEXT NOT NULL DEFAULT '',
        source_lang TEXT NOT NULL DEFAULT '',
        target_lang TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL,
        used_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        pinned INTEGER NOT NULL DEFAULT 0,
        deleted_at INTEGER
    );
    CREATE INDEX history_used ON history (used_at DESC) WHERE deleted_at IS NULL;
    CREATE INDEX history_lookup ON history (kind, query) WHERE deleted_at IS NULL;

    CREATE VIRTUAL TABLE history_fts USING fts5(
        query, result,
        content = 'history', content_rowid = 'seq',
        tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE TRIGGER history_fts_insert AFTER INSERT ON history BEGIN
        INSERT INTO history_fts (rowid, query, result) VALUES (new.seq, new.query, new.result);
    END;
    CREATE TRIGGER history_fts_delete AFTER DELETE ON history BEGIN
        INSERT INTO history_fts (history_fts, rowid, query, result)
            VALUES ('delete', old.seq, old.query, old.result);
    END;
    CREATE TRIGGER history_fts_update AFTER UPDATE OF query, result ON history BEGIN
        INSERT INTO history_fts (history_fts, rowid, query, result)
            VALUES ('delete', old.seq, old.query, old.result);
        INSERT INTO history_fts (rowid, query, result) VALUES (new.seq, new.query, new.result);
    END;

    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    "#,
    // 2: words to learn, with one review state per direction.
    r#"
    CREATE TABLE cards (
        seq INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        term TEXT NOT NULL,
        term_key TEXT NOT NULL,
        language TEXT NOT NULL DEFAULT '',
        meaning_lang TEXT NOT NULL DEFAULT '',
        meaning_html TEXT NOT NULL DEFAULT '',
        meaning_text TEXT NOT NULL DEFAULT '',
        transcription TEXT NOT NULL DEFAULT '',
        context TEXT NOT NULL DEFAULT '',
        source TEXT NOT NULL DEFAULT '',
        tags TEXT NOT NULL DEFAULT '[]',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        deleted_at INTEGER
    );
    CREATE UNIQUE INDEX cards_term ON cards (term_key, language) WHERE deleted_at IS NULL;
    CREATE INDEX cards_created ON cards (created_at DESC) WHERE deleted_at IS NULL;

    CREATE VIRTUAL TABLE cards_fts USING fts5(
        term, meaning_text, context, tags,
        content = 'cards', content_rowid = 'seq',
        tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE TRIGGER cards_fts_insert AFTER INSERT ON cards BEGIN
        INSERT INTO cards_fts (rowid, term, meaning_text, context, tags)
            VALUES (new.seq, new.term, new.meaning_text, new.context, new.tags);
    END;
    CREATE TRIGGER cards_fts_delete AFTER DELETE ON cards BEGIN
        INSERT INTO cards_fts (cards_fts, rowid, term, meaning_text, context, tags)
            VALUES ('delete', old.seq, old.term, old.meaning_text, old.context, old.tags);
    END;
    CREATE TRIGGER cards_fts_update AFTER UPDATE OF term, meaning_text, context, tags ON cards BEGIN
        INSERT INTO cards_fts (cards_fts, rowid, term, meaning_text, context, tags)
            VALUES ('delete', old.seq, old.term, old.meaning_text, old.context, old.tags);
        INSERT INTO cards_fts (rowid, term, meaning_text, context, tags)
            VALUES (new.seq, new.term, new.meaning_text, new.context, new.tags);
    END;

    CREATE TABLE card_states (
        card_id TEXT NOT NULL REFERENCES cards (id) ON DELETE CASCADE,
        direction TEXT NOT NULL CHECK (direction IN ('forward', 'reverse')),
        due_at INTEGER NOT NULL,
        interval_days REAL NOT NULL DEFAULT 0,
        ease REAL NOT NULL DEFAULT 2.5,
        reps INTEGER NOT NULL DEFAULT 0,
        lapses INTEGER NOT NULL DEFAULT 0,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (card_id, direction)
    );
    CREATE INDEX card_states_due ON card_states (due_at);
    "#,
];

/// Expired history is removed at most this often while adding entries.
const PURGE_INTERVAL_MS: i64 = 60 * 60 * 1000;

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub(crate) fn db_error(error: rusqlite::Error) -> String {
    format!("Грешка в базата данни: {error}")
}

pub struct Storage {
    connection: Mutex<Connection>,
    last_purge: AtomicI64,
}

impl Storage {
    /// Opens (or creates) the database file and brings its schema up to date.
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory).map_err(|error| {
                format!("Неуспешно създаване на {}: {error}", directory.display())
            })?;
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("Неуспешно отваряне на {}: {error}", path.display()))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(db_error)?;
        Self::with_connection(connection)
    }

    /// A throwaway database for tests.
    pub fn open_in_memory() -> Result<Self, String> {
        Self::with_connection(Connection::open_in_memory().map_err(db_error)?)
    }

    fn with_connection(mut connection: Connection) -> Result<Self, String> {
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(db_error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_error)?;
        migrate(&mut connection)?;
        let storage = Self {
            connection: Mutex::new(connection),
            last_purge: AtomicI64::new(0),
        };
        storage.purge_expired(now_ms())?;
        Ok(storage)
    }

    pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs `purge_expired` when the last run is older than an hour.
    pub(crate) fn purge_if_due(&self, now: i64) -> Result<(), String> {
        let last = self.last_purge.load(Ordering::Relaxed);
        if now - last >= PURGE_INTERVAL_MS {
            self.purge_expired(now)?;
        }
        Ok(())
    }

    pub(crate) fn mark_purged(&self, now: i64) {
        self.last_purge.store(now, Ordering::Relaxed);
    }

    /// A JSON value stored under `key`, or the default when it was never saved.
    pub fn setting<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T, String> {
        let value: Option<String> = self
            .connection()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_error)?;
        Ok(value
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default())
    }

    pub fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<(), String> {
        let value = serde_json::to_string(value).map_err(|error| error.to_string())?;
        self.connection()
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .map_err(db_error)?;
        Ok(())
    }
}

fn migrate(connection: &mut Connection) -> Result<(), String> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version > MIGRATIONS.len() as i64 {
        return Err(
            "Базата данни е създадена от по-нова версия на ContextAI. Обновете приложението."
                .to_string(),
        );
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let transaction = connection.transaction().map_err(db_error)?;
        transaction.execute_batch(migration).map_err(db_error)?;
        transaction
            .pragma_update(None, "user_version", index as i64 + 1)
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once_and_settings_round_trip() {
        let directory =
            std::env::temp_dir().join(format!("contextai-db-test-{}", std::process::id()));
        let path = directory.join("contextai.db");
        {
            let storage = Storage::open(&path).unwrap();
            assert_eq!(
                storage.setting::<Vec<String>>("missing").unwrap(),
                Vec::<String>::new()
            );
            storage.set_setting("words", &vec!["a", "b"]).unwrap();
        }
        // Reopening must not run the migrations again.
        let storage = Storage::open(&path).unwrap();
        assert_eq!(
            storage.setting::<Vec<String>>("words").unwrap(),
            vec!["a", "b"]
        );
        drop(storage);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
