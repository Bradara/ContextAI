import { type MouseEvent, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  type Action,
  type ProviderSettings,
  type StreamPayload,
  type TranslationResult,
  isTranslationService,
  resolveChatProvider,
  systemPrompt,
  translateInstruction,
  translationServiceNames,
} from "./ai";

type Submenu = "ai" | "translate" | "read";
type MenuItem = { id: string; label: string; icon: string; current?: boolean };
/// `expanded` and `action` come from the SnipDo extension, which skips the trigger icon.
type SelectionPayload = { text: string; expanded?: boolean; action?: string };

const aiActions: MenuItem[] = [
  { id: "explain", label: "Обясни", icon: "?" },
  { id: "summarize", label: "Обобщи", icon: "≡" },
  { id: "edit", label: "Редактирай", icon: "✎" },
  { id: "grammar", label: "Граматика", icon: "✓" },
  { id: "ask", label: "Попитай", icon: "✦" },
];

const readItems: MenuItem[] = [
  { id: "read", label: "Чети", icon: "🔊" },
  { id: "translate-read", label: "Преведи и чети", icon: "文" },
];

// Must match the .submenu styles: item height, padding + border, and the gap above it.
const SUBMENU_ITEM_HEIGHT = 30;
const SUBMENU_CHROME = 12 + 4;
const SUBMENU_WIDTH = 150;
const CLOSE_DELAY_MS = 250;

export default function Popup() {
  const [selection, setSelection] = useState("");
  const [expanded, setExpanded] = useState(false);
  const [submenu, setSubmenu] = useState<{ id: Submenu; left: number } | null>(null);
  const [opensUp, setOpensUp] = useState(false);
  const [settings, setSettings] = useState<ProviderSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const closeTimer = useRef<number | undefined>(undefined);
  // Ignores resize results that arrive after a newer hover.
  const layoutToken = useRef(0);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<SelectionPayload>("selection-captured", ({ payload }) => {
      window.clearTimeout(closeTimer.current);
      layoutToken.current += 1;
      setSelection(payload.text);
      setExpanded(false);
      setSubmenu(null);
      setOpensUp(false);
      setBusy(false);
      setError("");
      if (payload.expanded) void expand();
      // The selection state is not updated yet, so the action gets the text directly.
      if (payload.action === "translate-read") void translateAndRead(payload.text);
    }).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const defaultService = settings?.activeTranslationProvider || "ai";
  const translateItems: MenuItem[] = [
    { id: "ai", label: "ИИ", icon: "✦" },
    ...Object.entries(translationServiceNames)
      .filter(([id]) => settings?.providers.some((provider) => provider.id === id && provider.enabled))
      .map(([id, name]) => ({ id, label: name, icon: "文" })),
  ].map((item) => ({ ...item, current: item.id === defaultService }));

  async function expand() {
    try {
      await invoke("set_popup_expanded", { expanded: true });
      setExpanded(true);
      setSettings(await invoke<ProviderSettings>("load_provider_settings"));
    } catch (cause) {
      setError(String(cause));
    }
  }

  function itemsFor(id: Submenu) {
    return id === "ai" ? aiActions : id === "translate" ? translateItems : readItems;
  }

  function selectItem(menu: Submenu, id: string) {
    if (menu === "ai") return openChat(id as Action);
    if (menu === "translate") return openChat("translate", id);
    return id === "read" ? readAloud() : translateAndRead(selection);
  }

  async function openSubmenu(id: Submenu, buttonLeft: number) {
    let left = buttonLeft;
    window.clearTimeout(closeTimer.current);
    if (submenu?.id === id || busy) return;
    const items = itemsFor(id);
    // Keep the submenu inside the window when its button is near the right edge.
    left = Math.max(0, Math.min(left, window.innerWidth - SUBMENU_WIDTH));
    const token = ++layoutToken.current;
    try {
      const up = await invoke<boolean>("set_popup_expanded", {
        expanded: true,
        submenuHeight: items.length * SUBMENU_ITEM_HEIGHT + SUBMENU_CHROME,
      });
      if (token !== layoutToken.current) return;
      setOpensUp(up);
      setSubmenu({ id, left });
    } catch (cause) {
      setError(String(cause));
    }
  }

  function closeSubmenu() {
    window.clearTimeout(closeTimer.current);
    const token = ++layoutToken.current;
    setSubmenu(null);
    void invoke("set_popup_expanded", { expanded: true }).then(() => {
      if (token === layoutToken.current) setOpensUp(false);
    });
  }

  function scheduleClose() {
    window.clearTimeout(closeTimer.current);
    closeTimer.current = window.setTimeout(closeSubmenu, CLOSE_DELAY_MS);
  }

  function hoverProps(id: Submenu) {
    return {
      onMouseEnter: (event: MouseEvent<HTMLButtonElement>) =>
        void openSubmenu(id, event.currentTarget.offsetLeft),
      onMouseLeave: scheduleClose,
    };
  }

  async function run(task: () => Promise<unknown>) {
    setError("");
    try {
      await task();
    } catch (cause) {
      setError(String(cause));
    }
  }

  const openChat = (action: Action, service?: string) =>
    run(() => invoke("open_chat", { text: selection, action, service }));

  const readAloud = () => run(() => invoke("start_reading", { text: selection }));

  /// Collects a full (non-streamed) answer from the chat provider.
  async function completeWithAi(current: ProviderSettings, text: string) {
    const { providerId, model } = resolveChatProvider(current);
    const requestId = crypto.randomUUID();
    let answer = "";
    const stop = await listen<StreamPayload>("chat-delta", ({ payload }) => {
      if (payload.request_id === requestId) answer += payload.content;
    });
    try {
      await invoke("stream_chat", {
        requestId,
        providerId,
        model,
        messages: [
          { role: "system", content: systemPrompt(text) },
          {
            role: "user",
            content: `${translateInstruction(current.translationTargetLanguage)}\n\nТекст:\n${text}`,
          },
        ],
      });
    } finally {
      stop();
    }
    return answer.trim();
  }

  const translateAndRead = (text: string) =>
    run(async () => {
      closeSubmenu();
      setBusy(true);
      try {
        const current = await invoke<ProviderSettings>("load_provider_settings");
        const service = current.activeTranslationProvider;
        const translated = isTranslationService(service)
          ? (await invoke<TranslationResult>("translate_text", { providerId: service, text })).text
          : await completeWithAi(current, text);
        if (!translated) throw new Error("Преводът е празен.");
        await invoke("start_reading", { text: translated });
      } finally {
        setBusy(false);
      }
    });

  if (!expanded) {
    return (
      <main className="popup-collapsed">
        <button className="popup-trigger" title="ContextAI" onClick={() => void expand()}>
          ✦
        </button>
      </main>
    );
  }

  const submenuItems = submenu ? itemsFor(submenu.id) : [];

  return (
    <main className={`popup-shell${opensUp ? " up" : ""}`}>
      <div className="popup">
        <button
          className={`action${submenu?.id === "ai" ? " open" : ""}`}
          title="Действия с ИИ"
          disabled={busy}
          onClick={(event) => void openSubmenu("ai", event.currentTarget.offsetLeft)}
          {...hoverProps("ai")}
        >
          <span className="action-icon">✦</span>
          <span>ИИ ▾</span>
        </button>
        <button
          className={`action${submenu?.id === "translate" ? " open" : ""}`}
          title={`Преведи (${defaultService === "ai" ? "ИИ" : translationServiceNames[defaultService]}); задръжте за избор на услуга`}
          disabled={busy}
          onClick={() => void openChat("translate", defaultService)}
          {...hoverProps("translate")}
        >
          <span className="action-icon">文</span>
          <span>Преведи ▾</span>
        </button>
        <button
          className={`action${submenu?.id === "read" ? " open" : ""}`}
          title="Прочети на глас; задръжте за „Преведи и чети“"
          disabled={busy}
          onClick={() => void readAloud()}
          {...hoverProps("read")}
        >
          <span className="action-icon">{busy ? "…" : "🔊"}</span>
          <span>{busy ? "Превеждане" : "Чети ▾"}</span>
        </button>
        {error && <div className="popup-error" title={error}>{error}</div>}
      </div>
      {submenu && (
        <div
          className="submenu"
          style={{ marginLeft: submenu.left }}
          onMouseEnter={() => window.clearTimeout(closeTimer.current)}
          onMouseLeave={scheduleClose}
        >
          {submenuItems.map((item) => (
            <button
              key={item.id}
              className={`submenu-item${item.current ? " current" : ""}`}
              onClick={() => void selectItem(submenu.id, item.id)}
            >
              <span className="action-icon">{item.icon}</span>
              <span>{item.label}</span>
              {item.current && <span className="submenu-default">по подразб.</span>}
            </button>
          ))}
        </div>
      )}
    </main>
  );
}
