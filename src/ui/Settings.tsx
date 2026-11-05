import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import CopilotSettings from "./CopilotSettings";
import DictionarySettings from "./DictionarySettings";
import HistorySettings from "./HistorySettings";
import WordsSettings from "./WordsSettings";
import OtherSettings from "./OtherSettings";
import { defaultExtraVoices, translationLanguages } from "./languages";

type ProviderProfile = {
  id: string;
  enabled: boolean;
  endpoint: string;
  model: string;
  region: string;
  voice: string;
  voices?: string;
  thinking?: boolean;
};

type ProviderSettings = {
  activeChatProvider: string;
  activeTranslationProvider?: string;
  translationTargetLanguage?: string;
  providers: ProviderProfile[];
};

type ProviderDefinition = {
  id: string;
  name: string;
  category: "Локален модел" | "AI услуга" | "Превод" | "Глас";
  description: string;
  endpoint: string;
  model?: string;
  region?: string;
  voice?: string;
  voices?: string;
  needsSecret: boolean;
  secretLabel?: string;
};

const definitions: ProviderDefinition[] = [
  {
    id: "llama-cpp",
    name: "llama.cpp",
    category: "Локален модел",
    description: "OpenAI-съвместим сървър. Използвайте Tailscale URL за отдалечен сървър.",
    endpoint: "http://127.0.0.1:8080/v1",
    model: "local-model",
    needsSecret: true,
    secretLabel: "API key (ако сървърът изисква)",
  },
  {
    id: "ollama",
    name: "Ollama",
    category: "Локален модел",
    description: "Локален OpenAI-съвместим endpoint.",
    endpoint: "http://127.0.0.1:11434/v1",
    model: "qwen2.5:7b",
    needsSecret: false,
  },
  {
    id: "deepseek",
    name: "DeepSeek",
    category: "AI услуга",
    description: "DeepSeek Chat API.",
    endpoint: "https://api.deepseek.com",
    model: "deepseek-chat",
    needsSecret: true,
    secretLabel: "DeepSeek API key",
  },
  {
    id: "qwen",
    name: "Qwen / DashScope",
    category: "AI услуга",
    description: "OpenAI-съвместим DashScope API. Endpoint-ът зависи от региона.",
    endpoint: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
    model: "qwen-plus",
    needsSecret: true,
    secretLabel: "DashScope API key",
  },
  {
    id: "github-copilot",
    name: "GitHub Copilot",
    category: "AI услуга",
    description:
      "Официален GitHub Copilot SDK чрез инсталирания Copilot CLI. Изисква абонамент за Copilot; входът е през браузъра, без пароли или токени тук.",
    endpoint: "",
    needsSecret: false,
  },
  {
    id: "gemini",
    name: "Google Gemini",
    category: "AI услуга",
    description:
      "Gemini API с ключ от Google AI Studio (aistudio.google.com/apikey) през OpenAI-съвместимия endpoint.",
    endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
    model: "gemini-flash-latest",
    needsSecret: true,
    secretLabel: "Gemini API key",
  },
  {
    id: "deepl",
    name: "DeepL Translate",
    category: "Превод",
    description:
      "DeepL API Free endpoint; сменете го на Pro endpoint при Pro абонамент. И безплатният план изисква API ключ (deepl.com/pro-api).",
    endpoint: "https://api-free.deepl.com/v2",
    needsSecret: true,
    secretLabel: "DeepL API key",
  },
  {
    id: "azure-translator",
    name: "Azure AI Translator",
    category: "Превод",
    description: "Задайте ключа, региона и custom endpoint за вашия Azure ресурс.",
    endpoint: "https://api.cognitive.microsofttranslator.com",
    needsSecret: true,
    secretLabel: "Azure Translator key",
  },
  {
    id: "google-translate",
    name: "Google Cloud Translation",
    category: "Превод",
    description: "Cloud Translation API (Basic, v2) с API ключ от Google Cloud Console.",
    endpoint: "https://translation.googleapis.com/language/translate/v2",
    needsSecret: true,
    secretLabel: "Google Cloud API key",
  },
  {
    id: "azure-tts",
    name: "Azure Speech TTS",
    category: "Глас",
    description: "Синтез на реч с Azure Speech; използва се endpoint-ът по подразбиране за региона.",
    endpoint: "",
    region: "",
    voice: "bg-BG-KalinaNeural",
    voices: defaultExtraVoices,
    needsSecret: true,
    secretLabel: "Azure Speech key",
  },
];

const defaults: ProviderProfile[] = definitions.map((provider) => ({
  id: provider.id,
  enabled: false,
  endpoint: provider.endpoint,
  model: provider.model ?? "",
  region: provider.region ?? "",
  voice: provider.voice ?? "",
  ...(provider.voices !== undefined && { voices: provider.voices }),
}));

const translationServices = ["deepl", "azure-translator", "google-translate"];
const chatProviders = ["llama-cpp", "github-copilot", "gemini"];

export type Tab = "ai" | "translation" | "voice" | "dictionaries" | "history" | "words" | "other";

const tabs: { id: Tab; label: string; description: string; groups: ProviderDefinition["category"][] }[] = [
  { id: "ai", label: "ИИ", description: "Модели за чата и действията с ИИ.", groups: ["Локален модел", "AI услуга"] },
  { id: "translation", label: "Превод", description: "Услуги и език за превод.", groups: ["Превод"] },
  { id: "voice", label: "Глас", description: "Четене на глас (TTS).", groups: ["Глас"] },
  { id: "dictionaries", label: "Речници", description: "Речници за търсене и редът на резултатите.", groups: [] },
  { id: "history", label: "История", description: "Какво се записва и колко дълго се пази.", groups: [] },
  { id: "words", label: "Думи", description: "Упражненията със запазените думи.", groups: [] },
  { id: "other", label: "Други", description: "Системни настройки.", groups: [] },
];

const TAB_STORAGE_KEY = "contextai.settings.tab";

function initialTab(): Tab {
  try {
    const stored = localStorage.getItem(TAB_STORAGE_KEY);
    return tabs.some((tab) => tab.id === stored) ? (stored as Tab) : "ai";
  } catch {
    return "ai";
  }
}

export default function Settings({
  initialTab: requestedTab,
  backLabel = "Чат",
  onBack,
  onClose,
}: {
  initialTab?: Tab;
  backLabel?: string;
  onBack: () => void;
  onClose: () => void;
}) {
  const [providers, setProviders] = useState(defaults);
  const [activeChatProvider, setActiveChatProvider] = useState("ollama");
  const [activeTranslationProvider, setActiveTranslationProvider] = useState("ai");
  const [translationTargetLanguage, setTranslationTargetLanguage] = useState("bg");
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [savedSecrets, setSavedSecrets] = useState<Record<string, boolean>>({});
  const [busy, setBusy] = useState(true);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [testingSpeech, setTestingSpeech] = useState(false);
  const [tab, setTab] = useState<Tab>(() => requestedTab ?? initialTab());
  const currentTab = tabs.find((candidate) => candidate.id === tab)!;

  function selectTab(id: Tab) {
    setTab(id);
    try {
      localStorage.setItem(TAB_STORAGE_KEY, id);
    } catch {
      // The tab is only a convenience; ignore unavailable storage.
    }
  }

  const [geminiModels, setGeminiModels] = useState<{ id: string; name: string }[]>([]);
  const [loadingGeminiModels, setLoadingGeminiModels] = useState(false);

  async function refreshGeminiModels() {
    setError("");
    setNotice("");
    setLoadingGeminiModels(true);
    try {
      const models = await invoke<{ id: string; name: string }[]>("gemini_models");
      setGeminiModels(models);
      setNotice(`Заредени са ${models.length} Gemini модела.`);
    } catch (cause) {
      setError(`Неуспешно зареждане на Gemini моделите: ${String(cause)}`);
    } finally {
      setLoadingGeminiModels(false);
    }
  }

  useEffect(() => {
    let disposed = false;
    async function load() {
      try {
        const settings = await invoke<ProviderSettings>("load_provider_settings");
        if (disposed) return;
        const loaded = new Map(settings.providers.map((provider) => [provider.id, provider]));
        const merged = defaults.map((provider) => ({ ...provider, ...loaded.get(provider.id) }));
        setProviders(merged);
        const selected = settings.activeChatProvider || "ollama";
        setActiveChatProvider(
          chatProviders.includes(selected) && merged.some((provider) => provider.id === selected && provider.enabled)
            ? selected
            : "ollama",
        );
        setActiveTranslationProvider(settings.activeTranslationProvider || "ai");
        setTranslationTargetLanguage(settings.translationTargetLanguage || "bg");
        const statusEntries = await Promise.all(
          definitions
            .filter((definition) => definition.needsSecret)
            .map(async (definition) => [
              definition.id,
              await invoke<boolean>("provider_secret_is_saved", { providerId: definition.id }),
            ] as const),
        );
        if (!disposed) setSavedSecrets(Object.fromEntries(statusEntries));
      } catch (cause) {
        if (!disposed) setError(`Неуспешно зареждане на настройките: ${String(cause)}`);
      } finally {
        if (!disposed) setBusy(false);
      }
    }
    void load();
    return () => {
      disposed = true;
    };
  }, []);

  function updateProvider(id: string, patch: Partial<ProviderProfile>) {
    setProviders((current) =>
      current.map((provider) => (provider.id === id ? { ...provider, ...patch } : provider)),
    );
  }

  async function save() {
    setSaving(true);
    setError("");
    setNotice("");
    try {
      await invoke("save_provider_settings", {
        settings: { activeChatProvider, activeTranslationProvider, translationTargetLanguage, providers },
      });
      const persistedSettings = await invoke<ProviderSettings>("load_provider_settings");
      const persistedProviders = new Map(
        persistedSettings.providers.map((provider) => [provider.id, provider]),
      );
      setProviders(defaults.map((provider) => ({ ...provider, ...persistedProviders.get(provider.id) })));
      for (const definition of definitions) {
        const secret = secrets[definition.id];
        if (definition.needsSecret && secret?.trim()) {
          await invoke("save_provider_secret", {
            providerId: definition.id,
            secret: secret.trim(),
          });
        }
      }
      setSecrets({});
      const statusEntries = await Promise.all(
        definitions
          .filter((definition) => definition.needsSecret)
          .map(async (definition) => [
            definition.id,
            await invoke<boolean>("provider_secret_is_saved", { providerId: definition.id }),
          ] as const),
      );
      setSavedSecrets(Object.fromEntries(statusEntries));
      setNotice("Настройките са запазени. Ключовете се съхраняват отделно в Windows Credential Manager.");
    } catch (cause) {
      setError(`Неуспешно запазване: ${String(cause)}`);
    } finally {
      setSaving(false);
    }
  }

  async function removeSecret(id: string) {
    setError("");
    try {
      await invoke("save_provider_secret", { providerId: id, secret: "" });
      setSavedSecrets((current) => ({ ...current, [id]: false }));
      setSecrets((current) => ({ ...current, [id]: "" }));
      setNotice("API ключът е изтрит от Windows Credential Manager.");
    } catch (cause) {
      setError(`Неуспешно изтриване на ключа: ${String(cause)}`);
    }
  }

  async function testAzureSpeech() {
    setTestingSpeech(true);
    setError("");
    setNotice("");
    try {
      await invoke("start_reading", { text: "Това е проверка на Azure текст към реч." });
      setNotice("Проверката се изпълнява в малкия прозорец на четеца в долния десен ъгъл.");
    } catch (cause) {
      setError(`Проверката на Azure TTS не успя: ${String(cause)}`);
    } finally {
      setTestingSpeech(false);
    }
  }

  return (
    <main className="settings">
      <header className="settings-header" data-tauri-drag-region>
        <button className="back-button" onClick={onBack}>← {backLabel}</button>
        <div data-tauri-drag-region>
          <h1>Настройки</h1>
          <p>{currentTab.description}</p>
        </div>
        <button className="save-settings" disabled={busy || saving} onClick={() => void save()}>
          {saving ? "Запазване…" : "Запази"}
        </button>
        <button className="close settings-close" title="Затвори настройките" onClick={onClose}>
          ×
        </button>
      </header>

      <nav className="settings-tabs" role="tablist">
        {tabs.map((candidate) => (
          <button
            key={candidate.id}
            role="tab"
            aria-selected={candidate.id === tab}
            className={candidate.id === tab ? "active" : ""}
            onClick={() => selectTab(candidate.id)}
          >
            {candidate.label}
          </button>
        ))}
      </nav>

      {tab === "ai" && (
        <section className="active-provider">
          <label htmlFor="active-chat-provider">Доставчик за чата</label>
          <select
            id="active-chat-provider"
            value={activeChatProvider}
            onChange={(event) => setActiveChatProvider(event.target.value)}
          >
            <option value="ollama">Ollama · localhost:11434</option>
            {providers.some((provider) => provider.id === "llama-cpp" && provider.enabled) && (
              <option value="llama-cpp">llama.cpp · конфигуриран endpoint</option>
            )}
            {providers.some((provider) => provider.id === "github-copilot" && provider.enabled) && (
              <option value="github-copilot">GitHub Copilot</option>
            )}
            {providers.some((provider) => provider.id === "gemini" && provider.enabled) && (
              <option value="gemini">Google Gemini</option>
            )}
          </select>
        </section>
      )}

      {tab === "translation" && (
        <>
          <section className="active-provider">
            <label htmlFor="active-translation-provider">Услуга за превод</label>
            <select
              id="active-translation-provider"
              value={activeTranslationProvider}
              onChange={(event) => setActiveTranslationProvider(event.target.value)}
            >
              <option value="ai">ИИ · доставчикът за чата</option>
              {definitions
                .filter(
                  (definition) =>
                    translationServices.includes(definition.id) &&
                    (definition.id === activeTranslationProvider ||
                      providers.some((provider) => provider.id === definition.id && provider.enabled)),
                )
                .map((definition) => (
                  <option key={definition.id} value={definition.id}>
                    {definition.name}
                  </option>
                ))}
            </select>
          </section>

          <section className="active-provider">
            <label htmlFor="translation-target-language">Превод на</label>
            <select
              id="translation-target-language"
              value={translationTargetLanguage}
              onChange={(event) => setTranslationTargetLanguage(event.target.value)}
            >
              {translationLanguages.map((language) => (
                <option key={language.code} value={language.code}>
                  {language.name}
                </option>
              ))}
            </select>
          </section>
        </>
      )}

      {busy ? (
        <p className="settings-loading">Зареждане на настройките…</p>
      ) : (
        <div className="provider-groups">
          {tab === "dictionaries" && <DictionarySettings />}
          {tab === "history" && <HistorySettings />}
          {tab === "words" && <WordsSettings />}
          {tab === "other" && <OtherSettings />}
          {currentTab.groups.map((group) => (
            <section className="provider-group" key={group}>
              <h2>{group}</h2>
              {definitions.filter((provider) => provider.category === group).map((definition) => {
                const profile = providers.find((provider) => provider.id === definition.id)!;
                return (
                  <article className="provider-card" key={definition.id}>
                    <div className="provider-heading">
                      <div>
                        <h3>{definition.name}</h3>
                        <p>{definition.description}</p>
                      </div>
                      <label className="toggle">
                        <input
                          type="checkbox"
                          checked={profile.enabled}
                          onChange={(event) => updateProvider(definition.id, { enabled: event.target.checked })}
                        />
                        <span>Активен</span>
                      </label>
                    </div>

                    {definition.id !== "github-copilot" && definition.id !== "azure-tts" && (
                      <label className="setting-field">
                        <span>Endpoint</span>
                        <input
                          value={profile.endpoint}
                          onChange={(event) => updateProvider(definition.id, { endpoint: event.target.value })}
                          placeholder="https://…"
                          spellCheck={false}
                        />
                      </label>
                    )}

                    {definition.id === "github-copilot" && (
                      <CopilotSettings
                        profile={profile}
                        onChange={(patch) => updateProvider(definition.id, patch)}
                      />
                    )}

                    {definition.id !== "github-copilot" && (profile.model !== "" || definition.model !== undefined) ? (
                      <label className="setting-field">
                        <span>Модел</span>
                        <input
                          value={profile.model}
                          onChange={(event) => updateProvider(definition.id, { model: event.target.value })}
                          placeholder="Име на модел"
                          spellCheck={false}
                          list={definition.id === "gemini" ? "gemini-models" : undefined}
                        />
                        {definition.id === "gemini" && (
                          <>
                            <button
                              className="remove-secret refresh-models"
                              title="Изисква запазен ключ"
                              disabled={loadingGeminiModels}
                              onClick={() => void refreshGeminiModels()}
                            >
                              {loadingGeminiModels ? "Зареждане…" : "Обнови моделите"}
                            </button>
                            <datalist id="gemini-models">
                              {geminiModels.map((model) => (
                                <option key={model.id} value={model.id}>
                                  {model.name}
                                </option>
                              ))}
                            </datalist>
                          </>
                        )}
                      </label>
                    ) : null}

                    {definition.id === "gemini" && (
                      <label className="setting-field setting-check">
                        <input
                          type="checkbox"
                          checked={profile.thinking ?? true}
                          onChange={(event) => updateProvider(definition.id, { thinking: event.target.checked })}
                        />
                        <span>
                          Мислене (thinking) — по-задълбочени, но по-бавни отговори. Изключено: Gemini 2.5 Flash
                          не мисли изобщо, а по-новите модели мислят на ниско ниво.
                        </span>
                      </label>
                    )}

                    {(definition.id === "azure-translator" || definition.id === "azure-tts") && (
                      <label className="setting-field">
                        <span>Azure регион</span>
                        <input
                          type="text"
                          autoComplete="off"
                          value={profile.region}
                          onChange={(event) => updateProvider(definition.id, { region: event.target.value })}
                          placeholder="например westeurope"
                          spellCheck={false}
                        />
                      </label>
                    )}

                    {definition.id === "azure-tts" && (
                      <label className="setting-field">
                        <span>Глас по подразбиране</span>
                        <input
                          value={profile.voice}
                          onChange={(event) => updateProvider(definition.id, { voice: event.target.value })}
                          placeholder="bg-BG-KalinaNeural"
                        />
                      </label>
                    )}

                    {definition.id === "azure-tts" && (
                      <label className="setting-field">
                        <span>Гласове за други езици (по един на ред, езикът се разпознава сам)</span>
                        <textarea
                          rows={5}
                          value={profile.voices ?? defaultExtraVoices}
                          onChange={(event) => updateProvider(definition.id, { voices: event.target.value })}
                          placeholder="en-US-AvaNeural"
                          spellCheck={false}
                        />
                      </label>
                    )}

                    {definition.id === "azure-tts" && (
                      <button
                        className="test-speech"
                        disabled={busy || saving || testingSpeech}
                        onClick={() => void testAzureSpeech()}
                      >
                        {testingSpeech ? "Проверка…" : "Провери Azure TTS"}
                      </button>
                    )}

                    {definition.needsSecret && (
                      <label className="setting-field">
                        <span>{definition.secretLabel}</span>
                        <input
                          type="password"
                          autoComplete="new-password"
                          value={secrets[definition.id] ?? ""}
                          onChange={(event) =>
                            setSecrets((current) => ({ ...current, [definition.id]: event.target.value }))
                          }
                          placeholder={savedSecrets[definition.id] ? "Ключът е запазен; въведете нов за смяна" : "Въведете API ключ"}
                        />
                        {savedSecrets[definition.id] && (
                          <button className="remove-secret" onClick={() => void removeSecret(definition.id)}>
                            Изтрий запазения ключ
                          </button>
                        )}
                      </label>
                    )}
                  </article>
                );
              })}
            </section>
          ))}
        </div>
      )}

      {notice && <div className="settings-success">{notice}</div>}
      {error && <div className="error settings-error">{error}</div>}
    </main>
  );
}
