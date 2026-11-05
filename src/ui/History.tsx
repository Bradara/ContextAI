import { type ReactNode, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { chatProviderNames } from "./ai";
import { type ChatSummary, type HistoryEntry, type HistoryKind, dayLabel, providerLabel } from "./historyTypes";

type Filter = "all" | HistoryKind | "chats";
/// One line of the list: a translation or lookup, or a chat.
type Row = { kind: "entry"; time: number; entry: HistoryEntry } | { kind: "chat"; time: number; chat: ChatSummary };

const PAGE = 100;
const SEARCH_DELAY_MS = 200;
const filters: { id: Filter; label: string }[] = [
  { id: "all", label: "Всички" },
  { id: "translation", label: "Преводи" },
  { id: "dictionary", label: "Речник" },
  { id: "chats", label: "Чатове" },
];

function time(value: number) {
  return new Date(value).toLocaleTimeString("bg-BG", { hour: "2-digit", minute: "2-digit" });
}

function messageCount(count: number) {
  return count === 1 ? "1 съобщение" : `${count} съобщения`;
}

export default function History({
  tabs,
  onOpenDictionary,
  onOpenTranslation,
  onOpenChat,
  onSettings,
  onClose,
}: {
  tabs: ReactNode;
  onOpenDictionary: (text: string) => void;
  onOpenTranslation: (entry: HistoryEntry) => void;
  onOpenChat: (id: string) => void;
  onSettings: () => void;
  onClose: () => void;
}) {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  // Chats are few (the last 20 + pinned), so they come in one piece with the first page.
  const [chats, setChats] = useState<ChatSummary[]>([]);
  const [filter, setFilter] = useState<Filter>("all");
  const [search, setSearch] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [confirmClear, setConfirmClear] = useState(false);
  const [error, setError] = useState("");
  // Ignores pages that arrive after the filter or the search changed.
  const loadToken = useRef(0);

  async function load(before?: number) {
    const token = ++loadToken.current;
    setLoading(true);
    try {
      const wantsEntries = filter !== "chats";
      const wantsChats = (filter === "all" || filter === "chats") && before === undefined;
      const [page, chatList] = await Promise.all([
        wantsEntries
          ? invoke<HistoryEntry[]>("history_list", {
              query: {
                kind: filter === "all" ? null : filter,
                search: appliedSearch || null,
                before: before ?? null,
                limit: PAGE,
              },
            })
          : Promise.resolve([]),
        wantsChats ? invoke<ChatSummary[]>("chat_list", { search: appliedSearch || null }) : Promise.resolve(null),
      ]);
      if (token !== loadToken.current) return;
      setEntries((current) => (before === undefined ? page : [...current, ...page]));
      if (chatList !== null) setChats(chatList);
      else if (before === undefined) setChats([]);
      setHasMore(page.length === PAGE);
      setError("");
    } catch (cause) {
      if (token === loadToken.current) setError(String(cause));
    } finally {
      if (token === loadToken.current) setLoading(false);
    }
  }

  useEffect(() => {
    void load();
  }, [filter, appliedSearch]);

  useEffect(() => {
    const timer = window.setTimeout(() => setAppliedSearch(search.trim()), SEARCH_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [search]);

  // Translations, lookups and chats made while the list is open show up at once.
  const reload = useRef(load);
  reload.current = load;
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen("history-changed", () => void reload.current()).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  async function run(task: () => Promise<unknown>) {
    setError("");
    try {
      await task();
    } catch (cause) {
      setError(String(cause));
    }
  }

  const clear = () =>
    run(async () => {
      setConfirmClear(false);
      await invoke("history_clear", { includePinned: false });
    });

  // While older entries are still to load, chats older than them wait for their turn.
  const oldestEntry = entries.length ? entries[entries.length - 1].usedAt : 0;
  const rows: Row[] = [
    ...entries.map((entry): Row => ({ kind: "entry", time: entry.usedAt, entry })),
    ...chats
      .filter((chat) => !hasMore || chat.updatedAt >= oldestEntry)
      .map((chat): Row => ({ kind: "chat", time: chat.updatedAt, chat })),
  ].sort((a, b) => b.time - a.time);

  let lastDay = "";

  return (
    <main className="settings history">
      <header className="settings-header" data-tauri-drag-region>
        <div data-tauri-drag-region>
          <h1>История</h1>
          <p>Преводи и речник от последните 30 дни и последните 20 чата. Закачените (★) остават.</p>
        </div>
        <button className="settings-shortcut" title="Настройки на историята" onClick={onSettings}>
          ⚙
        </button>
        <button className="close settings-close" title="Затвори" onClick={onClose}>
          ×
        </button>
      </header>
      {tabs}

      <div className="history-toolbar">
        <input
          value={search}
          onChange={(event) => setSearch(event.target.value)}
          placeholder="Търсене в историята…"
          spellCheck={false}
        />
        <div className="history-filters" role="radiogroup">
          {filters.map((candidate) => (
            <button
              key={candidate.id}
              role="radio"
              aria-checked={candidate.id === filter}
              className={candidate.id === filter ? "active" : ""}
              onClick={() => setFilter(candidate.id)}
            >
              {candidate.label}
            </button>
          ))}
          {confirmClear ? (
            <span className="history-confirm">
              Изтриване на всички незакачени?
              <button className="danger" onClick={() => void clear()}>
                Да
              </button>
              <button onClick={() => setConfirmClear(false)}>Не</button>
            </span>
          ) : (
            <button className="history-clear" disabled={rows.length === 0} onClick={() => setConfirmClear(true)}>
              Изчисти…
            </button>
          )}
        </div>
      </div>

      <section className="history-list">
        {error && <div className="error">{error}</div>}
        {!loading && rows.length === 0 && !error && (
          <div className="empty">
            <span className="empty-mark">↺</span>
            <h1>{appliedSearch || filter !== "all" ? "Няма намерени записи" : "Историята е празна"}</h1>
            <p>Преводите, търсенията в речника и чатовете се появяват тук.</p>
          </div>
        )}
        <ul>
          {rows.map((row) => {
            const day = dayLabel(row.time);
            const heading = day !== lastDay ? day : null;
            lastDay = day;
            if (row.kind === "chat") {
              const chat = row.chat;
              return (
                <li key={`chat-${chat.id}`}>
                  {heading && <h2 className="history-day">{heading}</h2>}
                  <div className="history-item">
                    <button className="history-open" title="Отвори чата" onClick={() => onOpenChat(chat.id)}>
                      <span className="history-icon">✦</span>
                      <span className="history-body">
                        <strong>{chat.title}</strong>
                        <span className="history-result">{chat.preview}</span>
                        <span className="history-meta">
                          {time(chat.updatedAt)} · чат · {chatProviderNames[chat.provider] ?? chat.provider} ·{" "}
                          {messageCount(chat.messageCount)}
                        </span>
                      </span>
                    </button>
                    <div className="history-actions">
                      <button
                        className={chat.pinned ? "pinned" : ""}
                        title={chat.pinned ? "Откачи" : "Закачи (не се брои в последните 20)"}
                        onClick={() => void run(() => invoke("chat_pin", { id: chat.id, pinned: !chat.pinned }))}
                      >
                        {chat.pinned ? "★" : "☆"}
                      </button>
                      <button title="Изтрий" onClick={() => void run(() => invoke("chat_delete", { id: chat.id }))}>
                        ×
                      </button>
                    </div>
                  </div>
                </li>
              );
            }
            const entry = row.entry;
            const languages =
              entry.sourceLang || entry.targetLang ? ` · ${entry.sourceLang || "?"} → ${entry.targetLang || "?"}` : "";
            return (
              <li key={entry.id}>
                {heading && <h2 className="history-day">{heading}</h2>}
                <div className="history-item">
                  <button
                    className="history-open"
                    title="Отвори отново"
                    onClick={() => (entry.kind === "dictionary" ? onOpenDictionary(entry.query) : onOpenTranslation(entry))}
                  >
                    <span className="history-icon">{entry.kind === "dictionary" ? "📖" : "文"}</span>
                    <span className="history-body">
                      <strong>{entry.query}</strong>
                      <span className="history-result">{entry.result}</span>
                      <span className="history-meta">
                        {time(entry.usedAt)} · {providerLabel(entry)}
                        {languages}
                      </span>
                    </span>
                  </button>
                  <div className="history-actions">
                    <button
                      className={entry.pinned ? "pinned" : ""}
                      title={entry.pinned ? "Откачи" : "Закачи (не се изтрива след 30 дни)"}
                      onClick={() => void run(() => invoke("history_pin", { id: entry.id, pinned: !entry.pinned }))}
                    >
                      {entry.pinned ? "★" : "☆"}
                    </button>
                    <button title="Изтрий" onClick={() => void run(() => invoke("history_delete", { id: entry.id }))}>
                      ×
                    </button>
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
        {hasMore && (
          <button className="history-more" disabled={loading} onClick={() => void load(oldestEntry)}>
            {loading ? "Зареждане…" : "Още"}
          </button>
        )}
      </section>
    </main>
  );
}
