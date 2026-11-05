//! Tauri commands over the words to learn in `contextai_core::storage`, plus `cards-changed`
//! events so that the Words view and the ☆ buttons stay in sync.

use contextai_core::storage::{Card, CardEdit, CardQuery, NewCard, SavedCard, Storage};
use tauri::{AppHandle, Emitter, State};

fn changed(app: &AppHandle) {
    let _ = app.emit("cards-changed", ());
    crate::review::refresh(app);
}

#[tauri::command]
pub fn cards_list(storage: State<'_, Storage>, query: CardQuery) -> Result<Vec<Card>, String> {
    storage.cards(&query)
}

#[tauri::command]
pub fn cards_count(storage: State<'_, Storage>) -> Result<u32, String> {
    storage.card_count()
}

/// For each word, whether it is already saved.
#[tauri::command]
pub fn cards_saved(storage: State<'_, Storage>, terms: Vec<String>) -> Result<Vec<bool>, String> {
    storage.saved_terms(&terms)
}

#[tauri::command]
pub fn card_save(
    app: AppHandle,
    storage: State<'_, Storage>,
    card: NewCard,
) -> Result<SavedCard, String> {
    let saved = storage.save_card(card)?;
    changed(&app);
    Ok(saved)
}

#[tauri::command]
pub fn card_update(
    app: AppHandle,
    storage: State<'_, Storage>,
    id: String,
    edit: CardEdit,
) -> Result<Card, String> {
    let card = storage.update_card(&id, &edit)?;
    changed(&app);
    Ok(card)
}

#[tauri::command]
pub fn card_delete(app: AppHandle, storage: State<'_, Storage>, id: String) -> Result<(), String> {
    storage.delete_card(&id)?;
    changed(&app);
    Ok(())
}
