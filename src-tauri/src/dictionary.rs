//! Tauri commands over `contextai_core::dictionary`: paths from the app, the file picker and the
//! `dictionaries-changed` event. The dictionary logic itself lives in `contextai-core`.

use std::path::PathBuf;

use contextai_core::dictionary::{
    find_dictionaries, Dictionaries, DictionaryConfig, DictionaryInfo, LookupResult,
};
use tauri::{AppHandle, Emitter, Manager, State};

/// `dictionaries.json` in the config folder, indexes in the local data folder
/// (`%LOCALAPPDATA%\com.contextai.desktop\dictionaries` on Windows).
pub fn create(app: &AppHandle) -> Result<Dictionaries, String> {
    let paths = app.path();
    let settings = paths
        .app_config_dir()
        .map_err(|error| error.to_string())?
        .join("dictionaries.json");
    let cache = paths
        .app_local_data_dir()
        .map_err(|error| error.to_string())?
        .join("dictionaries");
    let app = app.clone();
    Ok(Dictionaries::new(settings, cache, move || {
        let _ = app.emit("dictionaries-changed", ());
    }))
}

/// Runs blocking dictionary work off the async runtime.
async fn blocking<T: Send + 'static>(
    dictionaries: &State<'_, Dictionaries>,
    work: impl FnOnce(Dictionaries) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let dictionaries = dictionaries.inner().clone();
    tauri::async_runtime::spawn_blocking(move || work(dictionaries))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub fn dictionary_list(
    dictionaries: State<'_, Dictionaries>,
) -> Result<Vec<DictionaryInfo>, String> {
    dictionaries.list()
}

/// Opens a file or folder picker over the chat window; returns the number of new dictionaries.
#[tauri::command]
pub async fn dictionary_add(
    app: AppHandle,
    dictionaries: State<'_, Dictionaries>,
    folder: bool,
) -> Result<usize, String> {
    use tauri_plugin_dialog::DialogExt;
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = app.dialog().file();
        if let Some(window) = app.get_webview_window("chat-panel") {
            dialog = dialog.set_parent(&window);
        }
        if folder {
            dialog
                .set_title("Папка с речници (.bgl, StarDict)")
                .blocking_pick_folder()
                .map(|folder| vec![folder])
        } else {
            dialog
                .set_title("Речници")
                .add_filter("Речници (.bgl, .ifo)", &["bgl", "BGL", "ifo", "IFO"])
                .blocking_pick_files()
        }
    })
    .await
    .map_err(|error| error.to_string())?;
    let Some(picked) = picked else { return Ok(0) };
    let picked: Vec<PathBuf> = picked
        .into_iter()
        .filter_map(|path| path.into_path().ok())
        .collect();
    let paths = if folder {
        let found: Vec<PathBuf> = picked
            .iter()
            .flat_map(|directory| find_dictionaries(directory))
            .collect();
        if found.is_empty() {
            return Err("В папката няма речници (.bgl или StarDict .ifo).".to_string());
        }
        found
    } else {
        picked
    };
    dictionaries.add(paths)
}

/// Saves the order, the on/off switches and removals.
#[tauri::command]
pub fn dictionary_save(
    state: State<'_, Dictionaries>,
    dictionaries: Vec<DictionaryConfig>,
) -> Result<(), String> {
    state.save(dictionaries)
}

/// Looks a word up and records the first article in the history.
#[tauri::command]
pub async fn dictionary_lookup(
    app: AppHandle,
    dictionaries: State<'_, Dictionaries>,
    query: String,
) -> Result<LookupResult, String> {
    let result = blocking(&dictionaries, move |dictionaries| {
        dictionaries.lookup(&query)
    })
    .await?;
    if let Some(entry) = result.history_entry() {
        crate::history::record(&app, entry);
    }
    Ok(result)
}

#[tauri::command]
pub async fn dictionary_suggest(
    dictionaries: State<'_, Dictionaries>,
    prefix: String,
) -> Result<Vec<String>, String> {
    blocking(&dictionaries, move |dictionaries| {
        dictionaries.suggest(&prefix)
    })
    .await
}
