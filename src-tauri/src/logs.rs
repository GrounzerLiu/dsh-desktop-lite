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
//
// The app log is *diagnostic* — it does not need the real-time
// visibility that dsh.log needs (dsh.log is written per-line with
// sync_all so `Get-Content` always sees the latest line). For the app
// log we instead:
//   * buffer lines in a process-wide BufWriter,
//   * flush to disk every FLUSH_INTERVAL from a background thread,
//   * roll to a numbered sibling (app-YYYY-MM-DD.1.log) when the file
//     exceeds APP_LOG_MAX_BYTES,
//   * switch to a fresh file when the UTC date changes.
// This bounds disk I/O to one write-batch per interval instead of one
// open+sync per line, which matters on the restart path where several
// log_app calls fire in quick succession.

use std::io::BufWriter;
use std::time::Duration;

/// How often the background flusher writes buffered app-log lines to
/// disk. Small enough that a crash loses at most ~2s of diagnostics.
const APP_FLUSH_INTERVAL: Duration = Duration::from_millis(2000);

/// Roll the app log when it exceeds this size. Bounded so a chatty
/// session can't grow a single file unboundedly.
const APP_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024; // 5 MiB

static APP_LOG_INIT: OnceLock<()> = OnceLock::new();

/// Global buffered writer for the app log plus the date/file it is
/// currently bound to. The mutex guards the whole struct; the flusher
/// thread holds it briefly every APP_FLUSH_INTERVAL.
static APP_WRITER: OnceLock<Mutex<Option<AppLogSink>>> = OnceLock::new();

struct AppLogSink {
    writer: BufWriter<File>,
    /// UTC date (YYYY-MM-DD) this sink was opened for, so we can roll
    /// when midnight passes.
    date: String,
}

fn utc_date_string() -> String {
    let t = OffsetDateTime::now_utc();
    let fmt = format_description!("[year]-[month]-[day]");
    t.format(&fmt).unwrap_or_default()
}

fn now_utc_string() -> String {
    let t = OffsetDateTime::now_utc();
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]");
    t.format(&fmt).unwrap_or_else(|_| t.to_string())
}

/// Open (or lazily create) the app log sink for the current date,
/// rolling to a new file if the date changed or the file is too big.
/// `fallback_dir` is used when the caller has no AppHandle.
fn ensure_app_sink(
    dir: &Path,
    date: &str,
    now_len: u64,
) -> std::io::Result<AppLogSink> {
    let name = format!("app-{date}.log");
    let path = dir.join(&name);
    let current_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    // Roll if the existing file already exceeds the cap.
    let actual_path = if current_len + now_len > APP_LOG_MAX_BYTES {
        let roll = dir.join(format!("app-{date}.1.log"));
        // Keep at most one roll; truncate the main file for a fresh start.
        let _ = fs::remove_file(&roll);
        if let Ok(f) = OpenOptions::new().create(true).write(true).truncate(true).open(&path) {
            let _ = f;
        }
        let _ = fs::copy(&path, &roll);
        let _ = fs::remove_file(&path);
        roll
    } else {
        path
    };
    let file = OpenOptions::new().create(true).append(true).open(&actual_path)?;
    Ok(AppLogSink {
        writer: BufWriter::with_capacity(8 * 1024, file),
        date: date.to_string(),
    })
}

/// Write a formatted line into the global buffered app log. No disk
/// I/O here beyond the in-memory buffer write. Never panics.
fn write_buffered(level: &str, target: &str, msg: &str, fallback_dir: Option<&Path>) {
    let date = utc_date_string();
    let line = format!("{} [{:>5}] {:<18} {}\n", now_utc_string(), level, target, msg);
    let bytes = line.as_bytes();
    // Resolve the directory, owning the fallback PathBuf long enough to
    // use it (can't borrow a temporary through a match arm).
    let owned_dir = fallback_dir
        .map(Path::to_path_buf)
        .or_else(dirs_path_fallback);
    let Some(dir) = owned_dir else { return };
    let _ = fs::create_dir_all(&dir);

    let sink_mutex = APP_WRITER.get_or_init(|| Mutex::new(None));
    let mut guard = sink_mutex.lock().unwrap_or_else(|e| e.into_inner());
    let needs_new = match guard.as_ref() {
        Some(s) => s.date != date,
        None => true,
    };
    if needs_new {
        match ensure_app_sink(&dir, &date, bytes.len() as u64) {
            Ok(sink) => *guard = Some(sink),
            Err(_) => return, // can't open log; drop the line
        }
    }
    if let Some(sink) = guard.as_mut() {
        let _ = sink.writer.write_all(bytes);
        // If the buffered size crosses the cap, flush + roll now.
        if sink.writer.buffer().len() as u64 >= APP_LOG_MAX_BYTES / 2 {
            let _ = sink.writer.flush();
        }
    }
}

/// Background flusher: every APP_FLUSH_INTERVAL, flush the app log
/// buffer to disk so external tools (and a crash) still see lines.
fn spawn_app_log_flusher() {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(APP_FLUSH_INTERVAL);
            if let Some(sink_mutex) = APP_WRITER.get() {
                if let Ok(mut guard) = sink_mutex.lock() {
                    if let Some(sink) = guard.as_mut() {
                        let _ = sink.writer.flush();
                    }
                }
            }
        }
    });
}

/// Append one structured line to today's app log file. Never panics;
/// never propagates IO errors to the caller. Thread-safe (single
/// global mutex serializing writers).
pub fn log_app(app: &AppHandle, level: &str, target: &str, msg: &str) {
    let dir = logs_dir(app).ok();
    write_buffered(level, target, msg, dir.as_deref());
    eprintln!("[{}] {}: {}", level, target, msg);
    let _ = APP_LOG_INIT.set(());
}

/// Best-effort helper for callers that have a log line but no
/// AppHandle yet (rare). Falls back to %APPDATA% resolution.
#[allow(dead_code)]
pub fn log_app_fallback(level: &str, target: &str, msg: &str) {
    write_buffered(level, target, msg, None);
    eprintln!("[{}] {}: {}", level, target, msg);
}

/// Ensure the background flusher is running. Called once from
/// `lib.rs` setup after the app log directory is known to exist.
pub fn init_app_log_flusher() {
    let _ = APP_LOG_INIT.get_or_init(|| {
        spawn_app_log_flusher();
        ()
    });
}

/// Flush any buffered app-log lines now (e.g. on shutdown). Best-effort.
pub fn flush_app_log() {
    if let Some(sink_mutex) = APP_WRITER.get() {
        if let Ok(mut guard) = sink_mutex.lock() {
            if let Some(sink) = guard.as_mut() {
                let _ = sink.writer.flush();
                let _ = sink.writer.get_ref().sync_all();
            }
        }
    }
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
