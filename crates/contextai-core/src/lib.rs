//! Platform-independent logic of ContextAI: dictionaries, the local database (history, words to
//! learn) and spaced repetition.
//! same code serves the desktop app now and other platforms (macOS, Linux, mobile) later.

pub mod dictionary;
pub mod language;
pub mod srs;
pub mod storage;
