//! The cache file per dictionary (UTF-8 articles + a sorted key index) and safe display HTML.

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use super::bgl::{decode_entities, normalize, ByteReader};
use super::{component_files, DictionaryEntry, ParsedDictionary};

/// Longer keys are not indexed; they are sentences rather than words.
const MAX_KEY_BYTES: usize = 250;
const CACHE_MAGIC: &[u8; 8] = b"CTXDIC02";

// ---------------------------------------------------------------------------------------------
// Cache file
//
// magic (8) | index offset u64 | source length u64 | source mtime u64 | article count u32 |
// title, source language, target language (each u32 length + UTF-8) | articles (4 × length-prefixed strings) |
// index: key count u32, then per key: u16 length, key, u64 article offset (sorted by key)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct SourceStamp {
    pub(crate) length: u64,
    pub(crate) modified: u64,
}

pub(crate) fn source_stamp(path: &Path) -> Result<SourceStamp, String> {
    // Fails only when the file that was added (.bgl / .ifo) is gone.
    fs::metadata(path).map_err(|_| "Файлът не е намерен.".to_string())?;
    let mut stamp = SourceStamp {
        length: 0,
        modified: 0,
    };
    for file in component_files(path) {
        let Ok(metadata) = fs::metadata(&file) else {
            continue;
        };
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        stamp.length = stamp.length.wrapping_add(metadata.len());
        stamp.modified = stamp.modified.max(modified);
    }
    Ok(stamp)
}

pub(crate) fn cache_file(cache_dir: &Path, source: &Path) -> PathBuf {
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

pub(crate) fn write_string(writer: &mut impl Write, text: &str) -> io::Result<u64> {
    writer.write_all(&(text.len() as u32).to_le_bytes())?;
    writer.write_all(text.as_bytes())?;
    Ok(4 + text.len() as u64)
}

pub(crate) fn write_cache(
    path: &Path,
    stamp: SourceStamp,
    dictionary: &ParsedDictionary,
) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut writer = BufWriter::new(File::create(&temporary)?);
    writer.write_all(CACHE_MAGIC)?;
    writer.write_all(&0_u64.to_le_bytes())?;
    writer.write_all(&stamp.length.to_le_bytes())?;
    writer.write_all(&stamp.modified.to_le_bytes())?;
    writer.write_all(&(dictionary.articles.len() as u32).to_le_bytes())?;
    let mut position = 36;
    for text in [
        &dictionary.title,
        &dictionary.source_lang,
        &dictionary.target_lang,
    ] {
        position += write_string(&mut writer, text)?;
    }

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

pub(crate) struct CacheHeader {
    pub(crate) index_offset: u64,
    pub(crate) stamp: SourceStamp,
    pub(crate) articles: u32,
    pub(crate) title: String,
    pub(crate) source_lang: String,
    pub(crate) target_lang: String,
}

pub(crate) fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

pub(crate) fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub(crate) fn read_string(reader: &mut impl Read) -> io::Result<String> {
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

pub(crate) fn read_header(reader: &mut impl Read) -> io::Result<CacheHeader> {
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
    let source_lang = read_string(reader)?;
    let target_lang = read_string(reader)?;
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
        source_lang,
        target_lang,
    })
}

/// The header of a cache that still matches its source file.
pub(crate) fn valid_header(cache: &Path, stamp: SourceStamp) -> Option<CacheHeader> {
    let mut file = BufReader::new(File::open(cache).ok()?);
    read_header(&mut file)
        .ok()
        .filter(|header| header.stamp == stamp)
}

pub(crate) struct DictionaryIndex {
    pub(crate) cache: PathBuf,
    pub(crate) stamp: SourceStamp,
    pub(crate) title: String,
    pub(crate) source_lang: String,
    pub(crate) target_lang: String,
    /// All keys concatenated; `key_ends[i]` is where key `i` ends.
    pub(crate) keys: String,
    pub(crate) key_ends: Vec<u32>,
    pub(crate) targets: Vec<u64>,
}

impl DictionaryIndex {
    pub(crate) fn open(cache: &Path) -> io::Result<Self> {
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
            source_lang: header.source_lang,
            target_lang: header.target_lang,
            keys,
            key_ends,
            targets,
        })
    }

    pub(crate) fn key(&self, index: usize) -> &str {
        let start = if index == 0 {
            0
        } else {
            self.key_ends[index - 1] as usize
        };
        &self.keys[start..self.key_ends[index] as usize]
    }

    /// The first key that is not less than `needle`.
    pub(crate) fn lower_bound(&self, needle: &str) -> usize {
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

    pub(crate) fn exact(&self, key: &str) -> Vec<u64> {
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

    pub(crate) fn with_prefix(&self, prefix: &str, limit: usize) -> Vec<&str> {
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

    pub(crate) fn read_entries(&self, offsets: &[u64]) -> io::Result<Vec<DictionaryEntry>> {
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

pub(crate) fn percent_encode(text: &str) -> String {
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
pub(crate) fn rewrite_word_links(html: &str) -> String {
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

pub(crate) fn sanitize_definition(html: &str) -> String {
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
