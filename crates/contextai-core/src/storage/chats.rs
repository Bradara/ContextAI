//! Chats: the conversations started with "Обясни", "Обобщи", "Редактирай", "Граматика",
//! "Попитай" or a free question. The window saves the whole conversation after each answer;
//! the newest 20 are kept, pinned ones in addition.

use rusqlite::{OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::{db_error, new_id, now_ms, Storage};

/// Unpinned chats kept; older ones are deleted when a chat is saved.
pub const KEPT_CHATS: usize = 20;
const MAX_TEXT_CHARS: usize = 50_000;
const PREVIEW_CHARS: usize = 300;
const SUMMARY_COLUMNS: &str = "c.id, c.title, c.action, c.selection, c.provider, c.model, \
    c.created_at, c.updated_at, c.pinned,
    (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id),
    COALESCE((SELECT m.content FROM messages m WHERE m.conversation_id = c.id
              AND m.role = 'assistant' ORDER BY m.position DESC LIMIT 1), '')";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// `user` or `assistant`.
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConversationInput {
    /// Empty for a new chat; the saved chat's id afterwards.
    pub id: Option<String>,
    pub title: String,
    pub action: String,
    pub selection: String,
    pub provider: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub action: String,
    /// The start of the selected text.
    pub selection: String,
    pub provider: String,
    pub model: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub pinned: bool,
    pub message_count: u32,
    /// The start of the last answer.
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub action: String,
    pub selection: String,
    pub provider: String,
    pub model: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub pinned: bool,
    pub messages: Vec<ChatMessage>,
}

fn limited(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn one_line(text: &str, max: usize) -> String {
    limited(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn summary_row(row: &Row) -> rusqlite::Result<ConversationSummary> {
    let selection: String = row.get(3)?;
    let preview: String = row.get(10)?;
    Ok(ConversationSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        action: row.get(2)?,
        selection: one_line(&selection, PREVIEW_CHARS),
        provider: row.get(4)?,
        model: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        pinned: row.get(8)?,
        message_count: row.get(9)?,
        // Edit and grammar answers wrap the corrected text in <result> markers.
        preview: one_line(
            &preview.replace("<result>", "").replace("</result>", ""),
            PREVIEW_CHARS,
        ),
    })
}

impl Storage {
    /// Saves the whole conversation (replacing its messages) when chats are recorded; returns
    /// its summary, or None when chats are switched off or there is no answer yet.
    pub fn save_conversation(
        &self,
        input: &ConversationInput,
    ) -> Result<Option<ConversationSummary>, String> {
        self.save_conversation_at(input, now_ms())
    }

    pub(crate) fn save_conversation_at(
        &self,
        input: &ConversationInput,
        now: i64,
    ) -> Result<Option<ConversationSummary>, String> {
        let has_answer = input
            .messages
            .iter()
            .any(|message| message.role == "assistant" && !message.content.trim().is_empty());
        if !self.history_settings()?.chats || !has_answer {
            return Ok(None);
        }
        let mut connection = self.connection();
        let transaction = connection.transaction().map_err(db_error)?;
        let existing = match &input.id {
            Some(id) => transaction
                .query_row(
                    "SELECT id FROM conversations WHERE id = ?1 AND deleted_at IS NULL",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(db_error)?,
            None => None,
        };
        let title = one_line(&input.title, 200);
        let selection = limited(input.selection.trim(), MAX_TEXT_CHARS);
        let id = match existing {
            Some(id) => {
                transaction
                    .execute(
                        "UPDATE conversations SET title = ?2, provider = ?3, model = ?4,
                         updated_at = ?5 WHERE id = ?1",
                        (&id, &title, &input.provider, &input.model, now),
                    )
                    .map_err(db_error)?;
                transaction
                    .execute("DELETE FROM messages WHERE conversation_id = ?1", [&id])
                    .map_err(db_error)?;
                id
            }
            None => {
                let id = new_id();
                transaction
                    .execute(
                        "INSERT INTO conversations (id, title, action, selection, provider, model,
                         created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                        (
                            &id,
                            &title,
                            &input.action,
                            &selection,
                            &input.provider,
                            &input.model,
                            now,
                        ),
                    )
                    .map_err(db_error)?;
                id
            }
        };
        for (position, message) in input.messages.iter().enumerate() {
            let role = if message.role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            transaction
                .execute(
                    "INSERT INTO messages (id, conversation_id, position, role, content, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    (
                        new_id(),
                        &id,
                        position as i64,
                        role,
                        limited(&message.content, MAX_TEXT_CHARS),
                        now,
                    ),
                )
                .map_err(db_error)?;
        }
        // Keep the newest unpinned chats; older ones become tombstones without their text.
        transaction
            .execute(
                "UPDATE conversations SET title = '', selection = '', deleted_at = ?1, updated_at = ?1
                 WHERE deleted_at IS NULL AND pinned = 0 AND id NOT IN (
                     SELECT id FROM conversations WHERE deleted_at IS NULL AND pinned = 0
                     ORDER BY updated_at DESC, seq DESC LIMIT ?2)",
                (now, KEPT_CHATS as i64),
            )
            .map_err(db_error)?;
        transaction
            .execute(
                "DELETE FROM messages WHERE conversation_id IN
                 (SELECT id FROM conversations WHERE deleted_at IS NOT NULL)",
                [],
            )
            .map_err(db_error)?;
        let summary = transaction
            .query_row(
                &format!("SELECT {SUMMARY_COLUMNS} FROM conversations c WHERE c.id = ?1"),
                [&id],
                summary_row,
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(Some(summary))
    }

    /// The kept chats, newest first; `search` matches the title, the selection or any message.
    pub fn conversations(&self, search: Option<&str>) -> Result<Vec<ConversationSummary>, String> {
        let pattern = search
            .map(str::trim)
            .filter(|search| !search.is_empty())
            .map(|search| {
                let escaped = search
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                format!("%{escaped}%")
            });
        let connection = self.connection();
        let mut statement = connection
            .prepare(&format!(
                "SELECT {SUMMARY_COLUMNS} FROM conversations c
                 WHERE c.deleted_at IS NULL AND (?1 IS NULL
                    OR c.title LIKE ?1 ESCAPE '\\' OR c.selection LIKE ?1 ESCAPE '\\'
                    OR EXISTS (SELECT 1 FROM messages m WHERE m.conversation_id = c.id
                               AND m.content LIKE ?1 ESCAPE '\\'))
                 ORDER BY c.updated_at DESC, c.seq DESC"
            ))
            .map_err(db_error)?;
        let rows = statement
            .query_map([pattern], summary_row)
            .map_err(db_error)?;
        rows.collect::<Result<_, _>>().map_err(db_error)
    }

    pub fn conversation(&self, id: &str) -> Result<Conversation, String> {
        let connection = self.connection();
        let mut conversation = connection
            .query_row(
                "SELECT id, title, action, selection, provider, model, created_at, updated_at,
                 pinned FROM conversations WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                |row| {
                    Ok(Conversation {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        action: row.get(2)?,
                        selection: row.get(3)?,
                        provider: row.get(4)?,
                        model: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        pinned: row.get(8)?,
                        messages: Vec::new(),
                    })
                },
            )
            .optional()
            .map_err(db_error)?
            .ok_or("Чатът не е намерен.")?;
        let mut statement = connection
            .prepare(
                "SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY position",
            )
            .map_err(db_error)?;
        conversation.messages = statement
            .query_map([id], |row| {
                Ok(ChatMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                })
            })
            .map_err(db_error)?
            .collect::<Result<_, _>>()
            .map_err(db_error)?;
        Ok(conversation)
    }

    pub fn pin_conversation(&self, id: &str, pinned: bool) -> Result<(), String> {
        self.connection()
            .execute(
                "UPDATE conversations SET pinned = ?2, updated_at = updated_at
                 WHERE id = ?1 AND deleted_at IS NULL",
                (id, pinned),
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn delete_conversation(&self, id: &str) -> Result<(), String> {
        let connection = self.connection();
        connection
            .execute(
                "UPDATE conversations SET title = '', selection = '', deleted_at = ?2,
                 updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                (id, now_ms()),
            )
            .map_err(db_error)?;
        connection
            .execute("DELETE FROM messages WHERE conversation_id = ?1", [id])
            .map_err(db_error)?;
        Ok(())
    }

    /// Deletes every chat (or every unpinned one); returns how many.
    pub(crate) fn clear_conversations(&self, include_pinned: bool) -> Result<usize, String> {
        let connection = self.connection();
        let removed = connection
            .execute(
                "UPDATE conversations SET title = '', selection = '', deleted_at = ?1,
                 updated_at = ?1 WHERE deleted_at IS NULL AND (pinned = 0 OR ?2)",
                (now_ms(), include_pinned),
            )
            .map_err(db_error)?;
        connection
            .execute(
                "DELETE FROM messages WHERE conversation_id IN
                 (SELECT id FROM conversations WHERE deleted_at IS NOT NULL)",
                [],
            )
            .map_err(db_error)?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::HistorySettings;

    fn chat(id: Option<String>, title: &str, answers: &[&str]) -> ConversationInput {
        let mut messages = vec![ChatMessage {
            role: "user".into(),
            content: format!("Обясни: {title}"),
        }];
        for answer in answers {
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: (*answer).into(),
            });
        }
        ConversationInput {
            id,
            title: title.into(),
            action: "explain".into(),
            selection: format!("текст {title}"),
            provider: "ollama".into(),
            model: "qwen".into(),
            messages,
        }
    }

    #[test]
    fn saving_again_updates_the_same_chat() {
        let storage = Storage::open_in_memory().unwrap();
        assert!(storage
            .save_conversation(&chat(None, "без отговор", &[]))
            .unwrap()
            .is_none());
        let first = storage
            .save_conversation_at(&chat(None, "тема", &["<result>първи</result>"]), 1_000)
            .unwrap()
            .unwrap();
        assert_eq!(first.preview, "първи");
        let again = storage
            .save_conversation_at(
                &chat(Some(first.id.clone()), "тема", &["първи", "втори"]),
                2_000,
            )
            .unwrap()
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!((again.message_count, again.updated_at), (3, 2_000));
        let loaded = storage.conversation(&first.id).unwrap();
        assert_eq!(loaded.messages.last().unwrap().content, "втори");
        assert_eq!(storage.conversations(Some("втор")).unwrap().len(), 1);
        assert!(storage.conversations(Some("няма%")).unwrap().is_empty());
    }

    #[test]
    fn only_the_newest_twenty_unpinned_are_kept() {
        let storage = Storage::open_in_memory().unwrap();
        let pinned = storage
            .save_conversation_at(&chat(None, "закачен", &["a"]), 0)
            .unwrap()
            .unwrap();
        storage.pin_conversation(&pinned.id, true).unwrap();
        for index in 1..=25 {
            storage
                .save_conversation_at(&chat(None, &format!("чат {index}"), &["a"]), index)
                .unwrap();
        }
        let kept = storage.conversations(None).unwrap();
        assert_eq!(kept.len(), KEPT_CHATS + 1);
        assert_eq!(kept[0].title, "чат 25");
        assert!(kept.iter().any(|chat| chat.id == pinned.id));
        assert!(!kept.iter().any(|chat| chat.title == "чат 5"));
        let orphaned: i64 = storage
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.deleted_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphaned, 0);
    }

    #[test]
    fn switched_off_and_cleared() {
        let storage = Storage::open_in_memory().unwrap();
        let saved = storage
            .save_conversation(&chat(None, "тема", &["a"]))
            .unwrap()
            .unwrap();
        storage.delete_conversation(&saved.id).unwrap();
        assert!(storage.conversation(&saved.id).is_err());

        storage
            .save_conversation(&chat(None, "друга", &["a"]))
            .unwrap();
        assert_eq!(storage.clear_history(false).unwrap(), 1);
        assert!(storage.conversations(None).unwrap().is_empty());

        storage
            .set_history_settings(&HistorySettings {
                chats: false,
                ..Default::default()
            })
            .unwrap();
        assert!(storage
            .save_conversation(&chat(None, "тема", &["a"]))
            .unwrap()
            .is_none());
    }
}
