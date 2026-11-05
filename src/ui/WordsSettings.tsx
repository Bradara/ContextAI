import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type Settings = { dailyNewCards: number };

/// Practice settings apply immediately instead of waiting for "Запази".
export default function WordsSettings() {
  const [value, setValue] = useState("");
  const [saved, setSaved] = useState<number | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    invoke<Settings>("review_settings")
      .then((settings) => {
        setValue(String(settings.dailyNewCards));
        setSaved(settings.dailyNewCards);
      })
      .catch((cause) => setError(String(cause)));
  }, []);

  // Saves a valid number shortly after typing stops.
  useEffect(() => {
    const number = Number(value);
    if (value === "" || !Number.isInteger(number) || number < 0 || number > 500 || number === saved) return;
    const timer = window.setTimeout(() => {
      invoke("review_save_settings", { settings: { dailyNewCards: number } })
        .then(() => {
          setSaved(number);
          setError("");
        })
        .catch((cause) => setError(String(cause)));
    }, 400);
    return () => window.clearTimeout(timer);
  }, [value, saved]);

  return (
    <section className="provider-group">
      <h2>Упражнения</h2>
      <article className="provider-card">
        <div className="provider-heading">
          <div>
            <h3>Нови думи на ден</h3>
            <p>
              Колко нови думи влизат в упражнението за един ден; двете посоки (дума → значение и значение → дума)
              се броят заедно. Повторенията на вече учени думи не се ограничават. Промяната важи веднага.
            </p>
          </div>
        </div>
        <label className="setting-field">
          <span>Лимит (0–500)</span>
          <input
            type="number"
            min={0}
            max={500}
            value={value}
            onChange={(event) => setValue(event.target.value)}
          />
        </label>
      </article>
      {error && <div className="error copilot-error">{error}</div>}
    </section>
  );
}
