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
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter};

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

/// Process status snapshot.
#[derive(Debug, Serialize, Clone)]
pub struct DshStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub url: Option<String>,
}

/// Global child-process handle. `OnceCell` gives us a one-shot slot for the
/// single dsh process this app supervises; the inner `Mutex` lets us
/// interrogate the child (or kill it) from the Tauri command thread.
struct DshHandle {
    child: Arc<Mutex<Option<Child>>>,
    url: Arc<Mutex<Option<String>>>,
}

impl DshHandle {
    fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            url: Arc::new(Mutex::new(None)),
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
    }

    // Stream stdout/stderr on background threads; the stdout thread reports
    // the resolved URL back as soon as the boot banner appears.
    spawn_stdout_relay(app.clone(), stdout);
    spawn_stderr_relay(app.clone(), stderr);

    // Wait (on a background thread) for the URL to be published, then
    // forward it to the frontend as a single "dsh-ready" event.
    let app_for_wait = app.clone();
    let url_arc = handle().url.clone();
    thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if let Some(url) = url_arc.lock().unwrap().clone() {
                let _ = app_for_wait.emit("dsh-ready", DshReady { url });
                return;
            }
            if std::time::Instant::now() >= deadline {
                let _ = app_for_wait.emit(
                    "dsh-error",
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

fn spawn_stdout_relay(app: AppHandle, stdout: ChildStdout) {
    let app2 = app.clone();
    let url_arc = handle().url.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            // Surface every line to the UI as a log event.
            let _ = app2.emit(
                "dsh-log",
                DshLog {
                    stream: "stdout",
                    line: line.clone(),
                },
            );
            if let Some(m) = url_regex().captures(&line) {
                if let Some(url) = m.get(1) {
                    *url_arc.lock().unwrap() = Some(url.as_str().to_string());
                    // do not break — keep draining the pipe so the child does
                    // not block on a full stdout buffer.
                }
            }
        }
    });
}

fn spawn_stderr_relay(app: AppHandle, stderr: ChildStderr) {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
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
