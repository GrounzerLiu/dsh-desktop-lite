# dsh-desktop-lite — 接手文档

> 用途：当你需要在另一个 DSH agent（或新的会话）里继续推进本项目时，直接把这篇文档喂给那个 agent 即可。所有关键决策、文件位置、当前状态、踩过的坑都在这里。

---

## 1. 项目是什么

**`dsh-desktop-lite`** 是一个 **Tauri 2** 桌面应用，**窗口壳** 用来承载 [DSH (DeepSeek Harness)](https://github.com/deepseek-ai/deepseek-harness) 的 Web UI。

它**不**打包 Node.js，也不打包 DSH CLI。**要求宿主机已安装**：
- Node.js ≥ 18
- DSH CLI（`npm i -g @deepseek-ai/dsh`）

本应用启动时：
1. 检测 node + dsh 是否存在
2. 启动 `dsh web --no-open --port 0` 子进程（OS 分配空闲端口）
3. 解析 stdout 中的 `dsh web: http://127.0.0.1:PORT` 拿到 URL
4. Tauri WebView 跳转到该 URL，把控制权交给 DSH Web

**"Lite"** 一词就是用来强调：仅做窗口壳，不打包依赖。

---

## 2. 目录与关键文件

```
D:\any\dsh-desktop-lite\
├── README.md                       # 简版使用说明
├── HANDOVER.md                     # ← 你正在看的
├── package.json                    # pnpm 项目元数据
├── pnpm-workspace.yaml             # pnpm 11 必需：allowBuilds: esbuild
├── pnpm-lock.yaml
├── .npmrc                          # pnpm 配置
├── .gitignore                      # 已加 Rust target/gen 排除
├── index.html                      # 启动加载页 + 错误页
├── tsconfig.json
├── vite.config.ts
├── src/
│   ├── main.ts                     # 前端入口：IPC + 事件 + 导航
│   ├── styles.css                  # 深色加载页样式
│   └── assets/                     # (未改，模板默认)
└── src-tauri/
    ├── Cargo.toml                  # Rust 依赖（已对齐 crates.io 最新）
    ├── build.rs
    ├── tauri.conf.json             # 窗口/CSP/打包配置
    ├── capabilities/
    │   └── default.json            # 权限白名单（最小集）
    ├── icons/                      # 模板默认图标
    └── src/
        ├── main.rs                 # (未改)
        ├── lib.rs                  # Tauri 命令 + RunEvent
        ├── deps.rs                 # node/dsh 依赖检测
        └── dsh.rs                  # dsh 子进程 + 端口解析
```

---

## 3. 当前状态

### ✅ 已完成
1. **DSH 启动机制研究**（已完成，详见 `D:\any\dsh-desktop-lite\HANDOVER.md` 第 6 节）
2. **项目骨架**：`npx create-tauri-app@latest` 生成 vanilla-ts + pnpm
3. **依赖安装**：`pnpm install` 完成，esbuild 原生二进制已就位
4. **Rust 后端**：`cargo check` 通过（0 warning），代码已写完
5. **前端**：TypeScript 编译干净，已用 IPC 替代不存在的 JS navigate API
6. **配置**：窗口、CSP、capabilities、README、.gitignore 全部就位
7. **`pnpm tauri dev` 实测编译成功**（343/361 编译单元），首次 build 进度顺利

### ⚠️ 当前未完成 / 留给接手者
1. **首次 `tauri dev` 没跑到窗口出现** —— 我中途主动 kill 了，因为同时跑两个 dsh 实例会端口/进程冲突。接手者重跑即可。
   - ✅ **2026-08-27 接手者已重跑并修了一个 regex bug**（见 §5.3 第 5 条），Tauri 窗口内 DSH Web 正常显示
2. **没有 release 打包** —— `pnpm tauri build` 还没跑过。
3. **没有图标定制** —— 用的是模板默认图标。
4. ~~**启动失败时只显示泛化 "DSH 启动失败"**~~ —— ✅ 2026-08-27 已改为结构化 `DshError { message, last_stderr }`，前端在错误页底部展示 dsh 子进程最近 30 行 stderr；wait 线程还会检测 dsh 提前退出并 emit 错误（见 §5.1 数据流注释）

---

## 4. 接手者第一步操作

```bash
# 1. 切到项目目录
cd D:\any\dsh-desktop-lite

# 2. 确认环境（应该都已经装好）
node --version          # 期望 v18+，实测 v26.7.0
cargo --version         # 期望 1.7+
pnpm --version          # 期望 10+
dsh --version           # 期望 0.1.1-rc.x
where dsh               # 期望 C:\Users\grounzer\AppData\Roaming\npm\dsh.cmd

# 3. 先用最便宜的命令验证 Rust 端
cd src-tauri
cargo check             # 期望：Finished `dev` profile in ~15s, 0 error 0 warning
cd ..

# 4. 验证前端 TS
pnpm exec tsc --noEmit  # 期望：无输出

# 5. 启动 Tauri 开发模式
pnpm tauri dev
# 期望：Vite 启动在 :1420，Rust 编译出 Tauri 窗口
#       窗口内：DSH 启动页 → 几秒后 → DSH Web UI
#       关闭窗口后 dsh 子进程应被自动 kill
```

如果 `tauri dev` 出错，**优先检查**：
- 端口 1420（Vite）是否被占用
- 是否有遗留的 `dsh-desktop-lite.exe` / `dsh web` 进程没清掉
  ```powershell
  Get-Process | Where-Object { $_.Name -match 'tauri|dsh' }
  ```

---

## 5. 核心架构与代码思路

### 5.1 数据流

```
[Frontend index.html]
    │
    │ invoke('check_deps') → 返回 { deps_ok, node, dsh, message }
    │
    │ 如果失败 → 显示错误页（带 Node.js / dsh 安装指引）
    │ 如果成功 ↓
    │
    │ listen('dsh-ready', url => invoke('navigate_to', { url }))
    │ listen('dsh-log',   line => 追加到 <pre id="log">)
    │ listen('dsh-error', msg =>  显示错误页)
    │
    │ invoke('start_dsh') → 触发 Rust 端 spawn
    │
    ↓
[Rust lib.rs]
    ├── check_deps()       → 调用 deps::check_all()
    ├── start_dsh()        → 调用 dsh::spawn_and_wait_for_url(app, dsh_path)
    ├── dsh_status()       → 返回运行状态
    └── navigate_to(url)   → app.get_webview_window("main").navigate(url)
                ↓
            [Rust dsh.rs]
                ├── Command::new(dsh_path).args(["web","--no-open","--port","0"])
                ├── child.stdout → 线程 → emit("dsh-log", {stream,line})
                │                      └─ 正则匹配 `dsh web: http://...` → 存到 url_arc
                ├── child.stderr → 线程 → emit("dsh-log", {stream:"stderr",line})
                ├── 后台线程 poll url_arc → emit("dsh-ready", {url})
                └── 进程句柄存到 OnceCell<Mutex<Option<Child>>>

[RunEvent::WindowEvent::CloseRequested / RunEvent::ExitRequested]
    └── dsh::shutdown() → child.kill() + wait()
```

### 5.2 关键技术决策

| 决策 | 为什么 |
|------|--------|
| **`--port 0` 让 OS 选端口** | DSH 默认 3080 容易被占用；OS 分配 100% 不会冲突 |
| **`--no-open`** | DSH 默认会 `open()` 拉起系统浏览器，Tauri 窗口本身就是唯一窗口 |
| **stdout 解析 URL 而非 TCP 探活** | TCP 探活要在前端轮询；DSH 自己会打印 URL，直接抓更优雅 |
| **OnceCell + Mutex 存子进程** | 全局只一个 dsh 进程；shutdown 时能 kill |
| **Rust 端 navigate 而非 JS** | Tauri 2 不允许 JS 直接 navigate webview 到任意 URL（安全），必须 Rust 调 |
| **Rust 端 emit 事件，前端 listen** | Tauri 2 标准模式；事件比 polling 干净 |
| **capabilities 最小化（只 core:default + opener:default）** | 见 5.3 踩坑 |

### 5.3 踩过的坑（接手者绕开）

1. **pnpm 11 的 `onlyBuiltDependencies` 配置位置变了**
   - ❌ 写在 `package.json` 的 `pnpm.onlyBuiltDependencies` → **被忽略**
   - ✅ 写在 `pnpm-workspace.yaml` 的 `allowBuilds: [esbuild: true]`
   - 不配的话 esbuild postinstall 不会跑，原生二进制缺失，Vite 启动失败

2. **Tauri 2 的 capability 没有 `core:window:allow-navigate`**
   - ❌ 写入 capabilities → build 失败 "Permission not found"
   - 原因：navigate 由 Rust 端执行，不在前端 capability 范畴
   - 当前 capabilities 是最小集：`core:default` + `opener:default`

3. **Tauri 2 的 `Webview` JS 类没有 `navigate()` 方法**
   - ❌ `import { getCurrentWebview } from "@tauri-apps/api/webview"; getCurrentWebview().navigate(url)` → TS 2339
   - ✅ 改成 `invoke("navigate_to", { url })`，由 Rust 调 `WebviewWindow::navigate()`

4. **`which` crate 7.x → 8.x API 兼容**
   - 两者都是 `which("name") -> Result<PathBuf>`，代码不用改

5. **首次真机窗口验证发现的 regex bug（2026-08-27 接手者修复）**
   - ❌ `dsh.rs` 的 url regex `dsh web:\s+https?://[^\s]+` 用 `find()` 取整段匹配 → 把 `"dsh web: http://127.0.0.1:63399"` 整串发给 `WebviewWindow::navigate`
   - 前端 `navigateTo` 后报 **`refusing non-http url: dsh web: http://127.0.0.1:63399`**，窗口停在错误页
   - ✅ 改成 capture group `r"dsh web:\s+(https?://\S+)"` + `m.get(1)`，只取纯 URL 段
   - 验证：PowerShell `[regex]::Match` 跑 4 种格式（含 `[dsh]` 前缀、带 query string、带 https）全部正确提取；非 dsh 启动行的 URL 不会误抓

6. **同时跑两个 dsh 会冲突**
   - 我就是因为这个主动 kill 了 `tauri dev`
   - 接手者若要在 DSH Web GUI 里继续开发，先关掉所有 `dsh web` 进程

---

## 6. DSH 启动机制（已研究，写在这里给接手者参考）

> 来源：阅读 `C:\Users\grounzer\AppData\Roaming\npm\node_modules\@deepseek-ai\dsh` 源码 + 实测

### 入口
- `dsh` CLI = `C:\Users\grounzer\AppData\Roaming\npm\dsh.cmd`（npm 包装脚本）
- 它调用 `node %dp0%\node_modules\@deepseek-ai\dsh\lib\bin.js`

### Web 启动参数（`dsh --profile web`）
```
--host <host>                 绑定 host（明确禁止 0.0.0.0）
--port <port>                 监听端口（0 = OS 分配）
--no-open                     不自动开浏览器
--trusted-host <authority>    可重复，接受额外 /api 信任来源
```

### 默认端口
- Web profile 默认 3080（`webStartup.port ?? 3080`）
- 但我们传 `--port 0`，所以**永远是 OS 选**（实测 54501、3099 等）
- 客户端 Vite 开发端口 1180 跟服务端 3080 是两回事

### 启动流程
1. 加载 cordis 插件树
2. 启动 web server（`@deepseek-ai/dsh-host-webserver`）
3. 启动前端静态资源服务（`@deepseek-ai/dsh-host-frontend-static`）
4. 启动 API proxy（`@deepseek-ai/dsh-host-apiproxy`）
5. **stdout 打印** `dsh web: http://127.0.0.1:<port>` ← **我们抓这行**
6. 如果 `--no-open` 没传，调 `open()` 拉系统浏览器

### 重要事实
- DSH 100% Node.js，**没有原生二进制** → Lite 方案不需要打包 node
- 启动时间：~5-6 秒（首次加载）
- HTTP 验证：`curl http://127.0.0.1:<port>/` → HTTP 200 in ~5ms

---

## 7. 进一步可做（接手者参考）

| 优先级 | 任务 | 说明 |
|--------|------|------|
| ~~🟢 高~~ | ~~首次 `pnpm tauri dev` 跑通~~ | ✅ 2026-08-27 已完成，修了 url regex bug |
| 🟢 高 | `pnpm tauri build` 产出 msi/nsis | 验证打包链 |
| 🟡 中 | 窗口状态记忆（`tauri-plugin-window-state`） | 关闭后下次记住大小位置 |
| 🟡 中 | 系统托盘 | 关闭窗口 ≠ 退出进程，可从托盘恢复 |
| 🟡 中 | 单实例锁（`tauri-plugin-single-instance`） | 避免重复启动 dsh |
| 🟡 中 | 自定义图标 | 替换 `src-tauri/icons/`，用 Tauri 推荐的 icns/ico |
| 🟡 中 | 日志持久化 | dsh 子进程日志写到 `%APPDATA%\dsh-desktop-lite\logs\` |
| ~~🟢 中~~ | ~~启动失败时显示 dsh 真实错误~~ | ✅ 2026-08-27 已实现：结构化 `DshError { message, last_stderr }`，错误页底部贴最近 30 行 stderr；wait 线程同时检测 dsh 提前退出 |
| ~~🟡 中~~ | ~~验证关闭 Tauri 窗口时 dsh 子进程被 `RunEvent::WindowEvent::CloseRequested` 钩子 kill~~ | ✅ 2026-08-27 已用 Win32 PostMessage WM_CLOSE 真机验证：dsh 子进程（监听 57376 的 node + dsh.cmd shim）被 kill，端口不再监听 |
| 🟡 中 | macOS / Linux 兼容性验证 | 项目是为 Windows 设计但 Tauri 跨平台，测试一下 |
| 🟢 低 | Release 自动更新（`tauri-plugin-updater`） | 接 GitHub Releases |
| 🟢 低 | 「Lite vs Full」方案二：sidecar 打包 Node.js + dsh | 真的零依赖安装，但包大 |

---

## 8. 依赖版本快照（用于复现）

### Rust（`src-tauri/Cargo.toml`）
```toml
[build-dependencies]
tauri-build = "2.6"

[dependencies]
tauri = { version = "2.11", features = [] }
tauri-plugin-opener = "2.5"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
once_cell = "1.21"
which = "8.0"
regex = "1.13"
```
（编译时实测拉到的版本：tauri 2.11.5, tauri-build 2.6.3, tauri-plugin-opener 2.5.4）

### Node（`package.json`）
```json
"dependencies": {
  "@tauri-apps/api": "^2",
  "@tauri-apps/plugin-opener": "^2"
},
"devDependencies": {
  "@tauri-apps/cli": "^2",
  "vite": "^6.0.3",
  "typescript": "~5.6.2"
}
```
（实测：@tauri-apps/api 2.11.1, @tauri-apps/cli 2.11.4, vite 6.4.3, typescript 5.6.3）

### pnpm
- pnpm 11.24.0
- **必装配置** `pnpm-workspace.yaml`：
  ```yaml
  allowBuilds:
    esbuild: true
  ```

### 宿主环境（接手者复用）
- Rust 1.97.1, Node v26.7.0, pnpm 11.24.0, npm 11.19.0
- DSH 0.1.1-rc.2 (安装路径 `C:\Users\grounzer\AppData\Roaming\npm\dsh.cmd`)
- 已实测：`dsh web --no-open --port 3099` → HTTP 200 in 5ms

---

## 9. 关键命令速查

```bash
# 验证 Rust 端
cd src-tauri && cargo check && cd ..

# 验证前端
pnpm exec tsc --noEmit

# 启动开发
pnpm tauri dev

# 打包（产出 msi + nsis）
pnpm tauri build

# 单独跑前端（不开 Tauri 窗口）
pnpm dev

# 看 DSH 单独能否起来（不经过 Tauri）
dsh web --no-open --port 3099
# 应该几秒后看到 "dsh web: http://127.0.0.1:3099"
# 浏览器打开它能看见 DSH Web

# 清残留进程
powershell -NoProfile -Command "Get-Process | Where-Object { $_.Name -match 'tauri|dsh' } | Stop-Process -Force"
```

---

## 10. 给接手 agent 的一句话总结

> 本项目 `D:\any\dsh-desktop-lite` 是一个 Tauri 2 桌面壳，依赖宿主机已装的 Node + dsh CLI，启动时 spawn `dsh web --no-open --port 0`，从 stdout 抓 `dsh web: http://127.0.0.1:PORT` 然后 navigate WebView。**当前编译通过但没看到窗口效果**（因为冲突我主动 kill 了），接手后直接 `pnpm tauri dev` 验证即可。所有架构决策、踩坑记录、依赖版本都在本文件里。
