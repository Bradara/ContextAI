import { chatProviderNames, translationServiceNames } from "./ai";

export type HistoryKind = "translation" | "dictionary";

export type HistoryEntry = {
  id: string;
  kind: HistoryKind | string;
  query: string;
  result: string;
  provider: string;
  sourceLang: string;
  targetLang: string;
  createdAt: number;
  usedAt: number;
  pinned: boolean;
};

export type HistorySettings = { translations: boolean; dictionary: boolean; chats: boolean };

/// Who produced the entry: a translation service, the chat model ("ИИ · Ollama") or the dictionaries.
export function providerLabel(entry: HistoryEntry) {
  if (entry.kind !== "translation") return entry.provider;
  if (entry.provider in translationServiceNames) return translationServiceNames[entry.provider];
  if (entry.provider in chatProviderNames) return `ИИ · ${chatProviderNames[entry.provider]}`;
  return entry.provider;
}

/// "Днес", "Вчера" or the date, for grouping the list by day.
export function dayLabel(time: number, now = new Date()) {
  const day = new Date(time);
  const startOf = (date: Date) => new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const days = Math.round((startOf(now) - startOf(day)) / 86_400_000);
  if (days === 0) return "Днес";
  if (days === 1) return "Вчера";
  return day.toLocaleDateString("bg-BG", {
    day: "numeric",
    month: "long",
    ...(day.getFullYear() !== now.getFullYear() && { year: "numeric" }),
  });
}
