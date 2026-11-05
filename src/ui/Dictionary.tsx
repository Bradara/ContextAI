import { type KeyboardEvent, type MouseEvent, type ReactNode, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type Entry = { headword: string; partOfSpeech: string; transcription: string; html: string };
type Group = { path: string; name: string; entries: Entry[] };
type LookupResult = {
  query: string;
  matched: string;
  groups: Group[];
  suggestions: string[];
  errors: string[];
  enabled: number;
};
export type DictionaryRequest = { text: string; id: number };

const SUGGEST_DELAY_MS = 120;
const VISIBLE_SUGGESTIONS = 8;

export default function Dictionary({
  tabs,
  request,
  onSearch,
  onSettings,
  onClose,
}: {
  tabs: ReactNode;
  request: DictionaryRequest;
  onSearch: (text: string) => void;
  onSettings: () => void;
  onClose: () => void;
}) {
  const [input, setInput] = useState(request.text);
  const [result, setResult] = useState<LookupResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [suggestions, setSuggestions] = useState<string[]>([]);
  const [highlighted, setHighlighted] = useState(-1);
  const inputRef = useRef<HTMLInputElement>(null);
  const resultsRef = useRef<HTMLElement>(null);
  // Ignores suggestions that arrive after the input changed again.
  const suggestToken = useRef(0);

  useEffect(() => {
    setInput(request.text);
    setSuggestions([]);
    const text = request.text.trim();
    if (!text) {
      setResult(null);
      inputRef.current?.focus();
      return;
    }
    let current = true;
    setLoading(true);
    setError("");
    invoke<LookupResult>("dictionary_lookup", { query: text })
      .then((found) => {
        if (!current) return;
        setResult(found);
        resultsRef.current?.scrollTo({ top: 0 });
      })
      .catch((cause) => current && setError(String(cause)))
      .finally(() => current && setLoading(false));
    return () => {
      current = false;
    };
  }, [request]);

  function changeInput(text: string) {
    setInput(text);
    setHighlighted(-1);
    const token = ++suggestToken.current;
    if (!text.trim()) {
      setSuggestions([]);
      return;
    }
    window.setTimeout(() => {
      if (token !== suggestToken.current) return;
      invoke<string[]>("dictionary_suggest", { prefix: text })
        .then((words) => token === suggestToken.current && setSuggestions(words.slice(0, VISIBLE_SUGGESTIONS)))
        .catch(() => token === suggestToken.current && setSuggestions([]));
    }, SUGGEST_DELAY_MS);
  }

  function search(text: string) {
    suggestToken.current += 1;
    setSuggestions([]);
    setHighlighted(-1);
    if (text.trim()) onSearch(text.trim());
  }

  function onInputKey(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (!suggestions.length) return;
      event.preventDefault();
      // -1 is the typed text itself.
      const count = suggestions.length;
      setHighlighted((index) =>
        event.key === "ArrowDown" ? (index + 1 >= count ? -1 : index + 1) : index - 1 < -1 ? count - 1 : index - 1,
      );
    } else if (event.key === "Escape") {
      suggestToken.current += 1;
      setSuggestions([]);
    }
  }

  /// Links to other articles (`bword:`) start a new search; nothing else may navigate the window.
  function onDefinitionClick(event: MouseEvent<HTMLElement>) {
    const link = (event.target as HTMLElement).closest("a");
    if (!link) return;
    event.preventDefault();
    const href = link.getAttribute("href") ?? "";
    if (!href.toLowerCase().startsWith("bword:")) return;
    try {
      search(decodeURIComponent(href.slice(6)).replace(/^\/\//, ""));
    } catch {
      search(link.textContent ?? "");
    }
  }

  const activeSuggestion = highlighted >= 0 && highlighted < suggestions.length ? suggestions[highlighted] : null;

  return (
    <main className="settings dictionary">
      <header className="settings-header" data-tauri-drag-region>
        <div data-tauri-drag-region>
          <h1>Речник</h1>
          <p>Търсене в активните речници по реда от настройките.</p>
        </div>
        <button className="settings-shortcut" title="Настройки на речниците" onClick={onSettings}>
          ⚙
        </button>
        <button className="close settings-close" title="Затвори" onClick={onClose}>
          ×
        </button>
      </header>
      {tabs}

      <form
        className="dictionary-search"
        onSubmit={(event) => {
          event.preventDefault();
          search(activeSuggestion ?? input);
        }}
      >
        <input
          ref={inputRef}
          value={input}
          onChange={(event) => changeInput(event.target.value)}
          onKeyDown={onInputKey}
          onBlur={() => window.setTimeout(() => setSuggestions([]), 150)}
          placeholder="Дума или израз…"
          spellCheck={false}
          autoFocus
        />
        <button type="submit" disabled={!input.trim() || loading}>
          {loading ? "…" : "Търси"}
        </button>
        {suggestions.length > 0 && (
          <ul className="dictionary-suggestions" role="listbox">
            {suggestions.map((word, index) => (
              <li key={word}>
                <button
                  type="button"
                  className={index === highlighted ? "active" : ""}
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => search(word)}
                >
                  {word}
                </button>
              </li>
            ))}
          </ul>
        )}
      </form>

      <section className="dictionary-results" ref={resultsRef} onClick={onDefinitionClick}>
        {loading && !result && (
          <p className="dictionary-note">Търсене… Първото търсене в нов речник го индексира и може да отнеме няколко секунди.</p>
        )}
        {error && <div className="error">{error}</div>}
        {!result && !loading && !error && (
          <div className="empty">
            <span className="empty-mark">📖</span>
            <h1>Търсене в речник</h1>
            <p>Маркирайте дума и изберете „Речник“ от менюто или я напишете по-горе.</p>
          </div>
        )}
        {result && result.enabled === 0 && (
          <div className="empty">
            <span className="empty-mark">📖</span>
            <h1>Няма активни речници</h1>
            <p>Добавете речници Babylon (.bgl) или StarDict (.ifo) от настройките.</p>
            <button className="dictionary-add" onClick={onSettings}>Добави речници</button>
          </div>
        )}
        {result && result.enabled > 0 && result.groups.length === 0 && (
          <div className="dictionary-missing">
            <p>
              Няма резултат за <strong>{result.query}</strong>.
            </p>
            {result.suggestions.length > 0 && (
              <>
                <p className="dictionary-note">Близки думи:</p>
                <div className="dictionary-chips">
                  {result.suggestions.map((word) => (
                    <button key={word} onClick={() => search(word)}>
                      {word}
                    </button>
                  ))}
                </div>
              </>
            )}
          </div>
        )}
        {result?.groups.map((group) => (
          <article className="dictionary-group" key={group.path}>
            <h2 title={group.path}>{group.name}</h2>
            {group.entries.map((entry, index) => (
              <div className="dictionary-entry" key={index}>
                <div className="dictionary-headword">
                  <strong>{entry.headword}</strong>
                  {entry.transcription && <span className="dictionary-transcription">[{entry.transcription}]</span>}
                  {entry.partOfSpeech && <em>{entry.partOfSpeech}</em>}
                </div>
                {/* Sanitized in Rust (ammonia): no scripts, styles, images or external links. */}
                <div className="dictionary-definition" dangerouslySetInnerHTML={{ __html: entry.html }} />
              </div>
            ))}
          </article>
        ))}
        {result?.errors.map((message) => (
          <div className="error" key={message}>
            {message}
          </div>
        ))}
      </section>
    </main>
  );
}
