export type MainView = "chat" | "dictionary" | "history";

const views: { id: MainView; label: string }[] = [
  { id: "chat", label: "Чат" },
  { id: "dictionary", label: "Речник" },
  { id: "history", label: "История" },
];

/// The row of tabs under the header of the chat window.
export default function ViewTabs({ current, onSelect }: { current: MainView; onSelect: (view: MainView) => void }) {
  return (
    <nav className="view-tabs" role="tablist">
      {views.map((view) => (
        <button
          key={view.id}
          role="tab"
          aria-selected={view.id === current}
          className={view.id === current ? "active" : ""}
          onClick={() => onSelect(view.id)}
        >
          {view.label}
        </button>
      ))}
    </nav>
  );
}
