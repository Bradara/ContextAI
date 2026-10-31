//! GitHub Copilot through the official Copilot SDK, driving the user's installed Copilot CLI.
//!
//! Sessions run in `ClientMode::Empty` with no tools and every permission request denied,
//! so Copilot only answers text and never touches files, shell or network on this machine.

use std::{path::PathBuf, sync::Arc, time::Duration};

use github_copilot_sdk::{
    handler::DenyAllHandler,
    types::{SessionConfig, SystemMessageConfig},
    CliProgram, Client, ClientMode, ClientOptions,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;

use crate::{ChatMessage, StreamPayload};

const REPLY_TIMEOUT: Duration = Duration::from_secs(300);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
pub struct CopilotState {
    client: Mutex<Option<(PathBuf, Client)>>,
}

/// ContextAI's own Copilot home (login, sessions), separate from the user's `~/.copilot`.
fn copilot_home(app: &AppHandle) -> Result<PathBuf, String> {
    let home = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("copilot");
    std::fs::create_dir_all(&home)
        .map_err(|error| format!("Неуспешно създаване на {}: {error}", home.display()))?;
    Ok(home)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotStatus {
    cli_path: Option<String>,
    authenticated: bool,
    login: Option<String>,
    message: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotModel {
    id: String,
    name: String,
    /// Premium-request multiplier (0 = included in the plan).
    multiplier: Option<f64>,
}

fn cli_name() -> &'static str {
    if cfg!(windows) {
        "copilot.exe"
    } else {
        "copilot"
    }
}

/// Finds the native Copilot CLI binary. The npm `copilot.cmd` shim cannot be spawned
/// directly, so the platform binary inside the npm package is used instead.
pub fn find_cli(configured: &str) -> Result<PathBuf, String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        if path.is_file() && path.extension().is_some_and(|ext| ext == "exe") {
            return Ok(path);
        }
        return Err(format!(
            "Copilot CLI не е намерен на „{configured}“. Посочете пътя до copilot.exe (не copilot.cmd)."
        ));
    }
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("COPILOT_CLI_PATH") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidates.push(
            PathBuf::from(appdata)
                .join(r"npm\node_modules\@github\copilot\node_modules\@github")
                .join(format!("copilot-win32-{arch}"))
                .join(cli_name()),
        );
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join(r"Microsoft\WinGet\Links")
                .join(cli_name()),
        );
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(cli_name())));
    }
    candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| {
        "Copilot CLI не е намерен. Инсталирайте го с `npm install -g @github/copilot` или `winget install GitHub.Copilot`, или посочете пътя до copilot.exe в настройките.".to_string()
    })
}

impl CopilotState {
    /// Returns the running client, starting the CLI on first use or when its path changed.
    async fn client(&self, cli: PathBuf, home: PathBuf) -> Result<Client, String> {
        let mut slot = self.client.lock().await;
        if let Some((path, client)) = slot.as_ref() {
            if *path == cli {
                return Ok(client.clone());
            }
        }
        if let Some((_, old)) = slot.take() {
            let _ = old.stop().await;
        }
        let mut options = ClientOptions::default();
        options.program = CliProgram::Path(cli.clone());
        options.mode = ClientMode::Empty;
        options.base_directory = Some(home);
        options
            .env
            .push(("COPILOT_GITHUB_TELEMETRY_DISABLED".into(), "true".into()));
        let client = Client::start(options)
            .await
            .map_err(|error| format!("Copilot CLI не стартира: {error}"))?;
        *slot = Some((cli, client.clone()));
        Ok(client)
    }

    /// Stops the CLI so the next call starts fresh (new login, uncached model list).
    async fn restart(&self) {
        if let Some((_, client)) = self.client.lock().await.take() {
            let _ = client.stop().await;
        }
    }
}

pub async fn status(app: &AppHandle, configured_path: &str) -> CopilotStatus {
    let cli = match find_cli(configured_path) {
        Ok(cli) => cli,
        Err(message) => {
            return CopilotStatus {
                cli_path: None,
                authenticated: false,
                login: None,
                message,
            }
        }
    };
    let cli_path = Some(cli.display().to_string());
    let result = async {
        let client = app
            .state::<CopilotState>()
            .client(cli, copilot_home(app)?)
            .await?;
        client
            .get_auth_status()
            .await
            .map_err(|error| format!("Неуспешна проверка на входа: {error}"))
    }
    .await;
    match result {
        Ok(auth) => CopilotStatus {
            cli_path,
            authenticated: auth.is_authenticated,
            message: if auth.is_authenticated {
                format!(
                    "Влезли сте като {}.",
                    auth.login.as_deref().unwrap_or("GitHub потребител")
                )
            } else {
                "Не сте влезли в GitHub. Натиснете „Вход в GitHub“.".to_string()
            },
            login: auth.login,
        },
        Err(message) => CopilotStatus {
            cli_path,
            authenticated: false,
            login: None,
            message,
        },
    }
}

pub async fn models(
    app: &AppHandle,
    configured_path: &str,
    refresh: bool,
) -> Result<Vec<CopilotModel>, String> {
    let cli = find_cli(configured_path)?;
    let state = app.state::<CopilotState>();
    if refresh {
        state.restart().await;
    }
    let client = state.client(cli, copilot_home(app)?).await?;
    let models = client
        .list_models()
        .await
        .map_err(|error| format!("Неуспешно зареждане на моделите: {error}"))?;
    Ok(models
        .into_iter()
        .filter(|model| {
            model.policy.as_ref().is_none_or(|policy| {
                !matches!(
                    policy.state,
                    github_copilot_sdk::rpc::ModelPolicyState::Disabled
                )
            })
        })
        .map(|model| CopilotModel {
            multiplier: model
                .billing
                .as_ref()
                .and_then(|billing| billing.multiplier),
            id: model.id,
            name: model.name,
        })
        .collect())
}

/// Runs `copilot login`, which opens the browser for GitHub authorization.
pub async fn login(app: &AppHandle, configured_path: &str) -> Result<(), String> {
    let cli = find_cli(configured_path)?;
    let mut command = tokio::process::Command::new(&cli);
    command
        .arg("login")
        .env("COPILOT_HOME", copilot_home(app)?)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = tokio::time::timeout(LOGIN_TIMEOUT, command.output())
        .await
        .map_err(|_| "Входът не завърши в рамките на 5 минути.".to_string())?
        .map_err(|error| format!("Неуспешно стартиране на copilot login: {error}"))?;
    // A running client keeps the old credentials, so pick up the new login.
    app.state::<CopilotState>().restart().await;
    if output.status.success() {
        return Ok(());
    }
    let details = String::from_utf8_lossy(&output.stderr);
    let details = if details.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout)
    } else {
        details
    };
    Err(format!("Входът в GitHub не успя: {}", details.trim()))
}

/// Our chat sends the whole history each time; a fresh Copilot session gets it as one prompt.
fn build_prompt(messages: &[ChatMessage]) -> (String, String) {
    let system = messages
        .iter()
        .filter(|message| message.role == "system")
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let conversation = messages
        .iter()
        .filter(|message| message.role != "system")
        .collect::<Vec<_>>();
    let prompt = match conversation.as_slice() {
        [only] => only.content.clone(),
        _ => {
            let mut transcript = String::from(
                "Продължи следния разговор, като отговориш на последното съобщение.\n\n",
            );
            for message in &conversation {
                let speaker = if message.role == "assistant" {
                    "Асистент"
                } else {
                    "Потребител"
                };
                transcript.push_str(&format!("{speaker}:\n{}\n\n", message.content));
            }
            transcript
        }
    };
    (system, prompt)
}

pub async fn stream(
    app: &AppHandle,
    configured_path: &str,
    request_id: &str,
    model: &str,
    messages: &[ChatMessage],
) -> Result<(), String> {
    let cli = find_cli(configured_path)?;
    let client = app
        .state::<CopilotState>()
        .client(cli, copilot_home(app)?)
        .await?;
    let (system, prompt) = build_prompt(messages);
    let mut config = SessionConfig::default()
        .with_permission_handler(Arc::new(DenyAllHandler))
        .with_streaming(true)
        .with_available_tools(Vec::<String>::new())
        .with_system_message(
            SystemMessageConfig::new()
                .with_mode("replace")
                .with_content(system),
        );
    if let Ok(directory) = app.path().app_cache_dir() {
        config = config.with_working_directory(directory);
    }
    if !model.trim().is_empty() {
        config = config.with_model(model.trim());
    }
    let session = client
        .create_session(config)
        .await
        .map_err(|error| format!("Неуспешно създаване на Copilot сесия: {error}"))?;
    let mut events = session.subscribe();
    let result = async {
        session
            .send(prompt)
            .await
            .map_err(|error| format!("Неуспешно изпращане към Copilot: {error}"))?;
        // Events arrive in order, so every delta is forwarded before `session.idle`.
        loop {
            let event = tokio::time::timeout(REPLY_TIMEOUT, events.recv())
                .await
                .map_err(|_| "Copilot не отговори навреме.".to_string())?
                .map_err(|error| format!("Връзката с Copilot прекъсна: {error}"))?;
            match event.event_type.as_str() {
                "assistant.message_delta" => {
                    let content = event
                        .data
                        .get("deltaContent")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    if !content.is_empty() {
                        app.emit(
                            "chat-delta",
                            StreamPayload {
                                request_id: request_id.to_string(),
                                content: content.to_string(),
                            },
                        )
                        .map_err(|error| error.to_string())?;
                    }
                }
                "session.error" => {
                    let message = event
                        .data
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("неизвестна грешка");
                    return Err(format!("Грешка от Copilot: {message}"));
                }
                "session.idle" => return Ok(()),
                _ => {}
            }
        }
    }
    .await;
    // Delete the session so these chats don't pile up in the Copilot CLI history.
    let id = session.id().clone();
    let _ = session.disconnect().await;
    let _ = client.delete_session(&id).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn single_message_is_sent_as_is() {
        let (system, prompt) =
            build_prompt(&[message("system", "Be brief."), message("user", "Здравей")]);
        assert_eq!(system, "Be brief.");
        assert_eq!(prompt, "Здравей");
    }

    #[test]
    fn history_becomes_a_transcript() {
        let (_, prompt) = build_prompt(&[
            message("user", "Първи въпрос"),
            message("assistant", "Отговор"),
            message("user", "Втори въпрос"),
        ]);
        assert!(prompt.contains("Потребител:\nПърви въпрос"));
        assert!(prompt.contains("Асистент:\nОтговор"));
        assert!(prompt.trim_end().ends_with("Втори въпрос"));
    }

    #[test]
    fn configured_cmd_shim_is_rejected() {
        assert!(find_cli(r"C:\Users\x\AppData\Roaming\npm\copilot.cmd").is_err());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Talks to the real Copilot CLI: `cargo test copilot_live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "starts the installed Copilot CLI"]
    async fn copilot_live_status_and_models() {
        let cli = find_cli("").expect("Copilot CLI");
        println!("cli: {}", cli.display());
        let state = CopilotState::default();
        let home = PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA"))
            .join("com.contextai.desktop")
            .join("copilot");
        std::fs::create_dir_all(&home).expect("home");
        let client = state.client(cli, home).await.expect("client");
        let auth = client.get_auth_status().await.expect("auth");
        println!(
            "authenticated: {} login: {:?}",
            auth.is_authenticated, auth.login
        );
        let models = client.list_models().await.expect("models");
        for model in models {
            println!(
                "{} | {} | {:?}",
                model.id,
                model.name,
                model.billing.and_then(|b| b.multiplier)
            );
        }
        state.restart().await;
    }
}
