import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/// Settings that apply immediately instead of waiting for "Запази".
export default function OtherSettings() {
  const [autostart, setAutostart] = useState<boolean | null>(null);
  const [detection, setDetection] = useState<boolean | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    invoke<boolean>("get_autostart")
      .then(setAutostart)
      .catch((cause) => setError(String(cause)));
    invoke<boolean>("get_detection_paused")
      .then((paused) => setDetection(!paused))
      .catch((cause) => setError(String(cause)));
    // The tray menu can toggle detection while the settings are open.
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<{ paused: boolean }>("detection-paused", ({ payload }) => setDetection(!payload.paused)).then(
      (stop) => {
        if (disposed) stop();
        else unlisten = stop;
      },
    );
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  async function toggleAutostart(enabled: boolean) {
    setError("");
    try {
      await invoke("set_autostart", { enabled });
      setAutostart(await invoke<boolean>("get_autostart"));
    } catch (cause) {
      setError(String(cause));
    }
  }

  async function toggleDetection(enabled: boolean) {
    setError("");
    try {
      await invoke("set_detection_paused", { paused: !enabled });
      setDetection(enabled);
    } catch (cause) {
      setError(String(cause));
    }
  }

  return (
    <section className="provider-group">
      <h2>Система</h2>
      <article className="provider-card">
        <div className="provider-heading">
          <div>
            <h3>Стартиране с Windows</h3>
            <p>
              ContextAI се стартира в системния трей при влизане в Windows. Промяната важи веднага, без „Запази“.
              {import.meta.env.DEV && " В режим на разработка се записва пътят до dev версията."}
            </p>
          </div>
          <label className="toggle">
            <input
              type="checkbox"
              checked={autostart ?? false}
              disabled={autostart === null}
              onChange={(event) => void toggleAutostart(event.target.checked)}
            />
            <span>Включено</span>
          </label>
        </div>
      </article>
      <article className="provider-card">
        <div className="provider-heading">
          <div>
            <h3>Засичане на селекция</h3>
            <p>
              Иконката до курсора се показва след влачене или двоен клик. Изключете го, ако ползвате SnipDo
              (разширението ContextAI), за да не се появяват две менюта. Същото е „Спри засичането“ в трея;
              изборът се запомня. Менюто за маркирания текст се отваря и с Ctrl+Alt+Shift+F8, дори при изключено
              засичане.
            </p>
          </div>
          <label className="toggle">
            <input
              type="checkbox"
              checked={detection ?? false}
              disabled={detection === null}
              onChange={(event) => void toggleDetection(event.target.checked)}
            />
            <span>Включено</span>
          </label>
        </div>
      </article>
      {error && <div className="error copilot-error">{error}</div>}
    </section>
  );
}
