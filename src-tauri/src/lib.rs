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
pub mod logs;
pub mod settings;

use once_cell::sync::OnceCell;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, RunEvent, WindowEvent};

/// Cached URL of the app's own boot page (Vite dev URL in dev mode, or
/// the tauri:// origin in a bundled build). Captured once at setup so
/// that a later `restart()` always navigates back to the real boot page
/// — even if the WebView is currently on a dsh URL.
pub(crate) static BOOT_URL: OnceCell<String> = OnceCell::new();

/// Returned to the frontend before it navigates; tells it which DSH state
/// to render (loading page, error page, etc.).
#[derive(Debug, Serialize, Clone)]
pub struct BootReport {
    pub deps_ok: bool,
    pub node: Option<String>,
    pub dsh: Option<String>,
    pub message: String,
}

/// Set to `true` by the tray menu's "Minimize to tray on close" check item.
/// Persisted to settings.json across launches.
static MINIMIZE_TO_TRAY: AtomicBool = AtomicBool::new(false);

/// Identifier used in tray menu events so we can match on it.
const MINIMIZE_TO_TRAY_ID: &str = "minimize_to_tray";
const SHOW_ID: &str = "show";
const HIDE_ID: &str = "hide";
const RESTART_ID: &str = "restart";
const QUIT_ID: &str = "quit";

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

/// Kill the running dsh and navigate the WebView back to the boot page,
/// which will re-run the normal boot flow and spawn a fresh dsh. Used by
/// the tray menu's "Restart DSH" entry, and callable from the frontend
/// as a recovery action when dsh becomes unresponsive.
#[tauri::command]
fn restart_dsh(app: tauri::AppHandle) -> Result<(), String> {
    dsh::restart(&app)
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

/// Frontend can read the current minimize-to-tray preference (e.g. to
/// render a hint in the UI). It's also persisted in settings.json so
/// the tray menu's CheckMenuItem is the source of truth.
#[tauri::command]
fn get_minimize_to_tray() -> bool {
    MINIMIZE_TO_TRAY.load(Ordering::Relaxed)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    // single-instance MUST be the first plugin (its docs require it): when
    // a second copy launches, the new process exits and we run this closure
    // in the original process, where the main window is already alive.
    builder = builder.plugin(tauri_plugin_single_instance::init(
        |app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        },
    ));

    builder
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::FULLSCREEN
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Load persisted preferences (currently just minimize-to-tray).
            let loaded = settings::load(app.handle())
                .unwrap_or_else(|_| settings::Settings::default());
            MINIMIZE_TO_TRAY.store(loaded.minimize_to_tray, Ordering::Relaxed);

            // Cache the boot page URL once, at the very start of setup,
            // BEFORE the WebView has had a chance to navigate to a dsh
            // URL. `restart()` later reads this so it can navigate the
            // WebView back here regardless of where it currently points.
            //
            // We deliberately do NOT use config.build.dev_url
            // (http://localhost:1420/ in dev) here: the dev server can
            // die or be unavailable, and navigating to it during a
            // restart would surface ERR_CONNECTION_REFUSED to the user.
            // Instead we always point at the tauri:// origin which:
            //   * in production, serves the bundled frontendDist assets
            //   * in dev, is intercepted by tauri-dev and serves the
            //     same Vite-served assets over the tauri:// scheme
            //     (no separate HTTP connection to 1420 needed)
            // This makes the boot URL robust regardless of Vite state.
            let boot_url = {
                if let Some(window) = app.get_webview_window("main") {
                    if let Ok(mut u) = window.url() {
                        u.set_path("/");
                        u.to_string()
                    } else {
                        "tauri://localhost/".to_string()
                    }
                } else {
                    "tauri://localhost/".to_string()
                }
            };
            let _ = BOOT_URL.set(boot_url);

            // ---- Tray icon + menu ----
            let show_item = MenuItem::with_id(app, SHOW_ID, "显示窗口", true, None::<&str>)?;
            let hide_item = MenuItem::with_id(app, HIDE_ID, "隐藏窗口", true, None::<&str>)?;
            let restart_item =
                MenuItem::with_id(app, RESTART_ID, "重启 DSH", true, None::<&str>)?;
            let min_tray_item = CheckMenuItem::with_id(
                app,
                MINIMIZE_TO_TRAY_ID,
                "关闭窗口时最小化到托盘",
                true,
                loaded.minimize_to_tray,
                None::<&str>,
            )?;
            let quit_item = MenuItem::with_id(app, QUIT_ID, "退出", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &show_item,
                    &hide_item,
                    &restart_item,
                    &min_tray_item,
                    &quit_item,
                ],
            )?;

            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("DSH Desktop Lite")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    SHOW_ID => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.unminimize();
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    HIDE_ID => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                    MINIMIZE_TO_TRAY_ID => {
                        // CheckMenuItem toggles itself on click. Read the
                        // new state and persist it.
                        let new_state = !MINIMIZE_TO_TRAY.load(Ordering::Relaxed);
                        MINIMIZE_TO_TRAY.store(new_state, Ordering::Relaxed);
                        let _ = settings::save(
                            app,
                            &settings::Settings {
                                minimize_to_tray: new_state,
                            },
                        );
                    }
                    RESTART_ID => {
                        // Kill the running dsh and navigate the WebView
                        // back to the boot page. The boot page's normal
                        // start_dsh will spawn a fresh dsh; its
                        // dsh-ready event will then navigate us to the
                        // new URL. If dsh is not running (e.g. cold
                        // start + immediate restart), this still works
                        // because the boot page will spawn one.
                        let _ = dsh::restart(app);
                    }
                    QUIT_ID => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // Left-click on the tray icon toggles the main window.
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            match w.is_visible() {
                                Ok(true) => {
                                    let _ = w.hide();
                                }
                                _ => {
                                    let _ = w.unminimize();
                                    let _ = w.show();
                                    let _ = w.set_focus();
                                }
                            }
                        }
                    }
                })
                .build(app)?;

            // ---- CloseRequested hook on the main window ----
            // The window-level on_window_event is the canonical place to
            // intercept CloseRequested on a specific window. We need to
            // capture main BEFORE moving the closure, since on_window_event
            // takes `&self` and our closure will outlive the borrow.
            let main_handle = app
                .get_webview_window("main")
                .ok_or_else(|| "main window missing in setup")?;
            let main_for_event = main_handle.clone();
            main_handle.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    if MINIMIZE_TO_TRAY.load(Ordering::Relaxed) {
                        // User wants to keep dsh running; just hide the
                        // window and let them come back via the tray.
                        api.prevent_close();
                        let _ = main_for_event.hide();
                    } else {
                        // Genuine quit path: kill the dsh child so we
                        // don't leak a Node process. The RunEvent hook
                        // below is a belt-and-braces fallback.
                        dsh::shutdown();
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            check_deps,
            start_dsh,
            restart_dsh,
            dsh_status,
            navigate_to,
            get_minimize_to_tray,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            // Belt-and-braces: even if the window was force-destroyed and
            // CloseRequested didn't fire, make sure dsh is killed when the
            // app is about to exit.
            if let RunEvent::ExitRequested { .. } = &event {
                dsh::shutdown();
            }
        });
}
