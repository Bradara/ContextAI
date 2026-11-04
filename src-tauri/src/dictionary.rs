//! Local dictionaries: Babylon (.bgl) parsing, an on-disk index per dictionary and lookups.
//!
//! A .bgl file is a small header followed by a gzip stream of blocks. The first lookup (or adding
//! the file) converts it into a cache file under `%LOCALAPPDATA%\com.contextai.desktop\dictionaries`
//! with the articles as UTF-8 and a sorted key index; later only the index is kept in memory and
//! the articles are read from the cache file on demand.
//!
//! Other formats (StarDict) can be added by returning `ParsedDictionary` from `read_dictionary`.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::UNIX_EPOCH,
};

use encoding_rs::Encoding;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

/// Entries shown per dictionary for one word.
const MAX_ENTRIES_PER_DICTIONARY: usize = 30;
const MAX_SUGGESTIONS: usize = 30;
/// Longer keys are not indexed; they are sentences rather than words.
const MAX_KEY_BYTES: usize = 250;
const CACHE_MAGIC: &[u8; 8] = b"CTXDIC01";

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryConfig {
    pub path: String,
    pub enabled: bool,
}

/// `dictionaries.json`; the order of the list is the order of the results.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DictionarySettings {
    pub dictionaries: Vec<DictionaryConfig>,
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Неуспешно създаване на {}: {error}", directory.display()))?;
    Ok(directory.join("dictionaries.json"))
}

fn cache_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?
        .join("dictionaries");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Неуспешно създаване на {}: {error}", directory.display()))?;
    Ok(directory)
}

fn load_settings(app: &AppHandle) -> Result<DictionarySettings, String> {
    let path = settings_path(app)?;
    match fs::read(&path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map_err(|error| format!("Невалидни настройки в {}: {error}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DictionarySettings::default()),
        Err(error) => Err(format!(
            "Неуспешно прочитане на {}: {error}",
            path.display()
        )),
    }
}

fn save_settings(app: &AppHandle, settings: &DictionarySettings) -> Result<(), String> {
    let path = settings_path(app)?;
    let serialized = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    fs::write(&path, serialized)
        .map_err(|error| format!("Неуспешно записване на {}: {error}", path.display()))
}

/// Paths are compared case-insensitively, as Windows does.
fn same_path(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn is_supported_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("bgl"))
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

// ---------------------------------------------------------------------------------------------
// Babylon .bgl parsing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
struct Article {
    headword: String,
    alternates: Vec<String>,
    part_of_speech: String,
    transcription: String,
    /// HTML as written by the dictionary; sanitized when shown.
    html: String,
}

#[derive(Debug, Default)]
struct ParsedDictionary {
    title: String,
    articles: Vec<Article>,
}

fn read_dictionary(path: &Path) -> Result<ParsedDictionary, String> {
    if !is_supported_file(path) {
        return Err("Поддържат се само речници във формат Babylon (.bgl).".to_string());
    }
    let contents = fs::read(path).map_err(|error| format!("Неуспешно прочитане: {error}"))?;
    parse_bgl(&contents)
}

fn be_uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, &byte| (value << 8) | u64::from(byte))
}

fn gzip_header_len(data: &[u8]) -> Option<usize> {
    if data.len() < 10 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return None;
    }
    let flags = data[3];
    let mut position = 10;
    if flags & 0x04 != 0 {
        let extra = u16::from_le_bytes([*data.get(position)?, *data.get(position + 1)?]);
        position += 2 + usize::from(extra);
    }
    for flag in [0x08, 0x10] {
        if flags & flag != 0 {
            position += data.get(position..)?.iter().position(|&byte| byte == 0)? + 1;
        }
    }
    if flags & 0x02 != 0 {
        position += 2;
    }
    (position <= data.len()).then_some(position)
}

/// Returns the decompressed block stream. The gzip trailer is not checked: many .bgl files
/// have a wrong CRC, and a stream cut short still yields the blocks before the damage.
fn bgl_payload(file: &[u8]) -> Result<Vec<u8>, String> {
    if file.len() < 6 || file[..3] != [0x12, 0x34, 0x00] || !matches!(file[3], 1 | 2) {
        return Err("Файлът не е речник във формат Babylon (.bgl).".to_string());
    }
    let offset = usize::from(u16::from_be_bytes([file[4], file[5]]));
    let gzip = file
        .get(offset..)
        .filter(|_| offset >= 6)
        .ok_or("Невалиден .bgl файл: липсват данни.")?;
    let start = gzip_header_len(gzip).ok_or("Невалиден .bgl файл: повреден gzip поток.")?;
    let mut payload = Vec::new();
    let result = flate2::read::DeflateDecoder::new(&gzip[start..]).read_to_end(&mut payload);
    match result {
        Err(error) if payload.is_empty() => Err(format!(
            "Невалиден .bgl файл: неуспешно разархивиране ({error})."
        )),
        _ => Ok(payload),
    }
}

/// Splits the stream into (type, data) blocks. The low nibble of the first byte is the type;
/// the high nibble is either the length + 4 or the number of length bytes - 1 that follow.
fn bgl_blocks(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut blocks = Vec::new();
    let mut position = 0;
    while position < data.len() {
        let first = data[position];
        position += 1;
        let high = usize::from(first >> 4);
        let length = if high < 4 {
            let Some(bytes) = data.get(position..position + high + 1) else {
                break;
            };
            position += high + 1;
            be_uint(bytes) as usize
        } else {
            high - 4
        };
        let Some(block) = data.get(position..position.saturating_add(length)) else {
            break;
        };
        position += length;
        blocks.push((first & 0x0f, block));
    }
    blocks
}

fn babylon_charset(code: u8) -> Option<&'static Encoding> {
    use encoding_rs::*;
    Some(match code {
        0x41 | 0x42 => WINDOWS_1252,
        0x43 => WINDOWS_1250,
        0x44 => WINDOWS_1251,
        0x45 => SHIFT_JIS,
        0x46 => BIG5,
        0x47 => GBK,
        0x48 => WINDOWS_1257,
        0x49 => WINDOWS_1253,
        0x4a => EUC_KR,
        0x4b => WINDOWS_1254,
        0x4c => WINDOWS_1255,
        0x4d => WINDOWS_1256,
        0x4e => WINDOWS_874,
        _ => return None,
    })
}

struct ByteReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ByteReader<'a> {
    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let bytes = self
            .data
            .get(self.position..self.position.checked_add(length)?)?;
        self.position += length;
        Some(bytes)
    }

    fn uint(&mut self, width: usize) -> Option<usize> {
        self.take(width).map(|bytes| be_uint(bytes) as usize)
    }

    /// A field preceded by its length in `width` big-endian bytes.
    fn chunk(&mut self, width: usize) -> Option<&'a [u8]> {
        let length = self.uint(width)?;
        self.take(length)
    }

    fn done(&self) -> bool {
        self.position >= self.data.len()
    }
}

struct RawEntry<'a> {
    word: &'a [u8],
    alternates: Vec<&'a [u8]>,
    definition: &'a [u8],
}

fn parse_entry(kind: u8, data: &[u8]) -> Option<RawEntry<'_>> {
    let mut reader = ByteReader { data, position: 0 };
    if kind == 11 {
        let word = reader.chunk(5)?;
        let count = reader.uint(4)?;
        let mut alternates = Vec::new();
        for _ in 0..count {
            let length = reader.uint(4)?;
            if length == 0 {
                break;
            }
            alternates.push(reader.take(length)?);
        }
        let definition = reader.chunk(4)?;
        return Some(RawEntry {
            word,
            alternates,
            definition,
        });
    }
    let word = reader.chunk(1)?;
    let definition = reader.chunk(2)?;
    let mut alternates = Vec::new();
    while !reader.done() {
        match reader.chunk(1) {
            Some(alternate) => alternates.push(alternate),
            None => break,
        }
    }
    Some(RawEntry {
        word,
        alternates,
        definition,
    })
}

fn detect_encoding<'a>(samples: impl Iterator<Item = &'a [u8]>) -> &'static Encoding {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    let mut fed = 0;
    for sample in samples {
        if std::str::from_utf8(sample).is_ok() {
            continue;
        }
        detector.feed(sample, false);
        detector.feed(b" ", false);
        fed += sample.len();
        if fed > 256 * 1024 {
            break;
        }
    }
    detector.guess(None, chardetng::Utf8Detection::Deny)
}

#[derive(Clone, Copy)]
struct Encodings {
    source: &'static Encoding,
    target: &'static Encoding,
}

fn parse_bgl(file: &[u8]) -> Result<ParsedDictionary, String> {
    let payload = bgl_payload(file)?;
    let blocks = bgl_blocks(&payload);

    let (mut title, mut utf8) = (Vec::new(), false);
    let (mut default, mut source, mut target) = (None, None, None);
    let mut raw_entries = Vec::new();
    for &(kind, data) in &blocks {
        match kind {
            0 if data.first() == Some(&8) => {
                default = data.get(1).copied().and_then(babylon_charset)
            }
            3 if data.len() > 2 => {
                let value = &data[2..];
                match be_uint(&data[..2]) {
                    0x01 => title = value.to_vec(),
                    0x11 if value.len() <= 8 => utf8 = be_uint(value) & 0x8000 != 0,
                    0x1a => source = babylon_charset(value[0]),
                    0x1b => target = babylon_charset(value[0]),
                    _ => {}
                }
            }
            1 | 7 | 10 | 11 | 13 => {
                if let Some(entry) = parse_entry(kind, data) {
                    raw_entries.push(entry);
                }
            }
            _ => {}
        }
    }
    if raw_entries.is_empty() {
        return Err("Речникът не съдържа статии.".to_string());
    }

    let encodings = if utf8 {
        Encodings {
            source: encoding_rs::UTF_8,
            target: encoding_rs::UTF_8,
        }
    } else {
        Encodings {
            source: source.or(default).unwrap_or_else(|| {
                detect_encoding(raw_entries.iter().take(5000).map(|entry| entry.word))
            }),
            target: target.or(default).unwrap_or_else(|| {
                detect_encoding(raw_entries.iter().take(2000).map(|entry| entry.definition))
            }),
        }
    };

    let articles = raw_entries
        .iter()
        .filter_map(|entry| article(entry, encodings))
        .collect();
    let title = clean_text(&decode_bgl_text(&title, encodings.source, encodings.source));
    Ok(ParsedDictionary { title, articles })
}

fn article(entry: &RawEntry, encodings: Encodings) -> Option<Article> {
    let key = clean_text(&decode_bgl_text(
        entry.word,
        encodings.source,
        encodings.source,
    ));
    let fields = definition_fields(entry.definition);
    let headword = fields
        .title
        .map(|title| clean_text(&decode_bgl_text(title, encodings.target, encodings.source)))
        .filter(|title| !title.is_empty() && normalize(title) == normalize(&key))
        .unwrap_or_else(|| key.clone());
    if headword.is_empty() {
        return None;
    }
    let mut alternates: Vec<String> = entry
        .alternates
        .iter()
        .map(|alternate| {
            clean_text(&decode_bgl_text(
                alternate,
                encodings.source,
                encodings.source,
            ))
        })
        .filter(|alternate| !alternate.is_empty() && *alternate != headword)
        .collect();
    if key != headword && !key.is_empty() {
        alternates.push(key);
    }
    let html = definition_text(&decode_bgl_text(
        fields.body,
        encodings.target,
        encodings.source,
    ));
    let transcription = fields
        .transcription
        .map(|bytes| clean_text(&decode_bgl_text(bytes, encodings.target, encodings.source)))
        .unwrap_or_default();
    Some(Article {
        headword,
        alternates,
        part_of_speech: fields.part_of_speech.unwrap_or_default().to_string(),
        transcription,
        html,
    })
}

#[derive(Default)]
struct DefinitionFields<'a> {
    body: &'a [u8],
    part_of_speech: Option<&'static str>,
    transcription: Option<&'a [u8]>,
    title: Option<&'a [u8]>,
}

fn part_of_speech(code: u8) -> Option<&'static str> {
    Some(match code {
        0x30 => "n.",
        0x31 => "adj.",
        0x32 => "v.",
        0x33 => "adv.",
        0x34 => "interj.",
        0x35 => "pron.",
        0x36 => "prep.",
        0x37 => "conj.",
        0x38 => "suff.",
        0x39 => "pref.",
        0x3a => "art.",
        _ => return None,
    })
}

/// The article text ends at a 0x14 byte (not followed by a space); binary fields follow it:
/// part of speech, transcription, display title and some whose meaning is unknown.
fn definition_fields(definition: &[u8]) -> DefinitionFields<'_> {
    let start = (0..definition.len().saturating_sub(1))
        .find(|&index| definition[index] == 0x14 && definition[index + 1] != b' ');
    let Some(start) = start else {
        return DefinitionFields {
            body: definition,
            ..Default::default()
        };
    };
    let mut fields = DefinitionFields {
        body: &definition[..start],
        ..Default::default()
    };
    let data = &definition[start + 1..];
    let byte = |index: usize| data.get(index).copied().map(usize::from);
    let mut index = 0;
    while let Some(code) = byte(index) {
        let next = match code {
            0x02 => {
                fields.part_of_speech = byte(index + 1).and_then(|code| part_of_speech(code as u8));
                index + 2
            }
            0x06 => index + 2,
            0x07 => index + 3,
            0x13 | 0x1a => index + 2 + byte(index + 1).unwrap_or(usize::MAX / 2),
            0x18 => {
                let Some(length) = byte(index + 1) else { break };
                fields.title = data.get(index + 2..index + 2 + length);
                index + 2 + length
            }
            0x28 => {
                let Some(length) = data.get(index + 1..index + 3).map(be_uint) else {
                    break;
                };
                index + 3 + length as usize
            }
            0x40..=0x4f => index + 2 + (code - 0x3f),
            0x50 => {
                let Some(length) = byte(index + 2) else { break };
                if byte(index + 1) == Some(0x1b) {
                    fields.transcription = data.get(index + 3..index + 3 + length);
                }
                index + 3 + length
            }
            0x60 => {
                let Some(length) = data.get(index + 2..index + 4).map(be_uint) else {
                    break;
                };
                let length = length as usize;
                if byte(index + 1) == Some(0x1b) {
                    fields.transcription = data.get(index + 4..index + 4 + length);
                }
                index + 4 + length
            }
            _ => break,
        };
        if next <= index || next > data.len() {
            break;
        }
        index = next;
    }
    fields
}

#[derive(Clone, Copy)]
enum Charset {
    Encoding(&'static Encoding),
    /// `<charset c=T>`: Unicode code points written as hex digits, e.g. `0431;`.
    CodePoints,
}

/// Recognizes `<charset c=X>` (returns the charset) and `</charset>` (returns None).
fn charset_tag(
    bytes: &[u8],
    default: &'static Encoding,
    source: &'static Encoding,
) -> Option<(usize, Option<Charset>)> {
    let starts_with = |prefix: &[u8]| {
        bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    if starts_with(b"</charset>") {
        return Some((10, None));
    }
    if !starts_with(b"<charset") {
        return None;
    }
    let end = bytes.iter().take(40).position(|&byte| byte == b'>')?;
    let inner = bytes[8..end].to_ascii_lowercase();
    let at = inner.windows(2).position(|pair| pair == b"c=")?;
    let code = *inner[at + 2..]
        .iter()
        .find(|&&byte| byte != b'"' && byte != b'\'')?;
    let charset = match code {
        b't' => Charset::CodePoints,
        b'u' => Charset::Encoding(encoding_rs::UTF_8),
        b'k' | b'e' => Charset::Encoding(source),
        b'g' => Charset::Encoding(encoding_rs::GBK),
        _ => Charset::Encoding(default),
    };
    Some((end + 1, Some(charset)))
}

/// Valid UTF-8 wins over the declared code page: many dictionaries mix both.
fn decode_bytes<'a>(bytes: &'a [u8], encoding: &'static Encoding) -> Cow<'a, str> {
    if encoding == encoding_rs::UTF_8 {
        return String::from_utf8_lossy(bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => encoding.decode_without_bom_handling(bytes).0,
    }
}

fn decode_code_points(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest
            .bytes()
            .take(4)
            .take_while(u8::is_ascii_hexdigit)
            .count();
        if digits >= 3 && rest.as_bytes().get(digits) == Some(&b';') {
            if let Some(character) = u32::from_str_radix(&rest[..digits], 16)
                .ok()
                .and_then(char::from_u32)
            {
                out.push(character);
                rest = &rest[digits + 1..];
                continue;
            }
        }
        let character = rest.chars().next().unwrap_or_default();
        out.push(character);
        rest = &rest[character.len_utf8()..];
    }
    out
}

fn decode_bgl_text(bytes: &[u8], default: &'static Encoding, source: &'static Encoding) -> String {
    let mut out = String::new();
    let mut stack: Vec<Charset> = Vec::new();
    let mut segment = 0;
    let mut position = 0;
    let flush = |out: &mut String, bytes: &[u8], charset: Option<&Charset>| match charset {
        Some(Charset::CodePoints) => {
            out.push_str(&decode_code_points(&String::from_utf8_lossy(bytes)))
        }
        Some(Charset::Encoding(encoding)) => out.push_str(&decode_bytes(bytes, encoding)),
        None => out.push_str(&decode_bytes(bytes, default)),
    };
    while position < bytes.len() {
        if bytes[position] == b'<' {
            if let Some((length, charset)) = charset_tag(&bytes[position..], default, source) {
                flush(&mut out, &bytes[segment..position], stack.last());
                match charset {
                    Some(charset) => stack.push(charset),
                    None => {
                        stack.pop();
                    }
                }
                position += length;
                segment = position;
                continue;
            }
        }
        position += 1;
    }
    flush(&mut out, &bytes[segment..], stack.last());
    out
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for character in text.chars() {
        match character {
            '<' => inside = true,
            '>' if inside => inside = false,
            _ if !inside => out.push(character),
            _ => {}
        }
    }
    out
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest
            .char_indices()
            .take(12)
            .find(|&(_, character)| character == ';')
            .map(|(index, _)| index);
        let decoded = end.and_then(|end| {
            let name = &rest[1..end];
            let character = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    let number = name.strip_prefix('#')?;
                    let value = match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => number.parse().ok()?,
                    };
                    char::from_u32(value)
                }
            }?;
            Some((character, end))
        });
        match decoded {
            Some((character, end)) => {
                out.push(character);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Removes the `$12$` markers Babylon appends to tell apart words written the same way.
fn strip_dollar_indexes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after.as_bytes().get(digits) == Some(&b'$') {
            rest = &after[digits + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Headwords and titles: plain text on one line.
fn clean_text(text: &str) -> String {
    let text = strip_dollar_indexes(&decode_entities(&strip_tags(text)));
    text.split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn definition_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() != Some(&'\n') {
                    out.push_str("<br>");
                }
            }
            '\n' => out.push_str("<br>"),
            '\t' => out.push(' '),
            _ if character.is_control() => {}
            _ => out.push(character),
        }
    }
    out.trim().to_string()
}

/// The search key: lower case, single spaces, no stress marks or soft hyphens.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if ('\u{300}'..='\u{36f}').contains(&character) || character == '\u{ad}' {
            continue;
        }
        if character.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        match character {
            '’' | '‘' | '`' => out.push('\''),
            _ => out.extend(character.to_lowercase()),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Cache file
//
// magic (8) | index offset u64 | source length u64 | source mtime u64 | article count u32 |
// title (u32 length + UTF-8) | articles (4 × length-prefixed strings) |
// index: key count u32, then per key: u16 length, key, u64 article offset (sorted by key)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
struct SourceStamp {
    length: u64,
    modified: u64,
}

fn source_stamp(path: &Path) -> Result<SourceStamp, String> {
    let metadata = fs::metadata(path).map_err(|_| "Файлът не е намерен.".to_string())?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs());
    Ok(SourceStamp {
        length: metadata.len(),
        modified,
    })
}

fn cache_file(cache_dir: &Path, source: &Path) -> PathBuf {
    // FNV-1a: stable across Rust versions, unlike DefaultHasher.
    let hash = source
        .to_string_lossy()
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    cache_dir.join(format!("{hash:016x}.idx"))
}

fn write_string(writer: &mut impl Write, text: &str) -> io::Result<u64> {
    writer.write_all(&(text.len() as u32).to_le_bytes())?;
    writer.write_all(text.as_bytes())?;
    Ok(4 + text.len() as u64)
}

fn write_cache(path: &Path, stamp: SourceStamp, dictionary: &ParsedDictionary) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut writer = BufWriter::new(File::create(&temporary)?);
    writer.write_all(CACHE_MAGIC)?;
    writer.write_all(&0_u64.to_le_bytes())?;
    writer.write_all(&stamp.length.to_le_bytes())?;
    writer.write_all(&stamp.modified.to_le_bytes())?;
    writer.write_all(&(dictionary.articles.len() as u32).to_le_bytes())?;
    let mut position = 36 + write_string(&mut writer, &dictionary.title)?;

    let mut keys: Vec<(String, u64)> = Vec::new();
    for article in &dictionary.articles {
        let offset = position;
        for text in [
            &article.headword,
            &article.part_of_speech,
            &article.transcription,
            &article.html,
        ] {
            position += write_string(&mut writer, text)?;
        }
        let mut seen = HashSet::new();
        for word in std::iter::once(&article.headword).chain(&article.alternates) {
            let key = normalize(word);
            if !key.is_empty() && key.len() <= MAX_KEY_BYTES && seen.insert(key.clone()) {
                keys.push((key, offset));
            }
        }
    }
    keys.sort_unstable();

    let index_offset = position;
    writer.write_all(&(keys.len() as u32).to_le_bytes())?;
    for (key, offset) in &keys {
        writer.write_all(&(key.len() as u16).to_le_bytes())?;
        writer.write_all(key.as_bytes())?;
        writer.write_all(&offset.to_le_bytes())?;
    }
    let mut file = writer.into_inner().map_err(|error| error.into_error())?;
    file.seek(SeekFrom::Start(8))?;
    file.write_all(&index_offset.to_le_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)
}

struct CacheHeader {
    index_offset: u64,
    stamp: SourceStamp,
    articles: u32,
    title: String,
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_string(reader: &mut impl Read) -> io::Result<String> {
    let length = read_u32(reader)? as usize;
    if length > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "string too long",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_header(reader: &mut impl Read) -> io::Result<CacheHeader> {
    let mut magic = [0; 8];
    reader.read_exact(&mut magic)?;
    if &magic != CACHE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a dictionary cache",
        ));
    }
    let index_offset = read_u64(reader)?;
    let stamp = SourceStamp {
        length: read_u64(reader)?,
        modified: read_u64(reader)?,
    };
    let articles = read_u32(reader)?;
    let title = read_string(reader)?;
    if index_offset == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete cache",
        ));
    }
    Ok(CacheHeader {
        index_offset,
        stamp,
        articles,
        title,
    })
}

/// The header of a cache that still matches its source file.
fn valid_header(cache: &Path, stamp: SourceStamp) -> Option<CacheHeader> {
    let mut file = BufReader::new(File::open(cache).ok()?);
    read_header(&mut file)
        .ok()
        .filter(|header| header.stamp == stamp)
}

struct DictionaryIndex {
    cache: PathBuf,
    stamp: SourceStamp,
    title: String,
    /// All keys concatenated; `key_ends[i]` is where key `i` ends.
    keys: String,
    key_ends: Vec<u32>,
    targets: Vec<u64>,
}

impl DictionaryIndex {
    fn open(cache: &Path) -> io::Result<Self> {
        let mut file = BufReader::new(File::open(cache)?);
        let header = read_header(&mut file)?;
        file.seek(SeekFrom::Start(header.index_offset))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        let invalid = || io::Error::new(io::ErrorKind::InvalidData, "damaged index");
        let mut reader = ByteReader {
            data: &data,
            position: 0,
        };
        let count = reader
            .take(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .ok_or_else(invalid)? as usize;
        let mut keys = Vec::new();
        let mut key_ends = Vec::with_capacity(count);
        let mut targets = Vec::with_capacity(count);
        for _ in 0..count {
            let length = reader
                .take(2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .ok_or_else(invalid)?;
            keys.extend_from_slice(reader.take(usize::from(length)).ok_or_else(invalid)?);
            key_ends.push(keys.len() as u32);
            let target = reader.take(8).ok_or_else(invalid)?;
            targets.push(u64::from_le_bytes(target.try_into().unwrap()));
        }
        let keys = String::from_utf8(keys).map_err(|_| invalid())?;
        Ok(Self {
            cache: cache.to_path_buf(),
            stamp: header.stamp,
            title: header.title,
            keys,
            key_ends,
            targets,
        })
    }

    fn key(&self, index: usize) -> &str {
        let start = if index == 0 {
            0
        } else {
            self.key_ends[index - 1] as usize
        };
        &self.keys[start..self.key_ends[index] as usize]
    }

    /// The first key that is not less than `needle`.
    fn lower_bound(&self, needle: &str) -> usize {
        let (mut low, mut high) = (0, self.key_ends.len());
        while low < high {
            let middle = (low + high) / 2;
            if self.key(middle) < needle {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn exact(&self, key: &str) -> Vec<u64> {
        let mut offsets = Vec::new();
        let mut index = self.lower_bound(key);
        while index < self.key_ends.len() && self.key(index) == key {
            if !offsets.contains(&self.targets[index]) {
                offsets.push(self.targets[index]);
            }
            index += 1;
        }
        offsets
    }

    fn with_prefix(&self, prefix: &str, limit: usize) -> Vec<&str> {
        let mut words: Vec<&str> = Vec::new();
        let mut index = self.lower_bound(prefix);
        while index < self.key_ends.len() && words.len() < limit {
            let key = self.key(index);
            if !key.starts_with(prefix) {
                break;
            }
            if words.last() != Some(&key) {
                words.push(key);
            }
            index += 1;
        }
        words
    }

    fn read_entries(&self, offsets: &[u64]) -> io::Result<Vec<DictionaryEntry>> {
        let mut file = BufReader::new(File::open(&self.cache)?);
        offsets
            .iter()
            .map(|&offset| {
                file.seek(SeekFrom::Start(offset))?;
                Ok(DictionaryEntry {
                    headword: read_string(&mut file)?,
                    part_of_speech: read_string(&mut file)?,
                    transcription: read_string(&mut file)?,
                    html: sanitize_definition(&read_string(&mut file)?),
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------------------------

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `bword://take off` links to other articles become `bword:take%20off`, which survives the
/// sanitizer (a URL host cannot contain spaces); the UI turns a click on them into a search.
fn rewrite_word_links(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let (mut copied, mut search) = (0, 0);
    while let Some(found) = lower[search..].find("bword://") {
        let start = search + found;
        let quote = match html[..start].chars().next_back() {
            Some(quote @ ('"' | '\'')) => quote,
            _ => {
                search = start + 8;
                continue;
            }
        };
        let Some(length) = html[start + 8..].find(quote) else {
            break;
        };
        out.push_str(&html[copied..start]);
        out.push_str("bword:");
        out.push_str(&percent_encode(&decode_entities(
            &html[start + 8..start + 8 + length],
        )));
        copied = start + 8 + length;
        search = copied;
    }
    out.push_str(&html[copied..]);
    out
}

fn sanitize_definition(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags(["font"])
        .add_tag_attributes("font", ["color"])
        .rm_tags(["img"])
        .url_schemes(HashSet::from(["bword"]))
        .url_relative(ammonia::UrlRelative::Deny)
        .link_rel(None)
        .clean(&rewrite_word_links(html))
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// State and commands
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct DictionaryState {
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
        let result = read_dictionary(source).and_then(|dictionary| {
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

/// Indexes dictionaries in the background and tells the windows after each one.
fn schedule_indexing(app: &AppHandle, paths: Vec<PathBuf>) {
    let state = app.state::<DictionaryState>();
    let paths: Vec<PathBuf> = {
        let mut indexing = lock(&state.indexing);
        paths
            .into_iter()
            .filter(|path| indexing.insert(path.clone()))
            .collect()
    };
    if paths.is_empty() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<DictionaryState>();
        let cache_dir = cache_dir(&app);
        for path in paths {
            if let Ok(cache_dir) = &cache_dir {
                if let Err(error) = state.ensure_cache(cache_dir, &path) {
                    eprintln!("Could not index {}: {error}", path.display());
                }
            }
            lock(&state.indexing).remove(&path);
            let _ = app.emit("dictionaries-changed", ());
        }
    });
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryInfo {
    path: String,
    enabled: bool,
    name: String,
    articles: u32,
    /// ready | indexing | error | missing
    status: &'static str,
    error: Option<String>,
}

fn dictionary_infos(app: &AppHandle) -> Result<Vec<DictionaryInfo>, String> {
    let settings = load_settings(app)?;
    let cache_dir = cache_dir(app)?;
    let state = app.state::<DictionaryState>();
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
            };
            let Ok(stamp) = source_stamp(&path) else {
                info.status = "missing";
                info.error = Some("Файлът не е намерен.".to_string());
                return info;
            };
            if let Some(header) = valid_header(&cache_file(&cache_dir, &path), stamp) {
                info.status = "ready";
                info.articles = header.articles;
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
    schedule_indexing(app, to_index);
    Ok(infos)
}

#[tauri::command]
pub fn dictionary_list(app: AppHandle) -> Result<Vec<DictionaryInfo>, String> {
    dictionary_infos(&app)
}

fn add_paths(app: &AppHandle, paths: Vec<PathBuf>) -> Result<usize, String> {
    let mut settings = load_settings(app)?;
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
            });
            added.push(path);
        }
    }
    save_settings(app, &settings)?;
    let count = added.len();
    schedule_indexing(app, added);
    Ok(count)
}

fn find_dictionaries(directory: &Path, depth: usize, found: &mut Vec<PathBuf>) {
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
                find_dictionaries(&path, depth + 1, found);
            }
        } else if is_supported_file(&path) {
            found.push(path);
        }
    }
}

/// Opens a file or folder picker over the chat window; returns the number of new dictionaries.
#[tauri::command]
pub async fn dictionary_add(app: AppHandle, folder: bool) -> Result<usize, String> {
    use tauri_plugin_dialog::DialogExt;
    let dialog_app = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = dialog_app.dialog().file();
        if let Some(window) = dialog_app.get_webview_window("chat-panel") {
            dialog = dialog.set_parent(&window);
        }
        if folder {
            dialog
                .set_title("Папка с речници (.bgl)")
                .blocking_pick_folder()
                .map(|folder| vec![folder])
        } else {
            dialog
                .set_title("Речници")
                .add_filter("Babylon (.bgl)", &["bgl", "BGL"])
                .blocking_pick_files()
        }
    })
    .await
    .map_err(|error| error.to_string())?;
    let Some(picked) = picked else { return Ok(0) };
    let picked: Vec<PathBuf> = picked
        .into_iter()
        .filter_map(|path| path.into_path().ok())
        .collect();
    let paths = if folder {
        let mut found = Vec::new();
        for directory in &picked {
            find_dictionaries(directory, 0, &mut found);
        }
        if found.is_empty() {
            return Err("В папката няма .bgl речници.".to_string());
        }
        found
    } else {
        picked
    };
    add_paths(&app, paths)
}

/// Saves the order, the on/off switches and removals. New paths are only added with a picker.
#[tauri::command]
pub fn dictionary_save(app: AppHandle, dictionaries: Vec<DictionaryConfig>) -> Result<(), String> {
    let current = load_settings(&app)?;
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
    save_settings(
        &app,
        &DictionarySettings {
            dictionaries: dictionaries.clone(),
        },
    )?;
    let cache_dir = cache_dir(&app)?;
    let state = app.state::<DictionaryState>();
    for existing in &current.dictionaries {
        let kept = dictionaries
            .iter()
            .find(|dictionary| dictionary.path == existing.path);
        if kept.is_none_or(|dictionary| !dictionary.enabled) {
            // A disabled dictionary keeps its cache file but not its index in memory.
            state.forget(&cache_dir, Path::new(&existing.path), kept.is_none());
        }
    }
    let _ = app.emit("dictionaries-changed", ());
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntry {
    headword: String,
    part_of_speech: String,
    transcription: String,
    html: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryGroup {
    path: String,
    name: String,
    entries: Vec<DictionaryEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LookupResult {
    query: String,
    /// The form of the query that matched (e.g. without trailing punctuation).
    matched: String,
    groups: Vec<DictionaryGroup>,
    suggestions: Vec<String>,
    errors: Vec<String>,
    /// Number of enabled dictionaries; 0 means there is nothing to search in.
    enabled: usize,
}

type EnabledIndexes = (Vec<(String, Arc<DictionaryIndex>)>, Vec<String>);

fn enabled_indexes(app: &AppHandle) -> Result<EnabledIndexes, String> {
    let settings = load_settings(app)?;
    let cache_dir = cache_dir(app)?;
    let state = app.state::<DictionaryState>();
    let mut indexes = Vec::new();
    let mut errors = Vec::new();
    for dictionary in settings
        .dictionaries
        .iter()
        .filter(|dictionary| dictionary.enabled)
    {
        match state.index(&cache_dir, Path::new(&dictionary.path)) {
            Ok(index) => indexes.push((dictionary.path.clone(), index)),
            Err(error) => errors.push(format!("{}: {error}", file_name(&dictionary.path))),
        }
    }
    Ok((indexes, errors))
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
    indexes: &[(String, Arc<DictionaryIndex>)],
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
fn nearby_words(indexes: &[(String, Arc<DictionaryIndex>)], word: &str) -> Vec<String> {
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

fn lookup(app: &AppHandle, query: &str) -> Result<LookupResult, String> {
    let (indexes, mut errors) = enabled_indexes(app)?;
    let candidates = query_candidates(query);
    let mut groups = Vec::new();
    let mut matched = String::new();
    for candidate in &candidates {
        for (path, index) in &indexes {
            let mut offsets = index.exact(candidate);
            if offsets.is_empty() {
                continue;
            }
            offsets.truncate(MAX_ENTRIES_PER_DICTIONARY);
            match index.read_entries(&offsets) {
                Ok(entries) => groups.push(DictionaryGroup {
                    path: path.clone(),
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

#[tauri::command]
pub async fn dictionary_lookup(app: AppHandle, query: String) -> Result<LookupResult, String> {
    tauri::async_runtime::spawn_blocking(move || lookup(&app, &query))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn dictionary_suggest(app: AppHandle, prefix: String) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let prefix = normalize(&prefix);
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let (indexes, _) = enabled_indexes(&app)?;
        Ok(suggestions(&indexes, &prefix, MAX_SUGGESTIONS))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};

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

    /// CONTEXTAI_BGL_DIR=D:\Downloads\Dictionary cargo test --lib dictionary::live_tests -- --ignored --nocapture
    #[test]
    #[ignore = "parses the .bgl files in CONTEXTAI_BGL_DIR"]
    fn parses_real_dictionaries() {
        let directory = PathBuf::from(std::env::var("CONTEXTAI_BGL_DIR").unwrap());
        let mut files = Vec::new();
        find_dictionaries(&directory, 0, &mut files);
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
        let mut files = Vec::new();
        find_dictionaries(&directory, 0, &mut files);
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
                "{}: {} keys, built {built:?}, 4 lookups {:?}",
                index.title,
                index.key_ends.len(),
                started.elapsed()
            );
        }
        let _ = fs::remove_dir_all(&cache);
    }
}
