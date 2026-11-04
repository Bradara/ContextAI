//! StarDict dictionaries: `name.ifo` (metadata) with `name.idx[.gz]` (sorted words with offsets),
//! `name.dict[.dz]` (articles) and an optional `name.syn` (synonyms pointing at idx entries).
//! A dictionary is added by its .ifo file; the other files are found next to it.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::dictionary::{Article, ParsedDictionary};

/// The companion files of `name.ifo`, if present, in the order ifo, idx, dict, syn.
pub(crate) fn component_files(ifo: &Path) -> Vec<PathBuf> {
    let mut files = vec![ifo.to_path_buf()];
    for candidates in [
        &["idx", "idx.gz"][..],
        &["dict", "dict.dz"],
        &["syn", "syn.dz"],
    ] {
        if let Some(path) = candidates
            .iter()
            .map(|extension| ifo.with_extension(extension))
            .find(|path| path.is_file())
        {
            files.push(path);
        }
    }
    files
}

fn find_component(ifo: &Path, extensions: &[&str]) -> Option<PathBuf> {
    extensions
        .iter()
        .map(|extension| ifo.with_extension(extension))
        .find(|path| path.is_file())
}

/// Reads a file, decompressing `.gz` / `.dz` (dictzip is a valid gzip stream). A stream that is
/// cut short still yields what was decompressed before the damage.
fn read_maybe_compressed(path: &Path) -> Result<Vec<u8>, String> {
    let contents = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let compressed = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("dz")
        });
    if !compressed {
        return Ok(contents);
    }
    let mut data = Vec::new();
    match flate2::read::GzDecoder::new(contents.as_slice()).read_to_end(&mut data) {
        Err(error) if data.is_empty() => Err(format!(
            "{}: неуспешно разархивиране ({error})",
            path.display()
        )),
        _ => Ok(data),
    }
}

struct Info {
    title: String,
    offset_bits: u32,
    same_type_sequence: Vec<u8>,
}

fn parse_ifo(text: &str) -> Result<Info, String> {
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if !lines
        .next()
        .is_some_and(|line| line.trim() == "StarDict's dict ifo file")
    {
        return Err("Невалиден .ifo файл на StarDict.".to_string());
    }
    let mut info = Info {
        title: String::new(),
        offset_bits: 32,
        same_type_sequence: Vec::new(),
    };
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "bookname" => info.title = value.to_string(),
            "idxoffsetbits" => info.offset_bits = if value == "64" { 64 } else { 32 },
            "sametypesequence" => info.same_type_sequence = value.as_bytes().to_vec(),
            _ => {}
        }
    }
    Ok(info)
}

fn be_uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, &byte| (value << 8) | u64::from(byte))
}

/// A null-terminated UTF-8 word followed by `width` bytes of data; returns (word, data).
fn next_word<'a>(data: &'a [u8], position: &mut usize, width: usize) -> Option<(String, &'a [u8])> {
    let rest = data.get(*position..)?;
    let end = rest.iter().position(|&byte| byte == 0)?;
    let fields = rest.get(end + 1..end + 1 + width)?;
    *position += end + 1 + width;
    // Some dictionaries put markup in the words (`<p i=01>take`).
    let word = String::from_utf8_lossy(&rest[..end]);
    let word = if word.contains('>') {
        crate::dictionary::plain_text(&word)
    } else {
        word.trim().to_string()
    };
    Some((word, fields))
}

/// The (type, data) fields of one article. With `sametypesequence` the type bytes are left out
/// and the last field runs to the end; otherwise each field starts with its type. Lower-case
/// types are null-terminated text, upper-case ones are prefixed with a 32-bit size.
fn article_fields<'a>(data: &'a [u8], types: &[u8]) -> Vec<(u8, &'a [u8])> {
    let mut fields = Vec::new();
    let mut position = 0;
    let take = |kind: u8, last: bool, position: &mut usize| -> Option<&'a [u8]> {
        let rest = data.get(*position..)?;
        if last {
            *position = data.len();
            return Some(rest);
        }
        if kind.is_ascii_lowercase() {
            let end = rest
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(rest.len());
            *position += (end + 1).min(rest.len());
            Some(&rest[..end])
        } else {
            let size = be_uint(rest.get(..4)?) as usize;
            let field = rest.get(4..4 + size)?;
            *position += 4 + size;
            Some(field)
        }
    };
    if types.is_empty() {
        while position < data.len() {
            let kind = data[position];
            position += 1;
            match take(kind, false, &mut position) {
                Some(field) => fields.push((kind, field)),
                None => break,
            }
        }
    } else {
        for (index, &kind) in types.iter().enumerate() {
            match take(kind, index + 1 == types.len(), &mut position) {
                Some(field) => fields.push((kind, field)),
                None => break,
            }
        }
    }
    fields
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn newlines_to_breaks(text: &str) -> String {
    text.replace("\r\n", "\n").trim().replace('\n', "<br>")
}

fn tag_name(tag: &str) -> (bool, String) {
    let inner = tag.trim_start_matches('<').trim_end_matches('>');
    let closing = inner.starts_with('/');
    let name = inner
        .trim_start_matches('/')
        .split(|character: char| character.is_whitespace() || character == '/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    (closing, name)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        rest[1..].split(quote).next().map(str::to_string)
    } else {
        rest.split(|character: char| character.is_whitespace() || character == '>')
            .next()
            .map(str::to_string)
    }
}

/// XDXF markup to HTML: headwords bold, transcription in brackets, labels and examples in
/// italics, colors as `<font>`, `<kref>` as links to other articles. Unknown tags are left to
/// the sanitizer, which drops them and keeps their text.
fn xdxf_to_html(text: &str) -> String {
    let text = newlines_to_breaks(text);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..=end];
        rest = &rest[end + 1..];
        let (closing, name) = tag_name(tag);
        match (name.as_str(), closing) {
            ("k", false) => out.push_str("<b>"),
            ("k", true) => out.push_str("</b>"),
            ("abr" | "ex", false) => out.push_str("<i>"),
            ("abr" | "ex", true) => out.push_str("</i>"),
            ("tr", false) => out.push('['),
            ("tr", true) => out.push(']'),
            ("c", false) => out.push_str(&format!(
                "<font color=\"{}\">",
                escape_html(&attribute(tag, "c").unwrap_or_else(|| "green".to_string()))
                    .replace('"', "")
            )),
            ("c", true) => out.push_str("</font>"),
            ("kref", false) => {
                let end = rest.find("</kref>").unwrap_or(rest.len());
                let word = &rest[..end];
                let target = crate::dictionary::plain_text(word).replace('"', "&quot;");
                out.push_str(&format!("<a href=\"bword://{target}\">{word}</a>"));
                rest = rest.get(end + 7..).unwrap_or_default();
            }
            _ => out.push_str(tag),
        }
    }
    out.push_str(rest);
    out
}

fn article(headword: String, data: &[u8], types: &[u8]) -> Article {
    let mut html = Vec::new();
    let mut transcription = String::new();
    for (kind, field) in article_fields(data, types) {
        let text = String::from_utf8_lossy(field);
        match kind {
            b'm' | b'l' | b'y' | b'w' => html.push(newlines_to_breaks(&escape_html(&text))),
            b't' => transcription = text.trim().to_string(),
            b'g' => html.push(newlines_to_breaks(&text)),
            b'h' => html.push(text.trim().to_string()),
            b'x' => html.push(xdxf_to_html(&text)),
            b'k' => html.push(newlines_to_breaks(&escape_html(
                &crate::dictionary::plain_text(&text),
            ))),
            // Resource lists, sounds and pictures are not shown.
            _ => {}
        }
    }
    Article {
        headword,
        alternates: Vec::new(),
        part_of_speech: String::new(),
        transcription,
        html: html.join("<br>"),
    }
}

pub(crate) fn read(ifo: &Path) -> Result<ParsedDictionary, String> {
    let text = fs::read(ifo).map_err(|error| format!("Неуспешно прочитане: {error}"))?;
    let info = parse_ifo(&String::from_utf8_lossy(&text))?;
    let idx_path =
        find_component(ifo, &["idx", "idx.gz"]).ok_or("Липсва .idx файлът до .ifo файла.")?;
    let dict_path = find_component(ifo, &["dict", "dict.dz"])
        .ok_or("Липсва .dict или .dict.dz файлът до .ifo файла.")?;
    let index = read_maybe_compressed(&idx_path)?;
    let dict = read_maybe_compressed(&dict_path)?;

    let width = if info.offset_bits == 64 { 8 } else { 4 };
    let mut articles = Vec::new();
    let mut position = 0;
    while let Some((word, fields)) = next_word(&index, &mut position, width + 4) {
        let offset = be_uint(&fields[..width]) as usize;
        let size = be_uint(&fields[width..]) as usize;
        // Keep the idx numbering for the synonyms even when an entry is unusable.
        let data = dict
            .get(offset..offset.saturating_add(size))
            .unwrap_or_default();
        articles.push(article(word, data, &info.same_type_sequence));
    }
    if let Some(syn_path) = find_component(ifo, &["syn", "syn.dz"]) {
        let synonyms = read_maybe_compressed(&syn_path)?;
        let mut position = 0;
        while let Some((word, target)) = next_word(&synonyms, &mut position, 4) {
            if let Some(article) = articles.get_mut(be_uint(target) as usize) {
                if !word.is_empty() && word != article.headword {
                    article.alternates.push(word);
                }
            }
        }
    }
    articles.retain(|article| !article.headword.is_empty() && !article.html.is_empty());
    if articles.is_empty() {
        return Err("Речникът не съдържа статии.".to_string());
    }
    Ok(ParsedDictionary {
        title: info.title,
        articles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn idx_entry(word: &str, offset: u32, size: u32) -> Vec<u8> {
        let mut entry = word.as_bytes().to_vec();
        entry.push(0);
        entry.extend_from_slice(&offset.to_be_bytes());
        entry.extend_from_slice(&size.to_be_bytes());
        entry
    }

    #[test]
    fn reads_compressed_dictionary_with_synonyms() {
        let directory =
            std::env::temp_dir().join(format!("contextai-stardict-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let ifo = directory.join("sample.ifo");
        fs::write(
            &ifo,
            "StarDict's dict ifo file\nversion=2.4.2\nwordcount=2\nbookname=Проба\nsametypesequence=tm\n",
        )
        .unwrap();
        let first = "ˈæpl\0ябълка\nплод".as_bytes();
        let second = "\0крушa".as_bytes();
        let mut dict = first.to_vec();
        dict.extend_from_slice(second);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&dict).unwrap();
        fs::write(directory.join("sample.dict.dz"), encoder.finish().unwrap()).unwrap();
        let mut idx = idx_entry("apple", 0, first.len() as u32);
        idx.extend(idx_entry("pear", first.len() as u32, second.len() as u32));
        fs::write(directory.join("sample.idx"), idx).unwrap();
        let mut syn = b"apples\0".to_vec();
        syn.extend_from_slice(&0_u32.to_be_bytes());
        fs::write(directory.join("sample.syn"), syn).unwrap();

        let dictionary = read(&ifo).unwrap();
        assert_eq!(dictionary.title, "Проба");
        assert_eq!(dictionary.articles.len(), 2);
        assert_eq!(dictionary.articles[0].transcription, "ˈæpl");
        assert_eq!(dictionary.articles[0].html, "ябълка<br>плод");
        assert_eq!(dictionary.articles[0].alternates, vec!["apples"]);
        assert_eq!(dictionary.articles[1].html, "крушa");
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn typed_fields_without_same_type_sequence() {
        let mut data = b"mtext\0".to_vec();
        data.push(b'W');
        data.extend_from_slice(&3_u32.to_be_bytes());
        data.extend_from_slice(b"wav");
        data.extend_from_slice(b"h<b>x</b>\0");
        let fields = article_fields(&data, b"");
        assert_eq!(
            fields,
            vec![
                (b'm', &b"text"[..]),
                (b'W', &b"wav"[..]),
                (b'h', &b"<b>x</b>"[..])
            ]
        );
    }

    #[test]
    fn xdxf_becomes_html() {
        let html =
            xdxf_to_html("<k>cat</k>\n<tr>kæt</tr> <abr>n</abr> котка, see <kref>kitten</kref>");
        assert_eq!(
            html,
            "<b>cat</b><br>[kæt] <i>n</i> котка, see <a href=\"bword://kitten\">kitten</a>"
        );
    }
}
