//! Tauri commands for practising words, and the tray counter "5 нови · 12 за повторение":
//! in the icon's tooltip, in the "Упражнение" menu item and as a dot on the icon. Windows shows
//! no text on tray icons, hence the dot.

use std::{sync::Mutex, time::Duration};

use contextai_core::{
    srs::Rating,
    storage::{Direction, ReviewCounts, ReviewItem, ReviewSettings, Storage},
};
use tauri::{image::Image, menu::MenuItem, AppHandle, Emitter, Manager, State};

/// The counts are recomputed this often, so that words due later today show up.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const TRAY_ID: &str = "contextai-tray";

/// The tray's "Упражнение" item and the last counts shown.
pub struct ReviewTray {
    pub item: MenuItem<tauri::Wry>,
    counts: Mutex<ReviewCounts>,
}

impl ReviewTray {
    pub fn new(item: MenuItem<tauri::Wry>) -> Self {
        Self {
            item,
            counts: Mutex::new(ReviewCounts::default()),
        }
    }
}

/// The 32×32 tray icon: a blue square, with an orange dot when words are waiting.
pub fn tray_icon(dot: bool) -> Image<'static> {
    let pixels = (0..32 * 32)
        .flat_map(|pixel| {
            let (x, y) = (pixel % 32, pixel / 32);
            let (dx, dy) = (x - 25, y - 7);
            if dot && dx * dx + dy * dy <= 36 {
                [255_u8, 159, 67, 255]
            } else if (8..24).contains(&x) && (8..24).contains(&y) {
                [72, 131, 255, 255]
            } else {
                [0, 0, 0, 0]
            }
        })
        .collect();
    Image::new_owned(pixels, 32, 32)
}

fn counts_text(counts: &ReviewCounts) -> String {
    format!("{} нови · {} за повторение", counts.new, counts.due)
}

/// Rewrites the tooltip from the detection state and the last counts.
pub fn update_tray(app: &AppHandle, paused: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let counts = app
        .try_state::<ReviewTray>()
        .map(|state| {
            state
                .counts
                .lock()
                .map(|counts| counts.clone())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let mut tooltip = "ContextAI".to_string();
    if paused {
        tooltip.push_str(" (засичането е спряно)");
    }
    if counts.total() > 0 {
        tooltip.push_str(&format!("\nДуми: {}", counts_text(&counts)));
    }
    let _ = tray.set_tooltip(Some(tooltip));
    let _ = tray.set_icon(Some(tray_icon(counts.total() > 0)));
}

/// Recomputes today's counts, updates the tray and tells the windows.
pub fn refresh(app: &AppHandle) {
    let counts = match app.state::<Storage>().review_counts() {
        Ok(counts) => counts,
        Err(error) => {
            eprintln!("Could not count the words to review: {error}");
            return;
        }
    };
    if let Some(state) = app.try_state::<ReviewTray>() {
        let label = if counts.total() > 0 {
            format!("Упражнение ({} · {})", counts.new, counts.due)
        } else {
            "Упражнение".to_string()
        };
        let _ = state.item.set_text(label);
        if let Ok(mut last) = state.counts.lock() {
            *last = counts.clone();
        }
    }
    update_tray(app, crate::detection_paused(app));
    let _ = app.emit("review-changed", counts);
}

/// Refreshes the counter now and every few minutes (words come due, the day changes).
pub fn start_refreshing(app: AppHandle) {
    std::thread::spawn(move || loop {
        refresh(&app);
        std::thread::sleep(REFRESH_INTERVAL);
    });
}

/// Shows the chat window on its practice view.
pub fn open(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("chat-panel")
        .ok_or_else(|| "Chat window is unavailable".to_string())?;
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())?;
    app.emit_to("chat-panel", "review-open", ())
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn review_counts(storage: State<'_, Storage>) -> Result<ReviewCounts, String> {
    storage.review_counts()
}

#[tauri::command]
pub fn review_next(storage: State<'_, Storage>) -> Result<Option<ReviewItem>, String> {
    storage.next_review()
}

#[tauri::command]
pub fn review_answer(
    app: AppHandle,
    storage: State<'_, Storage>,
    card_id: String,
    direction: Direction,
    rating: Rating,
) -> Result<ReviewCounts, String> {
    let counts = storage.answer_review(&card_id, direction, rating)?;
    refresh(&app);
    Ok(counts)
}

#[tauri::command]
pub fn review_settings(storage: State<'_, Storage>) -> Result<ReviewSettings, String> {
    storage.review_settings()
}

#[tauri::command]
pub fn review_save_settings(
    app: AppHandle,
    storage: State<'_, Storage>,
    settings: ReviewSettings,
) -> Result<(), String> {
    storage.set_review_settings(&settings)?;
    refresh(&app);
    Ok(())
}
