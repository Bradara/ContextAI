import { type ReactNode, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { type HistoryEntry, type HistoryKind, dayLabel, providerLabel } from "./historyTypes";

type Filter = "all" | HistoryKind;

const PAGE = 100;
const SEARCH_DELAY_MS = 200;
const filters: { id: Filter; label: string }[] = [
  { id: "all", label: "Всички" },
  { id: "translation", label: "Преводи" },
  { id: "dictionary", label: "Речник" },
];

function time(value: number) {
  return new Date(value).toLocaleTimeString("bg-BG", { hour: "2-digit", minute: "2-digit" });
}

export default function History({
  tabs,
  onOpenDictionary,
  onOpenTranslation,
  onSettings,
  onClose,
}: {
  tabs: ReactNode;
  onOpenDictionary: (text: string) => void;
  onOpenTranslation: (entry: HistoryEntry) => void;
  onSettings: () => void;
  onClose: () => void;
}) {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
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
      const page = await invoke<HistoryEntry[]>("history_list", {
        query: {
          kind: filter === "all" ? null : filter,
          search: appliedSearch || null,
          before: before ?? null,
          limit: PAGE,
        },
      });
      if (token !== loadToken.current) return;
      setEntries((current) => (before === undefined ? page : [...current, ...page]));
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

  // Translations and lookups made while the list is open show up at once.
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

  const togglePin = (entry: HistoryEntry) =>
    run(() => invoke("history_pin", { id: entry.id, pinned: !entry.pinned }));
  const remove = (entry: HistoryEntry) => run(() => invoke("history_delete", { id: entry.id }));
  const clear = () =>
    run(async () => {
      setConfirmClear(false);
      await invoke("history_clear", { includePinned: false });
    });

  function open(entry: HistoryEntry) {
    if (entry.kind === "dictionary") onOpenDictionary(entry.query);
    else onOpenTranslation(entry);
  }

  let lastDay = "";

  return (
    <main className="settings history">
      <header className="settings-header" data-tauri-drag-region>
        <div data-tauri-drag-region>
          <h1>История</h1>
          <p>Преводи и търсения в речника от последните 30 дни. Закачените (★) остават.</p>
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
            <button
              className="history-clear"
              disabled={entries.length === 0}
              onClick={() => setConfirmClear(true)}
            >
              Изчисти…
            </button>
          )}
        </div>
      </div>

      <section className="history-list">
        {error && <div className="error">{error}</div>}
        {!loading && entries.length === 0 && !error && (
          <div className="empty">
            <span className="empty-mark">↺</span>
            <h1>{appliedSearch || filter !== "all" ? "Няма намерени записи" : "Историята е празна"}</h1>
            <p>Преводите и търсенията в речника се появяват тук.</p>
          </div>
        )}
        <ul>
          {entries.map((entry) => {
            const day = dayLabel(entry.usedAt);
            const heading = day !== lastDay ? day : null;
            lastDay = day;
            const languages =
              entry.sourceLang || entry.targetLang ? ` · ${entry.sourceLang || "?"} → ${entry.targetLang || "?"}` : "";
            return (
              <li key={entry.id}>
                {heading && <h2 className="history-day">{heading}</h2>}
                <div className="history-item">
                  <button className="history-open" title="Отвори отново" onClick={() => open(entry)}>
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
                      onClick={() => void togglePin(entry)}
                    >
                      {entry.pinned ? "★" : "☆"}
                    </button>
                    <button title="Изтрий" onClick={() => void remove(entry)}>
                      ×
                    </button>
                  </div>
                </div>
              </li>
            );
          })}
        </ul>
        {hasMore && (
          <button
            className="history-more"
            disabled={loading}
            onClick={() => void load(entries[entries.length - 1]?.usedAt)}
          >
            {loading ? "Зареждане…" : "Още"}
          </button>
        )}
      </section>
    </main>
  );
}
