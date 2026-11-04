//! Start with Windows through the current user's `Run` registry key.
//!
//! This replaces tauri-plugin-autostart: its auto-launch crate writes the exe path without
//! quotes, so for a path with a space (e.g. `C:\Users\First Last\…`) Windows runs
//! `C:\Users\First` and the launch fails with error 193.

use std::path::Path;

use windows_registry::{Type, CURRENT_USER};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Task Manager's on/off switch for startup apps; an odd first byte means the user disabled it.
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
const APPROVED_ENABLED: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const VALUE_NAME: &str = "ContextAI";

fn quoted(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

fn current_command() -> Result<String, String> {
    std::env::current_exe()
        .map(|exe| quoted(&exe))
        .map_err(|error| format!("Неуспешно определяне на пътя до приложението: {error}"))
}

/// Whether an unquoted Run value written by the old plugin points at this executable.
fn is_unquoted_entry_for(value: &str, exe: &Path) -> bool {
    let value = value.trim();
    !value.starts_with('"') && value.eq_ignore_ascii_case(&exe.display().to_string())
}

pub fn is_enabled() -> Result<bool, String> {
    let Ok(run) = CURRENT_USER.open(RUN_KEY) else {
        return Ok(false);
    };
    if run.get_string(VALUE_NAME).is_err() {
        return Ok(false);
    }
    let disabled_in_task_manager = CURRENT_USER
        .open(APPROVED_KEY)
        .and_then(|key| key.get_value(VALUE_NAME))
        .is_ok_and(|value| value.first().is_some_and(|byte| byte & 1 == 1));
    Ok(!disabled_in_task_manager)
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let error = |error: windows_result::Error| format!("Грешка в регистъра на Windows: {error}");
    if enabled {
        let command = current_command()?;
        CURRENT_USER
            .create(RUN_KEY)
            .and_then(|run| run.set_string(VALUE_NAME, command))
            .map_err(error)?;
        // Re-enable the entry in case it was switched off in Task Manager.
        if let Ok(approved) = CURRENT_USER.options().write().open(APPROVED_KEY) {
            approved
                .set_bytes(VALUE_NAME, Type::Bytes, &APPROVED_ENABLED)
                .map_err(error)?;
        }
    } else {
        // Missing values are fine: autostart is already off.
        if let Ok(run) = CURRENT_USER.options().write().open(RUN_KEY) {
            let _ = run.remove_value(VALUE_NAME);
        }
        if let Ok(approved) = CURRENT_USER.options().write().open(APPROVED_KEY) {
            let _ = approved.remove_value(VALUE_NAME);
        }
    }
    Ok(())
}

/// Quotes an entry written by the old plugin so that autostart starts working without the
/// user having to toggle the setting again.
pub fn repair_unquoted_entry() {
    let (Ok(exe), Ok(run)) = (
        std::env::current_exe(),
        CURRENT_USER.options().read().write().open(RUN_KEY),
    ) else {
        return;
    };
    if let Ok(value) = run.get_string(VALUE_NAME) {
        if is_unquoted_entry_for(&value, &exe) {
            if let Err(error) = run.set_string(VALUE_NAME, quoted(&exe)) {
                eprintln!("Could not repair the autostart entry: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_with_spaces_is_quoted() {
        let exe = Path::new(r"C:\Users\First Last\AppData\Local\ContextAI\contextai.exe");
        assert_eq!(
            quoted(exe),
            r#""C:\Users\First Last\AppData\Local\ContextAI\contextai.exe""#
        );
    }

    #[test]
    fn only_the_old_unquoted_entry_for_this_exe_is_repaired() {
        let exe = Path::new(r"C:\Users\First Last\AppData\Local\ContextAI\contextai.exe");
        // The old plugin wrote the path unquoted with a trailing space.
        assert!(is_unquoted_entry_for(
            r"C:\Users\First Last\AppData\Local\ContextAI\contextai.exe ",
            exe
        ));
        assert!(!is_unquoted_entry_for(&quoted(exe), exe));
        assert!(!is_unquoted_entry_for(r"D:\Other\contextai.exe", exe));
    }
}
