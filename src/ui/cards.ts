import { translationLanguages } from "./languages";

export type Card = {
  id: string;
  term: string;
  language: string;
  meaningLang: string;
  meaningHtml: string;
  transcription: string;
  context: string;
  source: string;
  tags: string[];
  createdAt: number;
  updatedAt: number;
};

export type SavedCard = { card: Card; created: boolean };

/// What `card_save` expects; the languages are the dictionary's or the translation's pair.
export type NewCard = {
  term: string;
  sourceLang: string;
  targetLang: string;
  meaning: string;
  meaningIsText?: boolean;
  transcription?: string;
  context?: string;
  source?: string;
};

/// Up to three words are saved as a word to learn; longer selections are sentences.
export function isWord(text: string) {
  const trimmed = text.trim();
  return trimmed.length > 0 && trimmed.length <= 60 && trimmed.split(/\s+/).length <= 3;
}

const otherLanguages: Record<string, string> = {
  sr: "сръбски",
  mk: "македонски",
  hr: "хърватски",
  sk: "словашки",
  sl: "словенски",
  hu: "унгарски",
  sv: "шведски",
  da: "датски",
  no: "норвежки",
  fi: "фински",
  la: "латински",
  eo: "есперанто",
  ja: "японски",
  zh: "китайски",
  ko: "корейски",
  ar: "арабски",
  he: "иврит",
};

export function languageName(code: string) {
  return translationLanguages.find((language) => language.code === code)?.name ?? otherLanguages[code] ?? code;
}

/// "EN → BG", or "" when neither language is known.
export function languagePair(source: string, target: string) {
  if (!source && !target) return "";
  return `${(source || "?").toUpperCase()} → ${(target || "?").toUpperCase()}`;
}

/// Sanitized article HTML as editable text: line breaks kept, markup dropped.
export function htmlToText(html: string) {
  const marked = html.replace(/<br\s*\/?>/gi, "\n").replace(/<\/(p|div|li|tr)>/gi, "\n");
  const text = new DOMParser().parseFromString(marked, "text/html").body.textContent ?? "";
  return text.replace(/[ \t]+\n/g, "\n").replace(/\n{3,}/g, "\n\n").trim();
}
