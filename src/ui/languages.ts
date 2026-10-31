// Must match TRANSLATION_LANGUAGES in src-tauri/src/lib.rs.
export const translationLanguages: { code: string; name: string }[] = [
  { code: "bg", name: "български" },
  { code: "en", name: "английски" },
  { code: "de", name: "немски" },
  { code: "fr", name: "френски" },
  { code: "es", name: "испански" },
  { code: "it", name: "италиански" },
  { code: "pt", name: "португалски" },
  { code: "nl", name: "нидерландски" },
  { code: "ru", name: "руски" },
  { code: "uk", name: "украински" },
  { code: "pl", name: "полски" },
  { code: "cs", name: "чешки" },
  { code: "ro", name: "румънски" },
  { code: "el", name: "гръцки" },
  { code: "tr", name: "турски" },
];

/// Target language and the one used when the text is already in the target language.
export function translationTargets(code: string | undefined) {
  const target = translationLanguages.find((language) => language.code === code) ?? translationLanguages[0];
  const fallback = translationLanguages.find((language) => language.code === (target.code === "en" ? "bg" : "en"))!;
  return { target, fallback };
}

// Must match DEFAULT_EXTRA_VOICES in src-tauri/src/lib.rs.
export const defaultExtraVoices = [
  "en-US-AvaNeural",
  "de-DE-KatjaNeural",
  "fr-FR-DeniseNeural",
  "es-ES-ElviraNeural",
  "it-IT-ElsaNeural",
  "ru-RU-SvetlanaNeural",
  "uk-UA-PolinaNeural",
  "el-GR-AthinaNeural",
  "tr-TR-EmelNeural",
].join("\n");
