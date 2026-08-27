// dsh-desktop-lite boot screen.
//
// Flow:
//   1. Ask Rust which dependencies are available.
//   2. If something is missing, render the error screen and stop.
//   3. Otherwise, fire `start_dsh` and listen for `dsh-ready` / `dsh-log`
//      events. When the URL arrives, navigate the WebView to it.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

interface BootReport {
  deps_ok: boolean;
  node: string | null;
  dsh: string | null;
  message: string;
}

interface DshReady {
  url: string;
}

interface DshLog {
  stream: "stdout" | "stderr";
  line: string;
}

interface DshError {
  message: string;
  last_stderr: string[];
}

const loadingEl = document.getElementById("loading") as HTMLElement;
const errorEl = document.getElementById("error") as HTMLElement;
const errorMsgEl = document.getElementById("error-message") as HTMLElement;
const errorStderrEl = document.getElementById("error-stderr") as HTMLPreElement;
const errorStderrHeadingEl = document.getElementById(
  "error-stderr-heading",
) as HTMLElement;
const logEl = document.getElementById("log") as HTMLPreElement;

const MAX_LOG_LINES = 80;

function appendLog(text: string) {
  // Prepend so newest is on top; trim to a sane line count.
  const lines = (logEl.textContent ?? "").split("\n");
  lines.unshift(text);
  const trimmed = lines.slice(0, MAX_LOG_LINES).join("\n");
  logEl.textContent = trimmed;
  logEl.scrollTop = 0;
}

function showError(message: string, lastStderr: string[] = []) {
  errorMsgEl.textContent = message;
  if (lastStderr.length > 0) {
    errorStderrEl.textContent = lastStderr.join("\n");
    errorStderrEl.classList.remove("hidden");
    errorStderrHeadingEl.classList.remove("hidden");
  } else {
    errorStderrEl.textContent = "";
    errorStderrEl.classList.add("hidden");
    errorStderrHeadingEl.classList.add("hidden");
  }
  loadingEl.classList.add("hidden");
  errorEl.classList.remove("hidden");
}

async function navigateTo(url: string) {
  // Hide the loading screen before we hand the window over to DSH. The
  // page will be replaced by DSH's own UI, so any leftover DOM is fine.
  loadingEl.classList.add("hidden");
  // JS cannot navigate the webview to an arbitrary URL in Tauri 2 — the
  // Rust side controls that. Ask the backend to do it.
  await invoke("navigate_to", { url });
}

async function main() {
  // 1. Dependency check.
  let report: BootReport;
  try {
    report = await invoke<BootReport>("check_deps");
  } catch (e) {
    showError(`依赖检查失败：${String(e)}`);
    return;
  }
  if (!report.deps_ok) {
    showError(report.message);
    return;
  }
  appendLog(`✓ node: ${report.node}`);
  appendLog(`✓ dsh:  ${report.dsh}`);

  // 2. Subscribe to the dsh events BEFORE spawning so we never miss the
  //    ready signal (the child can boot in well under a second).
  const unlistens: UnlistenFn[] = [];

  unlistens.push(
    await listen<DshReady>("dsh-ready", (event) => {
      appendLog(`→ ${event.payload.url}`);
      navigateTo(event.payload.url).catch((e) => {
        showError(`跳转到 DSH 失败：${String(e)}`);
      });
    }),
  );

  unlistens.push(
    await listen<DshLog>("dsh-log", (event) => {
      const tag = event.payload.stream === "stderr" ? "✗" : "·";
      appendLog(`${tag} ${event.payload.line}`);
    }),
  );

  unlistens.push(
    await listen<DshError>("dsh-error", (event) => {
      const { message, last_stderr } = event.payload;
      showError(`DSH 启动失败：${message}`, last_stderr);
    }),
  );

  // 3. Start dsh. The actual URL arrives via the `dsh-ready` event.
  try {
    const ack = await invoke<string>("start_dsh");
    appendLog(ack);
  } catch (e) {
    showError(`启动 dsh 失败：${String(e)}`);
    unlistens.forEach((u) => u());
  }
}

window.addEventListener("DOMContentLoaded", () => {
  main().catch((e) => showError(`未捕获的错误：${String(e)}`));
});
