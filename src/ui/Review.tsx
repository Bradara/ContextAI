import { type MouseEvent, type ReactNode, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { type Card, languagePair } from "./cards";

export type ReviewCounts = {
  new: number;
  due: number;
  nextDueAt: number | null;
  newToday: number;
  dailyNewCards: number;
};

type Rating = "again" | "hard" | "good" | "easy";
type ReviewItem = {
  card: Card;
  direction: "forward" | "reverse";
  isNew: boolean;
  intervals: [number, number, number, number];
};

const ratings: { id: Rating; label: string; key: string }[] = [
  { id: "again", label: "Пак", key: "1" },
  { id: "hard", label: "Трудно", key: "2" },
  { id: "good", label: "Добре", key: "3" },
  { id: "easy", label: "Лесно", key: "4" },
];

/// "10 мин", "3 д", "2 мес", "1,5 г".
export function intervalLabel(days: number) {
  if (days < 1) return `${Math.max(1, Math.round(days * 24 * 60))} мин`;
  if (days < 30) return `${Math.round(days)} д`;
  if (days < 365) return `${Math.round(days / 30)} мес`;
  return `${(days / 365).toLocaleString("bg-BG", { maximumFractionDigits: 1 })} г`;
}

export function countsLabel(counts: ReviewCounts | null) {
  return counts ? `${counts.new} нови · ${counts.due} за повторение` : "";
}

/// The sentence with the word hidden, so that the example does not give the answer away.
function hideWord(context: string, term: string) {
  const escaped = term.trim().replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return escaped ? context.replace(new RegExp(escaped, "giu"), "_____") : context;
}

function preventLinks(event: MouseEvent<HTMLElement>) {
  if ((event.target as HTMLElement).closest("a")) event.preventDefault();
}

export default function Review({
  tabs,
  onBack,
  onClose,
}: {
  tabs: ReactNode;
  onBack: () => void;
  onClose: () => void;
}) {
  const [item, setItem] = useState<ReviewItem | null>(null);
  const [counts, setCounts] = useState<ReviewCounts | null>(null);
  const [revealed, setRevealed] = useState(false);
  const [loading, setLoading] = useState(true);
  const [answered, setAnswered] = useState(0);
  const [error, setError] = useState("");
  const busy = useRef(false);

  async function load() {
    setLoading(true);
    try {
      const [next, current] = await Promise.all([
        invoke<ReviewItem | null>("review_next"),
        invoke<ReviewCounts>("review_counts"),
      ]);
      setItem(next);
      setCounts(current);
      setRevealed(false);
      setError("");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void load();
  }, []);

  // A word answered "Пак" comes back after a few minutes; wait for it.
  useEffect(() => {
    if (item || !counts?.nextDueAt) return;
    const timer = window.setTimeout(() => void load(), Math.max(1000, counts.nextDueAt - Date.now() + 500));
    return () => window.clearTimeout(timer);
  }, [item, counts?.nextDueAt]);

  async function answer(rating: Rating) {
    if (!item || busy.current) return;
    busy.current = true;
    try {
      await invoke("review_answer", { cardId: item.card.id, direction: item.direction, rating });
      setAnswered((count) => count + 1);
      await load();
    } catch (cause) {
      setError(String(cause));
    } finally {
      busy.current = false;
    }
  }

  async function speak(text: string) {
    try {
      await invoke("start_reading", { text });
    } catch (cause) {
      setError(String(cause));
    }
  }

  // Space or Enter shows the answer, 1–4 rates it.
  const keys = useRef<(event: KeyboardEvent) => void>(() => {});
  keys.current = (event: KeyboardEvent) => {
    if (!item || event.ctrlKey || event.altKey || event.metaKey) return;
    if ((event.target as HTMLElement).closest("input, textarea, select")) return;
    if (!revealed && (event.key === " " || event.key === "Enter")) {
      event.preventDefault();
      setRevealed(true);
    } else if (revealed) {
      const rating = ratings.find((candidate) => candidate.key === event.key);
      if (rating) {
        event.preventDefault();
        void answer(rating.id);
      }
    }
  };
  useEffect(() => {
    const listener = (event: KeyboardEvent) => keys.current(event);
    window.addEventListener("keydown", listener);
    return () => window.removeEventListener("keydown", listener);
  }, []);

  const card = item?.card;
  const forward = item?.direction === "forward";
  const pair = card ? (forward ? languagePair(card.language, card.meaningLang) : languagePair(card.meaningLang, card.language)) : "";
  const context = card?.context ? hideWord(card.context, card.term) : "";

  const word = card && (
    <div className="review-word">
      <strong>{card.term}</strong>
      {card.transcription && <span className="dictionary-transcription">[{card.transcription}]</span>}
      <button className="review-speak" title="Прочети на глас" onClick={() => void speak(card.term)}>
        🔊
      </button>
    </div>
  );
  const meaning = card && (
    <div
      className="dictionary-definition review-meaning"
      onClick={preventLinks}
      // Sanitized in Rust when the card was saved.
      dangerouslySetInnerHTML={{ __html: card.meaningHtml }}
    />
  );

  return (
    <main className="settings review">
      <header className="settings-header" data-tauri-drag-region>
        <button className="back-button" onClick={onBack}>
          ← Думи
        </button>
        <div data-tauri-drag-region>
          <h1>Упражнение</h1>
          <p>{countsLabel(counts)}{answered > 0 && ` · отговорени сега: ${answered}`}</p>
        </div>
        <button className="close settings-close" title="Затвори" onClick={onClose}>
          ×
        </button>
      </header>
      {tabs}

      <section className="review-body">
        {error && <div className="error">{error}</div>}
        {!loading && !item && counts && (
          <div className="empty">
            <span className="empty-mark">✓</span>
            <h1>{counts.nextDueAt ? "Почивка" : "Готово за днес"}</h1>
            <p>
              {counts.nextDueAt
                ? `Следващата дума е след ${intervalLabel((counts.nextDueAt - Date.now()) / 86_400_000)}.`
                : counts.newToday >= counts.dailyNewCards && counts.dailyNewCards > 0
                  ? `Изпълнихте лимита от ${counts.dailyNewCards} нови думи за днес. Утре идват следващите.`
                  : "Няма думи за повторение. Запазете нови от речника или от превод."}
            </p>
          </div>
        )}
        {item && card && (
          <article className="review-card">
            <div className="review-badges">
              <span className="word-pair">{pair}</span>
              <span className={`review-kind${item.isNew ? " new" : ""}`}>
                {item.isNew ? "нова" : "повторение"} · {forward ? "дума → значение" : "значение → дума"}
              </span>
            </div>
            <div className="review-prompt">{forward ? word : meaning}</div>
            {context && (
              <blockquote className="word-context">
                {context.split("\n").map((line, index) => (
                  <p key={index}>{line}</p>
                ))}
              </blockquote>
            )}
            {revealed ? (
              <div className="review-answer">{forward ? meaning : word}</div>
            ) : (
              <button className="review-reveal" onClick={() => setRevealed(true)}>
                Покажи <kbd>интервал</kbd>
              </button>
            )}
          </article>
        )}
      </section>

      {item && revealed && (
        <footer className="review-ratings">
          {ratings.map((rating, index) => (
            <button key={rating.id} className={rating.id} onClick={() => void answer(rating.id)}>
              <span>{rating.label}</span>
              <small>
                {intervalLabel(item.intervals[index])} · <kbd>{rating.key}</kbd>
              </small>
            </button>
          ))}
        </footer>
      )}
    </main>
  );
}
