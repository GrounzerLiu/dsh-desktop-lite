//! Tiny stdlib-only persistence for user preferences that don't justify a
//! full plugin (e.g. `tauri-plugin-store`). The file lives in the app's
//! data dir; the format is one JSON object.
//!
//! Currently just one setting: `minimize_to_tray`. Add fields as needed;
//! keep the on-disk format backwards-compatible by giving new fields
//! `Default` impls.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

const SETTINGS_FILENAME: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// When true, clicking the window's close button hides the window and
    /// keeps dsh running. When false (the default), closing the window
    /// also kills dsh and exits the app.
    #[serde(default)]
    pub minimize_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            minimize_to_tray: false,
        }
    }
}

/// Resolve `<app_data_dir>/settings.json`, creating the parent dir if
/// needed. We deliberately do not return an error on creation failure
/// inside `save` — the worst case is the next launch forgets the
/// preference, which is recoverable.
fn settings_path(app: &AppHandle) -> io::Result<PathBuf> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::create_dir_all(&dir)?;
    Ok(dir.join(SETTINGS_FILENAME))
}

/// Load settings from disk. Missing file = default settings, not an
/// error. Malformed file = log + default, not a panic.
pub fn load(app: &AppHandle) -> io::Result<Settings> {
    let path = settings_path(app)?;
    if !path.exists() {
        return Ok(Settings::default());
    }
    let bytes = fs::read(&path)?;
    match serde_json::from_slice::<Settings>(&bytes) {
        Ok(s) => Ok(s),
        Err(e) => {
            eprintln!("[settings] {} not parseable ({}), using defaults", path.display(), e);
            Ok(Settings::default())
        }
    }
}

/// Persist settings to disk. Best-effort: we log on failure but never
/// panic, because this is a UX nice-to-have and the user can always
/// re-toggle the checkbox.
pub fn save(app: &AppHandle, settings: &Settings) -> io::Result<()> {
    let path = settings_path(app)?;
    let bytes = serde_json::to_vec_pretty(settings)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    fs::write(&path, bytes)
}
