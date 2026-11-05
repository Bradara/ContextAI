//! Babylon (.bgl) parsing and the text helpers shared by all formats.

use std::{borrow::Cow, io::Read};

use encoding_rs::Encoding;

use super::{Article, ParsedDictionary};
use crate::language::babylon_language;

pub(crate) fn be_uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, &byte| (value << 8) | u64::from(byte))
}

pub(crate) fn gzip_header_len(data: &[u8]) -> Option<usize> {
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
pub(crate) fn bgl_payload(file: &[u8]) -> Result<Vec<u8>, String> {
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
pub(crate) fn bgl_blocks(data: &[u8]) -> Vec<(u8, &[u8])> {
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

pub(crate) fn babylon_charset(code: u8) -> Option<&'static Encoding> {
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

pub(crate) struct ByteReader<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) position: usize,
}

impl<'a> ByteReader<'a> {
    pub(crate) fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let bytes = self
            .data
            .get(self.position..self.position.checked_add(length)?)?;
        self.position += length;
        Some(bytes)
    }

    pub(crate) fn uint(&mut self, width: usize) -> Option<usize> {
        self.take(width).map(|bytes| be_uint(bytes) as usize)
    }

    /// A field preceded by its length in `width` big-endian bytes.
    pub(crate) fn chunk(&mut self, width: usize) -> Option<&'a [u8]> {
        let length = self.uint(width)?;
        self.take(length)
    }

    pub(crate) fn done(&self) -> bool {
        self.position >= self.data.len()
    }
}

pub(crate) struct RawEntry<'a> {
    word: &'a [u8],
    alternates: Vec<&'a [u8]>,
    definition: &'a [u8],
}

pub(crate) fn parse_entry(kind: u8, data: &[u8]) -> Option<RawEntry<'_>> {
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

pub(crate) fn detect_encoding<'a>(samples: impl Iterator<Item = &'a [u8]>) -> &'static Encoding {
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
pub(crate) struct Encodings {
    source: &'static Encoding,
    target: &'static Encoding,
}

pub(crate) fn parse_bgl(file: &[u8]) -> Result<ParsedDictionary, String> {
    let payload = bgl_payload(file)?;
    let blocks = bgl_blocks(&payload);

    let (mut title, mut utf8) = (Vec::new(), false);
    let (mut source_lang, mut target_lang) = ("", "");
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
                    0x07 if value.len() <= 8 => source_lang = babylon_language(be_uint(value)),
                    0x08 if value.len() <= 8 => target_lang = babylon_language(be_uint(value)),
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
    Ok(ParsedDictionary {
        title,
        articles,
        source_lang: source_lang.to_string(),
        target_lang: target_lang.to_string(),
    })
}

pub(crate) fn article(entry: &RawEntry, encodings: Encodings) -> Option<Article> {
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
pub(crate) struct DefinitionFields<'a> {
    body: &'a [u8],
    part_of_speech: Option<&'static str>,
    transcription: Option<&'a [u8]>,
    title: Option<&'a [u8]>,
}

pub(crate) fn part_of_speech(code: u8) -> Option<&'static str> {
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
pub(crate) fn definition_fields(definition: &[u8]) -> DefinitionFields<'_> {
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
pub(crate) enum Charset {
    Encoding(&'static Encoding),
    /// `<charset c=T>`: Unicode code points written as hex digits, e.g. `0431;`.
    CodePoints,
}

/// Recognizes `<charset c=X>` (returns the charset) and `</charset>` (returns None).
pub(crate) fn charset_tag(
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
pub(crate) fn decode_bytes<'a>(bytes: &'a [u8], encoding: &'static Encoding) -> Cow<'a, str> {
    if encoding == encoding_rs::UTF_8 {
        return String::from_utf8_lossy(bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => encoding.decode_without_bom_handling(bytes).0,
    }
}

pub(crate) fn decode_code_points(text: &str) -> String {
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

pub(crate) fn decode_bgl_text(
    bytes: &[u8],
    default: &'static Encoding,
    source: &'static Encoding,
) -> String {
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

pub(crate) fn strip_tags(text: &str) -> String {
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

pub(crate) fn decode_entities(text: &str) -> String {
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
pub(crate) fn strip_dollar_indexes(text: &str) -> String {
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

/// Markup to plain text on one line (also used by the StarDict reader).
pub(crate) fn plain_text(text: &str) -> String {
    clean_text(text)
}

/// Headwords and titles: plain text on one line.
pub(crate) fn clean_text(text: &str) -> String {
    let text = strip_dollar_indexes(&decode_entities(&strip_tags(text)));
    text.split(|character: char| character.is_whitespace() || character.is_control())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn definition_text(text: &str) -> String {
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
