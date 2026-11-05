//! Words to learn. A card is one word (or short phrase) in one language with a copy of its
//! meaning, the sentences it was met in and tags. Each card has two review states, word →
//! meaning and meaning → word, scheduled independently (stage 4).

use rusqlite::{params_from_iter, types::Value, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::{db_error, history::fts_query, new_id, now_ms, Storage};
use crate::{
    dictionary::{html_to_text, normalize, sanitize_html},
    language::term_languages,
};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
const MAX_TERM_CHARS: usize = 200;
const MAX_TEXT_CHARS: usize = 20_000;
const DEFAULT_PAGE: u32 = 200;
const COLUMNS: &str = "c.id, c.term, c.language, c.meaning_lang, c.meaning_html, \
                       c.transcription, c.context, c.source, c.tags, c.created_at, c.updated_at";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NewCard {
    pub term: String,
    /// The pair of the dictionary or translation; the card's own languages follow the word's
    /// script (a Bulgarian word from an English–Bulgarian dictionary is a Bulgarian card).
    pub source_lang: String,
    pub target_lang: String,
    /// HTML (a dictionary article) or plain text (a translation, `meaning_is_text`).
    pub meaning: String,
    pub meaning_is_text: bool,
    pub transcription: String,
    /// The sentence the word was met in, if known.
    pub context: String,
    /// The dictionary or the translation service.
    pub source: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CardEdit {
    pub term: String,
    pub transcription: String,
    /// `None` keeps the meaning; edited meanings come as plain text.
    pub meaning_text: Option<String>,
    pub context: String,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub id: String,
    pub term: String,
    pub language: String,
    pub meaning_lang: String,
    /// Sanitized HTML.
    pub meaning_html: String,
    pub transcription: String,
    /// One sentence per line.
    pub context: String,
    pub source: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedCard {
    pub card: Card,
    /// False when the word was already saved (its card is returned, with the new context).
    pub created: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CardQuery {
    pub search: Option<String>,
    /// Only cards created before this time (for the next page).
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

fn limited(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn text_to_html(text: &str) -> String {
    escape_html(text.trim())
        .replace("\r\n", "\n")
        .replace('\n', "<br>")
}

/// Adds the new sentences that are not there yet, one per line.
fn merge_context(existing: &str, added: &str) -> String {
    let mut lines: Vec<&str> = existing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    for line in added.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !lines.iter().any(|known| known.trim() == line) {
            lines.push(line);
        }
    }
    limited(&lines.join("\n"), MAX_TEXT_CHARS)
}

fn clean_tags(tags: &[String]) -> Vec<String> {
    let mut clean: Vec<String> = Vec::new();
    for tag in tags {
        let tag = limited(&tag.replace([',', '\n'], " "), 40);
        if !tag.is_empty()
            && !clean
                .iter()
                .any(|known| known.to_lowercase() == tag.to_lowercase())
        {
            clean.push(tag);
        }
    }
    clean
}

fn card_row(row: &Row) -> rusqlite::Result<Card> {
    let tags: String = row.get(8)?;
    Ok(Card {
        id: row.get(0)?,
        term: row.get(1)?,
        language: row.get(2)?,
        meaning_lang: row.get(3)?,
        meaning_html: row.get(4)?,
        transcription: row.get(5)?,
        context: row.get(6)?,
        source: row.get(7)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

impl Storage {
    pub(super) fn card_by_id(connection: &rusqlite::Connection, id: &str) -> Result<Card, String> {
        connection
            .query_row(
                &format!("SELECT {COLUMNS} FROM cards c WHERE c.id = ?1"),
                [id],
                card_row,
            )
            .map_err(db_error)
    }

    pub fn card(&self, id: &str) -> Result<Card, String> {
        Self::card_by_id(&self.connection(), id)
    }

    /// Saves a word, or adds the new context to its card when the word is already saved.
    pub fn save_card(&self, card: NewCard) -> Result<SavedCard, String> {
        self.save_card_at(card, now_ms())
    }

    pub(crate) fn save_card_at(&self, card: NewCard, now: i64) -> Result<SavedCard, String> {
        let term = limited(
            &card.term.split_whitespace().collect::<Vec<_>>().join(" "),
            MAX_TERM_CHARS,
        );
        let term_key = normalize(&term);
        if term_key.is_empty() {
            return Err("Няма дума за запазване.".to_string());
        }
        let (language, meaning_lang) = term_languages(&term, &card.source_lang, &card.target_lang);
        let meaning_html = sanitize_html(&if card.meaning_is_text {
            text_to_html(&card.meaning)
        } else {
            card.meaning.clone()
        });
        let meaning_html = limited(&meaning_html, MAX_TEXT_CHARS);
        let meaning_text = html_to_text(&meaning_html);

        let mut connection = self.connection();
        let transaction = connection.transaction().map_err(db_error)?;
        let existing: Option<(String, String)> = transaction
            .query_row(
                "SELECT id, context FROM cards
                 WHERE term_key = ?1 AND language = ?2 AND deleted_at IS NULL",
                (&term_key, &language),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db_error)?;
        let (id, created) = match existing {
            Some((id, context)) => {
                let merged = merge_context(&context, &card.context);
                if merged != context {
                    transaction
                        .execute(
                            "UPDATE cards SET context = ?2, updated_at = ?3 WHERE id = ?1",
                            (&id, &merged, now),
                        )
                        .map_err(db_error)?;
                }
                (id, false)
            }
            None => {
                let id = new_id();
                transaction
                    .execute(
                        "INSERT INTO cards (id, term, term_key, language, meaning_lang, meaning_html,
                         meaning_text, transcription, context, source, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                        rusqlite::params![
                            &id,
                            &term,
                            &term_key,
                            &language,
                            &meaning_lang,
                            &meaning_html,
                            &meaning_text,
                            limited(&card.transcription, MAX_TERM_CHARS),
                            merge_context("", &card.context),
                            limited(&card.source, MAX_TERM_CHARS),
                            now,
                        ],
                    )
                    .map_err(db_error)?;
                // New words: word → meaning today, meaning → word a day later, so that the two
                // directions of one word do not come one after the other.
                for (direction, due) in [("forward", now), ("reverse", now + DAY_MS)] {
                    transaction
                        .execute(
                            "INSERT INTO card_states (card_id, direction, due_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4)",
                            (&id, direction, due, now),
                        )
                        .map_err(db_error)?;
                }
                (id, true)
            }
        };
        let card = Self::card_by_id(&transaction, &id)?;
        transaction.commit().map_err(db_error)?;
        Ok(SavedCard { card, created })
    }

    /// Newest first; `search` looks in the word, meaning, context and tags.
    pub fn cards(&self, query: &CardQuery) -> Result<Vec<Card>, String> {
        let mut sql = format!("SELECT {COLUMNS} FROM cards c");
        let mut conditions = vec!["c.deleted_at IS NULL".to_string()];
        let mut values: Vec<Value> = Vec::new();
        if let Some(search) = query.search.as_deref().and_then(fts_query) {
            sql.push_str(" JOIN cards_fts f ON f.rowid = c.seq");
            conditions.push("cards_fts MATCH ?".to_string());
            values.push(Value::Text(search));
        }
        if let Some(before) = query.before {
            conditions.push("c.created_at < ?".to_string());
            values.push(Value::Integer(before));
        }
        sql.push_str(&format!(
            " WHERE {} ORDER BY c.created_at DESC LIMIT ?",
            conditions.join(" AND ")
        ));
        values.push(Value::Integer(i64::from(
            query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, 1000),
        )));
        let connection = self.connection();
        let mut statement = connection.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map(params_from_iter(values), card_row)
            .map_err(db_error)?;
        rows.collect::<Result<_, _>>().map_err(db_error)
    }

    pub fn card_count(&self) -> Result<u32, String> {
        self.connection()
            .query_row(
                "SELECT COUNT(*) FROM cards WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(db_error)
    }

    /// For each word, whether it is saved (in any language).
    pub fn saved_terms(&self, terms: &[String]) -> Result<Vec<bool>, String> {
        let connection = self.connection();
        let mut statement = connection
            .prepare("SELECT 1 FROM cards WHERE term_key = ?1 AND deleted_at IS NULL LIMIT 1")
            .map_err(db_error)?;
        terms
            .iter()
            .map(|term| statement.exists([normalize(term)]).map_err(db_error))
            .collect()
    }

    pub fn update_card(&self, id: &str, edit: &CardEdit) -> Result<Card, String> {
        let term = limited(
            &edit.term.split_whitespace().collect::<Vec<_>>().join(" "),
            MAX_TERM_CHARS,
        );
        let term_key = normalize(&term);
        if term_key.is_empty() {
            return Err("Думата не може да е празна.".to_string());
        }
        let tags =
            serde_json::to_string(&clean_tags(&edit.tags)).map_err(|error| error.to_string())?;
        let connection = self.connection();
        let changed = connection
            .execute(
                "UPDATE cards SET term = ?2, term_key = ?3, transcription = ?4, context = ?5,
                 tags = ?6, updated_at = ?7 WHERE id = ?1 AND deleted_at IS NULL",
                rusqlite::params![
                    id,
                    &term,
                    &term_key,
                    limited(&edit.transcription, MAX_TERM_CHARS),
                    merge_context("", &edit.context),
                    &tags,
                    now_ms(),
                ],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(failure, _)
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    "Тази дума вече е запазена.".to_string()
                }
                other => db_error(other),
            })?;
        if changed == 0 {
            return Err("Думата не е намерена.".to_string());
        }
        if let Some(text) = &edit.meaning_text {
            let html = sanitize_html(&text_to_html(&limited(text, MAX_TEXT_CHARS)));
            connection
                .execute(
                    "UPDATE cards SET meaning_html = ?2, meaning_text = ?3 WHERE id = ?1",
                    (id, &html, html_to_text(&html)),
                )
                .map_err(db_error)?;
        }
        Self::card_by_id(&connection, id)
    }

    /// Removes the word's text and review states; the row stays as a tombstone for sync.
    pub fn delete_card(&self, id: &str) -> Result<(), String> {
        let connection = self.connection();
        connection
            .execute(
                "UPDATE cards SET term = '', term_key = '', meaning_html = '', meaning_text = '',
                 transcription = '', context = '', tags = '[]', deleted_at = ?2, updated_at = ?2
                 WHERE id = ?1 AND deleted_at IS NULL",
                (id, now_ms()),
            )
            .map_err(db_error)?;
        connection
            .execute("DELETE FROM card_states WHERE card_id = ?1", [id])
            .map_err(db_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_dictionary(term: &str, meaning: &str, context: &str) -> NewCard {
        NewCard {
            term: term.to_string(),
            source_lang: "en".to_string(),
            target_lang: "bg".to_string(),
            meaning: meaning.to_string(),
            context: context.to_string(),
            source: "English-Bulgarian".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn saving_twice_keeps_one_card_and_adds_the_context() {
        let storage = Storage::open_in_memory().unwrap();
        let first = storage
            .save_card_at(
                from_dictionary("Take", "<b>вземам</b><script>x</script>", "Take it."),
                1_000,
            )
            .unwrap();
        assert!(first.created);
        assert_eq!(first.card.language, "en");
        assert_eq!(first.card.meaning_lang, "bg");
        assert_eq!(first.card.meaning_html, "<b>вземам</b>");

        let again = storage
            .save_card_at(
                from_dictionary("take", "друго", "Take care.\nTake it."),
                2_000,
            )
            .unwrap();
        assert!(!again.created);
        assert_eq!(again.card.id, first.card.id);
        assert_eq!(again.card.context, "Take it.\nTake care.");
        assert_eq!(again.card.meaning_html, "<b>вземам</b>");
        assert_eq!(
            storage
                .saved_terms(&["TAKE".into(), "give".into()])
                .unwrap(),
            [true, false]
        );

        let states: i64 = storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM card_states WHERE card_id = ?1",
                [&first.card.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(states, 2);
    }

    #[test]
    fn a_word_in_the_meanings_language_becomes_its_own_card() {
        let storage = Storage::open_in_memory().unwrap();
        let card = storage
            .save_card(from_dictionary("ябълка", "apple", ""))
            .unwrap()
            .card;
        assert_eq!(
            (card.language.as_str(), card.meaning_lang.as_str()),
            ("bg", "en")
        );
    }

    #[test]
    fn translations_are_saved_as_text() {
        let storage = Storage::open_in_memory().unwrap();
        let card = storage
            .save_card(NewCard {
                term: "rock & roll".into(),
                source_lang: "en".into(),
                target_lang: "bg".into(),
                meaning: "рокендрол\n<музика>".into(),
                meaning_is_text: true,
                source: "deepl".into(),
                ..Default::default()
            })
            .unwrap()
            .card;
        assert_eq!(card.meaning_html, "рокендрол<br>&lt;музика&gt;");
    }

    #[test]
    fn edit_search_and_delete() {
        let storage = Storage::open_in_memory().unwrap();
        let take = storage
            .save_card_at(from_dictionary("take", "вземам", ""), 1_000)
            .unwrap()
            .card;
        storage
            .save_card_at(from_dictionary("give", "давам", ""), 2_000)
            .unwrap();

        let edited = storage
            .update_card(
                &take.id,
                &CardEdit {
                    term: "take off".into(),
                    transcription: "teɪk".into(),
                    meaning_text: Some("излитам\nсъбличам".into()),
                    context: "The plane took off.".into(),
                    tags: vec![
                        "глаголи".into(),
                        "Глаголи".into(),
                        " ".into(),
                        "travel".into(),
                    ],
                },
            )
            .unwrap();
        assert_eq!(edited.term, "take off");
        assert_eq!(edited.meaning_html, "излитам<br>събличам");
        assert_eq!(edited.tags, ["глаголи", "travel"]);

        let search = |text: &str| {
            storage
                .cards(&CardQuery {
                    search: Some(text.into()),
                    ..Default::default()
                })
                .unwrap()
                .into_iter()
                .map(|card| card.term)
                .collect::<Vec<_>>()
        };
        assert_eq!(search("съблич"), ["take off"]);
        assert_eq!(search("глагол"), ["take off"]);
        assert_eq!(search("plane"), ["take off"]);
        assert_eq!(search("давам"), ["give"]);
        assert_eq!(
            storage.update_card(
                &take.id,
                &CardEdit {
                    term: "give".into(),
                    ..Default::default()
                }
            ),
            Err("Тази дума вече е запазена.".to_string())
        );

        storage.delete_card(&take.id).unwrap();
        assert_eq!(storage.card_count().unwrap(), 1);
        assert!(search("съблич").is_empty());
        // The word can be saved again after it was deleted.
        assert!(
            storage
                .save_card(from_dictionary("take off", "", ""))
                .unwrap()
                .created
        );
    }
}
