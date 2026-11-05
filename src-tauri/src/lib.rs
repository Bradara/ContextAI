use std::{
    fs,
    net::IpAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
mod autostart;
mod cards;
mod copilot;
mod dictionary;
mod history;
mod review;

use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager,
};

const POPUP_COLLAPSED_SIZE: (f64, f64) = (30.0, 30.0);
const POPUP_EXPANDED_SIZE: (f64, f64) = (310.0, 56.0);
const PAUSE_LABEL: &str = "Спри засичането";
const RESUME_LABEL: &str = "Пусни засичането";

struct AppState {
    paused: Arc<AtomicBool>,
    /// Set while a synthetic Ctrl+C runs, so its key events do not hide the popup.
    capture_active: Arc<AtomicBool>,
    popup_expanded: Arc<AtomicBool>,
    popup_position: Arc<Mutex<Option<(i32, i32)>>>,
    reading_token: AtomicU64,
    player: Mutex<mpsc::Sender<PlayerCommand>>,
    reader_session: Mutex<ReaderSession>,
}

impl AppState {
    fn new(player: mpsc::Sender<PlayerCommand>) -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            capture_active: Arc::new(AtomicBool::new(false)),
            popup_expanded: Arc::new(AtomicBool::new(false)),
            popup_position: Arc::new(Mutex::new(None)),
            reading_token: AtomicU64::new(0),
            player: Mutex::new(player),
            reader_session: Mutex::new(ReaderSession::default()),
        }
    }
}

#[derive(Clone, Serialize)]
struct SelectionPayload {
    text: String,
    /// Opens the action bar directly instead of the small trigger icon.
    expanded: bool,
    /// A popup action to run right away (for example `translate-read`).
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
}

#[derive(Clone, Serialize)]
struct StreamPayload {
    request_id: String,
    content: String,
}

#[derive(Deserialize, Serialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ProviderProfile {
    id: String,
    enabled: bool,
    endpoint: String,
    model: String,
    region: String,
    voice: String,
    /// Extra Azure voices, one per line; the language comes from the voice prefix.
    /// `None` (older settings) means the built-in list.
    #[serde(skip_serializing_if = "Option::is_none")]
    voices: Option<String>,
    /// Gemini: `Some(false)` asks for the least thinking the model allows (faster replies).
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<bool>,
}

#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct ProviderSettings {
    active_chat_provider: String,
    #[serde(default)]
    active_translation_provider: String,
    #[serde(default)]
    translation_target_language: String,
    providers: Vec<ProviderProfile>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TranslationResult {
    text: String,
    detected_language: String,
    target_language: String,
}

const PROVIDER_IDS: [&str; 10] = [
    "llama-cpp",
    "ollama",
    "deepseek",
    "qwen",
    "github-copilot",
    "gemini",
    "deepl",
    "azure-translator",
    "google-translate",
    "azure-tts",
];

fn is_valid_provider_id(provider_id: &str) -> bool {
    PROVIDER_IDS.contains(&provider_id)
}

fn config_file(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Неуспешно създаване на директорията за настройки {}: {error}",
            directory.display()
        )
    })?;
    Ok(directory.join(name))
}

fn settings_file(app: &AppHandle) -> Result<PathBuf, String> {
    config_file(app, "providers.json")
}

/// App-wide switches that are not tied to a provider.
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Preferences {
    /// Turns off the built-in selection detection, e.g. when SnipDo hands over the text instead.
    detection_paused: bool,
}

fn load_preferences(app: &AppHandle) -> Preferences {
    config_file(app, "preferences.json")
        .ok()
        .and_then(|path| fs::read(path).ok())
        .and_then(|contents| serde_json::from_slice(&contents).ok())
        .unwrap_or_default()
}

fn save_preferences(app: &AppHandle, preferences: &Preferences) -> Result<(), String> {
    let path = config_file(app, "preferences.json")?;
    let serialized = serde_json::to_vec_pretty(preferences).map_err(|error| error.to_string())?;
    fs::write(&path, serialized).map_err(|error| {
        format!(
            "Неуспешно записване на настройките {}: {error}",
            path.display()
        )
    })
}

/// The tray item whose label follows the detection state.
struct PauseMenuItem(MenuItem<tauri::Wry>);

fn apply_detection_paused(app: &AppHandle, paused: bool) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.paused.store(paused, Ordering::Relaxed);
    if let Some(item) = app.try_state::<PauseMenuItem>() {
        let _ = item
            .0
            .set_text(if paused { RESUME_LABEL } else { PAUSE_LABEL });
    }
    review::update_tray(app, paused);
    if paused {
        let _ = hide_popup(app.clone());
    }
    let _ = app.emit("detection-paused", serde_json::json!({ "paused": paused }));
    let mut preferences = load_preferences(app);
    preferences.detection_paused = paused;
    save_preferences(app, &preferences)
}

fn detection_paused(app: &AppHandle) -> bool {
    app.try_state::<AppState>()
        .is_some_and(|state| state.paused.load(Ordering::Relaxed))
}

#[tauri::command]
fn get_detection_paused(app: AppHandle) -> bool {
    detection_paused(&app)
}

#[tauri::command]
fn set_detection_paused(app: AppHandle, paused: bool) -> Result<(), String> {
    apply_detection_paused(&app, paused)
}

#[tauri::command]
fn load_provider_settings(app: AppHandle) -> Result<ProviderSettings, String> {
    let path = settings_file(&app)?;
    match fs::read(&path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map_err(|error| format!("Невалидни настройки в {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ProviderSettings::default())
        }
        Err(error) => Err(format!(
            "Неуспешно прочитане на настройките {}: {error}",
            path.display()
        )),
    }
}

#[tauri::command]
fn save_provider_settings(app: AppHandle, settings: ProviderSettings) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    for provider in &settings.providers {
        if !is_valid_provider_id(&provider.id) {
            return Err("Невалиден идентификатор на доставчик.".to_string());
        }
        if !ids.insert(provider.id.as_str()) {
            return Err(format!("Дублиран профил за доставчик: {}", provider.id));
        }
    }
    if !matches!(
        settings.active_translation_provider.as_str(),
        "" | "ai" | "deepl" | "azure-translator" | "google-translate"
    ) {
        return Err("Невалидна услуга за превод.".to_string());
    }
    if !settings.translation_target_language.is_empty()
        && translation_language(&settings.translation_target_language).is_none()
    {
        return Err("Невалиден целеви език за превод.".to_string());
    }

    let path = settings_file(&app)?;
    let temporary_path = path.with_extension("json.tmp");
    let serialized = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    fs::write(&temporary_path, serialized).map_err(|error| {
        format!(
            "Неуспешно записване на настройките {}: {error}",
            temporary_path.display()
        )
    })?;
    fs::rename(&temporary_path, &path).map_err(|error| {
        format!(
            "Неуспешно подменяне на настройките {}: {error}",
            path.display()
        )
    })
}

#[cfg(windows)]
fn credential_entry(provider_id: &str) -> Result<keyring::Entry, String> {
    if !is_valid_provider_id(provider_id) {
        return Err("Невалиден идентификатор на доставчик.".to_string());
    }
    keyring::Entry::new("com.contextai.desktop", provider_id)
        .map_err(|error| format!("Неуспешно достъпване на Windows Credential Manager: {error}"))
}

#[cfg(windows)]
fn load_provider_secret(provider_id: &str) -> Result<Option<String>, String> {
    match credential_entry(provider_id)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(format!(
            "Неуспешно прочитане на ключа от Windows Credential Manager: {error}"
        )),
    }
}

#[cfg(not(windows))]
fn load_provider_secret(_provider_id: &str) -> Result<Option<String>, String> {
    Ok(None)
}

#[tauri::command]
fn save_provider_secret(provider_id: String, secret: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        let entry = credential_entry(&provider_id)?;
        if secret.trim().is_empty() {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(format!("Неуспешно изтриване на API ключа: {error}")),
            }
        } else {
            entry
                .set_password(secret.trim())
                .map_err(|error| format!("Неуспешно запазване на API ключа: {error}"))
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (provider_id, secret);
        Err("Съхранението на API ключове в момента е налично само за Windows.".to_string())
    }
}

#[tauri::command]
fn provider_secret_is_saved(provider_id: String) -> Result<bool, String> {
    #[cfg(windows)]
    {
        match credential_entry(&provider_id)?.get_password() {
            Ok(_) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(error) => Err(format!(
                "Неуспешна проверка на ключа в Windows Credential Manager: {error}"
            )),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = provider_id;
        Err("Съхранението на API ключове в момента е налично само за Windows.".to_string())
    }
}

#[tauri::command]
fn hide_popup(app: AppHandle) -> Result<(), String> {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(mut position) = state.popup_position.lock() {
            *position = None;
        }
        state.popup_expanded.store(false, Ordering::Relaxed);
    }
    let window = app
        .get_webview_window("popup-menu")
        .ok_or_else(|| "Popup window is unavailable".to_string())?;
    let _ = window.set_size(tauri::LogicalSize::new(
        POPUP_COLLAPSED_SIZE.0,
        POPUP_COLLAPSED_SIZE.1,
    ));
    window.hide().map_err(|error| error.to_string())
}

#[tauri::command]
fn open_chat(
    app: AppHandle,
    text: String,
    action: String,
    service: Option<String>,
) -> Result<(), String> {
    hide_popup(app.clone())?;
    let window = app
        .get_webview_window("chat-panel")
        .ok_or_else(|| "Chat window is unavailable".to_string())?;
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())?;
    app.emit_to(
        "chat-panel",
        "chat-start",
        serde_json::json!({ "text": text, "action": action, "service": service }),
    )
    .map_err(|error| error.to_string())
}

/// Shows the dictionary view of the chat window and looks up `text` (empty: just opens it).
#[tauri::command]
fn open_dictionary(app: AppHandle, text: String) -> Result<(), String> {
    hide_popup(app.clone())?;
    let window = app
        .get_webview_window("chat-panel")
        .ok_or_else(|| "Chat window is unavailable".to_string())?;
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())?;
    app.emit_to(
        "chat-panel",
        "dictionary-open",
        serde_json::json!({ "text": text }),
    )
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn open_settings(app: AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("chat-panel")
        .ok_or_else(|| "Chat window is unavailable".to_string())?;
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())?;
    app.emit_to("chat-panel", "settings-open", ())
        .map_err(|error| error.to_string())
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn is_valid_azure_region(region: &str) -> bool {
    !region.is_empty()
        && region
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

/// ISO 639-1 → ISO 639-3 (whatlang) codes for languages with Azure neural voices.
const LANGUAGE_CODES: [(&str, &str); 31] = [
    ("bg", "bul"),
    ("en", "eng"),
    ("de", "deu"),
    ("fr", "fra"),
    ("es", "spa"),
    ("it", "ita"),
    ("pt", "por"),
    ("nl", "nld"),
    ("ru", "rus"),
    ("uk", "ukr"),
    ("pl", "pol"),
    ("cs", "ces"),
    ("sk", "slk"),
    ("sl", "slv"),
    ("hr", "hrv"),
    ("sr", "srp"),
    ("mk", "mkd"),
    ("el", "ell"),
    ("tr", "tur"),
    ("ro", "ron"),
    ("hu", "hun"),
    ("sv", "swe"),
    ("da", "dan"),
    ("nb", "nob"),
    ("fi", "fin"),
    ("ja", "jpn"),
    ("ko", "kor"),
    ("zh", "cmn"),
    ("ar", "ara"),
    ("he", "heb"),
    ("hi", "hin"),
];

const DEFAULT_EXTRA_VOICES: &str = "en-US-AvaNeural
de-DE-KatjaNeural
fr-FR-DeniseNeural
es-ES-ElviraNeural
it-IT-ElsaNeural
ru-RU-SvetlanaNeural
uk-UA-PolinaNeural
el-GR-AthinaNeural
tr-TR-EmelNeural";

fn voice_language(voice: &str) -> Option<whatlang::Lang> {
    let prefix = voice.split('-').next()?.to_ascii_lowercase();
    LANGUAGE_CODES
        .iter()
        .find(|(short, _)| *short == prefix)
        .and_then(|(_, long)| whatlang::Lang::from_code(*long))
}

/// Picks the voice whose language matches the text. Detection is limited to the
/// configured languages, which keeps close languages (e.g. bg/ru) apart when only
/// one of them has a voice. Falls back to the default voice when unsure.
fn pick_voice<'a>(text: &str, default_voice: &'a str, voices: &'a str) -> &'a str {
    let candidates = std::iter::once(default_voice)
        .chain(voices.lines().map(str::trim))
        .filter(|voice| {
            !voice.is_empty()
                && voice.len() <= 64
                && voice
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
        .filter_map(|voice| Some((voice_language(voice)?, voice)))
        .collect::<Vec<_>>();
    let mut languages = candidates
        .iter()
        .map(|(language, _)| *language)
        .collect::<Vec<_>>();
    languages.sort_by_key(|language| language.code());
    languages.dedup();
    if languages.len() < 2 {
        return default_voice;
    }
    let Some(info) = whatlang::Detector::with_allowlist(languages).detect(text) else {
        return default_voice;
    };
    let default_language = voice_language(default_voice);
    // Short texts are rarely "reliable", but a different script is still a clear signal.
    let other_script =
        default_language.is_none_or(|language| !info.script().langs().contains(&language));
    if !info.is_reliable() && !other_script {
        return default_voice;
    }
    candidates
        .iter()
        .find(|(language, _)| *language == info.lang())
        .map_or(default_voice, |(_, voice)| voice)
}

async fn fetch_speech(app: AppHandle, text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Няма текст за четене.".to_string());
    }
    if text.chars().count() > 5000 {
        return Err("Текстът е твърде дълъг за четене (максимум 5000 знака).".to_string());
    }
    let profile = load_provider_settings(app)?
        .providers
        .into_iter()
        .find(|provider| provider.id == "azure-tts" && provider.enabled)
        .ok_or_else(|| "Активирайте и запазете Azure Speech TTS в настройките.".to_string())?;
    let region = profile.region.trim().to_ascii_lowercase();
    if !is_valid_azure_region(&region) {
        return Err(
            "Задайте валиден Azure регион (например westeurope) в настройките.".to_string(),
        );
    }
    let key = load_provider_secret("azure-tts")?
        .ok_or_else(|| "Липсва Azure Speech ключ в настройките.".to_string())?;
    let default_voice = if profile.voice.trim().is_empty() {
        "bg-BG-KalinaNeural"
    } else {
        profile.voice.trim()
    };
    let voices = profile.voices.as_deref().unwrap_or(DEFAULT_EXTRA_VOICES);
    let voice = pick_voice(text, default_voice, voices);
    let language = voice.splitn(3, '-').take(2).collect::<Vec<_>>().join("-");
    let ssml = format!(
        "<speak version='1.0' xml:lang='{}' xmlns='http://www.w3.org/2001/10/synthesis'><voice name='{}'>{}</voice></speak>",
        escape_xml(&language),
        escape_xml(voice),
        escape_xml(text)
    );
    let response = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Неуспешна конфигурация на Azure клиента: {error}"))?
        .post(format!(
            "https://{region}.tts.speech.microsoft.com/cognitiveservices/v1"
        ))
        .header("Ocp-Apim-Subscription-Key", key)
        .header("Content-Type", "application/ssml+xml")
        .header("X-Microsoft-OutputFormat", "riff-24khz-16bit-mono-pcm")
        .header("User-Agent", "ContextAI")
        .body(ssml)
        .send()
        .await
        .map_err(|error| format!("Неуспешна връзка с Azure Speech: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.map_err(|error| {
            format!("Azure Speech върна {status}, но грешката не може да се прочете: {error}")
        })?;
        return Err(format!("Azure Speech върна {status}: {}", body.trim()));
    }
    let audio = response
        .bytes()
        .await
        .map_err(|error| format!("Неуспешно получаване на аудиото: {error}"))?;
    Ok(audio.to_vec())
}

enum PlayerCommand {
    Play(Vec<u8>),
    Pause,
    Resume,
    Stop,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReaderSession {
    text: String,
    state: String,
    message: String,
}

fn emit_reader_state(app: &AppHandle, state: &'static str, message: impl Into<String>) {
    let message = message.into();
    if let Some(app_state) = app.try_state::<AppState>() {
        if let Ok(mut session) = app_state.reader_session.lock() {
            session.state = state.to_string();
            session.message = message.clone();
        }
    }
    let _ = app.emit_to(
        "reader",
        "reader-state",
        ReaderSession {
            state: state.to_string(),
            message,
            ..ReaderSession::default()
        },
    );
}

struct Playback {
    // Field order matters: the sink must drop before the stream it plays through.
    sink: rodio::Sink,
    _stream: rodio::OutputStream,
    device_lost: Arc<AtomicBool>,
}

// Opens the current default output device, so a changed device (e.g. unplugged headphones) is picked up.
fn open_playback(audio: &[u8], offset: Duration) -> Result<Playback, String> {
    use rodio::Source;

    let device_lost = Arc::new(AtomicBool::new(false));
    let lost = Arc::clone(&device_lost);
    let mut stream = rodio::OutputStreamBuilder::from_default_device()
        .map_err(|error| format!("Няма достъпно аудио устройство: {error}"))?
        .with_error_callback(move |_| lost.store(true, Ordering::SeqCst))
        .open_stream_or_fallback()
        .map_err(|error| format!("Няма достъпно аудио устройство: {error}"))?;
    stream.log_on_drop(false);
    let source = rodio::Decoder::new(std::io::Cursor::new(audio.to_vec()))
        .map_err(|error| format!("Неуспешно декодиране на аудиото: {error}"))?;
    let sink = rodio::Sink::connect_new(stream.mixer());
    sink.append(source.skip_duration(offset));
    Ok(Playback {
        sink,
        _stream: stream,
        device_lost,
    })
}

// The audio output stream is not Send, so it lives on its own thread for the whole app lifetime.
fn spawn_player(app: AppHandle) -> mpsc::Sender<PlayerCommand> {
    let (sender, receiver) = mpsc::channel::<PlayerCommand>();
    thread::spawn(move || {
        let mut playback: Option<Playback> = None;
        let mut last_audio: Vec<u8> = Vec::new();
        loop {
            match receiver.recv_timeout(Duration::from_millis(150)) {
                Ok(PlayerCommand::Play(audio)) => {
                    playback = None;
                    match open_playback(&audio, Duration::ZERO) {
                        Ok(opened) => {
                            playback = Some(opened);
                            last_audio = audio;
                            emit_reader_state(&app, "playing", "");
                        }
                        Err(error) => emit_reader_state(&app, "error", error),
                    }
                }
                Ok(PlayerCommand::Pause) => {
                    if let Some(playback) = playback.as_ref() {
                        playback.sink.pause();
                        emit_reader_state(&app, "paused", "");
                    }
                }
                Ok(PlayerCommand::Resume) => {
                    if let Some(playback) = playback.as_ref() {
                        playback.sink.play();
                        emit_reader_state(&app, "playing", "");
                    }
                }
                Ok(PlayerCommand::Stop) => playback = None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let Some(current) = playback.as_ref() else {
                        continue;
                    };
                    if current.device_lost.load(Ordering::SeqCst) {
                        // Continue from the same position on the new default device.
                        let position = current.sink.get_pos();
                        let was_paused = current.sink.is_paused();
                        playback = None;
                        match open_playback(&last_audio, position) {
                            Ok(reopened) => {
                                if was_paused {
                                    reopened.sink.pause();
                                }
                                playback = Some(reopened);
                            }
                            Err(error) => emit_reader_state(&app, "error", error),
                        }
                    } else if current.sink.empty() && !current.sink.is_paused() {
                        playback = None;
                        emit_reader_state(&app, "finished", "");
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    sender
}

fn send_player_command(app: &AppHandle, command: PlayerCommand) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(player) = state.player.lock() {
            let _ = player.send(command);
        }
    }
}

fn show_reader(app: &AppHandle, text: &str) -> Result<(), String> {
    let window = app
        .get_webview_window("reader")
        .ok_or_else(|| "Reader window is unavailable".to_string())?;
    if let (Ok(Some(monitor)), Ok(size)) = (app.primary_monitor(), window.outer_size()) {
        let scale = monitor.scale_factor();
        let position = monitor.position();
        let screen = monitor.size();
        let x = position.x + screen.width as i32 - size.width as i32 - (24.0 * scale) as i32;
        let y = position.y + screen.height as i32 - size.height as i32 - (72.0 * scale) as i32;
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    }
    if let Some(app_state) = app.try_state::<AppState>() {
        if let Ok(mut session) = app_state.reader_session.lock() {
            session.text = text.to_string();
            session.state = "loading".to_string();
            session.message.clear();
        }
    }
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())?;
    app.emit_to("reader", "reader-text", text)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_reader_session(app: AppHandle) -> Result<ReaderSession, String> {
    app.state::<AppState>()
        .reader_session
        .lock()
        .map(|session| session.clone())
        .map_err(|_| "Неуспешен достъп до състоянието на четеца.".to_string())
}

#[tauri::command]
async fn start_reading(app: AppHandle, text: String) -> Result<(), String> {
    let token = {
        let state = app.state::<AppState>();
        state.reading_token.fetch_add(1, Ordering::SeqCst) + 1
    };
    send_player_command(&app, PlayerCommand::Stop);
    let _ = hide_popup(app.clone());
    if let Err(error) = show_reader(&app, text.trim()) {
        eprintln!("Could not show reader window: {error}");
        return Err(error);
    }
    emit_reader_state(&app, "loading", "");

    match fetch_speech(app.clone(), &text).await {
        Ok(audio) => {
            let current = app.state::<AppState>().reading_token.load(Ordering::SeqCst);
            if current == token {
                send_player_command(&app, PlayerCommand::Play(audio));
            }
        }
        Err(error) => {
            let current = app.state::<AppState>().reading_token.load(Ordering::SeqCst);
            if current == token {
                emit_reader_state(&app, "error", error);
            }
        }
    }
    Ok(())
}

#[tauri::command]
fn reader_control(app: AppHandle, action: String) -> Result<(), String> {
    match action.as_str() {
        "pause" => send_player_command(&app, PlayerCommand::Pause),
        "resume" => send_player_command(&app, PlayerCommand::Resume),
        "stop" => {
            app.state::<AppState>()
                .reading_token
                .fetch_add(1, Ordering::SeqCst);
            send_player_command(&app, PlayerCommand::Stop);
            if let Some(window) = app.get_webview_window("reader") {
                window.hide().map_err(|error| error.to_string())?;
            }
            emit_reader_state(&app, "finished", "");
        }
        _ => return Err("Непозната команда за четеца.".to_string()),
    }
    Ok(())
}

/// Resizes the popup for the action bar plus an optional vertical submenu.
/// Returns true when the submenu opens upwards because there is no room below.
#[tauri::command]
fn set_popup_expanded(
    app: AppHandle,
    expanded: bool,
    submenu_height: Option<f64>,
) -> Result<bool, String> {
    let window = app
        .get_webview_window("popup-menu")
        .ok_or_else(|| "Popup window is unavailable".to_string())?;
    let state = app.state::<AppState>();
    state.popup_expanded.store(expanded, Ordering::Relaxed);
    if !expanded {
        window
            .set_size(tauri::LogicalSize::new(
                POPUP_COLLAPSED_SIZE.0,
                POPUP_COLLAPSED_SIZE.1,
            ))
            .map_err(|error| error.to_string())?;
        return Ok(false);
    }
    let submenu_height = submenu_height.unwrap_or(0.0).clamp(0.0, 400.0);
    let (width, height) = (
        POPUP_EXPANDED_SIZE.0,
        POPUP_EXPANDED_SIZE.1 + submenu_height,
    );
    let anchor = state
        .popup_position
        .lock()
        .ok()
        .and_then(|position| *position);
    let Some((left, top)) = anchor else {
        window
            .set_size(tauri::LogicalSize::new(width, height))
            .map_err(|error| error.to_string())?;
        return Ok(false);
    };
    // Window positions are physical pixels; sizes above are logical.
    let scale = window.scale_factor().unwrap_or(1.0);
    let (physical_width, physical_height) = ((width * scale) as i32, (height * scale) as i32);
    let (mut x, mut opens_up) = (left, false);
    if let Ok(Some(monitor)) = window.current_monitor() {
        let (screen_x, screen_y) = (monitor.position().x, monitor.position().y);
        let right = screen_x + monitor.size().width as i32;
        let bottom = screen_y + monitor.size().height as i32;
        x = x.min(right - physical_width).max(screen_x);
        opens_up = submenu_height > 0.0 && top + physical_height > bottom;
    }
    // When opening upwards the window grows above the bar, so the bar stays under the cursor.
    let y = if opens_up {
        top - (submenu_height * scale) as i32
    } else {
        top
    };
    window
        .set_position(tauri::PhysicalPosition::new(x, y))
        .and_then(|_| window.set_size(tauri::LogicalSize::new(width, height)))
        .map_err(|error| error.to_string())?;
    Ok(opens_up)
}

fn chat_completions_url(endpoint: &str) -> Result<reqwest::Url, String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let mut url =
        reqwest::Url::parse(endpoint).map_err(|error| format!("Невалиден endpoint: {error}"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Endpoint трябва да е HTTP(S) адрес без credentials, query или fragment.".to_string(),
        );
    }

    if !url.path().ends_with("/chat/completions") {
        let path = format!("{}/chat/completions", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url)
}

fn is_private_or_tailscale_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.ends_with(".ts.net")
    {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            let octets = address.octets();
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        }
        Ok(IpAddr::V6(address)) => {
            address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

const TRANSLATION_MAX_CHARS: usize = 50_000;

fn translation_url(provider_id: &str, endpoint: &str) -> Result<reqwest::Url, String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let mut url =
        reqwest::Url::parse(endpoint).map_err(|error| format!("Невалиден endpoint: {error}"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Endpoint-ът за превод трябва да е HTTPS адрес без credentials, query или fragment."
                .to_string(),
        );
    }
    let base = url.path().trim_end_matches('/').to_string();
    // Google's v2 endpoint (…/language/translate/v2) is the translate call itself.
    let path = if base.ends_with("/translate") || provider_id == "google-translate" {
        base
    } else if provider_id == "azure-translator"
        && base.is_empty()
        && url
            .host_str()
            .is_some_and(|host| host.ends_with(".cognitiveservices.azure.com"))
    {
        // Custom-domain Azure resources expose the Translator API under a fixed prefix.
        "/translator/text/v3.0/translate".to_string()
    } else {
        format!("{base}/translate")
    };
    url.set_path(&path);
    Ok(url)
}

fn translation_client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("Неуспешна конфигурация на HTTP клиента: {error}"))
}

async fn translation_error(service: &str, response: reqwest::Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let hint = match status.as_u16() {
        401 | 403 => " Проверете API ключа (и региона за Azure).",
        456 => " Изчерпана е месечната квота.",
        429 => " Твърде много заявки; опитайте отново след малко.",
        _ => "",
    };
    format!("{service} върна {status}.{hint} {}", body.trim())
        .trim()
        .to_string()
}

async fn translate_with_deepl(
    endpoint: &str,
    key: &str,
    text: &str,
    target: &str,
) -> Result<(String, String), String> {
    let response = translation_client()?
        .post(translation_url("deepl", endpoint)?)
        .header("Authorization", format!("DeepL-Auth-Key {key}"))
        .header("User-Agent", "ContextAI")
        .json(&serde_json::json!({ "text": [text], "target_lang": target }))
        .send()
        .await
        .map_err(|error| format!("Неуспешна връзка с DeepL: {error}"))?;
    if !response.status().is_success() {
        return Err(translation_error("DeepL", response).await);
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Невалиден отговор от DeepL: {error}"))?;
    let translated = value
        .pointer("/translations/0/text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "DeepL не върна превод.".to_string())?;
    let detected = value
        .pointer("/translations/0/detected_source_language")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    Ok((translated.to_string(), detected.to_ascii_lowercase()))
}

/// Target languages offered for translation: ISO 639-1 code and DeepL target code.
const TRANSLATION_LANGUAGES: [(&str, &str); 15] = [
    ("bg", "BG"),
    ("en", "EN-US"),
    ("de", "DE"),
    ("fr", "FR"),
    ("es", "ES"),
    ("it", "IT"),
    ("pt", "PT-PT"),
    ("nl", "NL"),
    ("ru", "RU"),
    ("uk", "UK"),
    ("pl", "PL"),
    ("cs", "CS"),
    ("ro", "RO"),
    ("el", "EL"),
    ("tr", "TR"),
];

fn translation_language(code: &str) -> Option<(&'static str, &'static str)> {
    TRANSLATION_LANGUAGES
        .iter()
        .copied()
        .find(|(short, _)| *short == code)
}

/// Target language plus the one used when the text is already in the target language.
fn translation_targets(configured: &str) -> (&'static str, &'static str) {
    let target = translation_language(configured).map_or("bg", |(short, _)| short);
    (target, if target == "en" { "bg" } else { "en" })
}

fn deepl_target(code: &str) -> &'static str {
    translation_language(code).map_or("EN-US", |(_, deepl)| deepl)
}

/// Google Cloud Translation v2 with an API key (sent as a header, not in the URL).
async fn translate_with_google(
    endpoint: &str,
    key: &str,
    text: &str,
    target: &str,
) -> Result<(String, String), String> {
    let response = translation_client()?
        .post(translation_url("google-translate", endpoint)?)
        .header("X-Goog-Api-Key", key)
        .header("User-Agent", "ContextAI")
        .json(&serde_json::json!({ "q": [text], "target": target, "format": "text" }))
        .send()
        .await
        .map_err(|error| format!("Неуспешна връзка с Google Translation: {error}"))?;
    if !response.status().is_success() {
        return Err(translation_error("Google Translation", response).await);
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Невалиден отговор от Google Translation: {error}"))?;
    let translated = value
        .pointer("/data/translations/0/translatedText")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Google Translation не върна превод.".to_string())?;
    let detected = value
        .pointer("/data/translations/0/detectedSourceLanguage")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    Ok((translated.to_string(), detected.to_ascii_lowercase()))
}

async fn translate_with_azure(
    endpoint: &str,
    region: &str,
    key: &str,
    text: &str,
    (primary, fallback): (&str, &str),
) -> Result<TranslationResult, String> {
    let mut url = translation_url("azure-translator", endpoint)?;
    // Request both languages at once so text already in the target can fall back.
    url.query_pairs_mut()
        .append_pair("api-version", "3.0")
        .append_pair("to", primary)
        .append_pair("to", fallback);
    let mut request = translation_client()?
        .post(url)
        .header("Ocp-Apim-Subscription-Key", key)
        .header("User-Agent", "ContextAI")
        .json(&serde_json::json!([{ "Text": text }]));
    let region = region.trim().to_ascii_lowercase();
    if !region.is_empty() && region != "global" {
        if !is_valid_azure_region(&region) {
            return Err("Невалиден Azure регион за Translator.".to_string());
        }
        request = request.header("Ocp-Apim-Subscription-Region", region);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Неуспешна връзка с Azure Translator: {error}"))?;
    if !response.status().is_success() {
        return Err(translation_error("Azure Translator", response).await);
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Невалиден отговор от Azure Translator: {error}"))?;
    let detected = value
        .pointer("/0/detectedLanguage/language")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let target = if detected.split('-').next() == Some(primary) {
        fallback
    } else {
        primary
    };
    let translated = value
        .pointer("/0/translations")
        .and_then(serde_json::Value::as_array)
        .and_then(|translations| {
            translations.iter().find(|translation| {
                translation.get("to").and_then(serde_json::Value::as_str) == Some(target)
            })
        })
        .and_then(|translation| translation.get("text"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Azure Translator не върна превод.".to_string())?;
    Ok(TranslationResult {
        text: translated.to_string(),
        detected_language: detected,
        target_language: target.to_string(),
    })
}

/// Translates into the configured language (Bulgarian by default), or into the
/// fallback language when the text is already in the target language.
/// Translates with a translation service and records the result in the history.
#[tauri::command]
async fn translate_text(
    app: AppHandle,
    provider_id: String,
    text: String,
) -> Result<TranslationResult, String> {
    let result = translate_with_service(app.clone(), &provider_id, &text).await?;
    history::record(
        &app,
        contextai_core::storage::NewHistoryEntry {
            kind: contextai_core::storage::HistoryKind::Translation,
            query: text,
            result: result.text.clone(),
            provider: provider_id,
            source_lang: result.detected_language.clone(),
            target_lang: result.target_language.clone(),
        },
    );
    Ok(result)
}

async fn translate_with_service(
    app: AppHandle,
    provider_id: &str,
    text: &str,
) -> Result<TranslationResult, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Няма текст за превод.".to_string());
    }
    if text.chars().count() > TRANSLATION_MAX_CHARS {
        return Err(format!(
            "Текстът е твърде дълъг за превод (максимум {TRANSLATION_MAX_CHARS} знака)."
        ));
    }
    let (service, missing_key) = match provider_id {
        "deepl" => (
            "DeepL",
            "Липсва DeepL API ключ. DeepL API Free също изисква ключ: създайте безплатен акаунт на deepl.com/pro-api и го въведете в настройките.",
        ),
        "azure-translator" => ("Azure Translator", "Липсва Azure Translator ключ в настройките."),
        "google-translate" => (
            "Google Cloud Translation",
            "Липсва Google Cloud API ключ в настройките.",
        ),
        _ => return Err("Неподдържана услуга за превод.".to_string()),
    };
    let settings = load_provider_settings(app)?;
    let (primary, fallback) = translation_targets(&settings.translation_target_language);
    let profile = settings
        .providers
        .into_iter()
        .find(|provider| provider.id == provider_id && provider.enabled)
        .ok_or_else(|| format!("Активирайте и запазете {service} в настройките."))?;
    let key = load_provider_secret(provider_id)?.ok_or_else(|| missing_key.to_string())?;

    if provider_id == "azure-translator" {
        return translate_with_azure(
            &profile.endpoint,
            &profile.region,
            &key,
            text,
            (primary, fallback),
        )
        .await;
    }
    let (endpoint, key, google) = (
        profile.endpoint.as_str(),
        key.as_str(),
        provider_id == "google-translate",
    );
    let translate_to = move |target: &'static str| async move {
        if google {
            translate_with_google(endpoint, key, text, target).await
        } else {
            translate_with_deepl(endpoint, key, text, deepl_target(target)).await
        }
    };
    let (translated, detected) = translate_to(primary).await?;
    // Services report the source as e.g. "en" or "pt-br"; compare the base language only.
    if detected.split('-').next() == Some(primary) {
        let (translated, _) = translate_to(fallback).await?;
        return Ok(TranslationResult {
            text: translated,
            detected_language: detected,
            target_language: fallback.to_string(),
        });
    }
    Ok(TranslationResult {
        text: translated,
        detected_language: detected,
        target_language: primary.to_string(),
    })
}

/// The Copilot CLI path from the GitHub Copilot profile (empty = auto-detect).
fn copilot_cli_setting(app: &AppHandle) -> Result<String, String> {
    Ok(load_provider_settings(app.clone())?
        .providers
        .into_iter()
        .find(|provider| provider.id == "github-copilot")
        .map(|provider| provider.endpoint)
        .unwrap_or_default())
}

#[tauri::command]
async fn copilot_status(app: AppHandle) -> Result<copilot::CopilotStatus, String> {
    let path = copilot_cli_setting(&app)?;
    Ok(copilot::status(&app, &path).await)
}

#[tauri::command]
async fn copilot_models(
    app: AppHandle,
    refresh: bool,
) -> Result<Vec<copilot::CopilotModel>, String> {
    let path = copilot_cli_setting(&app)?;
    copilot::models(&app, &path, refresh).await
}

#[tauri::command]
async fn copilot_login(app: AppHandle) -> Result<(), String> {
    let path = copilot_cli_setting(&app)?;
    copilot::login(&app, &path).await
}

#[derive(Serialize)]
struct ModelInfo {
    id: String,
    name: String,
}

/// Gemini's native model list; the OpenAI-compatible `/models` lacks the capability info
/// needed to hide embedding, image and speech models.
fn gemini_models_url(endpoint: &str) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(endpoint.trim().trim_end_matches('/'))
        .map_err(|error| format!("Невалиден endpoint: {error}"))?;
    if url.scheme() != "https" || url.query().is_some() || !url.username().is_empty() {
        return Err(
            "Gemini endpoint-ът трябва да е HTTPS адрес без query или credentials.".to_string(),
        );
    }
    let base = url.path().trim_end_matches('/');
    let base = base.strip_suffix("/openai").unwrap_or(base).to_string();
    url.set_path(&format!("{base}/models"));
    url.set_query(Some("pageSize=1000"));
    Ok(url)
}

fn is_gemini_chat_model(model: &serde_json::Value) -> bool {
    let name = model
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let generates = model
        .get("supportedGenerationMethods")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|methods| methods.iter().any(|method| method == "generateContent"));
    generates
        && !["embedding", "tts", "image", "aqa", "native-audio", "live"]
            .iter()
            .any(|excluded| name.contains(excluded))
}

/// The lowest `reasoning_effort` Gemini accepts: only 2.5 Flash models can turn thinking
/// off entirely. "minimal" is rejected by some newer models (e.g. behind
/// `gemini-flash-latest`), so newer models get "low".
fn gemini_min_reasoning(model: &str) -> &'static str {
    if model.contains("2.5-flash") {
        "none"
    } else {
        "low"
    }
}

#[tauri::command]
async fn gemini_models(app: AppHandle) -> Result<Vec<ModelInfo>, String> {
    let profile = load_provider_settings(app)?
        .providers
        .into_iter()
        .find(|provider| provider.id == "gemini")
        .ok_or_else(|| "Запазете профила на Google Gemini в настройките.".to_string())?;
    let key = load_provider_secret("gemini")?
        .ok_or_else(|| "Въведете и запазете Gemini API ключ.".to_string())?;
    let response = translation_client()?
        .get(gemini_models_url(&profile.endpoint)?)
        .header("x-goog-api-key", key)
        .send()
        .await
        .map_err(|error| format!("Неуспешна връзка с Gemini: {error}"))?;
    if !response.status().is_success() {
        return Err(translation_error("Gemini", response).await);
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("Невалиден отговор от Gemini: {error}"))?;
    let mut models = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| is_gemini_chat_model(model))
        .filter_map(|model| {
            let id = model
                .get("name")?
                .as_str()?
                .trim_start_matches("models/")
                .to_string();
            let name = model
                .get("displayName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&id)
                .to_string();
            Some(ModelInfo { id, name })
        })
        .collect::<Vec<_>>();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

#[tauri::command]
async fn stream_chat(
    app: AppHandle,
    request_id: String,
    provider_id: String,
    model: String,
    messages: Vec<ChatMessage>,
) -> Result<(), String> {
    let provider_id = if provider_id.is_empty() {
        "ollama".to_string()
    } else {
        provider_id
    };
    if provider_id == "github-copilot" {
        let path = copilot_cli_setting(&app)?;
        copilot::stream(&app, &path, &request_id, &model, &messages).await?;
        return app
            .emit(
                "chat-finished",
                StreamPayload {
                    request_id,
                    content: String::new(),
                },
            )
            .map_err(|error| error.to_string());
    }
    if !matches!(provider_id.as_str(), "llama-cpp" | "ollama" | "gemini") {
        return Err(format!(
            "Доставчикът „{provider_id}“ още не е свързан с чата."
        ));
    }
    if model.trim().is_empty() {
        return Err("Въведете име на модел в настройките на доставчика.".to_string());
    }

    let (endpoint, thinking) = if provider_id == "ollama" {
        ("http://127.0.0.1:11434/v1".to_string(), None)
    } else {
        let settings = load_provider_settings(app.clone())?;
        let profile = settings
            .providers
            .into_iter()
            .find(|provider| provider.id == provider_id && provider.enabled)
            .ok_or_else(|| format!("Активирайте и запазете {provider_id} в настройките."))?;
        (profile.endpoint, profile.thinking)
    };
    let url = chat_completions_url(&endpoint)?;
    let api_key = load_provider_secret(&provider_id)?;
    if api_key.is_some()
        && url.scheme() == "http"
        && !url.host_str().is_some_and(is_private_or_tailscale_host)
    {
        return Err(
            "За да се изпрати API ключ, използвайте HTTPS или частен/Tailscale адрес.".to_string(),
        );
    }
    let mut body = serde_json::json!({
        "model": model.trim(),
        "messages": messages,
        "stream": true
    });
    // Without reasoning_effort Gemini uses the model's default (thinking on).
    if provider_id == "gemini" && thinking == Some(false) {
        body["reasoning_effort"] = gemini_min_reasoning(model.trim()).into();
    }
    let mut request = Client::new().post(url).json(&body);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Неуспешна връзка с {provider_id}: {error}"))?
        .error_for_status()
        .map_err(|error| format!("{provider_id} върна грешка: {error}"))?;

    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| format!("Грешка при стрийминг от {provider_id}: {error}"))?;
        pending.extend_from_slice(&chunk);

        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=newline).collect::<Vec<_>>();
            let line = String::from_utf8(line)
                .map_err(|error| format!("Невалиден SSE отговор от {provider_id}: {error}"))?;
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }

            let value: serde_json::Value = serde_json::from_str(data)
                .map_err(|error| format!("Невалиден JSON от {provider_id}: {error}"))?;
            if let Some(error) = value.get("error") {
                return Err(format!("Грешка от {provider_id}: {error}"));
            }
            let content = value
                .pointer("/choices/0/delta/content")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !content.is_empty() {
                app.emit(
                    "chat-delta",
                    StreamPayload {
                        request_id: request_id.clone(),
                        content: content.to_string(),
                    },
                )
                .map_err(|error| error.to_string())?;
            }
        }
    }

    app.emit(
        "chat-finished",
        StreamPayload {
            request_id,
            content: String::new(),
        },
    )
    .map_err(|error| error.to_string())
}

/// Copy of every clipboard format, so files, HTML, RTF etc. survive the synthetic Ctrl+C.
#[cfg(windows)]
mod clipboard_snapshot {
    use std::{thread, time::Duration};

    use windows::Win32::{
        Foundation::{HANDLE, HGLOBAL},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
                OpenClipboard, SetClipboardData,
            },
            Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
        },
    };

    pub struct Snapshot(Vec<(u32, Vec<u8>)>);

    /// Formats whose data is a GDI/metafile handle rather than HGLOBAL memory.
    /// Windows re-synthesises CF_BITMAP etc. from the CF_DIB data that is kept.
    fn is_memory_format(format: u32) -> bool {
        !matches!(
            format,
            2 | 3 | 9 | 14 | 0x80 | 0x82 | 0x83 | 0x8E | 0x0200..=0x03FF
        )
    }

    struct OpenGuard;

    impl OpenGuard {
        fn open() -> Result<Self, String> {
            // Another app may hold the clipboard for a moment.
            for _ in 0..10 {
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Ok(Self);
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err("Clipboard е зает от друго приложение.".to_string())
        }
    }

    impl Drop for OpenGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    pub fn take() -> Result<Snapshot, String> {
        let _guard = OpenGuard::open()?;
        let mut formats = Vec::new();
        let mut format = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            if !is_memory_format(format) {
                continue;
            }
            let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
                continue;
            };
            let memory = HGLOBAL(handle.0);
            let size = unsafe { GlobalSize(memory) };
            let data = unsafe { GlobalLock(memory) };
            if data.is_null() {
                continue;
            }
            let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec();
            let _ = unsafe { GlobalUnlock(memory) };
            formats.push((format, bytes));
        }
        Ok(Snapshot(formats))
    }

    pub fn restore(snapshot: &Snapshot) -> Result<(), String> {
        let _guard = OpenGuard::open()?;
        unsafe { EmptyClipboard() }
            .map_err(|error| format!("Неуспешно изчистване на Clipboard: {error}"))?;
        for (format, bytes) in &snapshot.0 {
            let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }
                .map_err(|error| format!("Неуспешно заделяне на памет за Clipboard: {error}"))?;
            let data = unsafe { GlobalLock(memory) };
            if data.is_null() {
                continue;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len())
            };
            let _ = unsafe { GlobalUnlock(memory) };
            // On success the clipboard owns the memory, so it is not freed here.
            unsafe { SetClipboardData(*format, Some(HANDLE(memory.0))) }
                .map_err(|error| format!("Неуспешно възстановяване на Clipboard: {error}"))?;
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod clipboard_snapshot {
    pub struct Snapshot(Option<String>);

    pub fn take() -> Result<Snapshot, String> {
        let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
        Ok(Snapshot(clipboard.get_text().ok()))
    }

    pub fn restore(snapshot: &Snapshot) -> Result<(), String> {
        let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
        match &snapshot.0 {
            Some(text) => clipboard.set_text(text.clone()),
            None => clipboard.clear(),
        }
        .map_err(|error| format!("Неуспешно възстановяване на Clipboard: {error}"))
    }
}

/// Explorer, the desktop and the taskbar have no text selection; a drag or double-click
/// there means files, and a synthetic Ctrl+C would copy them.
#[cfg(windows)]
fn foreground_is_shell() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};

    let window = unsafe { GetForegroundWindow() };
    let mut buffer = [0u16; 64];
    let length = unsafe { GetClassNameW(window, &mut buffer) };
    let class = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
    matches!(
        class.as_str(),
        "CabinetWClass"
            | "ExploreWClass"
            | "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
    )
}

#[cfg(not(windows))]
fn foreground_is_shell() -> bool {
    false
}

fn press_with_control(key: Key) -> Result<(), String> {
    let mut enigo = Enigo::new(&Settings::default()).map_err(|error| format!("{error:?}"))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|error| format!("{error:?}"))?;
    // Give the OS time to register Ctrl, otherwise the letter replaces the selection.
    thread::sleep(Duration::from_millis(30));
    let key_result = enigo.key(key, Direction::Click);
    thread::sleep(Duration::from_millis(30));
    let release_result = enigo.key(Key::Control, Direction::Release);
    key_result
        .and(release_result)
        .map_err(|error| format!("{error:?}"))
}

/// The window the last selection came from, so a corrected text can replace it there.
static SOURCE_WINDOW: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

#[cfg(windows)]
fn remember_source_window() {
    let window = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
    SOURCE_WINDOW.store(window.0 as isize, Ordering::Relaxed);
}

#[cfg(not(windows))]
fn remember_source_window() {}

/// Puts `text` in place of the selection in the source window: focus it, paste, restore the clipboard.
/// Works while the source app still has the selection; otherwise the text lands at its cursor.
#[tauri::command]
async fn replace_selection(app: AppHandle, text: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, IsIconic, IsWindow, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };

        let raw = SOURCE_WINDOW.load(Ordering::Relaxed);
        let window = HWND(raw as *mut _);
        if raw == 0 || !unsafe { IsWindow(Some(window)) }.as_bool() {
            return Err(
                "Прозорецът с оригиналния текст не е наличен. Използвайте „Копирай“.".to_string(),
            );
        }
        if let Some(chat) = app.get_webview_window("chat-panel") {
            let _ = chat.hide();
        }
        let capture_active = Arc::clone(&app.state::<AppState>().capture_active);
        // Hold the flag so the synthetic Ctrl+V is not taken for the user typing.
        capture_active.store(true, Ordering::Release);
        let result = tauri::async_runtime::spawn_blocking(move || {
            let window = HWND(raw as *mut _);
            let previous = clipboard_snapshot::take()?;
            arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(text))
                .map_err(|error| format!("Неуспешен запис в клипборда: {error}"))?;
            unsafe {
                if IsIconic(window).as_bool() {
                    let _ = ShowWindow(window, SW_RESTORE);
                }
                let _ = SetForegroundWindow(window);
            }
            let deadline = Instant::now() + Duration::from_millis(1000);
            while unsafe { GetForegroundWindow() } != window && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if unsafe { GetForegroundWindow() } != window {
                // The text stays on the clipboard so the user can paste it by hand.
                return Err(
                    "Оригиналният прозорец не можа да се активира. Текстът е копиран в клипборда."
                        .to_string(),
                );
            }
            // Let the app restore its focus and selection before the paste.
            thread::sleep(Duration::from_millis(150));
            press_with_control(Key::V).map_err(|error| format!("Неуспешно поставяне: {error}"))?;
            // Apps read the clipboard after the key event; restoring too early would paste the old content.
            thread::sleep(Duration::from_millis(500));
            for attempt in 0..5 {
                match clipboard_snapshot::restore(&previous) {
                    Ok(()) => break,
                    Err(error) if attempt == 4 => eprintln!("{error}"),
                    Err(_) => thread::sleep(Duration::from_millis(50)),
                }
            }
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result);
        // Let delayed synthetic key events drain before re-enabling hiding.
        tokio::time::sleep(Duration::from_millis(300)).await;
        capture_active.store(false, Ordering::Release);
        result
    }
    #[cfg(not(windows))]
    {
        let _ = (app, text);
        Err("Замяната в момента е налична само за Windows.".to_string())
    }
}

#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|error| format!("Неуспешен запис в клипборда: {error}"))
}

/// Presses Ctrl+`key` and reports whether the clipboard changed within 500 ms.
fn copy_selection_with(key: Key) -> Result<bool, String> {
    #[cfg(windows)]
    let clipboard_sequence =
        unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
    press_with_control(key)
        .map_err(|error| format!("Неуспешно копиране на селекцията: {error}"))?;
    #[cfg(windows)]
    {
        let deadline = Instant::now() + Duration::from_millis(500);
        while unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
            == clipboard_sequence
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        Ok(
            (unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() })
                != clipboard_sequence,
        )
    }
    #[cfg(not(windows))]
    {
        thread::sleep(Duration::from_millis(100));
        Ok(true)
    }
}

fn capture_selection() -> Result<Option<String>, String> {
    if foreground_is_shell() {
        return Ok(None);
    }
    remember_source_window();
    let previous = clipboard_snapshot::take()?;
    // Ctrl+Insert first: SnipDo's "Invoke on Ctrl+C" would show its bar again over the popup.
    // Ctrl+C is the fallback for the few apps without Ctrl+Insert.
    let selection_copied = copy_selection_with(Key::Insert)? || copy_selection_with(Key::C)?;

    // Nothing was copied, so the user's clipboard is untouched and must not be rewritten.
    if !selection_copied {
        return Ok(None);
    }
    let selected_text = arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_text())
        .ok();
    // The clipboard may be briefly locked by another app; a failed restore must not drop the selection.
    for attempt in 0..5 {
        match clipboard_snapshot::restore(&previous) {
            Ok(()) => break,
            Err(error) if attempt == 4 => eprintln!("{error}"),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }

    Ok(selected_text
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty()))
}

#[derive(Default)]
struct MouseGesture {
    cursor: (f64, f64),
    down: Option<(f64, f64)>,
    last_click: Option<(Instant, (f64, f64))>,
}

fn start_global_listener(app: AppHandle, state: &AppState) {
    let paused = Arc::clone(&state.paused);
    let popup_position = Arc::clone(&state.popup_position);
    let reader_app = app.clone();
    let popup_app = app.clone();
    let popup_hit = move |_popup_position: &Arc<Mutex<Option<(i32, i32)>>>, x: f64, y: f64| {
        // Mouse coordinates are physical pixels, so compare them with real window rectangles.
        ["reader", "popup-menu"].into_iter().any(|label| {
            let app = if label == "reader" {
                &reader_app
            } else {
                &popup_app
            };
            app.get_webview_window(label)
                .filter(|window| window.is_visible().unwrap_or(false))
                .and_then(|window| Some((window.outer_position().ok()?, window.outer_size().ok()?)))
                .is_some_and(|(position, size)| {
                    x >= f64::from(position.x)
                        && x <= f64::from(position.x) + f64::from(size.width)
                        && y >= f64::from(position.y)
                        && y <= f64::from(position.y) + f64::from(size.height)
                })
        })
    };
    let capture_active = Arc::clone(&state.capture_active);
    let capture_active_listener = Arc::clone(&capture_active);
    let gesture = Arc::new(Mutex::new(MouseGesture::default()));
    let app_handle = app.clone();

    thread::spawn(move || {
        let listener = rdev::listen(move |event| {
            let event_type = &event.event_type;
            if let rdev::EventType::MouseMove { x, y } = event_type {
                if let Ok(mut state) = gesture.lock() {
                    state.cursor = (*x, *y);
                }
                return;
            }
            if matches!(event_type, rdev::EventType::Wheel { .. })
                || matches!(event_type, rdev::EventType::KeyPress(_))
            {
                // Ignore the synthetic Ctrl+C sent by capture_selection.
                if capture_active_listener.load(Ordering::Acquire) {
                    return;
                }
                let _ = hide_popup(app_handle.clone());
                return;
            }

            if matches!(event_type, rdev::EventType::ButtonPress(rdev::Button::Left)) {
                let Ok(mut gesture_state) = gesture.lock() else {
                    return;
                };
                let (x, y) = gesture_state.cursor;
                let is_inside_popup = popup_hit(&popup_position, x, y);
                if !is_inside_popup {
                    let _ = hide_popup(app_handle.clone());
                }
                gesture_state.down = Some((x, y));
                return;
            }

            if matches!(
                event_type,
                rdev::EventType::ButtonRelease(rdev::Button::Left)
            ) {
                let Ok(mut gesture_state) = gesture.lock() else {
                    return;
                };
                let (x, y) = gesture_state.cursor;
                let dragged = gesture_state.down.take().is_some_and(|(start_x, start_y)| {
                    let dx = x - start_x;
                    let dy = y - start_y;
                    dx * dx + dy * dy > 225.0
                });
                let now = Instant::now();
                let double_click = !dragged
                    && gesture_state
                        .last_click
                        .is_some_and(|(last, (last_x, last_y))| {
                            let dx = x - last_x;
                            let dy = y - last_y;
                            now.duration_since(last) <= Duration::from_millis(500)
                                && dx * dx + dy * dy <= 100.0
                        });
                let should_capture = dragged || double_click;
                gesture_state.last_click = if dragged { None } else { Some((now, (x, y))) };
                drop(gesture_state);
                let inside_popup = popup_hit(&popup_position, x, y);
                if should_capture
                    && !inside_popup
                    && !paused.load(Ordering::Relaxed)
                    && capture_active
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                {
                    let app = app_handle.clone();
                    let capture_active = Arc::clone(&capture_active);
                    thread::spawn(move || {
                        match capture_selection() {
                            Ok(Some(text)) => {
                                let payload = SelectionPayload {
                                    text,
                                    expanded: false,
                                    action: None,
                                };
                                if let Err(error) = show_popup(&app, (x, y), payload) {
                                    eprintln!("Could not show selection popup: {error}");
                                }
                            }
                            Ok(None) => {}
                            Err(error) => eprintln!("Could not capture selected text: {error}"),
                        }
                        // Let delayed synthetic key events drain before re-enabling hiding.
                        thread::sleep(Duration::from_millis(300));
                        capture_active.store(false, Ordering::Release);
                    });
                }
            }
        });
        if let Err(error) = listener {
            eprintln!("Global mouse/keyboard listener stopped: {error:?}");
        }
    });
}

/// Actions another program (the SnipDo extension) may request; `menu` opens the action bar.
const EXTERNAL_ACTIONS: [&str; 10] = [
    "menu",
    "dictionary",
    "explain",
    "summarize",
    "edit",
    "grammar",
    "ask",
    "translate",
    "read",
    "translate-read",
];
const MAX_EXTERNAL_REQUEST_BYTES: u64 = 2_000_000;
/// Bits in `FRONTEND_READY`: set once a window's page has loaded.
const POPUP_LOADED: u8 = 1;
const CHAT_LOADED: u8 = 2;
static FRONTEND_READY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[derive(Deserialize)]
struct ExternalRequest {
    action: String,
    text: String,
}

fn handle_external_request(app: &AppHandle, request: ExternalRequest) -> Result<(), String> {
    if !EXTERNAL_ACTIONS.contains(&request.action.as_str()) {
        return Err(format!("Непознато действие: {}", request.action));
    }
    let text = request.text.trim().to_string();
    if text.is_empty() {
        return Err("Няма маркиран текст.".to_string());
    }
    remember_source_window();
    match request.action.as_str() {
        "read" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = start_reading(app, text).await {
                    eprintln!("Could not read external selection: {error}");
                }
            });
            Ok(())
        }
        "dictionary" => open_dictionary(app.clone(), text),
        "menu" | "translate-read" => {
            let cursor = app.cursor_position().map_err(|error| error.to_string())?;
            let action = (request.action == "translate-read").then_some(request.action);
            let payload = SelectionPayload {
                text,
                expanded: true,
                action,
            };
            show_popup(app, (cursor.x, cursor.y), payload)
        }
        action => open_chat(app.clone(), text, action.to_string(), None),
    }
}

/// Text handed over by SnipDo arrives on a per-user named pipe as one JSON line
/// (`{"action": "...", "text": "..."}`) and is answered with `ok` or `error: ...`.
/// A pipe works from SnipDo's MSIX container, where temp files would be virtualized.
#[cfg(windows)]
fn start_external_listener(app: AppHandle) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    async fn serve(app: AppHandle, pipe: NamedPipeServer) -> std::io::Result<()> {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        (&mut reader)
            .take(MAX_EXTERNAL_REQUEST_BYTES)
            .read_line(&mut line)
            .await?;
        // A cold start launched by the extension must not emit events before the windows listen.
        for _ in 0..200 {
            if FRONTEND_READY.load(Ordering::Acquire) == POPUP_LOADED | CHAT_LOADED {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let result = serde_json::from_str::<ExternalRequest>(&line)
            .map_err(|error| format!("Невалидна заявка: {error}"))
            .and_then(|request| handle_external_request(&app, request));
        let reply = match result {
            Ok(()) => "ok\n".to_string(),
            Err(error) => format!("error: {}\n", error.replace('\n', " ")),
        };
        reader.get_mut().write_all(reply.as_bytes()).await
    }

    tauri::async_runtime::spawn(async move {
        let name = external_pipe_name();
        // The first instance flag stops another process from owning the name before ContextAI.
        let mut server = match ServerOptions::new().first_pipe_instance(true).create(&name) {
            Ok(server) => server,
            Err(error) => {
                eprintln!("Could not create {name}: {error}");
                return;
            }
        };
        loop {
            if let Err(error) = server.connect().await {
                eprintln!("External request connection failed: {error}");
                let _ = server.disconnect();
                continue;
            }
            let connected = server;
            server = match ServerOptions::new().create(&name) {
                Ok(server) => server,
                Err(error) => {
                    eprintln!("Could not create {name}: {error}");
                    return;
                }
            };
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = serve(app, connected).await {
                    eprintln!("External request failed: {error}");
                }
            });
        }
    });
}

#[cfg(not(windows))]
fn start_external_listener(_app: AppHandle) {}

fn external_pipe_name() -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .collect();
    format!(r"\\.\pipe\contextai-{user}")
}

/// Opens the action bar for the current selection. The SnipDo extension presses it with `keyCombo`.
/// A function key, because SendKeys types letters as text under a Cyrillic keyboard layout.
const MENU_SHORTCUT_LABEL: &str = "Ctrl+Alt+Shift+F8";

fn menu_shortcut() -> Shortcut {
    Shortcut::new(
        Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT),
        Code::F8,
    )
}

/// Copies the selection and shows the expanded popup at the cursor; works while detection is paused.
fn open_menu_for_selection(app: AppHandle) {
    let capture_active = Arc::clone(&app.state::<AppState>().capture_active);
    if capture_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    thread::spawn(move || {
        wait_for_modifiers_release();
        match capture_selection() {
            Ok(Some(text)) => {
                let payload = SelectionPayload {
                    text,
                    expanded: true,
                    action: None,
                };
                let shown = app
                    .cursor_position()
                    .map_err(|error| error.to_string())
                    .and_then(|cursor| show_popup(&app, (cursor.x, cursor.y), payload));
                if let Err(error) = shown {
                    eprintln!("Could not show selection popup: {error}");
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("Could not capture selected text: {error}"),
        }
        // Let delayed synthetic key events drain before re-enabling hiding.
        thread::sleep(Duration::from_millis(300));
        capture_active.store(false, Ordering::Release);
    });
}

/// The synthetic Ctrl+C would become Ctrl+Alt+Shift+C while the shortcut's modifiers are still down.
#[cfg(windows)]
fn wait_for_modifiers_release() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT,
    };

    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline
        && [VK_CONTROL, VK_MENU, VK_SHIFT]
            .iter()
            .any(|key| unsafe { GetAsyncKeyState(i32::from(key.0)) } < 0)
    {
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(windows))]
fn wait_for_modifiers_release() {
    thread::sleep(Duration::from_millis(200));
}

/// Shows the popup next to `cursor` (physical pixels) and hands it the selection.
fn show_popup(
    app: &AppHandle,
    cursor: (f64, f64),
    payload: SelectionPayload,
) -> Result<(), String> {
    let window = app
        .get_webview_window("popup-menu")
        .ok_or_else(|| "Popup window is unavailable".to_string())?;
    let (left, top) = (cursor.0 as i32 + 10, cursor.1 as i32 + 10);
    let state = app.state::<AppState>();
    state.popup_expanded.store(false, Ordering::Relaxed);
    let _ = window.set_size(tauri::LogicalSize::new(
        POPUP_COLLAPSED_SIZE.0,
        POPUP_COLLAPSED_SIZE.1,
    ));
    let _ = window.set_position(tauri::PhysicalPosition::new(left, top));
    show_without_activation(&window)?;
    if let Ok(mut position) = state.popup_position.lock() {
        *position = Some((left, top));
    }
    app.emit("selection-captured", payload)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn show_without_activation(window: &tauri::WebviewWindow) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };

    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE.0 as isize);
    }
    window.show().map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn show_without_activation(window: &tauri::WebviewWindow) -> Result<(), String> {
    window.show().map_err(|error| error.to_string())
}

fn set_popup_non_activating(window: &tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
        };

        let hwnd = window.hwnd().map_err(|error| error.to_string())?;
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | WS_EX_NOACTIVATE.0 as isize);
        }
    }
    Ok(())
}

/// Whether ContextAI starts with Windows (the current user's `Run` registry key).
#[tauri::command]
fn get_autostart() -> Result<bool, String> {
    #[cfg(windows)]
    return autostart::is_enabled();
    #[cfg(not(windows))]
    Err("Автоматичното стартиране се поддържа само в Windows.".to_string())
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    return autostart::set_enabled(enabled)
        .map_err(|error| format!("Неуспешна промяна на автоматичното стартиране: {error}"));
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Err("Автоматичното стартиране се поддържа само в Windows.".to_string())
    }
}

pub fn run() {
    // Only the chat keeps its place: the popup follows the cursor and the reader sits bottom-right.
    let window_state_flags = tauri_plugin_window_state::StateFlags::POSITION
        | tauri_plugin_window_state::StateFlags::SIZE;
    tauri::Builder::default()
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(window_state_flags)
                .with_denylist(&["popup-menu", "reader"])
                .build(),
        )
        // The chat is hidden rather than closed, so the plugin's save-on-exit alone would lose
        // moves when the app is killed; losing focus (which hiding also causes) saves it early.
        .on_window_event(move |window, event| {
            if window.label() == "chat-panel" && matches!(event, tauri::WindowEvent::Focused(false))
            {
                use tauri_plugin_window_state::AppHandleExt;
                if let Err(error) = window.app_handle().save_window_state(window_state_flags) {
                    eprintln!("Could not save the chat window position: {error}");
                }
            }
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed && *shortcut == menu_shortcut() {
                        open_menu_for_selection(app.clone());
                    }
                })
                .build(),
        )
        .on_page_load(|webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                let bit = match webview.label() {
                    "popup-menu" => POPUP_LOADED,
                    "chat-panel" => CHAT_LOADED,
                    _ => 0,
                };
                FRONTEND_READY.fetch_or(bit, Ordering::Release);
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_autostart,
            set_autostart,
            get_detection_paused,
            set_detection_paused,
            replace_selection,
            copy_text,
            hide_popup,
            open_chat,
            open_settings,
            open_dictionary,
            dictionary::dictionary_list,
            dictionary::dictionary_add,
            dictionary::dictionary_save,
            dictionary::dictionary_lookup,
            dictionary::dictionary_suggest,
            history::history_list,
            history::history_add,
            history::history_pin,
            history::history_delete,
            history::history_clear,
            history::history_settings,
            history::history_save_settings,
            cards::cards_list,
            cards::cards_count,
            cards::cards_saved,
            cards::card_save,
            cards::card_update,
            cards::card_delete,
            review::review_counts,
            review::review_next,
            review::review_answer,
            review::review_settings,
            review::review_save_settings,
            start_reading,
            reader_control,
            get_reader_session,
            set_popup_expanded,
            load_provider_settings,
            save_provider_settings,
            save_provider_secret,
            provider_secret_is_saved,
            translate_text,
            gemini_models,
            copilot_status,
            copilot_models,
            copilot_login,
            stream_chat
        ])
        .setup(|app| {
            app.manage(AppState::new(spawn_player(app.handle().clone())));
            app.manage(copilot::CopilotState::default());
            app.manage(dictionary::create(app.handle())?);
            app.manage(history::open(app.handle()));
            #[cfg(windows)]
            autostart::repair_unquoted_entry();
            // The windows are `"create": false` in tauri.conf.json: a bundled page loads at once and
            // its commands would read AppState before it is managed.
            for config in app.config().app.windows.clone() {
                tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?.build()?;
            }
            let state = app.state::<AppState>();
            let popup = app
                .get_webview_window("popup-menu")
                .ok_or("Popup window is missing")?;
            set_popup_non_activating(&popup)?;

            let pause_item = MenuItem::with_id(app, "pause", PAUSE_LABEL, true, None::<&str>)?;
            app.manage(PauseMenuItem(pause_item.clone()));
            let settings_item =
                MenuItem::with_id(app, "settings", "Настройки", true, None::<&str>)?;
            let chat_item = MenuItem::with_id(app, "chat", "Отвори чата", true, None::<&str>)?;
            let dictionary_item =
                MenuItem::with_id(app, "dictionary", "Речник", true, None::<&str>)?;
            let review_item = MenuItem::with_id(app, "review", "Упражнение", true, None::<&str>)?;
            app.manage(review::ReviewTray::new(review_item.clone()));
            let quit_item = MenuItem::with_id(app, "quit", "Изход", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &pause_item,
                    &settings_item,
                    &chat_item,
                    &dictionary_item,
                    &review_item,
                    &quit_item,
                ],
            )?;
            TrayIconBuilder::with_id(review::TRAY_ID)
                .icon(review::tray_icon(false))
                .tooltip("ContextAI")
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "pause" => {
                        let paused = !app.state::<AppState>().paused.load(Ordering::Relaxed);
                        if let Err(error) = apply_detection_paused(app, paused) {
                            eprintln!("Could not save detection state: {error}");
                        }
                    }
                    "settings" => {
                        if let Err(error) = open_settings(app.clone()) {
                            eprintln!("Could not open settings: {error}");
                        }
                    }
                    "chat" => {
                        if let Some(window) = app.get_webview_window("chat-panel") {
                            if let Err(error) = window.show().and_then(|_| window.set_focus()) {
                                eprintln!("Could not open chat: {error}");
                            }
                        }
                    }
                    "dictionary" => {
                        if let Err(error) = open_dictionary(app.clone(), String::new()) {
                            eprintln!("Could not open the dictionary: {error}");
                        }
                    }
                    "review" => {
                        if let Err(error) = review::open(app) {
                            eprintln!("Could not open the practice: {error}");
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            if load_preferences(app.handle()).detection_paused {
                apply_detection_paused(app.handle(), true)?;
            }
            review::start_refreshing(app.handle().clone());
            // The SnipDo extension reads this to start ContextAI when it is not running.
            if let (Ok(exe), Ok(path)) = (
                std::env::current_exe(),
                config_file(app.handle(), "contextai-path.txt"),
            ) {
                if let Err(error) = fs::write(&path, exe.to_string_lossy().as_bytes()) {
                    eprintln!("Could not write {}: {error}", path.display());
                }
            }
            start_global_listener(app.handle().clone(), &state);
            start_external_listener(app.handle().clone());
            // Another program may already own the shortcut; the rest of ContextAI still works.
            if let Err(error) = app.global_shortcut().register(menu_shortcut()) {
                eprintln!("Could not register {MENU_SHORTCUT_LABEL}: {error}");
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ContextAI");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_configured_provider_ids() {
        assert!(is_valid_provider_id("llama-cpp"));
        assert!(is_valid_provider_id("azure-translator"));
        assert!(!is_valid_provider_id("unknown-provider"));
        assert!(!is_valid_provider_id("../other-app"));
    }

    #[test]
    fn provider_settings_use_camel_case_and_do_not_include_secrets() {
        let settings = ProviderSettings {
            active_chat_provider: String::new(),
            active_translation_provider: "deepl".to_string(),
            translation_target_language: String::new(),
            providers: vec![ProviderProfile {
                id: "llama-cpp".to_string(),
                enabled: true,
                endpoint: "http://127.0.0.1:8080/v1".to_string(),
                model: "local-model".to_string(),
                region: String::new(),
                voice: String::new(),
                voices: None,
                thinking: None,
            }],
        };
        let value = serde_json::to_value(settings).expect("settings serialize");

        assert!(value.get("activeChatProvider").is_some());
        assert_eq!(value["activeTranslationProvider"], "deepl");
        assert!(value["providers"][0].get("endpoint").is_some());
        assert!(value["providers"][0].get("apiKey").is_none());
    }

    #[test]
    fn provider_region_round_trips_without_transformation() {
        let settings = ProviderSettings {
            active_chat_provider: "ollama".to_string(),
            active_translation_provider: String::new(),
            translation_target_language: String::new(),
            providers: vec![ProviderProfile {
                id: "azure-tts".to_string(),
                enabled: true,
                region: "westeurope".to_string(),
                ..ProviderProfile::default()
            }],
        };
        let serialized = serde_json::to_vec(&settings).expect("settings serialize");
        let restored: ProviderSettings =
            serde_json::from_slice(&serialized).expect("settings deserialize");

        assert_eq!(restored.providers[0].region, "westeurope");
    }

    #[test]
    fn speech_text_is_escaped_for_ssml() {
        assert_eq!(
            escape_xml("A & B < C > \"D\""),
            "A &amp; B &lt; C &gt; &quot;D&quot;"
        );
    }

    #[test]
    fn openai_compatible_endpoint_gets_chat_completions_path() {
        assert_eq!(
            chat_completions_url("http://100.88.12.4:8080/v1")
                .expect("valid endpoint")
                .as_str(),
            "http://100.88.12.4:8080/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://model.example/v1/chat/completions/")
                .expect("valid endpoint")
                .as_str(),
            "https://model.example/v1/chat/completions"
        );
    }

    #[test]
    fn openai_compatible_endpoint_rejects_embedded_secrets_and_query() {
        assert!(chat_completions_url("http://user:pass@localhost:8080/v1").is_err());
        assert!(chat_completions_url("https://model.example/v1?token=secret").is_err());
        assert!(chat_completions_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn settings_without_translation_provider_still_load() {
        let restored: ProviderSettings =
            serde_json::from_str(r#"{"activeChatProvider":"ollama","providers":[]}"#)
                .expect("legacy settings deserialize");

        assert_eq!(restored.active_translation_provider, "");
    }

    #[test]
    fn voice_follows_detected_language() {
        let default = "bg-BG-KalinaNeural";
        let voices = DEFAULT_EXTRA_VOICES;
        assert_eq!(
            pick_voice(
                "Това е съвсем обикновено изречение на български език.",
                default,
                voices
            ),
            default
        );
        assert_eq!(
            pick_voice(
                "This is a perfectly ordinary sentence written in English.",
                default,
                voices
            ),
            "en-US-AvaNeural"
        );
        assert_eq!(
            pick_voice(
                "Das ist ein ganz gewöhnlicher Satz in deutscher Sprache.",
                default,
                voices
            ),
            "de-DE-KatjaNeural"
        );
        // Short Latin text is not "reliable", but must not be read by the Bulgarian voice.
        assert_ne!(pick_voice("Hello world", default, voices), default);
    }

    #[test]
    fn voice_falls_back_without_extra_voices() {
        let default = "bg-BG-KalinaNeural";
        assert_eq!(pick_voice("This is English text.", default, ""), default);
        assert_eq!(
            pick_voice("This is English text.", default, "not a voice\n<script>"),
            default
        );
    }

    #[test]
    fn translation_target_falls_back_when_text_is_already_in_it() {
        assert_eq!(translation_targets(""), ("bg", "en"));
        assert_eq!(translation_targets("de"), ("de", "en"));
        assert_eq!(translation_targets("en"), ("en", "bg"));
        assert_eq!(translation_targets("xx"), ("bg", "en"));
        assert_eq!(deepl_target("en"), "EN-US");
        assert_eq!(deepl_target("pt"), "PT-PT");
    }

    #[test]
    fn translation_endpoints_get_translate_path() {
        assert_eq!(
            translation_url("deepl", "https://api-free.deepl.com/v2/")
                .expect("valid endpoint")
                .as_str(),
            "https://api-free.deepl.com/v2/translate"
        );
        assert_eq!(
            translation_url(
                "azure-translator",
                "https://api.cognitive.microsofttranslator.com"
            )
            .expect("valid endpoint")
            .as_str(),
            "https://api.cognitive.microsofttranslator.com/translate"
        );
        assert_eq!(
            translation_url(
                "azure-translator",
                "https://my-res.cognitiveservices.azure.com/"
            )
            .expect("valid endpoint")
            .as_str(),
            "https://my-res.cognitiveservices.azure.com/translator/text/v3.0/translate"
        );
        assert_eq!(
            translation_url(
                "google-translate",
                "https://translation.googleapis.com/language/translate/v2/"
            )
            .expect("valid endpoint")
            .as_str(),
            "https://translation.googleapis.com/language/translate/v2"
        );
    }

    #[test]
    fn translation_endpoint_requires_https_without_secrets() {
        assert!(translation_url("deepl", "http://api-free.deepl.com/v2").is_err());
        assert!(translation_url("deepl", "https://api-free.deepl.com/v2?auth_key=x").is_err());
        assert!(translation_url("deepl", "https://user:pass@api-free.deepl.com/v2").is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "uses the real system clipboard"]
    fn clipboard_snapshot_restores_after_clear() {
        use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard};

        let snapshot = clipboard_snapshot::take().expect("snapshot");
        unsafe {
            OpenClipboard(None).expect("open");
            EmptyClipboard().expect("empty");
            CloseClipboard().expect("close");
        }
        clipboard_snapshot::restore(&snapshot).expect("restore");
    }

    #[test]
    fn gemini_models_use_the_native_endpoint() {
        assert_eq!(
            gemini_models_url("https://generativelanguage.googleapis.com/v1beta/openai/")
                .expect("valid endpoint")
                .as_str(),
            "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000"
        );
        assert!(
            gemini_models_url("http://generativelanguage.googleapis.com/v1beta/openai").is_err()
        );
    }

    #[test]
    fn gemini_model_filter_keeps_only_text_chat_models() {
        let model = |name: &str, methods: &[&str]| serde_json::json!({ "name": name, "supportedGenerationMethods": methods });
        assert!(is_gemini_chat_model(&model(
            "models/gemini-flash-latest",
            &["generateContent"]
        )));
        assert!(!is_gemini_chat_model(&model(
            "models/gemini-embedding-001",
            &["embedContent"]
        )));
        assert!(!is_gemini_chat_model(&model(
            "models/gemini-2.5-flash-preview-tts",
            &["generateContent"]
        )));
        assert!(!is_gemini_chat_model(&model(
            "models/gemini-2.5-flash-image",
            &["generateContent"]
        )));
    }

    #[test]
    fn gemini_thinking_goes_as_low_as_the_model_allows() {
        assert_eq!(gemini_min_reasoning("gemini-2.5-flash"), "none");
        assert_eq!(gemini_min_reasoning("gemini-2.5-flash-lite"), "none");
        assert_eq!(gemini_min_reasoning("gemini-2.5-pro"), "low");
        assert_eq!(gemini_min_reasoning("gemini-flash-latest"), "low");
    }

    #[test]
    fn private_host_check_accepts_local_and_tailscale_targets() {
        assert!(is_private_or_tailscale_host("127.0.0.1"));
        assert!(is_private_or_tailscale_host("192.168.1.5"));
        assert!(is_private_or_tailscale_host("100.96.2.3"));
        assert!(is_private_or_tailscale_host("server.example.ts.net"));
        assert!(!is_private_or_tailscale_host("203.0.113.10"));
        assert!(!is_private_or_tailscale_host("api.example.com"));
    }
}

#[cfg(all(test, windows))]
mod gemini_live_tests {
    use super::*;

    /// Times Gemini with and without thinking: `cargo test gemini_live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "sends two requests with the saved Gemini key"]
    async fn gemini_live_thinking_speed() {
        let key = load_provider_secret("gemini")
            .expect("keyring")
            .expect("saved Gemini key");
        let url = "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions";
        let model = "gemini-flash-latest";
        for effort in [None, Some(gemini_min_reasoning(model))] {
            let mut body = serde_json::json!({
                "model": model,
                "messages": [{ "role": "user", "content": "Преведи на английски: Добро утро, как си?" }],
            });
            if let Some(effort) = effort {
                body["reasoning_effort"] = effort.into();
            }
            let started = Instant::now();
            let response = Client::new()
                .post(url)
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await
                .expect("send");
            let status = response.status();
            let text = response.text().await.expect("body");
            let reply: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            println!(
                "model={model} reasoning_effort={effort:?} status={status} time={:.1}s reply={:?}",
                started.elapsed().as_secs_f64(),
                reply
                    .pointer("/choices/0/message/content")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&text[..text.len().min(300)])
            );
        }
    }
}
