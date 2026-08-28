//! On-disk persistence for dsh and app diagnostics.
//!
//! Strategy: two log-file families under `<app_data_dir>/logs/`:
//!   * `dsh-YYYY-MM-DD.log` — child stdout/stderr, streamed per line at
//!     spawn time (append-only, low volume, mirrored to the frontend
//!     `dsh-log` event stream).
//!   * `app-YYYY-MM-DD.log` — structured diagnostics from the Tauri
//!     host: setup, tray, window events, spawn/restart/shutdown
//!     decisions, navigation. Append-only, verbose enough to reproduce
//!     a "refresh dies, restart doesn't fix it" bug.
//! Both families share the same retention / pruning policy (7 days).
//! The file is opened per spawn/per boot via a fresh handle; we never
//! hold a global writer so tests that lack an AppHandle can still load
//! the module without initialization.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Manager};
use time::macros::format_description;
use time::OffsetDateTime;

/// How many days of logs to keep on disk. Anything older is pruned at
/// the next `prune_old_logs` call (which runs at every dsh spawn and
/// at app boot).
const LOG_RETENTION_DAYS: i64 = 7;

const DSH_FILENAME_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("dsh-[year]-[month]-[day].log");
const APP_FILENAME_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("app-[year]-[month]-[day].log");

const LOGS_SUBDIR: &str = "logs";

/// Resolved path of `<app_data_dir>/logs`. Creates the directory if
/// missing. Used by the public helpers below.
fn logs_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir unavailable: {e}"))?;
    let dir = base.join(LOGS_SUBDIR);
    fs::create_dir_all(&dir).map_err(|e| format!("create logs dir failed: {e}"))?;
    Ok(dir)
}

/// Resolved logs directory as a plain PathBuf, without an AppHandle.
/// Used by the diagnostic logger when no handle is available.
#[allow(dead_code)]
pub fn logs_dir_path() -> Option<PathBuf> {
    dirs_path_fallback()
}

fn dirs_path_fallback() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|appdata| PathBuf::from(appdata).join("com.deepseek.dsh-desktop-lite").join(LOGS_SUBDIR))
}

/// Path of today's dsh log file (UTC). We use UTC so a user who travels or
/// who keeps their machine on across midnight doesn't accidentally
/// create two files for the same logical day. Note that this is the
/// day the dsh process *spawned*, not the day of any particular line —
/// a dsh that started at 23:59 UTC and ran for two hours will keep
/// appending to yesterday's file. A future dsh restart will then
/// re-resolve to the new day. This is acceptable: log files for a
/// multi-day incident are still grouped under the spawn day, which
/// makes "what was the dsh doing on day X" easier to find.
pub(crate) fn current_log_path(app: &AppHandle) -> Result<PathBuf, String> {
    let now = OffsetDateTime::now_utc();
    let name = now
        .format(DSH_FILENAME_FORMAT)
        .map_err(|e| format!("format log filename: {e}"))?;
    Ok(logs_dir(app)?.join(name))
}

fn current_app_log_path(app: &AppHandle) -> Result<PathBuf, String> {
    let now = OffsetDateTime::now_utc();
    let name = now
        .format(APP_FILENAME_FORMAT)
        .map_err(|e| format!("format app log filename: {e}"))?;
    Ok(logs_dir(app)?.join(name))
}

/// Open today's log file in append mode, ready for per-line writes.
/// We use a plain `File` (no `BufWriter`) so each `write_all` lands in
/// the OS page cache immediately, then `sync_all()` commits it to
/// disk. With a `BufWriter`, lines could sit in the user-space buffer
/// until the relay thread ended (i.e. until the dsh child exited),
/// leaving the file at 0 bytes from `Get-Content`'s point of view for
/// the whole run. Per-line disk I/O is acceptable here: dsh's
/// stdout/stderr is low volume (one banner line + occasional log
/// lines).
pub fn open_today_writer(app: &AppHandle) -> Option<File> {
    let path = current_log_path(app).ok()?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// One-time cleanup of log files older than `LOG_RETENTION_DAYS`. Called
/// from `spawn_and_wait_for_url` so a fresh spawn also re-prunes
/// (cheap when there's nothing to delete; idempotent).
pub fn prune_old_logs(app: &AppHandle) {
    let dir = match logs_dir(app) {
        Ok(d) => d,
        Err(_) => return,
    };
    prune_dir(&dir);
}

fn prune_dir(dir: &Path) {
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(LOG_RETENTION_DAYS);
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_log_file(&path) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        let Some(modified) = modified else { continue };
        let file_time = match OffsetDateTime::from_unix_timestamp(modified.as_secs() as i64) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if file_time < cutoff {
            let _ = fs::remove_file(&path);
        }
    }
}

fn is_log_file(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str());
    if ext != Some("log") {
        return false;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with("dsh-") || name.starts_with("app-")
}

// ── app diagnostics logger ──────────────────────────────────────────

static APP_LOG_INIT: OnceLock<()> = OnceLock::new();
static APP_LOG_MUTEX: Mutex<()> = Mutex::new(());

fn now_utc_string() -> String {
    let t = OffsetDateTime::now_utc();
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]");
    t.format(&fmt).unwrap_or_else(|_| t.to_string())
}

/// Append one structured line to today's app log file. Never panics;
/// never propagates IO errors to the caller. Thread-safe (single
/// global mutex serializing writers).
pub fn log_app(app: &AppHandle, level: &str, target: &str, msg: &str) {
    let Some(path) = current_app_log_path(app).ok() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _guard = APP_LOG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let line = format!("{} [{:>5}] {:<18} {}\n", now_utc_string(), level, target, msg);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
        let _ = f.sync_all();
    }
    eprintln!("[{}] {}: {}", level, target, msg);
    let _ = APP_LOG_INIT.set(());
}

/// Best-effort helper for callers that have a log line but no
/// AppHandle yet (rare). Falls back to %APPDATA% resolution.
#[allow(dead_code)]
pub fn log_app_fallback(level: &str, target: &str, msg: &str) {
    let Some(dir) = dirs_path_fallback() else { return };
    let _ = fs::create_dir_all(&dir);
    let now = OffsetDateTime::now_utc();
    let Ok(name) = now.format(APP_FILENAME_FORMAT) else { return };
    let path = dir.join(name);
    let _guard = APP_LOG_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let line = format!("{} [{:>5}] {:<18} {}\n", now_utc_string(), level, target, msg);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
        let _ = f.sync_all();
    }
    eprintln!("[{}] {}: {}", level, target, msg);
}

/// Write a single line to the log file with a stream prefix
/// (`·` for stdout, `✗` for stderr). Flushes OS-level buffers after
/// every line so the file is immediately readable from outside the
/// process, at the cost of one disk sync per line. Best-effort: any
/// IO error is swallowed and the file is left in whatever state it
/// managed to reach; we never want log writes to crash the relay.
pub fn write_line(file: &mut File, stream: &str, line: &str) {
    let tag = if stream == "stderr" { '✗' } else { '·' };
    let _ = file.write_all(&[tag as u8, b' ']);
    if line.ends_with('\n') {
        let _ = file.write_all(line.as_bytes());
    } else {
        let _ = file.write_all(line.as_bytes());
        let _ = file.write_all(b"\n");
    }
    let _ = file.flush();
    let _ = file.sync_all();
}

/// Return the logs directory path for external consumers (tray "open
/// logs folder" action).
pub fn logs_dir_for_app(app: &AppHandle) -> Option<PathBuf> {
    logs_dir(app).ok()
}
