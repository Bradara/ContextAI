//! Tauri commands over the history in `contextai_core::storage`, plus `history-changed` events so
//! that an open History view refreshes when something is recorded.

use contextai_core::storage::{
    HistoryEntry, HistoryQuery, HistorySettings, NewHistoryEntry, Storage,
};
use tauri::{AppHandle, Emitter, Manager, State};

/// `contextai.db` in the local data folder. A database that cannot be opened is replaced by one
/// in memory, so that the rest of ContextAI keeps working (without saving history).
pub fn open(app: &AppHandle) -> Storage {
    let opened = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())
        .and_then(|directory| Storage::open(&directory.join("contextai.db")));
    match opened {
        Ok(storage) => storage,
        Err(error) => {
            eprintln!("Could not open the database, history is not saved: {error}");
            Storage::open_in_memory().expect("an in-memory SQLite database")
        }
    }
}

fn changed(app: &AppHandle) {
    let _ = app.emit("history-changed", ());
}

/// Records an entry if its kind is switched on. A failure is only logged: the translation or
/// lookup itself already succeeded.
pub fn record(app: &AppHandle, entry: NewHistoryEntry) {
    match app.state::<Storage>().add_history(entry) {
        Ok(Some(_)) => changed(app),
        Ok(None) => {}
        Err(error) => eprintln!("Could not save the history entry: {error}"),
    }
}

#[tauri::command]
pub fn history_list(
    storage: State<'_, Storage>,
    query: HistoryQuery,
) -> Result<Vec<HistoryEntry>, String> {
    storage.history(&query)
}

/// For results produced in the WebView (a translation by the chat model).
#[tauri::command]
pub fn history_add(app: AppHandle, entry: NewHistoryEntry) {
    record(&app, entry);
}

#[tauri::command]
pub fn history_pin(
    app: AppHandle,
    storage: State<'_, Storage>,
    id: String,
    pinned: bool,
) -> Result<(), String> {
    storage.pin_history(&id, pinned)?;
    changed(&app);
    Ok(())
}

#[tauri::command]
pub fn history_delete(
    app: AppHandle,
    storage: State<'_, Storage>,
    id: String,
) -> Result<(), String> {
    storage.delete_history(&id)?;
    changed(&app);
    Ok(())
}

#[tauri::command]
pub fn history_clear(
    app: AppHandle,
    storage: State<'_, Storage>,
    include_pinned: bool,
) -> Result<usize, String> {
    let removed = storage.clear_history(include_pinned)?;
    changed(&app);
    Ok(removed)
}

#[tauri::command]
pub fn history_settings(storage: State<'_, Storage>) -> Result<HistorySettings, String> {
    storage.history_settings()
}

#[tauri::command]
pub fn history_save_settings(
    storage: State<'_, Storage>,
    settings: HistorySettings,
) -> Result<(), String> {
    storage.set_history_settings(&settings)
}
