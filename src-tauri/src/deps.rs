//! Dependency detection: verify the host system has `node` and `dsh` available.
//!
//! The DSH CLI is a pure Node.js program distributed as an npm package; it has
//! no native binary of its own, so this wrapper only needs a `node` runtime
//! and the `dsh` command on `PATH` (or the conventional Windows npm shim).

use serde::Serialize;
use which::which;

#[derive(Debug, Serialize, Clone)]
pub struct DepStatus {
    pub node: Option<String>,
    pub dsh: Option<String>,
}

impl DepStatus {
    pub fn is_ok(&self) -> bool {
        self.node.is_some() && self.dsh.is_some()
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct DepError {
    pub kind: String,
    pub message: String,
}

/// Locate the `node` executable on `PATH`.
///
/// On Windows, npm-installed commands are typically `.cmd` shims; `which`
/// already handles PATHEXT, so a bare `node` lookup works on every platform.
pub fn find_node() -> Option<String> {
    which("node")
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// Locate the `dsh` command. On Windows the user usually installed
/// `@deepseek-ai/dsh` globally, so the executable is `dsh.cmd` under
/// `%APPDATA%\npm`. Fall back to a bare `dsh` lookup for macOS/Linux.
pub fn find_dsh() -> Option<String> {
    // Try the bare name first — `which` honours PATHEXT on Windows.
    if let Ok(p) = which("dsh") {
        return Some(p.to_string_lossy().to_string());
    }
    // Windows fallback: look for the npm shim explicitly.
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let candidate = std::path::PathBuf::from(appdata)
                .join("npm")
                .join("dsh.cmd");
            if candidate.exists() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Check both dependencies and return their paths (or `None` when missing).
pub fn check_all() -> DepStatus {
    DepStatus {
        node: find_node(),
        dsh: find_dsh(),
    }
}

/// Format a human-friendly error message for missing dependencies.
pub fn explain_missing(status: &DepStatus) -> Option<DepError> {
    match (status.node.as_ref(), status.dsh.as_ref()) {
        (None, None) => Some(DepError {
            kind: "both_missing".into(),
            message: "未找到 node 和 dsh。请先安装 Node.js (>=18) 并 `npm i -g @deepseek-ai/dsh`。"
                .into(),
        }),
        (None, _) => Some(DepError {
            kind: "node_missing".into(),
            message: "未找到 node。请先安装 Node.js (>=18)：https://nodejs.org".into(),
        }),
        (_, None) => Some(DepError {
            kind: "dsh_missing".into(),
            message: "未找到 dsh。请先运行 `npm i -g @deepseek-ai/dsh` 安装 DSH CLI。".into(),
        }),
        (Some(_), Some(_)) => None,
    }
}
