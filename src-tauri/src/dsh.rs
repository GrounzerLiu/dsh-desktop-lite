//! Spawn and supervise the `dsh web` child process.
//!
//! `dsh web --no-open --port 0` lets the OS pick a free port; the chosen URL
//! is printed to stdout as `dsh web: http://127.0.0.1:<port>`. We read the
//! child's combined output line by line, surface boot diagnostics to the
//! frontend via events, and once the URL is seen we publish it as the
//! authoritative entry point for the WebView.

use once_cell::sync::OnceCell;
use regex::Regex;
use serde::Serialize;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter, Manager};

/// The "URL is ready" payload published to the frontend.
#[derive(Debug, Serialize, Clone)]
pub struct DshReady {
    pub url: String,
}

/// A diagnostic line streamed from the dsh child process.
#[derive(Debug, Serialize, Clone)]
pub struct DshLog {
    pub stream: &'static str, // "stdout" | "stderr"
    pub line: String,
}

/// Failure payload. The frontend renders `message` prominently and
/// tacks `last_stderr` underneath so the user can see what dsh itself
/// said before giving up.
#[derive(Debug, Serialize, Clone)]
pub struct DshError {
    pub message: String,
    pub last_stderr: Vec<String>,
}

/// Process status snapshot.
#[derive(Debug, Serialize, Clone)]
pub struct DshStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub url: Option<String>,
}

/// How many trailing stderr lines we keep for the error report. Bounded so
/// the payload stays small and the VecDeque doesn't grow forever.
const STDERR_TAIL_CAP: usize = 30;

/// Global child-process handle. `OnceCell` gives us a one-shot slot for the
/// single dsh process this app supervises; the inner `Mutex` lets us
/// interrogate the child (or kill it) from the Tauri command thread.
struct DshHandle {
    child: Arc<Mutex<Option<Child>>>,
    url: Arc<Mutex<Option<String>>>,
    /// Ring buffer of the most recent stderr lines, used to enrich the
    /// `dsh-error` event so the user sees what dsh said before it died.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Set by `restart()` before killing the old dsh, so the old wait
    /// thread knows to stop emitting events for a process it is about to
    /// be killed. Reset back to false by `spawn_and_wait_for_url` so a
    /// fresh wait thread can do its job for the new dsh.
    shutting_down: Arc<AtomicBool>,
}

impl DshHandle {
    fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            url: Arc::new(Mutex::new(None)),
            stderr_tail: Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_CAP))),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }
}

static HANDLE: OnceCell<DshHandle> = OnceCell::new();

fn handle() -> &'static DshHandle {
    HANDLE.get_or_init(DshHandle::new)
}

fn url_regex() -> &'static Regex {
    // Matches a URL that DSH announces with its boot banner, e.g.
    //   "dsh web: http://127.0.0.1:63399"
    // We capture only the URL portion (group 1) so we never pass the
    // surrounding "dsh web: " prefix to `WebviewWindow::navigate`, which
    // refuses anything that is not a bare http(s) URL.
    static RE: OnceCell<Regex> = OnceCell::new();
    RE.get_or_init(|| {
        Regex::new(r"dsh web:\s+(https?://\S+)")
            .expect("valid url regex")
    })
}

/// Pull the bare URL out of a DSH boot-banner line, or `None` if the line
/// isn't the banner. Exposed (crate-private) for unit tests.
pub(crate) fn extract_url(line: &str) -> Option<String> {
    let caps = url_regex().captures(line)?;
    caps.get(1).map(|m| m.as_str().to_string())
}

/// Spawn the dsh web child process. Returns the chosen URL once the boot
/// line is observed, otherwise an error.
///
/// We pass `--no-open` so DSH does not try to launch a system browser — the
/// WebView inside the Tauri window is the only browser this app wants.
/// `--port 0` lets the OS pick a free port, sidestepping the 3080 default
/// which may already be in use.
pub fn spawn_and_wait_for_url(app: &AppHandle, dsh_path: &str) -> Result<String, String> {
    let mut cmd = Command::new(dsh_path);
    cmd.arg("web")
        .arg("--no-open")
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Inherit the user's working directory so DSH sees their $DSH_HOME, env,
    // and the project they were last in. On Windows a `dsh.cmd` shim is a
    // batch script — Command::new already handles the .cmd resolution.
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("启动 dsh 失败 ({}): {}", dsh_path, e))?;

    let pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法获取 dsh stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法获取 dsh stderr".to_string())?;

    // Stash the child handle so the shutdown hook can kill it.
    {
        let h = handle();
        *h.child.lock().unwrap() = Some(child);
        // If we're called during/after a restart, clear the shutdown
        // flag so the new wait thread is free to emit dsh-ready for the
        // new dsh.
        h.shutting_down.store(false, Ordering::Relaxed);
    }

    // Prune logs older than the retention window (best-effort) and
    // open today's log file. We pass an Option<Arc<Mutex<File>>> to
    // the relays so they can keep streaming on IO error: if open
    // fails, we just skip file logging for this spawn.
    crate::logs::prune_old_logs(app);
    let log_writer: Option<Arc<Mutex<std::fs::File>>> =
        crate::logs::open_today_writer(app).map(|f| Arc::new(Mutex::new(f)));

    // Stream stdout/stderr on background threads; the stdout thread reports
    // the resolved URL back as soon as the boot banner appears. The stderr
    // thread also keeps a rolling tail so we can attach the last N lines
    // to any `dsh-error` event. Both threads also mirror each line to
    // the on-disk log writer if one is available.
    spawn_stdout_relay(app.clone(), stdout, log_writer.clone());
    spawn_stderr_relay(app.clone(), stderr, log_writer.clone());

    // Wait (on a background thread) for the URL to be published, then
    // forward it to the frontend as a single "dsh-ready" event. If the
    // child exits or the 60s deadline elapses first, surface a structured
    // `dsh-error` with the trailing stderr.
    //
    // The loop is cooperatively cancelled by `restart()` setting the
    // `shutting_down` flag before killing the old dsh — we then stop
    // emitting events so the *old* frontend page (which is about to be
    // replaced anyway) doesn't navigate to a dead URL or flash an error.
    let app_for_wait = app.clone();
    let url_arc = handle().url.clone();
    let shutting_down_arc = handle().shutting_down.clone();
    thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            // Cooperatively stop if restart() is mid-flight.
            if shutting_down_arc.load(Ordering::Relaxed) {
                return;
            }
            if let Some(url) = url_arc.lock().unwrap().clone() {
                // Re-check after taking the lock — restart() may have
                // flipped shutting_down while we were waiting for it.
                if shutting_down_arc.load(Ordering::Relaxed) {
                    return;
                }
                let _ = app_for_wait.emit("dsh-ready", DshReady { url });
                return;
            }
            // Check if the child has already exited without ever printing
            // the boot banner. `try_wait` reuses the same `Child` handle
            // we stashed in HANDLE.
            let exited = {
                let mut guard = handle().child.lock().unwrap();
                match guard.as_mut() {
                    Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                    None => true, // someone (shutdown) already reaped it
                }
            };
            if exited {
                if shutting_down_arc.load(Ordering::Relaxed) {
                    return;
                }
                emit_dsh_error(
                    &app_for_wait,
                    "dsh 进程已退出，未打印启动 URL。",
                );
                return;
            }
            if std::time::Instant::now() >= deadline {
                if shutting_down_arc.load(Ordering::Relaxed) {
                    return;
                }
                emit_dsh_error(
                    &app_for_wait,
                    "等待 dsh 启动超时 (60s)。请检查依赖或查看日志。",
                );
                return;
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    // Return the pid immediately; the actual URL arrives via the event.
    Ok(format!("dsh started (pid {})", pid))
}

/// Snapshot the trailing stderr lines and emit a `dsh-error` event so the
/// frontend can show the user what dsh said before giving up.
fn emit_dsh_error(app: &AppHandle, message: &str) {
    let last_stderr: Vec<String> = handle()
        .stderr_tail
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();
    let _ = app.emit(
        "dsh-error",
        DshError {
            message: message.to_string(),
            last_stderr,
        },
    );
}

fn spawn_stdout_relay(
    app: AppHandle,
    stdout: ChildStdout,
    log_writer: Option<Arc<Mutex<std::fs::File>>>,
) {
    let app2 = app.clone();
    let url_arc = handle().url.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            // Mirror to disk first so a write failure doesn't suppress
            // the in-memory / event path; file logging is best-effort.
            if let Some(w) = &log_writer {
                if let Ok(mut g) = w.lock() {
                    let _ = crate::logs::write_line(&mut g, "stdout", &line);
                }
            }
            // Surface every line to the UI as a log event.
            let _ = app2.emit(
                "dsh-log",
                DshLog {
                    stream: "stdout",
                    line: line.clone(),
                },
            );
            if let Some(url) = extract_url(&line) {
                *url_arc.lock().unwrap() = Some(url);
                // do not break — keep draining the pipe so the child does
                // not block on a full stdout buffer.
            }
        }
    });
}

fn spawn_stderr_relay(
    app: AppHandle,
    stderr: ChildStderr,
    log_writer: Option<Arc<Mutex<std::fs::File>>>,
) {
    let tail_arc = handle().stderr_tail.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(w) = &log_writer {
                if let Ok(mut g) = w.lock() {
                    let _ = crate::logs::write_line(&mut g, "stderr", &line);
                }
            }
            // Push onto the rolling tail (newest at the back) for later
            // use by emit_dsh_error.
            {
                let mut tail = tail_arc.lock().unwrap();
                if tail.len() == STDERR_TAIL_CAP {
                    tail.pop_front();
                }
                tail.push_back(line.clone());
            }
            let _ = app.emit(
                "dsh-log",
                DshLog {
                    stream: "stderr",
                    line,
                },
            );
        }
    });
}

/// Kill the dsh child process if it is still running. Best-effort: any
/// failure (already exited, no handle stashed) is silently ignored because
/// the OS will reap the process when this app exits anyway.
pub fn shutdown() {
    if let Some(h) = HANDLE.get() {
        if let Some(mut child) = h.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Kill the running dsh child (if any) and point the WebView back at the
/// app's own boot page. The fresh `index.html` + `main.ts` will run the
/// usual boot flow (`check_deps` → `start_dsh`) and spawn a new dsh. The
/// `dsh-ready` event from the new spawn is picked up by the boot-time
/// listener and navigates the WebView to the fresh URL.
///
/// This is what the tray menu's "Restart DSH" entry calls. It deliberately
/// does NOT spawn dsh itself — if it did, the boot page's own start_dsh
/// would race it and we would end up with two dsh instances on different
/// ports.
pub fn restart(app: &AppHandle) -> Result<(), String> {
    // Order matters: flip `shutting_down` and clear the remembered URL
    // *before* killing the old child. The wait thread for the old dsh
    // polls every 100ms; if it observes the stale URL after the kill
    // but before we clear it, it would emit `dsh-ready` for a dead
    // process and the (about-to-be-replaced) frontend would navigate
    // to a dead origin. The shutting_down flag is a second line of
    // defence: even if clearing races the wait thread, the flag tells
    // the wait thread to drop its events.
    if let Some(h) = HANDLE.get() {
        h.shutting_down.store(true, Ordering::Relaxed);
        h.url.lock().unwrap().take();
        h.stderr_tail.lock().unwrap().clear();
    }
    shutdown();
    // Resolve the boot URL via the lib.rs-cached `BOOT_URL` rather than
    // inspecting the current WebView URL — by the time we're called the
    // WebView is on a dsh URL, and using that would navigate us back
    // to the dead dsh origin (see review C.1).
    let boot_url = crate::BOOT_URL
        .get()
        .ok_or_else(|| "boot URL not initialized yet".to_string())?;
    if let Some(window) = app.get_webview_window("main") {
        window
            .navigate(boot_url.parse().map_err(|e| format!("bad boot url: {e}"))?)
            .map_err(|e| format!("navigate to boot page failed: {e}"))?;
    }
    Ok(())
}

/// Lightweight status snapshot for the frontend.
pub fn status() -> DshStatus {
    if let Some(h) = HANDLE.get() {
        let mut guard = h.child.lock().unwrap();
        let running = guard
            .as_mut()
            .map(|c| matches!(c.try_wait(), Ok(None)))
            .unwrap_or(false);
        if !running {
            *guard = None;
        }
        DshStatus {
            running,
            pid: guard.as_ref().map(|c| c.id()),
            url: h.url.lock().unwrap().clone(),
        }
    } else {
        DshStatus {
            running: false,
            pid: None,
            url: None,
        }
    }
}

/// Helper used only by tests / diagnostics.
#[allow(dead_code)]
pub fn placeholder() -> PathBuf {
    PathBuf::new()
}

#[cfg(test)]
mod tests {
    use super::extract_url;

    #[test]
    fn extracts_bare_url_from_canonical_banner() {
        assert_eq!(
            extract_url("dsh web: http://127.0.0.1:63399").as_deref(),
            Some("http://127.0.0.1:63399"),
        );
    }

    #[test]
    fn extracts_bare_url_with_log_prefix() {
        // Some DSH builds wrap the banner in a logger frame, e.g. [dsh].
        assert_eq!(
            extract_url("[dsh] dsh web: http://127.0.0.1:3099").as_deref(),
            Some("http://127.0.0.1:3099"),
        );
    }

    #[test]
    fn extracts_https_url() {
        assert_eq!(
            extract_url("dsh web: https://localhost:3080").as_deref(),
            Some("https://localhost:3080"),
        );
    }

    #[test]
    fn preserves_path_and_query() {
        assert_eq!(
            extract_url("dsh web: http://127.0.0.1:3099/foo?x=1").as_deref(),
            Some("http://127.0.0.1:3099/foo?x=1"),
        );
    }

    #[test]
    fn does_not_swallow_extra_trailing_token() {
        // Without a trailing word boundary the \S+ group should still
        // stop at whitespace, so a trailing token on the same line stays
        // out of the URL.
        assert_eq!(
            extract_url("dsh web: http://127.0.0.1:54501 ready").as_deref(),
            Some("http://127.0.0.1:54501"),
        );
    }

    #[test]
    fn ignores_lines_without_dsh_banner() {
        // A URL that isn't preceded by "dsh web:" must not be picked up,
        // otherwise we could navigate to an unrelated host that just
        // happened to appear in some dsh log line.
        assert_eq!(extract_url("random http://not-from-dsh.example/ line"), None);
        assert_eq!(extract_url(""), None);
        assert_eq!(
            extract_url("listening on http://127.0.0.1:9999"),
            None,
        );
    }

    #[test]
    fn rejects_non_http_schemes() {
        // We never want to feed file://, ws://, etc. into navigate.
        assert_eq!(extract_url("dsh web: file:///etc/passwd"), None);
        assert_eq!(extract_url("dsh web: ws://127.0.0.1:63399"), None);
    }
}
