//! History of translations and dictionary lookups: kept 30 days, pinned entries for good,
//! searchable with FTS5. Looking the same thing up again moves the old entry to the top
//! instead of adding a copy.

use rusqlite::{params_from_iter, types::Value, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::{db_error, new_id, now_ms, Storage};

pub const RETENTION_DAYS: i64 = 30;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Longer texts are cut; the history is for finding things again, not an archive.
const MAX_TEXT_CHARS: usize = 20_000;
const DEFAULT_PAGE: u32 = 100;
const SETTINGS_KEY: &str = "history";
const COLUMNS: &str = "h.id, h.kind, h.query, h.result, h.provider, h.source_lang, \
                       h.target_lang, h.created_at, h.used_at, h.pinned";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryKind {
    Translation,
    Dictionary,
}

impl HistoryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Translation => "translation",
            Self::Dictionary => "dictionary",
        }
    }
}

/// Which kinds are recorded. Chats are kept separately (the last 20) and switched here too.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HistorySettings {
    pub translations: bool,
    pub dictionary: bool,
    pub chats: bool,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            translations: true,
            dictionary: true,
            chats: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewHistoryEntry {
    pub kind: HistoryKind,
    pub query: String,
    pub result: String,
    /// The service or chat provider id (`deepl`, `ollama`) or the dictionary names.
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub source_lang: String,
    #[serde(default)]
    pub target_lang: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: String,
    /// `translation` | `dictionary` (a string, so that newer kinds still load).
    pub kind: String,
    pub query: String,
    pub result: String,
    pub provider: String,
    pub source_lang: String,
    pub target_lang: String,
    /// Milliseconds since 1970.
    pub created_at: i64,
    pub used_at: i64,
    pub pinned: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HistoryQuery {
    pub kind: Option<HistoryKind>,
    /// Words to find in the query or the result; each matches as a prefix.
    pub search: Option<String>,
    /// Only entries used before this time (for the next page).
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

fn truncate(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// `слънч дом` → `"слънч"* "дом"*`: every word must match the beginning of a word.
pub(super) fn fts_query(search: &str) -> Option<String> {
    let terms: Vec<String> = search
        .split_whitespace()
        .map(|term| term.replace('"', ""))
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn history_row(row: &Row) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        kind: row.get(1)?,
        query: row.get(2)?,
        result: row.get(3)?,
        provider: row.get(4)?,
        source_lang: row.get(5)?,
        target_lang: row.get(6)?,
        created_at: row.get(7)?,
        used_at: row.get(8)?,
        pinned: row.get(9)?,
    })
}

impl Storage {
    pub fn history_settings(&self) -> Result<HistorySettings, String> {
        self.setting(SETTINGS_KEY)
    }

    pub fn set_history_settings(&self, settings: &HistorySettings) -> Result<(), String> {
        self.set_setting(SETTINGS_KEY, settings)
    }

    /// Records the entry unless its kind is switched off; returns what was stored.
    pub fn add_history(&self, entry: NewHistoryEntry) -> Result<Option<HistoryEntry>, String> {
        self.add_history_at(entry, now_ms())
    }

    pub(crate) fn add_history_at(
        &self,
        entry: NewHistoryEntry,
        now: i64,
    ) -> Result<Option<HistoryEntry>, String> {
        let settings = self.history_settings()?;
        let enabled = match entry.kind {
            HistoryKind::Translation => settings.translations,
            HistoryKind::Dictionary => settings.dictionary,
        };
        let query = truncate(&entry.query);
        if !enabled || query.is_empty() {
            return Ok(None);
        }
        let result = truncate(&entry.result);
        self.purge_if_due(now)?;
        let connection = self.connection();
        let existing: Option<String> = connection
            .query_row(
                "SELECT id FROM history
                 WHERE deleted_at IS NULL AND kind = ?1 AND query = ?2
                   AND provider = ?3 AND target_lang = ?4
                 ORDER BY used_at DESC LIMIT 1",
                (
                    entry.kind.as_str(),
                    &query,
                    &entry.provider,
                    &entry.target_lang,
                ),
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let id = match existing {
            Some(id) => {
                connection
                    .execute(
                        "UPDATE history SET result = ?2, source_lang = ?3, used_at = ?4,
                         updated_at = ?4 WHERE id = ?1",
                        (&id, &result, &entry.source_lang, now),
                    )
                    .map_err(db_error)?;
                id
            }
            None => {
                let id = new_id();
                connection
                    .execute(
                        "INSERT INTO history (id, kind, query, result, provider, source_lang,
                         target_lang, created_at, used_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?8)",
                        (
                            &id,
                            entry.kind.as_str(),
                            &query,
                            &result,
                            &entry.provider,
                            &entry.source_lang,
                            &entry.target_lang,
                            now,
                        ),
                    )
                    .map_err(db_error)?;
                id
            }
        };
        connection
            .query_row(
                &format!("SELECT {COLUMNS} FROM history h WHERE h.id = ?1"),
                [&id],
                history_row,
            )
            .map(Some)
            .map_err(db_error)
    }

    /// Newest first. `search` uses the full-text index; `before` pages through older entries.
    pub fn history(&self, query: &HistoryQuery) -> Result<Vec<HistoryEntry>, String> {
        let mut sql = format!("SELECT {COLUMNS} FROM history h");
        let mut conditions = vec!["h.deleted_at IS NULL".to_string()];
        let mut values: Vec<Value> = Vec::new();
        if let Some(search) = query.search.as_deref().and_then(fts_query) {
            sql.push_str(" JOIN history_fts f ON f.rowid = h.seq");
            conditions.push("history_fts MATCH ?".to_string());
            values.push(Value::Text(search));
        }
        if let Some(kind) = query.kind {
            conditions.push("h.kind = ?".to_string());
            values.push(Value::Text(kind.as_str().to_string()));
        }
        if let Some(before) = query.before {
            conditions.push("h.used_at < ?".to_string());
            values.push(Value::Integer(before));
        }
        sql.push_str(&format!(
            " WHERE {} ORDER BY h.used_at DESC LIMIT ?",
            conditions.join(" AND ")
        ));
        values.push(Value::Integer(i64::from(
            query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, 500),
        )));
        let connection = self.connection();
        let mut statement = connection.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map(params_from_iter(values), history_row)
            .map_err(db_error)?;
        rows.collect::<Result<_, _>>().map_err(db_error)
    }

    pub fn pin_history(&self, id: &str, pinned: bool) -> Result<(), String> {
        self.connection()
            .execute(
                "UPDATE history SET pinned = ?2, updated_at = ?3
                 WHERE id = ?1 AND deleted_at IS NULL",
                (id, pinned, now_ms()),
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Removes the text at once; the empty row stays as a tombstone for sync until purged.
    pub fn delete_history(&self, id: &str) -> Result<(), String> {
        self.connection()
            .execute(
                "UPDATE history SET query = '', result = '', deleted_at = ?2, updated_at = ?2
                 WHERE id = ?1 AND deleted_at IS NULL",
                (id, now_ms()),
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Deletes every entry (or every unpinned one); returns how many.
    pub fn clear_history(&self, include_pinned: bool) -> Result<usize, String> {
        self.connection()
            .execute(
                "UPDATE history SET query = '', result = '', deleted_at = ?1, updated_at = ?1
                 WHERE deleted_at IS NULL AND (pinned = 0 OR ?2)",
                (now_ms(), include_pinned),
            )
            .map_err(db_error)
    }

    /// Drops unpinned entries not used for 30 days and tombstones older than that.
    pub fn purge_expired(&self, now: i64) -> Result<(), String> {
        let cutoff = now - RETENTION_DAYS * DAY_MS;
        self.connection()
            .execute(
                "DELETE FROM history
                 WHERE (deleted_at IS NOT NULL AND deleted_at < ?1)
                    OR (deleted_at IS NULL AND pinned = 0 AND used_at < ?1)",
                [cutoff],
            )
            .map_err(db_error)?;
        self.mark_purged(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translation(query: &str, result: &str) -> NewHistoryEntry {
        NewHistoryEntry {
            kind: HistoryKind::Translation,
            query: query.to_string(),
            result: result.to_string(),
            provider: "deepl".to_string(),
            source_lang: "en".to_string(),
            target_lang: "bg".to_string(),
        }
    }

    fn lookup(word: &str) -> NewHistoryEntry {
        NewHistoryEntry {
            kind: HistoryKind::Dictionary,
            query: word.to_string(),
            result: format!("{word}: значение"),
            provider: "English-Bulgarian".to_string(),
            source_lang: String::new(),
            target_lang: String::new(),
        }
    }

    #[test]
    fn repeated_query_moves_to_the_top_instead_of_duplicating() {
        let storage = Storage::open_in_memory().unwrap();
        let first = storage
            .add_history_at(translation("apple", "ябълка"), 1_000)
            .unwrap()
            .unwrap();
        storage.add_history_at(lookup("pear"), 2_000).unwrap();
        let again = storage
            .add_history_at(translation("apple", "ябълка (плод)"), 3_000)
            .unwrap()
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(again.created_at, 1_000);
        assert_eq!(again.used_at, 3_000);
        let all = storage.history(&HistoryQuery::default()).unwrap();
        assert_eq!(
            all.iter()
                .map(|entry| entry.query.as_str())
                .collect::<Vec<_>>(),
            ["apple", "pear"]
        );
        assert_eq!(all[0].result, "ябълка (плод)");
    }

    #[test]
    fn switched_off_kinds_are_not_recorded() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .set_history_settings(&HistorySettings {
                dictionary: false,
                ..Default::default()
            })
            .unwrap();
        assert!(storage.add_history(lookup("pear")).unwrap().is_none());
        assert!(storage
            .add_history(translation("pear", "круша"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn search_filters_and_pages() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .add_history_at(translation("Good morning", "Добро утро"), 1_000)
            .unwrap();
        storage.add_history_at(lookup("слънце"), 2_000).unwrap();
        storage.add_history_at(lookup("Слънчоглед"), 3_000).unwrap();

        let search = |text: &str, kind: Option<HistoryKind>| {
            storage
                .history(&HistoryQuery {
                    search: Some(text.to_string()),
                    kind,
                    ..Default::default()
                })
                .unwrap()
                .into_iter()
                .map(|entry| entry.query)
                .collect::<Vec<_>>()
        };
        // Prefix match, case-insensitive, in Cyrillic; the result text is searched too.
        assert_eq!(search("слън", None), ["Слънчоглед", "слънце"]);
        assert_eq!(search("утро", None), ["Good morning"]);
        assert_eq!(search("слън утро", None), Vec::<String>::new());
        assert_eq!(search("\"", None).len(), 3);
        assert_eq!(
            search("good", Some(HistoryKind::Dictionary)),
            Vec::<String>::new()
        );

        let page = storage
            .history(&HistoryQuery {
                before: Some(3_000),
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page[0].query, "слънце");
    }

    #[test]
    fn delete_clear_and_purge() {
        let storage = Storage::open_in_memory().unwrap();
        let day = DAY_MS;
        let old = storage.add_history_at(lookup("old"), 0).unwrap().unwrap();
        let pinned = storage
            .add_history_at(lookup("pinned"), 0)
            .unwrap()
            .unwrap();
        let recent = storage
            .add_history_at(lookup("recent"), 25 * day)
            .unwrap()
            .unwrap();
        storage.pin_history(&pinned.id, true).unwrap();

        storage.purge_expired(31 * day).unwrap();
        let left: Vec<String> = storage
            .history(&HistoryQuery::default())
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(left, [recent.id.clone(), pinned.id.clone()]);
        assert!(!left.contains(&old.id));

        storage.delete_history(&recent.id).unwrap();
        assert!(storage
            .history(&HistoryQuery {
                search: Some("recent".into()),
                ..Default::default()
            })
            .unwrap()
            .is_empty());
        assert_eq!(storage.clear_history(false).unwrap(), 0);
        assert_eq!(storage.clear_history(true).unwrap(), 1);
        assert!(storage
            .history(&HistoryQuery::default())
            .unwrap()
            .is_empty());
    }
}
