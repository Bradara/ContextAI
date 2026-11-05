//! Local dictionaries: Babylon (.bgl) and StarDict (.ifo), an on-disk index per dictionary and
//! lookups across the enabled ones in the configured order.
//!
//! The first lookup (or adding the file) converts a dictionary into a cache file with the
//! articles as UTF-8 and a sorted key index (`index.rs`); later only the index is kept in memory
//! and the articles are read from the cache file on demand.
//!
//! Nothing here depends on Tauri: `Dictionaries` gets its two paths and a change callback.

mod bgl;
mod index;
mod stardict;

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

pub use bgl::normalize;
use bgl::parse_bgl;
use index::{cache_file, source_stamp, valid_header, write_cache, CacheHeader, DictionaryIndex};

/// Entries shown per dictionary for one word.
const MAX_ENTRIES_PER_DICTIONARY: usize = 30;
const MAX_SUGGESTIONS: usize = 30;

// ---------------------------------------------------------------------------------------------
// Settings and formats
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryConfig {
    pub path: String,
    pub enabled: bool,
    /// The user's choice of languages (ISO 639-1); empty = as the dictionary says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_lang: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_lang: String,
}

impl DictionaryConfig {
    /// The configured languages, or the ones the dictionary declares (or that were detected).
    fn languages(&self, detected_source: &str, detected_target: &str) -> (String, String) {
        let pick = |chosen: &str, detected: &str| {
            if chosen.is_empty() { detected } else { chosen }.to_string()
        };
        (
            pick(&self.source_lang, detected_source),
            pick(&self.target_lang, detected_target),
        )
    }
}

/// `dictionaries.json`; the order of the list is the order of the results.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DictionarySettings {
    pub dictionaries: Vec<DictionaryConfig>,
}

/// Paths are compared case-insensitively, as Windows does.
fn same_path(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Format {
    Babylon,
    StarDict,
}

fn format_of(path: &Path) -> Option<Format> {
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("bgl") {
        Some(Format::Babylon)
    } else if extension.eq_ignore_ascii_case("ifo") {
        Some(Format::StarDict)
    } else {
        None
    }
}

pub fn is_supported_file(path: &Path) -> bool {
    format_of(path).is_some()
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Article {
    pub(crate) headword: String,
    pub(crate) alternates: Vec<String>,
    pub(crate) part_of_speech: String,
    pub(crate) transcription: String,
    /// HTML as written by the dictionary; sanitized when shown.
    pub(crate) html: String,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedDictionary {
    pub(crate) title: String,
    /// ISO 639-1 codes; empty when unknown.
    pub(crate) source_lang: String,
    pub(crate) target_lang: String,
    pub(crate) articles: Vec<Article>,
}

fn read_dictionary(path: &Path) -> Result<ParsedDictionary, String> {
    match format_of(path) {
        Some(Format::Babylon) => {
            let contents =
                fs::read(path).map_err(|error| format!("Неуспешно прочитане: {error}"))?;
            parse_bgl(&contents)
        }
        Some(Format::StarDict) => stardict::read(path),
        None => Err("Поддържат се речници Babylon (.bgl) и StarDict (.ifo).".to_string()),
    }
}

/// The files a dictionary is read from; a change in any of them rebuilds the index.
fn component_files(path: &Path) -> Vec<PathBuf> {
    match format_of(path) {
        Some(Format::StarDict) => stardict::component_files(path),
        _ => vec![path.to_path_buf()],
    }
}

/// Every supported dictionary under `directory`, up to 8 folders deep, in name order.
pub fn find_dictionaries(directory: &Path) -> Vec<PathBuf> {
    fn walk(directory: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        let mut entries: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if depth < 8 {
                    walk(&path, depth + 1, found);
                }
            } else if is_supported_file(&path) {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(directory, 0, &mut found);
    found
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryInfo {
    pub path: String,
    pub enabled: bool,
    pub name: String,
    pub articles: u32,
    /// ready | indexing | error | missing
    pub status: &'static str,
    pub error: Option<String>,
    /// Languages in use (the user's choice or the detected ones) and the detected ones alone.
    pub source_lang: String,
    pub target_lang: String,
    pub detected_source_lang: String,
    pub detected_target_lang: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntry {
    pub headword: String,
    pub part_of_speech: String,
    pub transcription: String,
    /// Sanitized HTML: no scripts, styles, images or external links.
    pub html: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryGroup {
    pub path: String,
    pub name: String,
    pub source_lang: String,
    pub target_lang: String,
    pub entries: Vec<DictionaryEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupResult {
    pub query: String,
    /// The form of the query that matched (e.g. without trailing punctuation).
    pub matched: String,
    pub groups: Vec<DictionaryGroup>,
    pub suggestions: Vec<String>,
    pub errors: Vec<String>,
    /// Number of enabled dictionaries; 0 means there is nothing to search in.
    pub enabled: usize,
}

/// Dictionary HTML made safe to show: no scripts, styles, images or external links.
pub(crate) fn sanitize_html(html: &str) -> String {
    index::sanitize_definition(html)
}

/// The text of an article on one line, for search and summaries.
pub(crate) fn html_to_text(html: &str) -> String {
    let html = html
        .replace("<br>", " · ")
        .replace("</p>", " · ")
        .replace("</div>", " · ");
    bgl::plain_text(&html).trim_matches([' ', '·']).to_string()
}

/// Characters of the first article kept in the history.
const HISTORY_SUMMARY_CHARS: usize = 300;

impl LookupResult {
    /// What the history keeps of a lookup: the first headword, the start of its article and
    /// the dictionaries that had it. None when nothing was found.
    pub fn history_entry(&self) -> Option<crate::storage::NewHistoryEntry> {
        let first = self.groups.first()?.entries.first()?;
        let text = html_to_text(&first.html);
        let text = text.as_str();
        let summary: String = match text.char_indices().nth(HISTORY_SUMMARY_CHARS) {
            Some((end, _)) => format!("{}…", &text[..end]),
            None => text.to_string(),
        };
        let names: Vec<&str> = self
            .groups
            .iter()
            .map(|group| group.name.as_str())
            .collect();
        let group = &self.groups[0];
        let (source_lang, target_lang) = crate::language::term_languages(
            &first.headword,
            &group.source_lang,
            &group.target_lang,
        );
        Some(crate::storage::NewHistoryEntry {
            kind: crate::storage::HistoryKind::Dictionary,
            query: first.headword.clone(),
            result: summary,
            provider: names.join(", "),
            source_lang,
            target_lang,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Index state
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct DictionaryState {
    indexes: Mutex<HashMap<PathBuf, Arc<DictionaryIndex>>>,
    errors: Mutex<HashMap<PathBuf, String>>,
    indexing: Mutex<HashSet<PathBuf>>,
    /// Serializes cache builds; converting a large dictionary takes seconds and lots of memory.
    build: Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl DictionaryState {
    /// Builds the cache file when it is missing or older than the dictionary.
    fn ensure_cache(&self, cache_dir: &Path, source: &Path) -> Result<CacheHeader, String> {
        let stamp = source_stamp(source)?;
        let cache = cache_file(cache_dir, source);
        if let Some(header) = valid_header(&cache, stamp) {
            return Ok(header);
        }
        let _build = lock(&self.build);
        if let Some(header) = valid_header(&cache, stamp) {
            return Ok(header);
        }
        let result = fs::create_dir_all(cache_dir)
            .map_err(|error| format!("Неуспешно създаване на {}: {error}", cache_dir.display()))
            .and_then(|()| read_dictionary(source))
            .and_then(|dictionary| {
                write_cache(&cache, stamp, &dictionary)
                    .map_err(|error| format!("Неуспешно записване на индекса: {error}"))
            });
        if let Err(error) = result {
            lock(&self.errors).insert(source.to_path_buf(), error.clone());
            return Err(error);
        }
        lock(&self.errors).remove(source);
        valid_header(&cache, stamp).ok_or_else(|| "Индексът не може да бъде прочетен.".to_string())
    }

    fn index(&self, cache_dir: &Path, source: &Path) -> Result<Arc<DictionaryIndex>, String> {
        let stamp = source_stamp(source)?;
        if let Some(index) = lock(&self.indexes).get(source) {
            if index.stamp == stamp {
                return Ok(index.clone());
            }
        }
        self.ensure_cache(cache_dir, source)?;
        let index = DictionaryIndex::open(&cache_file(cache_dir, source))
            .map_err(|error| format!("Повреден индекс: {error}"))?;
        let index = Arc::new(index);
        lock(&self.indexes).insert(source.to_path_buf(), index.clone());
        Ok(index)
    }

    fn forget(&self, cache_dir: &Path, source: &Path, delete_cache: bool) {
        lock(&self.indexes).remove(source);
        if delete_cache {
            lock(&self.errors).remove(source);
            let _ = fs::remove_file(cache_file(cache_dir, source));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

type ChangeListener = Arc<dyn Fn() + Send + Sync>;

struct Inner {
    settings_path: PathBuf,
    cache_dir: PathBuf,
    state: DictionaryState,
    on_change: ChangeListener,
}

/// The configured dictionaries. Cheap to clone; clones share the loaded indexes.
#[derive(Clone)]
pub struct Dictionaries {
    inner: Arc<Inner>,
}

type EnabledIndexes = (Vec<(DictionaryConfig, Arc<DictionaryIndex>)>, Vec<String>);

impl Dictionaries {
    /// `settings_path` is the JSON list of dictionaries, `cache_dir` holds one index per
    /// dictionary; `on_change` runs after background indexing finishes a dictionary and after
    /// the list is saved (from any thread).
    pub fn new(
        settings_path: PathBuf,
        cache_dir: PathBuf,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                settings_path,
                cache_dir,
                state: DictionaryState::default(),
                on_change: Arc::new(on_change),
            }),
        }
    }

    pub fn settings(&self) -> Result<DictionarySettings, String> {
        let path = &self.inner.settings_path;
        match fs::read(path) {
            Ok(contents) => serde_json::from_slice(&contents)
                .map_err(|error| format!("Невалидни настройки в {}: {error}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(DictionarySettings::default())
            }
            Err(error) => Err(format!(
                "Неуспешно прочитане на {}: {error}",
                path.display()
            )),
        }
    }

    fn write_settings(&self, settings: &DictionarySettings) -> Result<(), String> {
        let path = &self.inner.settings_path;
        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory).map_err(|error| {
                format!("Неуспешно създаване на {}: {error}", directory.display())
            })?;
        }
        let serialized = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
        fs::write(path, serialized)
            .map_err(|error| format!("Неуспешно записване на {}: {error}", path.display()))
    }

    /// Indexes dictionaries on a background thread, calling `on_change` after each one.
    fn schedule_indexing(&self, paths: Vec<PathBuf>) {
        let paths: Vec<PathBuf> = {
            let mut indexing = lock(&self.inner.state.indexing);
            paths
                .into_iter()
                .filter(|path| indexing.insert(path.clone()))
                .collect()
        };
        if paths.is_empty() {
            return;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            let inner = &this.inner;
            for path in paths {
                if let Err(error) = inner.state.ensure_cache(&inner.cache_dir, &path) {
                    eprintln!("Could not index {}: {error}", path.display());
                }
                lock(&inner.state.indexing).remove(&path);
                (inner.on_change)();
            }
        });
    }

    /// The configured dictionaries with their status; starts indexing the ones without a cache.
    pub fn list(&self) -> Result<Vec<DictionaryInfo>, String> {
        let settings = self.settings()?;
        let state = &self.inner.state;
        let mut to_index = Vec::new();
        let infos = settings
            .dictionaries
            .iter()
            .map(|dictionary| {
                let path = PathBuf::from(&dictionary.path);
                let mut info = DictionaryInfo {
                    path: dictionary.path.clone(),
                    enabled: dictionary.enabled,
                    name: file_name(&dictionary.path),
                    articles: 0,
                    status: "indexing",
                    error: None,
                    source_lang: dictionary.source_lang.clone(),
                    target_lang: dictionary.target_lang.clone(),
                    detected_source_lang: String::new(),
                    detected_target_lang: String::new(),
                };
                let Ok(stamp) = source_stamp(&path) else {
                    info.status = "missing";
                    info.error = Some("Файлът не е намерен.".to_string());
                    return info;
                };
                if let Some(header) = valid_header(&cache_file(&self.inner.cache_dir, &path), stamp)
                {
                    info.status = "ready";
                    info.articles = header.articles;
                    (info.source_lang, info.target_lang) =
                        dictionary.languages(&header.source_lang, &header.target_lang);
                    info.detected_source_lang = header.source_lang;
                    info.detected_target_lang = header.target_lang;
                    if !header.title.is_empty() {
                        info.name = header.title;
                    }
                } else if let Some(error) = lock(&state.errors).get(&path) {
                    info.status = "error";
                    info.error = Some(error.clone());
                } else {
                    to_index.push(path);
                }
                info
            })
            .collect();
        self.schedule_indexing(to_index);
        Ok(infos)
    }

    /// Adds the supported files that are not in the list yet; returns how many were added.
    pub fn add(&self, paths: Vec<PathBuf>) -> Result<usize, String> {
        let mut settings = self.settings()?;
        let mut added = Vec::new();
        for path in paths {
            let text = path.to_string_lossy().into_owned();
            if is_supported_file(&path)
                && !settings
                    .dictionaries
                    .iter()
                    .any(|dictionary| same_path(&dictionary.path, &text))
            {
                settings.dictionaries.push(DictionaryConfig {
                    path: text,
                    enabled: true,
                    ..Default::default()
                });
                added.push(path);
            }
        }
        self.write_settings(&settings)?;
        let count = added.len();
        self.schedule_indexing(added);
        Ok(count)
    }

    /// Saves the order, the on/off switches and removals. New paths are only added with `add`.
    pub fn save(&self, dictionaries: Vec<DictionaryConfig>) -> Result<(), String> {
        let current = self.settings()?;
        let mut seen = HashSet::new();
        for dictionary in &dictionaries {
            if !current
                .dictionaries
                .iter()
                .any(|existing| existing.path == dictionary.path)
            {
                return Err("Непознат речник.".to_string());
            }
            if !seen.insert(dictionary.path.to_lowercase()) {
                return Err("Речникът е добавен два пъти.".to_string());
            }
        }
        self.write_settings(&DictionarySettings {
            dictionaries: dictionaries.clone(),
        })?;
        for existing in &current.dictionaries {
            let kept = dictionaries
                .iter()
                .find(|dictionary| dictionary.path == existing.path);
            if kept.is_none_or(|dictionary| !dictionary.enabled) {
                // A disabled dictionary keeps its cache file but not its index in memory.
                self.inner.state.forget(
                    &self.inner.cache_dir,
                    Path::new(&existing.path),
                    kept.is_none(),
                );
            }
        }
        (self.inner.on_change)();
        Ok(())
    }

    fn enabled_indexes(&self) -> Result<EnabledIndexes, String> {
        let settings = self.settings()?;
        let mut indexes = Vec::new();
        let mut errors = Vec::new();
        for dictionary in settings
            .dictionaries
            .iter()
            .filter(|dictionary| dictionary.enabled)
        {
            match self
                .inner
                .state
                .index(&self.inner.cache_dir, Path::new(&dictionary.path))
            {
                Ok(index) => indexes.push((dictionary.clone(), index)),
                Err(error) => errors.push(format!("{}: {error}", file_name(&dictionary.path))),
            }
        }
        Ok((indexes, errors))
    }

    /// Looks `query` up in the enabled dictionaries, in their configured order. Blocks while a
    /// dictionary without a cache is indexed.
    pub fn lookup(&self, query: &str) -> Result<LookupResult, String> {
        let (indexes, mut errors) = self.enabled_indexes()?;
        let candidates = query_candidates(query);
        let mut groups = Vec::new();
        let mut matched = String::new();
        for candidate in &candidates {
            for (dictionary, index) in &indexes {
                let path = &dictionary.path;
                let (source_lang, target_lang) =
                    dictionary.languages(&index.source_lang, &index.target_lang);
                let mut offsets = index.exact(candidate);
                if offsets.is_empty() {
                    continue;
                }
                offsets.truncate(MAX_ENTRIES_PER_DICTIONARY);
                match index.read_entries(&offsets) {
                    Ok(entries) => groups.push(DictionaryGroup {
                        path: path.clone(),
                        source_lang,
                        target_lang,
                        name: if index.title.is_empty() {
                            file_name(path)
                        } else {
                            index.title.clone()
                        },
                        entries,
                    }),
                    Err(error) => errors.push(format!("{}: {error}", file_name(path))),
                }
            }
            if !groups.is_empty() {
                matched = candidate.clone();
                break;
            }
        }
        let suggestions = match (groups.is_empty(), candidates.last()) {
            (true, Some(word)) => nearby_words(&indexes, word),
            _ => Vec::new(),
        };
        Ok(LookupResult {
            query: query.to_string(),
            matched,
            groups,
            suggestions,
            errors,
            enabled: indexes.len(),
        })
    }

    /// Keys that start with `prefix`, across the enabled dictionaries.
    pub fn suggest(&self, prefix: &str) -> Result<Vec<String>, String> {
        let prefix = normalize(prefix);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let (indexes, _) = self.enabled_indexes()?;
        Ok(suggestions(&indexes, &prefix, MAX_SUGGESTIONS))
    }
}

fn query_candidates(query: &str) -> Vec<String> {
    let mut candidates = vec![normalize(query)];
    let trimmed = normalize(query.trim_matches(|character: char| !character.is_alphanumeric()));
    if !candidates.contains(&trimmed) {
        candidates.push(trimmed);
    }
    candidates.retain(|candidate| !candidate.is_empty());
    candidates
}

fn suggestions(
    indexes: &[(DictionaryConfig, Arc<DictionaryIndex>)],
    prefix: &str,
    limit: usize,
) -> Vec<String> {
    let mut words: Vec<String> = indexes
        .iter()
        .flat_map(|(_, index)| index.with_prefix(prefix, limit))
        .map(str::to_string)
        .collect();
    words.sort();
    words.dedup();
    words.truncate(limit);
    words
}

/// Words that share the longest possible beginning with `word`, for "did you mean".
fn nearby_words(indexes: &[(DictionaryConfig, Arc<DictionaryIndex>)], word: &str) -> Vec<String> {
    let characters: Vec<char> = word.chars().collect();
    for length in (2.min(characters.len())..=characters.len()).rev() {
        let prefix: String = characters[..length].iter().collect();
        let words = suggestions(indexes, &prefix, MAX_SUGGESTIONS);
        if !words.is_empty() {
            return words;
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::bgl::*;
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    fn block(kind: u8, data: &[u8]) -> Vec<u8> {
        // Two length bytes: high nibble 1.
        let mut out = vec![0x10 | kind];
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    fn entry(word: &[u8], definition: &[u8], alternates: &[&[u8]]) -> Vec<u8> {
        let mut data = vec![word.len() as u8];
        data.extend_from_slice(word);
        data.extend_from_slice(&(definition.len() as u16).to_be_bytes());
        data.extend_from_slice(definition);
        for alternate in alternates {
            data.push(alternate.len() as u8);
            data.extend_from_slice(alternate);
        }
        block(1, &data)
    }

    fn bgl(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        for block in blocks {
            encoder.write_all(block).unwrap();
        }
        let mut file = vec![0x12, 0x34, 0x00, 0x01, 0x00, 0x06];
        file.extend(encoder.finish().unwrap());
        file
    }

    fn sample_dictionary() -> Vec<u8> {
        let (definition, _, _) = encoding_rs::WINDOWS_1251.encode("<b>ябълка</b>\nплод");
        let mut definition = definition.into_owned();
        // Part of speech: noun.
        definition.extend_from_slice(&[0x14, 0x02, 0x30]);
        bgl(&[
            block(3, &[0x00, 0x01, b'E', b'n', b'-', b'B', b'g']),
            block(3, &[0x00, 0x1b, 0x44]),
            entry(b"Apple$1$", &definition, &[b"apples"]),
            entry(
                b"take off",
                b"<a href=\"bword://take on\">take on</a><script>x()</script>",
                &[],
            ),
        ])
    }

    #[test]
    fn parses_babylon_entries_with_code_page_and_fields() {
        let dictionary = parse_bgl(&sample_dictionary()).unwrap();
        assert_eq!(dictionary.title, "En-Bg");
        assert_eq!(dictionary.articles.len(), 2);
        let apple = &dictionary.articles[0];
        assert_eq!(apple.headword, "Apple");
        assert_eq!(apple.alternates, vec!["apples"]);
        assert_eq!(apple.part_of_speech, "n.");
        assert_eq!(apple.html, "<b>ябълка</b><br>плод");
    }

    #[test]
    fn rejects_files_that_are_not_babylon() {
        assert!(parse_bgl(b"not a dictionary").is_err());
    }

    #[test]
    fn cache_round_trip_finds_headwords_and_alternates() {
        let directory =
            std::env::temp_dir().join(format!("contextai-dict-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("sample.bgl");
        fs::write(&source, sample_dictionary()).unwrap();

        let state = DictionaryState::default();
        let index = state.index(&directory, &source).unwrap();
        assert_eq!(index.exact("apples"), index.exact("apple"));
        let entries = index.read_entries(&index.exact("apple")).unwrap();
        assert_eq!(entries[0].headword, "Apple");
        assert!(index.exact("pear").is_empty());
        assert_eq!(index.with_prefix("ta", 10), vec!["take off"]);

        let link = index.read_entries(&index.exact("take off")).unwrap();
        assert!(
            link[0].html.contains("href=\"bword:take%20on\""),
            "{}",
            link[0].html
        );
        assert!(!link[0].html.contains("script"));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn decodes_charset_tags() {
        let text = decode_bgl_text(
            b"a <charset c=T>0431;0432;</charset> b",
            encoding_rs::WINDOWS_1252,
            encoding_rs::WINDOWS_1252,
        );
        assert_eq!(text, "a бв b");
    }

    #[test]
    fn keys_are_normalized() {
        assert_eq!(normalize("  Те\u{301}кст   ДУМА "), "текст дума");
        assert_eq!(strip_dollar_indexes("bank$2$ $x$"), "bank $x$");
        assert_eq!(clean_text("<b>rock</b> &amp; roll"), "rock & roll");
        assert_eq!(query_candidates("„думата“,"), vec!["„думата“,", "думата"]);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// CONTEXTAI_BGL_DIR=D:\Downloads\Dictionary cargo test -p contextai-core dictionary::live_tests -- --ignored --nocapture
    #[test]
    #[ignore = "parses the .bgl files in CONTEXTAI_BGL_DIR"]
    fn parses_real_dictionaries() {
        let directory = PathBuf::from(std::env::var("CONTEXTAI_BGL_DIR").unwrap());
        let files = find_dictionaries(&directory);
        for file in files {
            let started = std::time::Instant::now();
            match read_dictionary(&file) {
                Ok(dictionary) => {
                    println!(
                        "{} | {:?} | {} articles | {:?}",
                        file.display(),
                        dictionary.title,
                        dictionary.articles.len(),
                        started.elapsed()
                    );
                    for article in dictionary
                        .articles
                        .iter()
                        .step_by(dictionary.articles.len() / 3 + 1)
                        .take(3)
                    {
                        let html: String = article.html.chars().take(160).collect();
                        println!(
                            "   {:?} alts={:?} pos={:?} tr={:?}\n      {}",
                            article.headword,
                            article.alternates.iter().take(4).collect::<Vec<_>>(),
                            article.part_of_speech,
                            article.transcription,
                            html
                        );
                    }
                }
                Err(error) => println!("{} | ERROR {error}", file.display()),
            }
        }
    }

    #[test]
    #[ignore = "indexes the .bgl files in CONTEXTAI_BGL_DIR"]
    fn looks_up_real_dictionaries() {
        let directory = PathBuf::from(std::env::var("CONTEXTAI_BGL_DIR").unwrap());
        let cache = std::env::temp_dir().join("contextai-dict-live");
        fs::create_dir_all(&cache).unwrap();
        let files = find_dictionaries(&directory);
        let state = DictionaryState::default();
        for file in files {
            let started = std::time::Instant::now();
            let index = state.index(&cache, &file).unwrap();
            let built = started.elapsed();
            let started = std::time::Instant::now();
            for word in ["take", "дом", "casa", "Ябълка"] {
                let entries = index.read_entries(&index.exact(&normalize(word))).unwrap();
                if let Some(entry) = entries.first() {
                    let html: String = entry.html.chars().take(200).collect();
                    println!("{} [{word}] {} -> {html}", index.title, entry.headword);
                }
            }
            println!(
                "{} [{} -> {}]: {} keys, built {built:?}, 4 lookups {:?}",
                index.title,
                index.source_lang,
                index.target_lang,
                index.key_ends.len(),
                started.elapsed()
            );
        }
        let _ = fs::remove_dir_all(&cache);
    }
}
