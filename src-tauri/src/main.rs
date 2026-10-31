#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(all(windows, debug_assertions))]
    hide_own_console();
    contextai_lib::run();
}

/// Debug builds are console apps so `tauri dev` shows the logs. Started from autostart or Explorer,
/// Windows opens a console just for this process; detaching closes it. A terminal's console is kept.
#[cfg(all(windows, debug_assertions))]
fn hide_own_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};

    let mut processes = [0u32; 2];
    if unsafe { GetConsoleProcessList(&mut processes) } == 1 {
        let _ = unsafe { FreeConsole() };
    }
}
