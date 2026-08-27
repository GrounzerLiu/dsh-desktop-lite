//! On-disk persistence for dsh child process logs.
//!
//! Strategy: one log file per UTC day at
//! `<app_data_dir>/logs/dsh-YYYY-MM-DD.log`, append-only. On startup we
//! prune anything older than [`LOG_RETENTION_DAYS`] days in that directory
//! so the on-disk footprint stays bounded.
//!
//! The log file is opened (and BufWriter-wrapped) once per dsh child
//! spawn; the writer is moved into the stdout/stderr relay threads so
//! every line emitted by dsh is also written to disk in addition to
//! being forwarded to the frontend as a `dsh-log` event. We don't try
//! to share a single writer across processes — each spawn opens its own
//! (the previous one's `BufWriter` is dropped when the relay thread
//! ends), so concurrent writes can't interleave and we don't need
//! locking.
//!
//! Tradeoff: on a crash we may lose the last few unflushed lines.
//! Acceptable because dsh-process logs are best-effort diagnostics, not
//! the source of truth for anything.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use time::macros::format_description;
use time::OffsetDateTime;

/// How many days of logs to keep on disk. Anything older is pruned at
/// the next `prune_old_logs` call (which runs at every dsh spawn).
const LOG_RETENTION_DAYS: i64 = 7;

/// Filename template inside the logs directory. Day-granular so multiple
/// spawns on the same day append into the same file.
const LOG_FILENAME_FORMAT: &[time::format_description::FormatItem<'static>] =
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

/// Path of today's log file (UTC). We use UTC so a user who travels or
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
        .format(LOG_FILENAME_FORMAT)
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
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(LOG_RETENTION_DAYS);
    let entries = match fs::read_dir(&dir) {
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
    path.extension().and_then(|e| e.to_str()) == Some("log")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("dsh-"))
}

/// Write a single line to the log file with a stream prefix
/// (`·` for stdout, `✗` for stderr). Flushes OS-level buffers after
/// every line so the file is immediately readable from outside the
/// process, at the cost of one disk sync per line. Best-effort: any
/// IO error is swallowed and the file is left in whatever state it
/// managed to reach; we never want log writes to crash the relay.
pub fn write_line(file: &mut File, stream: &str, line: &str) {
    let tag = if stream == "stderr" { '✗' } else { '·' };
    // We do four small writes so a partial prefix still appears in the
    // file even if a write fails partway.
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
