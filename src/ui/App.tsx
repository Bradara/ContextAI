import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import Dictionary, { type DictionaryRequest } from "./Dictionary";
import History from "./History";
import type { HistoryEntry } from "./historyTypes";
import Settings from "./Settings";
import ViewTabs, { type MainView } from "./ViewTabs";
import Words from "./Words";
import { type NewCard, isWord } from "./cards";
import {
  type Action,
  type ProviderSettings,
  type StreamPayload,
  type TranslationResult,
  chatProviderNames,
  displayText,
  instructions,
  remoteChatProviders,
  isTranslationService,
  replacementText,
  resolveChatProvider,
  systemPrompt,
  translateInstruction,
  translationServiceNames,
} from "./ai";

type Message = { role: "user" | "assistant"; content: string };
type ChatStart = { text: string; action: Action; service?: string };
type View = MainView | "settings";

export default function App() {
  const [selection, setSelection] = useState("");
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [model, setModel] = useState("qwen2.5:7b");
  const [chatProvider, setChatProvider] = useState("ollama");
  const [streaming, setStreaming] = useState(false);
  const [error, setError] = useState("");
  const [view, setView] = useState<View>("chat");
  // Settings opened from the dictionary go back there and start on its tab.
  const [settingsFrom, setSettingsFrom] = useState<View>("chat");
  const [dictionaryRequest, setDictionaryRequest] = useState<DictionaryRequest>({ text: "", id: 0 });
  // The popup action that started the chat; decides whether an answer can replace the selection.
  const [action, setAction] = useState<Action | null>(null);
  const actionRef = useRef<Action | null>(null);
  // Target language of a translation by the chat model, for its history entry.
  const translationTargetRef = useRef("");
  // The translation shown in the chat, so that a translated word can be saved to learn.
  const [translation, setTranslation] = useState<{ source: string; target: string; provider: string } | null>(null);
  const [wordSaved, setWordSaved] = useState(false);
  const [copiedIndex, setCopiedIndex] = useState<number | null>(null);
  const streamingRef = useRef(false);
  const requestId = useRef("");
  const streamBuffer = useRef("");
  const chatHistory = useRef<Message[]>([]);
  const selectionRef = useRef("");
  const modelRef = useRef(model);
  const chatProviderRef = useRef(chatProvider);
  modelRef.current = model;
  chatProviderRef.current = chatProvider;

  async function refreshChatProvider() {
    const settings = await invoke<ProviderSettings>("load_provider_settings");
    const { providerId, model: selectedModel } = resolveChatProvider(settings);
    setChatProvider(providerId);
    chatProviderRef.current = providerId;
    setModel(selectedModel);
    modelRef.current = selectedModel;
    return settings;
  }

  async function translateWithService(serviceId: string, text: string) {
    const request: Message = {
      role: "user",
      content: `Преведи избрания текст (${translationServiceNames[serviceId]}).`,
    };
    chatHistory.current = [request];
    setMessages(chatHistory.current);
    streamingRef.current = true;
    setStreaming(true);
    streamBuffer.current = "";
    const id = crypto.randomUUID();
    requestId.current = id;
    try {
      const result = await invoke<TranslationResult>("translate_text", { providerId: serviceId, text });
      if (requestId.current !== id) return;
      setTranslation({ source: result.detectedLanguage, target: result.targetLanguage, provider: serviceId });
      chatHistory.current = [...chatHistory.current, { role: "assistant", content: result.text }];
      setMessages(chatHistory.current);
    } catch (cause) {
      if (requestId.current === id) setError(String(cause));
    } finally {
      if (requestId.current === id) {
        streamingRef.current = false;
        setStreaming(false);
      }
    }
  }

  useEffect(() => {
    const unlisteners: (() => void)[] = [];
    let disposed = false;
    const keepOrRemove = (unlisten: () => void) => {
      if (disposed) unlisten();
      else unlisteners.push(unlisten);
    };
    const subscribe = async () => {
      void refreshChatProvider().catch((cause) => {
        setError(`Неуспешно зареждане на доставчика: ${String(cause)}`);
      });
      keepOrRemove(
        await listen("settings-open", () => {
          setSettingsFrom("chat");
          setView("settings");
        }),
      );
      keepOrRemove(
        await listen<{ text: string }>("dictionary-open", ({ payload }) => {
          // An empty text (tray menu) keeps the last search.
          const text = payload.text.trim();
          if (text) setDictionaryRequest((current) => ({ text, id: current.id + 1 }));
          setView("dictionary");
        }),
      );
      keepOrRemove(
        await listen<ChatStart>("chat-start", ({ payload }) => {
          setView("chat");
          setSelection(payload.text);
          selectionRef.current = payload.text;
          setAction(payload.action);
          actionRef.current = payload.action;
          setTranslation(null);
          setWordSaved(false);
          setCopiedIndex(null);
          setMessages([]);
          chatHistory.current = [];
          setInput("");
          setError("");
          streamingRef.current = false;
          void refreshChatProvider()
            .then((settings) => {
              // The popup can pick a service explicitly; "ai" forces the chat model.
              const service = payload.service ?? settings.activeTranslationProvider;
              if (payload.action === "translate" && isTranslationService(service)) {
                return translateWithService(service, payload.text);
              }
              translationTargetRef.current = settings.translationTargetLanguage ?? "";
              if (payload.action === "translate") {
                setTranslation({ source: "", target: translationTargetRef.current, provider: chatProviderRef.current });
              }
              const instruction =
                payload.action === "translate"
                  ? translateInstruction(settings.translationTargetLanguage)
                  : instructions[payload.action];
              return sendMessage(`${instruction}\n\nТекст:\n${payload.text}`);
            })
            .catch((cause) => {
              setError(`Неуспешно зареждане на доставчика: ${String(cause)}`);
            });
        }),
      );
      keepOrRemove(
        await listen<StreamPayload>("chat-delta", ({ payload }) => {
          if (payload.request_id !== requestId.current) return;
          streamBuffer.current += payload.content;
          setMessages([...chatHistory.current, { role: "assistant", content: streamBuffer.current }]);
        }),
      );
      keepOrRemove(
        await listen<StreamPayload>("chat-finished", ({ payload }) => {
          if (payload.request_id !== requestId.current) return;
          chatHistory.current = [
            ...chatHistory.current,
            { role: "assistant", content: streamBuffer.current },
          ];
          // The first answer of "Преведи" with the chat model is a translation for the history.
          if (actionRef.current === "translate" && chatHistory.current.length === 2 && streamBuffer.current.trim()) {
            void invoke("history_add", {
              entry: {
                kind: "translation",
                query: selectionRef.current,
                result: streamBuffer.current.trim(),
                provider: chatProviderRef.current,
                targetLang: translationTargetRef.current,
              },
            });
          }
          setMessages(chatHistory.current);
          streamingRef.current = false;
          setStreaming(false);
        }),
      );
    };
    void subscribe();
    return () => {
      disposed = true;
      for (const unlisten of unlisteners) unlisten();
    };
  }, []);

  async function sendMessage(text: string) {
    const content = text.trim();
    if (!content || streamingRef.current) return;
    const userMessage: Message = { role: "user", content };
    const history = [...chatHistory.current, userMessage];
    chatHistory.current = history;
    setMessages(history);
    setInput("");
    setError("");
    streamingRef.current = true;
    setStreaming(true);
    streamBuffer.current = "";
    requestId.current = crypto.randomUUID();
    try {
      await invoke("stream_chat", {
        requestId: requestId.current,
        providerId: chatProviderRef.current,
        model: modelRef.current,
        messages: [
          {
            role: "system",
            content: systemPrompt(selectionRef.current),
          },
          ...history,
        ],
      });
    } catch (cause) {
      setError(`${String(cause)} Проверете endpoint-а, модела и дали сървърът работи.`);
      streamingRef.current = false;
      setStreaming(false);
    }
  }

  /// Shows a translation from the history as a finished chat that can be continued.
  function openTranslation(entry: HistoryEntry) {
    requestId.current = crypto.randomUUID();
    streamingRef.current = false;
    setStreaming(false);
    setSelection(entry.query);
    selectionRef.current = entry.query;
    // No "Замени в оригинала": the source window of that selection is gone.
    setAction(null);
    actionRef.current = null;
    setCopiedIndex(null);
    setError("");
    setInput("");
    setTranslation({ source: entry.sourceLang, target: entry.targetLang, provider: entry.provider });
    setWordSaved(false);
    chatHistory.current = [
      { role: "user", content: "Преведи избрания текст (от историята)." },
      { role: "assistant", content: entry.result },
    ];
    setMessages(chatHistory.current);
    setView("chat");
  }

  // Whether the translated word is already among the words to learn.
  useEffect(() => {
    if (!translation || !isWord(selection)) return;
    let current = true;
    invoke<boolean[]>("cards_saved", { terms: [selection] })
      .then(([saved]) => current && setWordSaved(saved))
      .catch(() => {});
    return () => {
      current = false;
    };
  }, [translation, selection]);

  async function saveTranslatedWord(meaning: string) {
    if (!translation) return;
    const card: NewCard = {
      term: selection,
      sourceLang: translation.source,
      targetLang: translation.target,
      meaning,
      meaningIsText: true,
      source:
        translationServiceNames[translation.provider] ??
        (chatProviderNames[translation.provider] ? `ИИ · ${chatProviderNames[translation.provider]}` : translation.provider),
    };
    try {
      await invoke("card_save", { card });
      setWordSaved(true);
    } catch (cause) {
      setError(String(cause));
    }
  }

  function openSettings(from: View) {
    setSettingsFrom(from);
    setView("settings");
  }

  const tabs = (current: MainView) => <ViewTabs current={current} onSelect={setView} />;
  const hide = () => void getCurrentWindow().hide();

  async function replaceInSource(text: string) {
    setError("");
    try {
      await invoke("replace_selection", { text });
    } catch (cause) {
      setError(String(cause));
    }
  }

  async function copyAnswer(text: string, index: number) {
    setError("");
    try {
      await invoke("copy_text", { text });
      setCopiedIndex(index);
    } catch (cause) {
      setError(String(cause));
    }
  }

  if (view === "settings") {
    return (
      <Settings
        initialTab={
          settingsFrom === "dictionary" ? "dictionaries" : settingsFrom === "history" ? "history" : undefined
        }
        backLabel={settingsFrom === "dictionary" ? "Речник" : settingsFrom === "history" ? "История" : "Чат"}
        onBack={() => {
          setView(settingsFrom);
          void refreshChatProvider().catch((cause) => {
            setError(`Неуспешно зареждане на доставчика: ${String(cause)}`);
          });
        }}
        onClose={() => void getCurrentWindow().hide()}
      />
    );
  }

  if (view === "dictionary") {
    return (
      <Dictionary
        tabs={tabs("dictionary")}
        request={dictionaryRequest}
        onSearch={(text) => setDictionaryRequest((current) => ({ text, id: current.id + 1 }))}
        onSettings={() => openSettings("dictionary")}
        onClose={hide}
      />
    );
  }

  if (view === "words") {
    return (
      <Words
        tabs={tabs("words")}
        onLookup={(text) => {
          setDictionaryRequest((current) => ({ text, id: current.id + 1 }));
          setView("dictionary");
        }}
        onClose={hide}
      />
    );
  }

  if (view === "history") {
    return (
      <History
        tabs={tabs("history")}
        onOpenDictionary={(text) => {
          setDictionaryRequest((current) => ({ text, id: current.id + 1 }));
          setView("dictionary");
        }}
        onOpenTranslation={openTranslation}
        onSettings={() => openSettings("history")}
        onClose={hide}
      />
    );
  }

  return (
    <main className="chat">
      <header className="header" data-tauri-drag-region>
        <div className="brand" data-tauri-drag-region>
          <span className="brand-mark">✦</span>
          <div data-tauri-drag-region>
            <strong>ContextAI</strong>
            <small>
              {remoteChatProviders.includes(chatProvider) ? "Асистент" : "Локален асистент"} ·{" "}
              {chatProviderNames[chatProvider]}
            </small>
          </div>
        </div>
        <button className="settings-shortcut" title="Настройки на доставчиците" onClick={() => openSettings("chat")}>
          ⚙
        </button>
        <button className="close" title="Затвори" onClick={() => void getCurrentWindow().hide()}>
          ×
        </button>
      </header>
      {tabs("chat")}

      <section className="model-row">
        <label htmlFor="model">{chatProviderNames[chatProvider]} модел</label>
        <input
          id="model"
          value={model}
          onChange={(event) => {
            setModel(event.target.value);
            modelRef.current = event.target.value;
          }}
          spellCheck={false}
          placeholder="qwen2.5:7b"
        />
      </section>

      <section className="conversation">
        {selection && (
          <blockquote className="quote">
            <span>ИЗБРАН ТЕКСТ</span>
            <p>{selection}</p>
          </blockquote>
        )}
        {messages.length === 0 && !selection && (
          <div className="empty">
            <span className="empty-mark">✦</span>
            <h1>Как мога да помогна?</h1>
            <p>Маркирайте текст в приложение или напишете въпрос.</p>
          </div>
        )}
        {messages.map((message, index) => {
          const finished = !(streaming && index === messages.length - 1);
          // A clear context means the answer no longer belongs to a selection.
          const replacement =
            message.role === "assistant" && finished && selection
              ? replacementText(message.content, action, index === 1)
              : "";
          const savable =
            message.role === "assistant" && finished && index === 1 && translation !== null && isWord(selection);
          return (
            <article className={`message ${message.role}`} key={`${index}-${message.role}`}>
              <div className="message-label">{message.role === "assistant" ? "ContextAI" : "Вие"}</div>
              <div className="message-content">{displayText(message.content)}</div>
              {(replacement || savable) && (
                <div className="message-actions">
                  {replacement && (
                    <>
                      <button className="primary" onClick={() => void replaceInSource(replacement)}>
                        Замени в оригинала
                      </button>
                      <button onClick={() => void copyAnswer(replacement, index)}>
                        {copiedIndex === index ? "Копирано ✓" : "Копирай"}
                      </button>
                    </>
                  )}
                  {savable &&
                    (wordSaved ? (
                      <span className="message-saved">★ В Думи</span>
                    ) : (
                      <button onClick={() => void saveTranslatedWord(displayText(message.content).trim())}>
                        ☆ Запази думата
                      </button>
                    ))}
                </div>
              )}
            </article>
          );
        })}
        {streaming && !streamBuffer.current && <div className="typing"><i /><i /><i /></div>}
        {error && <div className="error">{error}</div>}
      </section>

      <form
        className="composer"
        onSubmit={(event) => {
          event.preventDefault();
          void sendMessage(input);
        }}
      >
        <textarea
          value={input}
          onChange={(event) => setInput(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              void sendMessage(input);
            }
          }}
          placeholder="Попитайте нещо..."
          rows={2}
        />
        <button className="send" type="submit" disabled={streaming || !input.trim()}>
          {streaming ? "…" : "↑"}
        </button>
      </form>
      <footer className="footer">
        <span>
          {remoteChatProviders.includes(chatProvider)
            ? `Заявките се изпращат до ${chatProviderNames[chatProvider]}.`
            : `Заявките се изпращат до локален ${chatProviderNames[chatProvider]} сървър.`}
        </span>
        {selection && (
          <button
            onClick={() => {
              setSelection("");
              selectionRef.current = "";
            }}
          >
            Изчисти контекста
          </button>
        )}
      </footer>
    </main>
  );
}
