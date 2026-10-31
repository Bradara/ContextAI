import { translationTargets } from "./languages";

export type ProviderProfile = {
  id: string;
  enabled: boolean;
  endpoint: string;
  model: string;
  region: string;
  voice: string;
};
export type ProviderSettings = {
  activeChatProvider: string;
  activeTranslationProvider?: string;
  translationTargetLanguage?: string;
  providers: ProviderProfile[];
};
export type TranslationResult = { text: string; detectedLanguage: string; targetLanguage: string };
export type StreamPayload = { request_id: string; content: string };
export type Action = "translate" | "explain" | "summarize" | "edit" | "grammar" | "ask";

export const translationServiceNames: Record<string, string> = {
  deepl: "DeepL",
  "azure-translator": "Azure Translator",
  "google-translate": "Google Translation",
};

export function isTranslationService(id: string | undefined): id is string {
  return id !== undefined && id in translationServiceNames;
}

export function translateInstruction(language: string | undefined) {
  const { target, fallback } = translationTargets(language);
  return (
    `Преведи цитирания текст на ${target.name} език. ` +
    `Ако вече е на ${target.name}, преведи го на ${fallback.name}. Върни само превода.`
  );
}

export const instructions: Record<Exclude<Action, "translate">, string> = {
  explain: "Обясни цитирания текст ясно и кратко на български.",
  summarize: "Обобщи цитирания текст на български в основните точки.",
  edit:
    "Редактирай цитирания текст за яснота и коректност. Запази езика му. " +
    "Върни редактирания текст между <result> и </result>, без нищо друго вътре, " +
    "а след това накратко на български какво си променил.",
  grammar:
    "Провери цитирания текст за граматични, правописни и пунктуационни грешки, без да променяш стила и смисъла му. " +
    "Ако текстът е правилен, отговори само „Текстът е граматично правилен.“ " +
    "Иначе изброй всяка грешка с кратко обяснение на български и накрая дай поправения текст на неговия оригинален език " +
    "между <result> и </result>, без нищо друго вътре.",
  ask: "Отговори на въпроса ми, като използваш цитирания текст като контекст.",
};

const RESULT_BLOCK = /<result>([\s\S]*?)<\/result>/;

/// Text that can replace the selection in the source app: the <result> block from "Граматика" and
/// "Редактирай", or the whole first answer of a translation. Empty when there is nothing to replace.
export function replacementText(content: string, action: Action | null, isFirstAnswer: boolean) {
  const tagged = RESULT_BLOCK.exec(content)?.[1].trim();
  if (tagged) return tagged;
  return action === "translate" && isFirstAnswer ? content.trim() : "";
}

/// The answer without the <result> markers, which are only for `replacementText`.
export function displayText(content: string) {
  return content.replace(/<\/?result>/g, "");
}

export function systemPrompt(selection: string) {
  return `You are a helpful desktop assistant. Reply in Bulgarian unless asked otherwise. Selected text:\n${selection}`;
}

export const chatProviderNames: Record<string, string> = {
  ollama: "Ollama",
  "llama-cpp": "llama.cpp",
  "github-copilot": "GitHub Copilot",
  gemini: "Google Gemini",
};

/// Cloud providers; the others run on the user's machine or network.
export const remoteChatProviders = ["github-copilot", "gemini"];

/// The chat provider and model from the settings; Ollama unless another provider is selected and enabled.
/// An empty Copilot model lets Copilot choose.
export function resolveChatProvider(settings: ProviderSettings) {
  const selected = settings.activeChatProvider || "ollama";
  const profile = settings.providers.find((provider) => provider.id === selected && provider.enabled);
  const providerId = selected !== "ollama" && selected in chatProviderNames && profile ? selected : "ollama";
  const model = profile?.model.trim() || (providerId === "ollama" ? "qwen2.5:7b" : "");
  return { providerId, model };
}
