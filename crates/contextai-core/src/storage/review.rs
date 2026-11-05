//! The review queue: which word to practise next, the answers and today's counts.
//!
//! "Today" is the local calendar day. Words learned with an interval of a day or more are due
//! for the whole day they fall on; words answered "again" come back after their minutes. New
//! words (never reviewed, either direction) are limited per day, both directions together.

use chrono::{Local, TimeZone};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use super::{cards::Card, db_error, new_id, now_ms, Storage};
use crate::srs::{self, Rating, SrsState, DAY_MS};

const SETTINGS_KEY: &str = "review";
const MAX_DAILY_NEW: u32 = 500;

const DUE: &str = "s.first_reviewed_at IS NOT NULL
    AND (s.due_at <= :now OR (s.interval_days >= 1 AND s.due_at < :day_end))";
const NEW: &str = "s.first_reviewed_at IS NULL AND s.due_at < :day_end";
const FROM: &str = "FROM card_states s JOIN cards c ON c.id = s.card_id AND c.deleted_at IS NULL";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReviewSettings {
    /// New words per day, both directions together.
    pub daily_new_cards: u32,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            daily_new_cards: 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Word → meaning.
    Forward,
    /// Meaning → word.
    Reverse,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Reverse => "reverse",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "reverse" {
            Self::Reverse
        } else {
            Self::Forward
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewCounts {
    /// New words still allowed today (within the daily limit).
    pub new: u32,
    /// Learned words due today.
    pub due: u32,
    /// When a word answered "again" comes back, if that is later today.
    pub next_due_at: Option<i64>,
    pub new_today: u32,
    pub daily_new_cards: u32,
}

impl ReviewCounts {
    pub fn total(&self) -> u32 {
        self.new + self.due
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItem {
    pub card: Card,
    pub direction: Direction,
    pub is_new: bool,
    /// Days until the next review for each answer (again, hard, good, easy).
    pub intervals: [f64; 4],
}

/// The `:now` and `:day_end` parameters that `sql` uses (SQLite refuses unused names).
fn day_params<'a>(
    sql: &str,
    now: &'a i64,
    day_end: &'a i64,
) -> Vec<(&'static str, &'a dyn rusqlite::ToSql)> {
    let all: [(&'static str, &'a dyn rusqlite::ToSql); 2] = [(":now", now), (":day_end", day_end)];
    all.into_iter()
        .filter(|(name, _)| sql.contains(name))
        .collect()
}

/// Midnight of the local day that contains `now`.
pub(crate) fn local_day_start(now: i64) -> i64 {
    Local
        .timestamp_millis_opt(now)
        .single()
        .and_then(|time| time.date_naive().and_hms_opt(0, 0, 0))
        .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
        .map_or(now - now.rem_euclid(DAY_MS), |midnight| {
            midnight.timestamp_millis()
        })
}

impl Storage {
    pub fn review_settings(&self) -> Result<ReviewSettings, String> {
        self.setting(SETTINGS_KEY)
    }

    pub fn set_review_settings(&self, settings: &ReviewSettings) -> Result<(), String> {
        self.set_setting(
            SETTINGS_KEY,
            &ReviewSettings {
                daily_new_cards: settings.daily_new_cards.min(MAX_DAILY_NEW),
            },
        )
    }

    pub fn review_counts(&self) -> Result<ReviewCounts, String> {
        self.review_counts_at(now_ms())
    }

    pub(crate) fn review_counts_at(&self, now: i64) -> Result<ReviewCounts, String> {
        let settings = self.review_settings()?;
        let day_start = local_day_start(now);
        let day_end = day_start + DAY_MS;
        let connection = self.connection();
        let count = |condition: &str| -> Result<u32, String> {
            let sql = format!("SELECT COUNT(*) {FROM} WHERE {condition}");
            connection
                .query_row(&sql, &*day_params(&sql, &now, &day_end), |row| row.get(0))
                .map_err(db_error)
        };
        let due = count(DUE)?;
        let waiting = count(NEW)?;
        let new_today: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM card_states WHERE first_reviewed_at >= ?1",
                [day_start],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let next_due_at: Option<i64> = connection
            .query_row(
                &format!(
                    "SELECT MIN(s.due_at) {FROM} WHERE s.first_reviewed_at IS NOT NULL
                     AND s.interval_days < 1 AND s.due_at > ?1 AND s.due_at < ?2"
                ),
                [now, day_end],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        Ok(ReviewCounts {
            new: waiting.min(settings.daily_new_cards.saturating_sub(new_today)),
            due,
            next_due_at,
            new_today,
            daily_new_cards: settings.daily_new_cards,
        })
    }

    /// The next word to practise: learned words that are due first, then new ones.
    pub fn next_review(&self) -> Result<Option<ReviewItem>, String> {
        self.next_review_at(now_ms())
    }

    pub(crate) fn next_review_at(&self, now: i64) -> Result<Option<ReviewItem>, String> {
        let counts = self.review_counts_at(now)?;
        let day_end = local_day_start(now) + DAY_MS;
        let connection = self.connection();
        let pick = |condition: &str, order: &str| {
            let sql = format!(
                "SELECT s.card_id, s.direction, s.interval_days, s.ease, s.reps, s.lapses
                 {FROM} WHERE {condition} ORDER BY {order} LIMIT 1"
            );
            connection
                .query_row(&sql, &*day_params(&sql, &now, &day_end), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        SrsState {
                            interval_days: row.get(2)?,
                            ease: row.get(3)?,
                            reps: row.get(4)?,
                            lapses: row.get(5)?,
                        },
                    ))
                })
                .optional()
                .map_err(db_error)
        };
        let mut is_new = false;
        let mut found = pick(DUE, "s.due_at")?;
        if found.is_none() && counts.new > 0 {
            found = pick(NEW, "c.created_at, c.seq, s.direction")?;
            is_new = true;
        }
        let Some((card_id, direction, state)) = found else {
            return Ok(None);
        };
        Ok(Some(ReviewItem {
            card: Self::card_by_id(&connection, &card_id)?,
            direction: Direction::parse(&direction),
            is_new,
            intervals: srs::preview(state),
        }))
    }

    /// Records the answer and schedules the word; returns the new counts.
    pub fn answer_review(
        &self,
        card_id: &str,
        direction: Direction,
        rating: Rating,
    ) -> Result<ReviewCounts, String> {
        self.answer_review_at(card_id, direction, rating, now_ms())
    }

    pub(crate) fn answer_review_at(
        &self,
        card_id: &str,
        direction: Direction,
        rating: Rating,
        now: i64,
    ) -> Result<ReviewCounts, String> {
        {
            let mut connection = self.connection();
            let transaction = connection.transaction().map_err(db_error)?;
            let (state, last_reviewed_at): (SrsState, Option<i64>) = transaction
                .query_row(
                    "SELECT interval_days, ease, reps, lapses, last_reviewed_at FROM card_states
                     WHERE card_id = ?1 AND direction = ?2",
                    (card_id, direction.as_str()),
                    |row| {
                        Ok((
                            SrsState {
                                interval_days: row.get(0)?,
                                ease: row.get(1)?,
                                reps: row.get(2)?,
                                lapses: row.get(3)?,
                            },
                            row.get(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(db_error)?
                .ok_or("Думата вече не съществува.")?;
            let next = srs::schedule(state, rating);
            transaction
                .execute(
                    "UPDATE card_states SET interval_days = ?3, ease = ?4, reps = ?5, lapses = ?6,
                     due_at = ?7, first_reviewed_at = COALESCE(first_reviewed_at, ?8),
                     last_reviewed_at = ?8, updated_at = ?8
                     WHERE card_id = ?1 AND direction = ?2",
                    rusqlite::params![
                        card_id,
                        direction.as_str(),
                        next.interval_days,
                        next.ease,
                        next.reps,
                        next.lapses,
                        srs::due_at(next, now),
                        now,
                    ],
                )
                .map_err(db_error)?;
            let elapsed = last_reviewed_at.map_or(0.0, |last| (now - last) as f64 / DAY_MS as f64);
            transaction
                .execute(
                    "INSERT INTO reviews (id, card_id, direction, rated_at, rating, interval_days,
                     elapsed_days) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        new_id(),
                        card_id,
                        direction.as_str(),
                        now,
                        rating.as_number(),
                        next.interval_days,
                        elapsed,
                    ],
                )
                .map_err(db_error)?;
            transaction.commit().map_err(db_error)?;
        }
        self.review_counts_at(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::NewCard;

    const HOUR: i64 = 60 * 60 * 1000;

    fn save(storage: &Storage, term: &str, now: i64) -> String {
        storage
            .save_card_at(
                NewCard {
                    term: term.into(),
                    source_lang: "en".into(),
                    target_lang: "bg".into(),
                    meaning: "значение".into(),
                    meaning_is_text: true,
                    ..Default::default()
                },
                now,
            )
            .unwrap()
            .card
            .id
    }

    /// Noon of some day, so that "later today" and "tomorrow" do not cross midnight by accident.
    fn noon() -> i64 {
        local_day_start(1_750_000_000_000) + 12 * HOUR
    }

    #[test]
    fn new_words_are_limited_per_day_and_reverse_comes_tomorrow() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .set_review_settings(&ReviewSettings { daily_new_cards: 2 })
            .unwrap();
        let now = noon();
        for term in ["one", "two", "three"] {
            save(&storage, term, now);
        }
        // Only the forward direction is due today; three waiting, two allowed.
        let counts = storage.review_counts_at(now).unwrap();
        assert_eq!((counts.new, counts.due), (2, 0));

        let first = storage.next_review_at(now).unwrap().unwrap();
        assert_eq!(
            (first.card.term.as_str(), first.direction),
            ("one", Direction::Forward)
        );
        assert!(first.is_new);
        let counts = storage
            .answer_review_at(&first.card.id, first.direction, Rating::Good, now)
            .unwrap();
        assert_eq!((counts.new, counts.new_today), (1, 1));

        let second = storage.next_review_at(now).unwrap().unwrap();
        assert_eq!(second.card.term, "two");
        storage
            .answer_review_at(&second.card.id, second.direction, Rating::Good, now)
            .unwrap();
        assert!(storage.next_review_at(now).unwrap().is_none());

        // Tomorrow: "one" and "two" are due again, the reverse directions are new, limit resets.
        let tomorrow = now + DAY_MS;
        let counts = storage.review_counts_at(tomorrow).unwrap();
        assert_eq!((counts.due, counts.new), (2, 2));
        let next = storage.next_review_at(tomorrow).unwrap().unwrap();
        assert!(!next.is_new);
    }

    #[test]
    fn again_comes_back_after_ten_minutes() {
        let storage = Storage::open_in_memory().unwrap();
        let now = noon();
        let id = save(&storage, "word", now);
        let item = storage.next_review_at(now).unwrap().unwrap();
        assert_eq!(item.intervals[2], 1.0);
        let counts = storage
            .answer_review_at(&id, Direction::Forward, Rating::Again, now)
            .unwrap();
        assert_eq!(counts.due, 0);
        assert_eq!(counts.next_due_at, Some(now + 10 * 60 * 1000));
        assert!(storage
            .next_review_at(now + 5 * 60 * 1000)
            .unwrap()
            .is_none());
        let back = storage
            .next_review_at(now + 11 * 60 * 1000)
            .unwrap()
            .unwrap();
        assert_eq!(back.card.id, id);

        let reviews: i64 = storage
            .connection()
            .query_row("SELECT COUNT(*) FROM reviews", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reviews, 1);
    }

    #[test]
    fn deleted_words_leave_the_queue() {
        let storage = Storage::open_in_memory().unwrap();
        let now = noon();
        let id = save(&storage, "gone", now);
        storage.delete_card(&id).unwrap();
        assert_eq!(storage.review_counts_at(now).unwrap().total(), 0);
        assert!(storage
            .answer_review_at(&id, Direction::Forward, Rating::Good, now)
            .is_err());
    }
}
