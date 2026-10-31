import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type ReaderStatus = "loading" | "playing" | "paused" | "finished" | "error";
type ReaderState = { state: ReaderStatus; message: string };
type ReaderSession = ReaderState & { text: string };

const statusLabels: Record<ReaderStatus, string> = {
  loading: "Зареждане на глас…",
  playing: "Чете се",
  paused: "На пауза",
  finished: "Приключи",
  error: "Грешка",
};

export default function Reader() {
  const [text, setText] = useState("");
  const [status, setStatus] = useState<ReaderStatus>("loading");
  const [message, setMessage] = useState("");
  const textRef = useRef<HTMLParagraphElement>(null);

  useEffect(() => {
    const unlisteners: (() => void)[] = [];
    let disposed = false;
    const keep = (unlisten: () => void) => {
      if (disposed) unlisten();
      else unlisteners.push(unlisten);
    };
    void (async () => {
      keep(
        await listen<string>("reader-text", ({ payload }) => {
          setText(payload);
          setStatus("loading");
          setMessage("");
          textRef.current?.scrollTo({ top: 0 });
        }),
      );
      keep(
        await listen<ReaderState>("reader-state", ({ payload }) => {
          setStatus(payload.state);
          setMessage(payload.message);
        }),
      );
      const session = await invoke<ReaderSession>("get_reader_session");
      if (!session.state) return;
      setText(session.text);
      setStatus(session.state);
      setMessage(session.message);
    })();
    return () => {
      disposed = true;
      for (const unlisten of unlisteners) unlisten();
    };
  }, []);

  const control = (action: "pause" | "resume" | "stop") =>
    void invoke("reader_control", { action }).catch((cause) => {
      setStatus("error");
      setMessage(String(cause));
    });

  return (
    <main className="reader">
      <header className="reader-header">
        <span className={`reader-status ${status}`}>{statusLabels[status]}</span>
        <button className="reader-close" title="Затвори" onClick={() => control("stop")}>
          ×
        </button>
      </header>
      <p className="reader-text" ref={textRef}>{text}</p>
      {status === "error" && <div className="reader-error">{message}</div>}
      <footer className="reader-controls">
        {status === "playing" && (
          <button onClick={() => control("pause")}>⏸ Пауза</button>
        )}
        {status === "paused" && (
          <button onClick={() => control("resume")}>▶ Продължи</button>
        )}
        <button onClick={() => control("stop")}>■ Спри</button>
      </footer>
    </main>
  );
}
