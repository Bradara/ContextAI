import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

type CopilotStatus = { cliPath?: string; authenticated: boolean; login?: string; message: string };
type CopilotModel = { id: string; name: string; multiplier?: number };
type Profile = { enabled: boolean; endpoint: string; model: string };

export default function CopilotSettings({
  profile,
  onChange,
}: {
  profile: Profile;
  onChange: (patch: Partial<Profile>) => void;
}) {
  const [status, setStatus] = useState<CopilotStatus | null>(null);
  const [models, setModels] = useState<CopilotModel[]>([]);
  const [working, setWorking] = useState("");
  const [error, setError] = useState("");

  async function load(refresh: boolean) {
    setError("");
    setWorking(refresh ? "Обновяване на моделите…" : "Проверка на Copilot…");
    try {
      const current = await invoke<CopilotStatus>("copilot_status");
      setStatus(current);
      if (current.authenticated) {
        setModels(await invoke<CopilotModel[]>("copilot_models", { refresh }));
      }
    } catch (cause) {
      setError(String(cause));
    } finally {
      setWorking("");
    }
  }

  async function login() {
    setError("");
    setWorking("Завършете входа в отворения браузър…");
    try {
      await invoke("copilot_login");
    } catch (cause) {
      setError(String(cause));
    }
    await load(true);
  }

  // Starting the CLI takes a moment, so only do it while the profile is enabled.
  useEffect(() => {
    if (profile.enabled) void load(false);
  }, [profile.enabled]);

  const selectedKnown = models.some((model) => model.id === profile.model);

  return (
    <>
      <label className="setting-field">
        <span>Път до copilot.exe</span>
        <input
          value={profile.endpoint}
          onChange={(event) => onChange({ endpoint: event.target.value })}
          placeholder="Празно = автоматично откриване"
          spellCheck={false}
        />
      </label>

      {profile.enabled && (
        <>
          <div className={`copilot-status${status?.authenticated ? " ok" : ""}`}>
            {working || status?.message || "—"}
          </div>
          <div className="copilot-actions">
            {status?.cliPath && !status.authenticated && (
              <button disabled={!!working} onClick={() => void login()}>
                Вход в GitHub
              </button>
            )}
            <button disabled={!!working} onClick={() => void load(true)}>
              Обнови моделите
            </button>
          </div>
          <label className="setting-field">
            <span>Модел</span>
            <select
              value={profile.model}
              onChange={(event) => onChange({ model: event.target.value })}
              disabled={!models.length}
            >
              <option value="">По подразбиране (според Copilot)</option>
              {!selectedKnown && profile.model && <option value={profile.model}>{profile.model}</option>}
              {models.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.name}
                  {model.multiplier !== undefined && model.multiplier !== null ? ` · ×${model.multiplier}` : ""}
                </option>
              ))}
            </select>
          </label>
          {error && <div className="error copilot-error">{error}</div>}
        </>
      )}
    </>
  );
}
