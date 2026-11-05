//! Language codes (ISO 639-1, as the translation services use them), detection and the script a
//! language is written in.

use whatlang::{Lang, Script};

/// Babylon's language numbers in the .bgl header (`sourceLang` 0x07, `targetLang` 0x08).
/// Checked against real files: 0 English, 3 Spanish, 7 Russian, 42 Bulgarian. 17–28 and 58 are
/// "other …" groups without one language.
const BABYLON_LANGUAGES: [&str; 62] = [
    "en", "fr", "it", "es", "nl", "pt", "de", "ru", "ja", "zh", "zh", "el", "ko", "tr", "he", "ar",
    "th", "", "", "", "", "", "", "", "", "", "", "", "", "pl", "hu", "cs", "lt", "lv", "ca", "hr",
    "sr", "sk", "sq", "ur", "sl", "et", "bg", "da", "fi", "is", "no", "ro", "sv", "uk", "be", "fa",
    "eu", "mk", "af", "fo", "la", "eo", "", "hy", "hi", "so",
];

pub(crate) fn babylon_language(code: u64) -> &'static str {
    usize::try_from(code)
        .ok()
        .and_then(|index| BABYLON_LANGUAGES.get(index))
        .copied()
        .unwrap_or("")
}

fn iso_code(lang: Lang) -> &'static str {
    match lang {
        Lang::Eng => "en",
        Lang::Bul => "bg",
        Lang::Rus => "ru",
        Lang::Ukr => "uk",
        Lang::Bel => "be",
        Lang::Srp => "sr",
        Lang::Mkd => "mk",
        Lang::Deu => "de",
        Lang::Fra => "fr",
        Lang::Spa => "es",
        Lang::Ita => "it",
        Lang::Por => "pt",
        Lang::Nld => "nl",
        Lang::Pol => "pl",
        Lang::Ces => "cs",
        Lang::Slk => "sk",
        Lang::Slv => "sl",
        Lang::Hrv => "hr",
        Lang::Ron => "ro",
        Lang::Hun => "hu",
        Lang::Ell => "el",
        Lang::Tur => "tr",
        Lang::Swe => "sv",
        Lang::Dan => "da",
        Lang::Nob => "no",
        Lang::Fin => "fi",
        Lang::Est => "et",
        Lang::Lit => "lt",
        Lang::Lav => "lv",
        Lang::Lat => "la",
        Lang::Epo => "eo",
        Lang::Cat => "ca",
        Lang::Ara => "ar",
        Lang::Heb => "he",
        Lang::Pes => "fa",
        Lang::Hin => "hi",
        Lang::Jpn => "ja",
        Lang::Cmn => "zh",
        Lang::Kor => "ko",
        Lang::Tha => "th",
        Lang::Afr => "af",
        Lang::Hye => "hy",
        other => other.code(),
    }
}

/// The language of a text, when whatlang is reasonably sure (needs a few words).
pub fn detect(text: &str) -> Option<&'static str> {
    let info = whatlang::detect(text)?;
    (info.is_reliable() || info.confidence() > 0.5).then(|| iso_code(info.lang()))
}

/// Like `detect`, but takes whatlang's best guess even when it is unsure. For long samples of
/// headwords, whose single words look unreliable one by one.
pub(crate) fn detect_loose(text: &str) -> Option<&'static str> {
    whatlang::detect(text).map(|info| iso_code(info.lang()))
}

/// Script families that tell the two sides of a dictionary apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Latin,
    Cyrillic,
    Greek,
    Arabic,
    Hebrew,
    Cjk,
    Hangul,
    Other,
}

fn family_of_language(code: &str) -> Family {
    match code {
        "bg" | "ru" | "uk" | "be" | "sr" | "mk" | "kk" => Family::Cyrillic,
        "el" => Family::Greek,
        "ar" | "fa" | "ur" => Family::Arabic,
        "he" => Family::Hebrew,
        "zh" | "ja" => Family::Cjk,
        "ko" => Family::Hangul,
        "th" | "hy" | "hi" => Family::Other,
        _ => Family::Latin,
    }
}

fn family_of_text(text: &str) -> Option<Family> {
    Some(match whatlang::detect_script(text)? {
        Script::Latin => Family::Latin,
        Script::Cyrillic => Family::Cyrillic,
        Script::Greek => Family::Greek,
        Script::Arabic => Family::Arabic,
        Script::Hebrew => Family::Hebrew,
        Script::Mandarin | Script::Hiragana | Script::Katakana => Family::Cjk,
        Script::Hangul => Family::Hangul,
        _ => Family::Other,
    })
}

/// Whether `text` is written in the script of `language` (e.g. Cyrillic for "bg").
pub(crate) fn written_in_script_of(text: &str, language: &str) -> bool {
    family_of_text(text) == Some(family_of_language(language))
}

/// The languages of a word and of its meaning, given the dictionary's (or translation's) pair.
/// A word written in the meaning's script swaps the pair, so a two-way dictionary works both
/// ways; a word in neither script, or a pair marked as one language, falls back to detection.
pub fn term_languages(term: &str, source: &str, target: &str) -> (String, String) {
    let detected = || detect(term).unwrap_or("").to_string();
    let Some(script) = family_of_text(term) else {
        return (source.to_string(), target.to_string());
    };
    if source.is_empty() {
        let term_language = detected();
        return (term_language, target.to_string());
    }
    let (source_family, target_family) = (family_of_language(source), family_of_language(target));
    if script == source_family && (source != target || target.is_empty()) {
        (source.to_string(), target.to_string())
    } else if script == target_family && script != source_family {
        (target.to_string(), source.to_string())
    } else if script == source_family {
        // A one-language label on a two-way dictionary: the meaning is in the other language.
        (source.to_string(), target.to_string())
    } else {
        (detected(), target.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn babylon_codes() {
        assert_eq!(babylon_language(0), "en");
        assert_eq!(babylon_language(3), "es");
        assert_eq!(babylon_language(7), "ru");
        assert_eq!(babylon_language(42), "bg");
        assert_eq!(babylon_language(20), "");
        assert_eq!(babylon_language(999), "");
    }

    #[test]
    fn a_word_in_the_meanings_script_swaps_the_pair() {
        let pair = |term, source, target| term_languages(term, source, target);
        assert_eq!(pair("take", "en", "bg"), ("en".into(), "bg".into()));
        assert_eq!(pair("ябълка", "en", "bg"), ("bg".into(), "en".into()));
        assert_eq!(pair("дом", "ru", "bg"), ("ru".into(), "bg".into()));
        assert_eq!(pair("casa", "es", "bg"), ("es".into(), "bg".into()));
    }

    #[test]
    fn detects_longer_texts() {
        assert_eq!(
            detect("Това е изречение на български език за проверка."),
            Some("bg")
        );
        assert_eq!(
            detect("This is a sentence written in plain English."),
            Some("en")
        );
    }
}
