# dsh-desktop-lite

一个轻量的 **Tauri 2 桌面壳**，用来承载 [DSH (DeepSeek Harness)](https://github.com/deepseek-ai/deepseek-harness) 的 Web UI —— 不用开浏览器，直接在一个原生窗口里使用 DSH。

本项目**不打包** Node.js 和 DSH CLI，而是使用宿主机已安装的 `node` + `dsh`。这也是 "**Lite**" 的含义：只做窗口壳，不打包依赖。

## ✨ 特性

- **原生窗口承载 DSH Web UI**：启动时自动运行 `dsh web --no-open --port 0`，从 stdout 解析实际端口后导航到 DSH Web
- **系统托盘**：
  - 关闭窗口时可选择"最小化到托盘"（勾选后关窗不退出，进程留在托盘）
  - 托盘菜单：显示窗口 / 隐藏窗口 / 重启 DSH / 关闭时最小化到托盘 / 打开日志目录 / 退出
  - 左键单击托盘图标 = 显示/隐藏窗口切换
- **重启 DSH**：一键 kill 旧 dsh 进程并重新拉起，重启期间显示加载画面
- **窗口状态记忆**：记住上次的窗口大小/位置/最大化状态
- **单实例锁**：重复启动时自动聚焦已有窗口，不会起多个 dsh
- **日志持久化**：
  - `app-YYYY-MM-DD.log` —— 应用自身诊断（启动/托盘/重启/导航等，缓冲写入 + 2s flush + 5MiB 滚动）
  - `dsh-YYYY-MM-DD.log` —— dsh 子进程 stdout/stderr（实时写入）
  - 日志保留 7 天，自动清理
- **深色/浅色主题跟随系统**：启动页无白屏闪烁
- **DSH 鲸鱼图标**：全套 Windows/macOS/Linux 图标

## 🖥 运行环境

| 依赖 | 版本要求 | 安装方式 |
|------|---------|---------|
| Windows 10/11 | — | — |
| [Node.js](https://nodejs.org) | ≥ 18 | 官方安装包 |
| [DSH CLI](https://github.com/deepseek-ai/deepseek-harness) | 最新 | `npm i -g @deepseek-ai/dsh` |

> 也可以运行在 macOS / Linux（代码跨平台），但只验证过 Windows。

## 🚀 安装

### 方式一：安装包（推荐）

从 [GitHub Releases](https://github.com/GrounzerLiu/dsh-desktop-lite/releases) 下载 `DSH Desktop Lite_<version>_x64-setup.exe`，双击安装即可。

> 如果 Releases 页面还没有安装包，请先按"方式二"自行构建，或用 `pnpm tauri build` 产出的安装包。

### 方式二：源码构建

```bash
# 1. 克隆
git clone https://github.com/GrounzerLiu/dsh-desktop-lite.git
cd dsh-desktop-lite

# 2. 安装依赖
pnpm install

# 3. 启动开发模式（Vite + Tauri 热更新）
pnpm tauri dev

# 4. 打包 Windows 安装包（产出 msi + nsis）
pnpm tauri build
```

构建产物在 `src-tauri/target/release/bundle/{msi,nsis}/`。

## 🧭 使用

首次启动会自动完成：

1. 检测 `node` 和 `dsh` 是否存在（缺失时显示安装指引）
2. 运行 `dsh web --no-open --port 0`（OS 自动分配空闲端口）
3. 加载画面显示启动日志
4. dsh 就绪后自动导航到 DSH Web UI

### 托盘操作

| 操作 | 效果 |
|------|------|
| 右键托盘图标 | 打开菜单（显示/隐藏/重启/退出/日志目录） |
| 左键托盘图标 | 显示或隐藏主窗口 |
| 托盘 → 重启 DSH | 重启 dsh 子进程（显示加载画面） |
| 托盘 → 打开日志目录 | 用资源管理器打开日志文件夹 |
| 托盘 → 退出 | 完全退出（同时 kill dsh 子进程） |

### 日志位置

```
%APPDATA%\com.deepseek.dsh-desktop-lite\logs\
├── app-2026-08-28.log     # 应用诊断日志
└── dsh-2026-08-28.log     # dsh 子进程输出
```

### 配置文件

```
%APPDATA%\com.deepseek.dsh-desktop-lite\
├── .window-state.json     # 窗口大小/位置记忆
└── settings.json          # 用户设置（如"关闭时最小化到托盘"）
```

## 🏗 架构

```
+-----------------------------------+
|  Tauri WebView (系统原生)          |
|  启动页 index.html → DSH Web UI   |
+---------------+-------------------+
                | IPC (invoke / event)
+---------------+-------------------+
|  Rust (src-tauri/src)             |
|   deps.rs   — 定位 node & dsh     |
|   dsh.rs    — spawn + 监控子进程  |
|   lib.rs    — Tauri 命令/托盘/事件 |
|   logs.rs   — 日志持久化/轮转      |
|   settings.rs — 用户设置持久化     |
+---------------+-------------------+
                | 子进程
+---------------+-------------------+
|  node dsh web --no-open --port 0  |   ← OS 分配空闲端口
+-----------------------------------+
```

### 关键设计

- **`--port 0`**：让 OS 分配空闲端口，避免默认 3080 被占用
- **`--no-open`**：不自动打开系统浏览器（窗口本身就是唯一入口）
- **stdout 解析 URL**：从 `dsh web: http://127.0.0.1:<port>` 捕获真实地址，而非 TCP 探测
- **Rust 侧导航**：Tauri 2 不允许 JS 直接 navigate WebView 到任意 URL，由 Rust 端 `navigate_to` 命令完成
- **绕过 dsh.cmd shim**：直接 `node bin.js` 启动，避免 Windows 下 cmd.exe 包装层导致的空白终端窗口

## 🧩 技术栈

- **前端**：TypeScript + Vite（启动页）
- **后端**：Rust + Tauri 2.11
- **插件**：`tauri-plugin-window-state`（窗口状态）、`tauri-plugin-single-instance`（单实例）、`tauri-plugin-opener`
- **托盘**：Tauri 2 内置 `tray-icon` feature

## ⚠️ 已知限制

- dsh 内部插件（tavily-mcp、chrome-devtools-mcp 等）由 dsh 自身 spawn，可能产生独立的终端窗口 —— 这是 dsh 的行为，本项目无法控制
- 启动页背景色为固定深色（`backgroundColor`），浅色系统用户启动瞬间会有 0.5s 的深→浅过渡

## 📦 发布流程

```bash
# 1. 改版本号（package.json 和 src-tauri/tauri.conf.json 两处）
#    或者用 npm version patch（会同步两处）

# 2. 打包
pnpm tauri build

# 3. 发布到 GitHub Releases（手动）
#    用 gh CLI：
#    gh release create v0.1.0 \
#      "src-tauri/target/release/bundle/nsis/DSH Desktop Lite_0.1.0_x64-setup.exe" \
#      "src-tauri/target/release/bundle/msi/DSH Desktop Lite_0.1.0_x64_en-US.msi"
```

## 📄 License

MIT
