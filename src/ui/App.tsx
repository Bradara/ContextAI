import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import Settings from "./Settings";
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

export default function App() {
  const [selection, setSelection] = useState("");
  const [messages, setMessages] = useState<Message[]>([]);
  const [input, setInput] = useState("");
  const [model, setModel] = useState("qwen2.5:7b");
  const [chatProvider, setChatProvider] = useState("ollama");
  const [streaming, setStreaming] = useState(false);
  const [error, setError] = useState("");
  const [showSettings, setShowSettings] = useState(false);
  // The popup action that started the chat; decides whether an answer can replace the selection.
  const [action, setAction] = useState<Action | null>(null);
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
          setShowSettings(true);
        }),
      );
      keepOrRemove(
        await listen<ChatStart>("chat-start", ({ payload }) => {
          setSelection(payload.text);
          selectionRef.current = payload.text;
          setAction(payload.action);
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

  if (showSettings) {
    return (
      <Settings
        onBack={() => {
          setShowSettings(false);
          void refreshChatProvider().catch((cause) => {
            setError(`Неуспешно зареждане на доставчика: ${String(cause)}`);
          });
        }}
        onClose={() => void getCurrentWindow().hide()}
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
        <button className="settings-shortcut" title="Настройки на доставчиците" onClick={() => setShowSettings(true)}>
          ⚙
        </button>
        <button className="close" title="Затвори" onClick={() => void getCurrentWindow().hide()}>
          ×
        </button>
      </header>

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
          return (
            <article className={`message ${message.role}`} key={`${index}-${message.role}`}>
              <div className="message-label">{message.role === "assistant" ? "ContextAI" : "Вие"}</div>
              <div className="message-content">{displayText(message.content)}</div>
              {replacement && (
                <div className="message-actions">
                  <button className="primary" onClick={() => void replaceInSource(replacement)}>
                    Замени в оригинала
                  </button>
                  <button onClick={() => void copyAnswer(replacement, index)}>
                    {copiedIndex === index ? "Копирано ✓" : "Копирай"}
                  </button>
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
