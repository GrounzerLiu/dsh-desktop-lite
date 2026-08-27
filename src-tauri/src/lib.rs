//! dsh-desktop-lite: a thin Tauri window that hosts the DSH Web UI.
//!
//! Architecture: this binary contains zero business logic. It just:
//!  1. Detects `node` and `dsh` on the host.
//!  2. Spawns `dsh web --no-open --port 0` as a child process.
//!  3. Reads the boot URL from the child's stdout and tells the WebView
//!     where to navigate.
//!  4. Forwards the child's stdout/stderr to the frontend as log events.
//!  5. Kills the child when the Tauri window closes.

pub mod deps;
pub mod dsh;

use serde::Serialize;
use tauri::{Manager, RunEvent, WindowEvent};

/// Returned to the frontend before it navigates; tells it which DSH state
/// to render (loading page, error page, etc.).
#[derive(Debug, Serialize, Clone)]
pub struct BootReport {
    pub deps_ok: bool,
    pub node: Option<String>,
    pub dsh: Option<String>,
    pub message: String,
}

/// Dependency check: where are node and dsh?
#[tauri::command]
fn check_deps() -> BootReport {
    let status = deps::check_all();
    if let Some(err) = deps::explain_missing(&status) {
        BootReport {
            deps_ok: false,
            node: status.node,
            dsh: status.dsh,
            message: err.message,
        }
    } else {
        BootReport {
            deps_ok: true,
            node: status.node,
            dsh: status.dsh,
            message: "依赖检查通过".into(),
        }
    }
}

/// Start dsh web in the background. The actual URL arrives via the
/// `dsh-ready` event once the child process boots.
#[tauri::command]
fn start_dsh(app: tauri::AppHandle) -> Result<String, String> {
    let status = deps::check_all();
    let dsh_path = status
        .dsh
        .ok_or_else(|| "未找到 dsh 命令".to_string())?;
    dsh::spawn_and_wait_for_url(&app, &dsh_path)
}

/// Live status snapshot for diagnostics / future "stop" button.
#[tauri::command]
fn dsh_status() -> dsh::DshStatus {
    dsh::status()
}

/// Navigate the main webview to a URL. The frontend cannot do this directly
/// in Tauri 2, so we expose it as a command. We refuse anything that is not
/// http(s); the DSH server always serves on `http://127.0.0.1:<port>`.
#[tauri::command]
fn navigate_to(app: tauri::AppHandle, url: String) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("refusing non-http url: {url}"));
    }
    let win = app
        .get_webview_window("main")
        .ok_or_else(|| "main window missing".to_string())?;
    win.navigate(url.parse().map_err(|e| format!("bad url: {e}"))?)
        .map_err(|e| format!("navigate failed: {e}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            check_deps,
            start_dsh,
            dsh_status,
            navigate_to,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|_app_handle, event| {
        // When the last window closes, terminate the dsh child so we do not
        // leave a stray Node process behind.
        if let RunEvent::WindowEvent {
            event: WindowEvent::CloseRequested { .. },
            ..
        } = &event
        {
            dsh::shutdown();
        }
        if let RunEvent::ExitRequested { .. } = &event {
            dsh::shutdown();
        }
    });
}
