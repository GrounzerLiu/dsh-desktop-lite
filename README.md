# dsh-desktop-lite

A thin Tauri 2 wrapper for the [DSH (DeepSeek Harness)](https://github.com/deepseek-ai/deepseek-harness) Web UI.

This app does **not** bundle Node.js or the DSH CLI. It assumes the host
already has them, and just provides a native window that hosts the DSH web
surface (instead of opening it in a system browser tab).

## Architecture

```
+-----------------------------------+
|  Tauri WebView (system native)    |   <- loads /dist/index.html
|  DSH Web UI after navigation      |   <- then navigates to 127.0.0.1:<port>
+-----------+-----------------------+
            | IPC
+-----------+-----------------------+
|  Rust (src-tauri/src)             |
|   deps.rs   - locate node & dsh   |
|   dsh.rs    - spawn + supervise   |
|   lib.rs    - Tauri commands      |
+-----------+-----------------------+
            | child process
+-----------+-----------------------+
|  dsh web --no-open --port 0       |   <- OS picks a free port
+-----------------------------------+
```

## Prerequisites

- **Node.js ≥ 18** (`node` on `PATH`)
- **DSH CLI**: `npm i -g @deepseek-ai/dsh`

## Develop

```bash
pnpm install
pnpm tauri dev
```

## Build a Windows installer

```bash
pnpm tauri build
```

Outputs land in `src-tauri/target/release/bundle/{msi,nsis}/`.

## Why "Lite"

This is the lightweight variant — it does not bundle Node.js or DSH. A
future "full" sibling project could sidecar the Node runtime and the DSH
`node_modules`, making the installer fully self-contained.
