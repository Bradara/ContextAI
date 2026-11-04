import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type DictionaryInfo = {
  path: string;
  enabled: boolean;
  name: string;
  articles: number;
  status: "ready" | "indexing" | "error" | "missing";
  error?: string;
};

function dictionaryCount(count: number) {
  return count === 1 ? "1 речник" : `${count} речника`;
}

function statusText(info: DictionaryInfo) {
  switch (info.status) {
    case "ready":
      return `${info.articles.toLocaleString("bg-BG")} статии`;
    case "indexing":
      return "Индексиране…";
    case "missing":
      return "Файлът не е намерен";
    default:
      return info.error ?? "Грешка";
  }
}

/// The dictionary list applies immediately instead of waiting for "Запази".
export default function DictionarySettings() {
  const [dictionaries, setDictionaries] = useState<DictionaryInfo[] | null>(null);
  const [adding, setAdding] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");

  async function reload() {
    try {
      setDictionaries(await invoke<DictionaryInfo[]>("dictionary_list"));
    } catch (cause) {
      setError(String(cause));
    }
  }

  useEffect(() => {
    void reload();
    // Indexing runs in the background and reports each finished dictionary.
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen("dictionaries-changed", () => void reload()).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  async function add(folder: boolean) {
    setError("");
    setNotice("");
    setAdding(true);
    try {
      const added = await invoke<number>("dictionary_add", { folder });
      if (added > 0) setNotice(`Добавени: ${dictionaryCount(added)}. Индексирането продължава във фонов режим.`);
      else setNotice("Не са добавени нови речници.");
      await reload();
    } catch (cause) {
      setError(String(cause));
    } finally {
      setAdding(false);
    }
  }

  async function save(next: DictionaryInfo[]) {
    setError("");
    setNotice("");
    setDictionaries(next);
    try {
      await invoke("dictionary_save", {
        dictionaries: next.map(({ path, enabled }) => ({ path, enabled })),
      });
    } catch (cause) {
      setError(String(cause));
      await reload();
    }
  }

  function move(index: number, step: number) {
    if (!dictionaries) return;
    const target = index + step;
    if (target < 0 || target >= dictionaries.length) return;
    const next = [...dictionaries];
    [next[index], next[target]] = [next[target], next[index]];
    void save(next);
  }

  const list = dictionaries ?? [];
  const active = list.filter((dictionary) => dictionary.enabled).length;

  return (
    <section className="provider-group">
      <h2>Речници</h2>
      <article className="provider-card">
        <div className="provider-heading">
          <div>
            <h3>Babylon (.bgl)</h3>
            <p>
              Добавете файлове или цяла папка (търси се и в подпапките). Резултатите се показват по реда от списъка;
              изключените речници не се претърсват. Промените важат веднага, без „Запази“. Файловете остават на
              мястото си; ContextAI пази само индекс за бързо търсене.
            </p>
          </div>
        </div>
        <div className="copilot-actions">
          <button disabled={adding} onClick={() => void add(false)}>
            Добави файлове…
          </button>
          <button disabled={adding} onClick={() => void add(true)}>
            Добави папка…
          </button>
        </div>
      </article>

      {dictionaries === null && <p className="settings-loading">Зареждане…</p>}
      {dictionaries !== null && list.length === 0 && (
        <p className="dictionary-note">Все още няма добавени речници.</p>
      )}
      {list.length > 0 && (
        <p className="dictionary-note">
          Активни: {active} от {dictionaryCount(list.length)}.
        </p>
      )}

      <ol className="dictionary-list">
        {list.map((dictionary, index) => (
          <li className={`provider-card dictionary-item${dictionary.enabled ? "" : " disabled"}`} key={dictionary.path}>
            <span className="dictionary-order">{index + 1}</span>
            <div className="dictionary-info">
              <h3>{dictionary.name}</h3>
              <p className="dictionary-path" title={dictionary.path}>
                {dictionary.path}
              </p>
              <p className={`dictionary-status ${dictionary.status}`}>{statusText(dictionary)}</p>
            </div>
            <div className="dictionary-controls">
              <label className="toggle">
                <input
                  type="checkbox"
                  checked={dictionary.enabled}
                  onChange={(event) =>
                    void save(
                      list.map((item, position) =>
                        position === index ? { ...item, enabled: event.target.checked } : item,
                      ),
                    )
                  }
                />
                <span>Активен</span>
              </label>
              <div className="dictionary-buttons">
                <button title="Нагоре" disabled={index === 0} onClick={() => move(index, -1)}>
                  ↑
                </button>
                <button title="Надолу" disabled={index === list.length - 1} onClick={() => move(index, 1)}>
                  ↓
                </button>
                <button
                  className="danger"
                  title="Премахни от списъка (файлът не се изтрива)"
                  onClick={() => void save(list.filter((_, position) => position !== index))}
                >
                  ×
                </button>
              </div>
            </div>
          </li>
        ))}
      </ol>

      {notice && <div className="settings-success dictionary-message">{notice}</div>}
      {error && <div className="error copilot-error">{error}</div>}
    </section>
  );
}
