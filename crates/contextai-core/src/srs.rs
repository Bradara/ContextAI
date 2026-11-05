//! Spaced repetition: a simplified SM-2, as in Anki. A pure function of the review state and
//! the answer, so that it can be replaced (e.g. by FSRS) and replayed from the review log.

use serde::{Deserialize, Serialize};

pub const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// "Пак" shows the word again in the same session.
const AGAIN_MINUTES: f64 = 10.0;
const MIN_EASE: f64 = 1.3;
const MAX_INTERVAL_DAYS: f64 = 3650.0;

/// The four answers, in the order of their keys 1–4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    Again,
    Hard,
    Good,
    Easy,
}

impl Rating {
    pub const ALL: [Rating; 4] = [Rating::Again, Rating::Hard, Rating::Good, Rating::Easy];

    pub fn as_number(self) -> u8 {
        self as u8 + 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SrsState {
    /// Days until the next review (fractions for "again").
    pub interval_days: f64,
    pub ease: f64,
    /// Successful reviews in a row.
    pub reps: u32,
    /// Times a learned word was forgotten.
    pub lapses: u32,
}

impl Default for SrsState {
    fn default() -> Self {
        Self {
            interval_days: 0.0,
            ease: 2.5,
            reps: 0,
            lapses: 0,
        }
    }
}

/// Whole days from one day on; "again" stays in minutes.
fn days(value: f64) -> f64 {
    value.round().clamp(1.0, MAX_INTERVAL_DAYS)
}

/// The state after answering `rating`.
pub fn schedule(state: SrsState, rating: Rating) -> SrsState {
    let previous = state.interval_days.max(1.0);
    match rating {
        Rating::Again => SrsState {
            interval_days: AGAIN_MINUTES / (24.0 * 60.0),
            ease: (state.ease - 0.2).max(MIN_EASE),
            reps: 0,
            lapses: state.lapses + u32::from(state.reps > 0),
        },
        Rating::Hard => SrsState {
            interval_days: if state.reps == 0 {
                1.0
            } else {
                days((previous * 1.2).max(previous + 1.0))
            },
            ease: (state.ease - 0.15).max(MIN_EASE),
            reps: state.reps + 1,
            lapses: state.lapses,
        },
        Rating::Good => SrsState {
            interval_days: match state.reps {
                0 => 1.0,
                1 => 3.0,
                _ => days((previous * state.ease).max(previous + 1.0)),
            },
            reps: state.reps + 1,
            ..state
        },
        Rating::Easy => SrsState {
            interval_days: if state.reps == 0 {
                4.0
            } else {
                days((previous * state.ease * 1.3).max(previous + 2.0))
            },
            ease: state.ease + 0.15,
            reps: state.reps + 1,
            lapses: state.lapses,
        },
    }
}

/// When the word is due after `state`, answered at `now` (ms).
pub fn due_at(state: SrsState, now: i64) -> i64 {
    now + (state.interval_days * DAY_MS as f64).round() as i64
}

/// The interval each answer would give, in days, for the labels on the buttons.
pub fn preview(state: SrsState) -> [f64; 4] {
    Rating::ALL.map(|rating| schedule(state, rating).interval_days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_word_intervals() {
        let new = SrsState::default();
        let [again, hard, good, easy] = preview(new);
        assert!((again * 24.0 * 60.0 - 10.0).abs() < 1e-9);
        assert_eq!((hard, good, easy), (1.0, 1.0, 4.0));
    }

    #[test]
    fn good_answers_grow_the_interval() {
        let mut state = SrsState::default();
        let mut intervals = Vec::new();
        for _ in 0..4 {
            state = schedule(state, Rating::Good);
            intervals.push(state.interval_days);
        }
        assert_eq!(intervals, [1.0, 3.0, 8.0, 20.0]);
    }

    #[test]
    fn forgetting_a_learned_word_starts_over_with_lower_ease() {
        let learned = schedule(schedule(SrsState::default(), Rating::Good), Rating::Good);
        let forgotten = schedule(learned, Rating::Again);
        assert_eq!((forgotten.reps, forgotten.lapses), (0, 1));
        assert!((forgotten.ease - 2.3).abs() < 1e-9);
        assert_eq!(schedule(forgotten, Rating::Good).interval_days, 1.0);
        // A new word answered "again" is not a lapse.
        assert_eq!(schedule(SrsState::default(), Rating::Again).lapses, 0);
    }

    #[test]
    fn ease_has_a_floor() {
        let mut state = SrsState::default();
        for _ in 0..20 {
            state = schedule(state, Rating::Again);
        }
        assert_eq!(state.ease, MIN_EASE);
    }
}
