import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { HistorySettings as Settings } from "./historyTypes";

const switches: { key: keyof Settings; title: string; description: string }[] = [
  {
    key: "translations",
    title: "Преводи",
    description: "Текстът и преводът от DeepL, Azure, Google или ИИ.",
  },
  {
    key: "dictionary",
    title: "Търсения в речника",
    description: "Намерената дума и началото на статията.",
  },
  {
    key: "chats",
    title: "Чатове",
    description:
      "Последните 20 разговора („Обясни“, „Обобщи“, „Редактирай“, „Граматика“, „Попитай“ и свободни въпроси); закачените не се броят.",
  },
];

/// History switches apply immediately instead of waiting for "Запази".
export default function HistorySettings() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");

  useEffect(() => {
    invoke<Settings>("history_settings")
      .then(setSettings)
      .catch((cause) => setError(String(cause)));
  }, []);

  async function toggle(key: keyof Settings, enabled: boolean) {
    if (!settings) return;
    const next = { ...settings, [key]: enabled };
    setError("");
    setNotice("");
    try {
      await invoke("history_save_settings", { settings: next });
      setSettings(next);
    } catch (cause) {
      setError(String(cause));
    }
  }

  async function clearAll() {
    setConfirmClear(false);
    setError("");
    try {
      const removed = await invoke<number>("history_clear", { includePinned: true });
      setNotice(`Изтрити записи: ${removed}.`);
    } catch (cause) {
      setError(String(cause));
    }
  }

  return (
    <section className="provider-group">
      <h2>История</h2>
      <p className="dictionary-note">
        Преводите и търсенията се пазят 30 дни, от чатовете – последните 20; закачените (★) – докато не ги изтриете. Историята е само на този компютър
        (файлът contextai.db в %LOCALAPPDATA%\com.contextai.desktop). Промените важат веднага, без „Запази“.
      </p>
      {switches.map((item) => (
        <article className="provider-card" key={item.key}>
          <div className="provider-heading">
            <div>
              <h3>{item.title}</h3>
              <p>{item.description}</p>
            </div>
            <label className="toggle">
              <input
                type="checkbox"
                checked={settings?.[item.key] ?? false}
                disabled={settings === null}
                onChange={(event) => void toggle(item.key, event.target.checked)}
              />
              <span>Записвай</span>
            </label>
          </div>
        </article>
      ))}
      <article className="provider-card">
        <div className="provider-heading">
          <div>
            <h3>Изтриване на историята</h3>
            <p>Изтрива всички записи, включително закачените. Речниците и настройките не се засягат.</p>
          </div>
        </div>
        <div className="copilot-actions">
          {confirmClear ? (
            <>
              <button className="danger" onClick={() => void clearAll()}>
                Да, изтрий всичко
              </button>
              <button onClick={() => setConfirmClear(false)}>Отказ</button>
            </>
          ) : (
            <button onClick={() => setConfirmClear(true)}>Изтрий цялата история…</button>
          )}
        </div>
      </article>
      {notice && <div className="settings-success dictionary-message">{notice}</div>}
      {error && <div className="error copilot-error">{error}</div>}
    </section>
  );
}
