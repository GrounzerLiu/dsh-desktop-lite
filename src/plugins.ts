// Plugin management window logic.
//
// Lists third-party DSH plugin entries from the Rust side (which scans the
// web profile's package.json bundles + each bundle's patch file), lets the
// user toggle them (writes cordis.patch.yml), and offers a manual "restart
// DSH" button because a config change only takes effect on the next dsh
// boot.

import { invoke } from "@tauri-apps/api/core";

interface PluginEntry {
  id: string;
  bundle: string;
  enabled: boolean;
}

const statusEl = document.getElementById("status") as HTMLElement;
const listEl = document.getElementById("plugin-list") as HTMLElement;

let toggling = new Set<string>();

function showStatus(text: string, isError = false) {
  statusEl.textContent = text;
  statusEl.className = `status ${isError ? "err" : "ok"}`;
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function render(plugins: PluginEntry[]) {
  listEl.innerHTML = "";
  if (plugins.length === 0) {
    listEl.innerHTML = '<div class="empty">未发现第三方插件</div>';
    return;
  }
  for (const p of plugins) {
    const row = document.createElement("div");
    row.className = "plugin-row";
    row.innerHTML = `
      <label class="switch" title="${escapeHtml(p.id)}">
        <input type="checkbox" ${p.enabled ? "checked" : ""} />
        <span class="slider"></span>
      </label>
      <div class="meta">
        <div class="pid">${escapeHtml(p.id)}</div>
        <div class="bundle">${escapeHtml(p.bundle)}</div>
      </div>
      <span class="state ${p.enabled ? "on" : "off"}">${p.enabled ? "启用" : "已禁用"}</span>
    `;
    const input = row.querySelector("input") as HTMLInputElement;
    input.addEventListener("change", () => {
      toggle(p.id, input.checked, input);
    });
    listEl.appendChild(row);
  }
}

async function load() {
  try {
    const plugins = await invoke<PluginEntry[]>("list_plugins");
    render(plugins);
  } catch (e) {
    showStatus(`加载插件列表失败：${String(e)}`, true);
  }
}

async function toggle(id: string, enabled: boolean, input: HTMLInputElement) {
  if (toggling.has(id)) {
    input.checked = !enabled; // still busy, revert the click
    return;
  }
  toggling.add(id);
  input.disabled = true;
  try {
    await invoke("set_plugin", { id, enabled });
    showStatus(`已${enabled ? "启用" : "禁用"} ${id}，重启 DSH 后生效`, false);
  } catch (e) {
    input.checked = !enabled; // failed, roll back the switch
    showStatus(`切换 ${id} 失败：${String(e)}`, true);
  } finally {
    toggling.delete(id);
    // Re-query so the whole list reflects the new state.
    try {
      const plugins = await invoke<PluginEntry[]>("list_plugins");
      render(plugins);
    } catch {
      /* keep current view */
    }
  }
}

document.getElementById("refresh")?.addEventListener("click", () => {
  showStatus("", false);
  statusEl.classList.add("hidden");
  load();
});

document.getElementById("restart")?.addEventListener("click", async () => {
  try {
    await invoke("restart_dsh");
    showStatus("已触发 DSH 重启，配置将在新实例中生效", false);
  } catch (e) {
    showStatus(`重启 DSH 失败：${String(e)}`, true);
  }
});

load();
