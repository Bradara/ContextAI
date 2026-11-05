import { type ReactNode, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { type Card, htmlToText, languagePair } from "./cards";
import type { ReviewCounts } from "./Review";

const PAGE = 200;
const SEARCH_DELAY_MS = 200;

type Draft = {
  term: string;
  transcription: string;
  meaning: string;
  meaningChanged: boolean;
  context: string;
  tags: string;
};

function wordCount(count: number) {
  return count === 1 ? "1 дума" : `${count} думи`;
}

function date(value: number) {
  return new Date(value).toLocaleDateString("bg-BG", { day: "numeric", month: "short", year: "numeric" });
}

export default function Words({
  tabs,
  onLookup,
  onReview,
  onSettings,
  onClose,
}: {
  tabs: ReactNode;
  onLookup: (term: string) => void;
  onReview: () => void;
  onSettings: () => void;
  onClose: () => void;
}) {
  const [cards, setCards] = useState<Card[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const [search, setSearch] = useState("");
  const [appliedSearch, setAppliedSearch] = useState("");
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [error, setError] = useState("");
  const [counts, setCounts] = useState<ReviewCounts | null>(null);
  const loadToken = useRef(0);

  // Today's practice counter, kept current by the backend (answers, new words, time).
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    invoke<ReviewCounts>("review_counts")
      .then((current) => !disposed && setCounts(current))
      .catch(() => {});
    void listen<ReviewCounts>("review-changed", ({ payload }) => setCounts(payload)).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  async function load(before?: number) {
    const token = ++loadToken.current;
    setLoading(true);
    try {
      const [page, count] = await Promise.all([
        invoke<Card[]>("cards_list", {
          query: { search: appliedSearch || null, before: before ?? null, limit: PAGE },
        }),
        invoke<number>("cards_count"),
      ]);
      if (token !== loadToken.current) return;
      setCards((current) => (before === undefined ? page : [...current, ...page]));
      setHasMore(page.length === PAGE);
      setTotal(count);
      setError("");
    } catch (cause) {
      if (token === loadToken.current) setError(String(cause));
    } finally {
      if (token === loadToken.current) setLoading(false);
    }
  }

  useEffect(() => {
    void load();
  }, [appliedSearch]);

  useEffect(() => {
    const timer = window.setTimeout(() => setAppliedSearch(search.trim()), SEARCH_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [search]);

  // Words saved from the dictionary or a translation while the list is open.
  const reload = useRef(load);
  reload.current = load;
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen("cards-changed", () => void reload.current()).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  function startEdit(card: Card) {
    setConfirmDelete(null);
    setEditing(card.id);
    setDraft({
      term: card.term,
      transcription: card.transcription,
      meaning: htmlToText(card.meaningHtml),
      meaningChanged: false,
      context: card.context,
      tags: card.tags.join(", "),
    });
  }

  async function saveEdit(id: string) {
    if (!draft) return;
    setError("");
    try {
      await invoke("card_update", {
        id,
        edit: {
          term: draft.term,
          transcription: draft.transcription,
          // Untouched meanings keep the dictionary's formatting.
          meaningText: draft.meaningChanged ? draft.meaning : null,
          context: draft.context,
          tags: draft.tags.split(",").map((tag) => tag.trim()),
        },
      });
      setEditing(null);
      setDraft(null);
    } catch (cause) {
      setError(String(cause));
    }
  }

  async function remove(id: string) {
    setError("");
    setConfirmDelete(null);
    try {
      await invoke("card_delete", { id });
    } catch (cause) {
      setError(String(cause));
    }
  }

  const field = (key: keyof Draft) => ({
    value: String(draft?.[key] ?? ""),
    onChange: (event: { target: { value: string } }) =>
      setDraft((current) =>
        current ? { ...current, [key]: event.target.value, ...(key === "meaning" && { meaningChanged: true }) } : current,
      ),
  });

  return (
    <main className="settings words">
      <header className="settings-header" data-tauri-drag-region>
        <div data-tauri-drag-region>
          <h1>Думи</h1>
          <p>
            {total === null ? "Запазени думи за учене." : `${wordCount(total)} за учене.`}
          </p>
        </div>
        <button
          className={`review-start${counts && counts.new + counts.due > 0 ? " waiting" : ""}`}
          title="Упражнение: нови думи и думи за повторение днес"
          onClick={onReview}
        >
          Упражнение{counts && counts.new + counts.due > 0 ? ` · ${counts.new} нови · ${counts.due}` : ""}
        </button>
        <button className="settings-shortcut" title="Настройки на упражненията" onClick={onSettings}>
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
          placeholder="Търсене в думите, значенията, примерите и таговете…"
          spellCheck={false}
        />
      </div>

      <section className="history-list words-list">
        {error && <div className="error">{error}</div>}
        {!loading && cards.length === 0 && !error && (
          <div className="empty">
            <span className="empty-mark">☆</span>
            <h1>{appliedSearch ? "Няма намерени думи" : "Още няма запазени думи"}</h1>
            <p>Натиснете ☆ до статия в речника или под превода на дума, за да я запазите тук.</p>
          </div>
        )}
        <ul>
          {cards.map((card) => {
            const pair = languagePair(card.language, card.meaningLang);
            if (editing === card.id && draft) {
              return (
                <li key={card.id} className="word-card editing">
                  <label className="setting-field">
                    <span>Дума</span>
                    <input {...field("term")} spellCheck={false} />
                  </label>
                  <label className="setting-field">
                    <span>Транскрипция</span>
                    <input {...field("transcription")} spellCheck={false} />
                  </label>
                  <label className="setting-field">
                    <span>Значение</span>
                    <textarea rows={5} {...field("meaning")} />
                  </label>
                  <label className="setting-field">
                    <span>Примери (по едно изречение на ред)</span>
                    <textarea rows={3} {...field("context")} />
                  </label>
                  <label className="setting-field">
                    <span>Тагове (със запетая)</span>
                    <input {...field("tags")} placeholder="глаголи, работа" />
                  </label>
                  {draft.meaningChanged && (
                    <p className="dictionary-note">Редактираното значение се запазва като обикновен текст.</p>
                  )}
                  <div className="copilot-actions">
                    <button className="primary" onClick={() => void saveEdit(card.id)}>
                      Запази
                    </button>
                    <button
                      onClick={() => {
                        setEditing(null);
                        setDraft(null);
                      }}
                    >
                      Отказ
                    </button>
                  </div>
                </li>
              );
            }
            return (
              <li key={card.id} className="word-card">
                <div className="word-heading">
                  <strong>{card.term}</strong>
                  {card.transcription && <span className="dictionary-transcription">[{card.transcription}]</span>}
                  {pair && <span className="word-pair">{pair}</span>}
                  <div className="history-actions word-actions">
                    <button title="Търси в речника" onClick={() => onLookup(card.term)}>
                      📖
                    </button>
                    <button title="Редактирай" onClick={() => startEdit(card)}>
                      ✎
                    </button>
                    {confirmDelete === card.id ? (
                      <span className="history-confirm">
                        Изтриване?
                        <button className="danger" onClick={() => void remove(card.id)}>
                          Да
                        </button>
                        <button onClick={() => setConfirmDelete(null)}>Не</button>
                      </span>
                    ) : (
                      <button title="Изтрий" onClick={() => setConfirmDelete(card.id)}>
                        ×
                      </button>
                    )}
                  </div>
                </div>
                {card.meaningHtml && (
                  <div
                    className={`dictionary-definition word-meaning${expanded === card.id ? " expanded" : ""}`}
                    title={expanded === card.id ? "" : "Покажи цялото значение"}
                    onClick={(event) => {
                      // Links inside an article must not navigate the window.
                      if ((event.target as HTMLElement).closest("a")) event.preventDefault();
                      setExpanded(expanded === card.id ? null : card.id);
                    }}
                    // Sanitized in Rust when the card was saved.
                    dangerouslySetInnerHTML={{ __html: card.meaningHtml }}
                  />
                )}
                {card.context && (
                  <blockquote className="word-context">
                    {card.context.split("\n").map((line, index) => (
                      <p key={index}>{line}</p>
                    ))}
                  </blockquote>
                )}
                <div className="history-meta word-meta">
                  {card.tags.map((tag) => (
                    <button key={tag} className="word-tag" title="Покажи думите с този таг" onClick={() => setSearch(tag)}>
                      {tag}
                    </button>
                  ))}
                  <span>
                    {date(card.createdAt)}
                    {card.source && ` · ${card.source}`}
                  </span>
                </div>
              </li>
            );
          })}
        </ul>
        {hasMore && (
          <button
            className="history-more"
            disabled={loading}
            onClick={() => void load(cards[cards.length - 1]?.createdAt)}
          >
            {loading ? "Зареждане…" : "Още"}
          </button>
        )}
      </section>
    </main>
  );
}
