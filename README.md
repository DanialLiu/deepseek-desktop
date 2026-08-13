# deepseek-desktop

> 语言 / Language：[中文](#中文说明) · [English](#english)

## 中文说明

deepseek-desktop 是 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的跨平台桌面壳，基于 **Tauri v2** 构建。

Harness 是一个 Node.js 宿主程序，通过回环 HTTP 服务器（`dsh web`）对外提供浏览器界面。本项目把这个宿主程序包装进原生桌面窗口：Tauri（Rust）外壳以 **sidecar**（应用内置的官方 Node 二进制）方式启动 harness，等待它就绪后的 URL，然后把 webview 指向 `http://127.0.0.1:<port>`。

harness 的依赖闭包**不会**打包进应用内。它以独立的 `node-app-<triple>.tar.gz` 形式发布，并在首次启动时下载、校验、解压（见[运行时下载](#运行时下载)）。装好应用即装好一切——运行时无需系统级 Node、pnpm 或 Python。

### 架构

```
deepseek-desktop/
├── harness/                            # git 子模块 → deepseek-ai/deepseek-harness（固定版本）
├── scripts/
│   ├── build-sidecar.mjs               # 部署 @deepseek-ai/dsh 依赖闭包 + 拉取 Node
│   ├── make-dmg.sh                     # macOS：由已构建 .app 制作 .dmg
│   └── env.sh                          # source 后把工作区本地 Rust 加入 PATH
├── src-tauri/
│   ├── src/main.rs                     # 调用 deepseek_desktop_lib::run()
│   ├── src/lib.rs                      # 确保运行时、启动 sidecar、解析 URL 行
│   ├── src/runtime.rs                  # 首次启动时下载 / sha256 校验 / 解压闭包
│   ├── runtime-manifest.json           # {version, url, sha256}，编译期内置（构建产物）
│   ├── tauri.conf.json                 # externalBin = binaries/node
│   └── binaries/
│       ├── node-<triple>               # 内置 Node sidecar（构建产物）
│       └── node-app-<triple>.tar.gz    # 首次启动下载用的 harness 闭包（构建产物）
└── dist/index.html                     # 加载页；就绪后跳转到 sidecar URL
```

> 当前布局中 `src-tauri/resources/` 有意留空。早期版本曾把 harness 闭包打包在那里；现在改为以可下载 tarball 形式发布。

### 为什么用「node 载体」（而非单个 pkg --sea exe）

Harness 的 `web` profile 构建在一套 **profile 系统**之上：运行时通过 `healProfilesModuleFallback` 符号链接，从磁盘上的 `$DSH_HOME/profiles/` 目录解析裸插件标识符，并且像 `directory-picker-auto` 这样的插件会执行嵌套的 `ctx.loader.create({ name: … })`。这些解析都依赖真实文件系统路径。`@yao-pkg/pkg --sea` 的 VFS 无法满足（其符号链接会悬空，且解析器只处理来自 VFS 内部的导入）。SDK 的单文件 exe 之所以能避免这点，是因为它用 `bareModuleBaseUrl` 启动一个扁平配置，而非 profile 系统。

可靠的路线——也正是 `dsh web` 在生产环境的实际运行方式——是在磁盘上同时提供扁平的依赖闭包和官方 Node 二进制。`build-sidecar.mjs` 通过 Harness 自带的 `pnpm deploy --legacy` 路由（外加一个工作区闭包补齐步骤，补上 legacy deploy 会丢弃的传递 `workspace:` 包）生成该闭包。

### 运行时下载

harness 闭包（约 3 万个文件）太大、太脆弱，不适合打进 `.app`，所以外壳在首次启动时拉取它：

1. `build-sidecar.mjs` 把暂存的闭包打包为 `binaries/node-app-<triple>.tar.gz`（顶层目录 `node-app`），并写入 `src-tauri/runtime-manifest.json`：
   ```json
   {
     "version": "0.1.0",
     "url": "https://your-cdn.example.com/deepseek-desktop/node-app-aarch64-apple-darwin.tar.gz",
     "sha256": "…"
   }
   ```
2. `runtime.rs` 通过 `include_str!` 把这个 manifest 内置进二进制。
3. 首次启动时 `ensure_runtime()` 把 `url` 下载到 `<app-data>/runtime/`，校验 SHA-256，解压到 `<app-data>/runtime/node-app`，并写入 `.installed` 版本标记。版本升级会重新下载；版本不变则复用。

下载 URL 来自 sidecar 构建时的 `RUNTIME_BASE_URL`（见[构建](#构建)）。本地测试可以直接托管 `binaries/` 目录：

```bash
cd src-tauri/binaries && python3 -m http.server 8000
RUNTIME_BASE_URL=http://127.0.0.1:8000 pnpm run sidecar
```

### 版本同步

桌面应用版本**不是**硬编码的——它由 harness 派生而来，因此打包版本始终与其包装的 harness 一致。唯一事实来源是 `@deepseek-ai/dsh` 包的版本（`harness/apps/cli/package.json`）。

每次执行 `pnpm run sidecar`，`build-sidecar.mjs` 会：

1. 从暂存树读取已部署的 `@deepseek-ai/dsh` 版本；
2. 把它写入 `package.json`、`src-tauri/tauri.conf.json` 和 `src-tauri/Cargo.toml`；
3. 记录到 `runtime-manifest.json`（运行时的下载/版本标记，默认取 harness 版本；`RUNTIME_VERSION` 可覆盖以强制重新下载）。

随后 `tauri build` 从 `tauri.conf.json` 读取版本，因此产出的 `.app`/`.dmg` 会报告 harness 版本（例如 `CFBundleShortVersionString`），而 `make-dmg.sh` 会从已构建应用的 `CFBundleShortVersionString` 推导 `.dmg` 文件名。结果：只需升级 harness 子模块并重新构建，即可让所有版本字段保持同步。

### 环境要求

- Node.js ≥ 22，pnpm ≥ 10
- Rust（stable）。构建使用工作区本地工具链，无需系统安装：
  ```bash
  # 一次性：把工具链装进工作区（使用 rsproxy 镜像）
  RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
    CARGO_HOME="$PWD/.cargo" RUSTUP_HOME="$PWD/.rustup" \
    sh <(curl -fsSL https://sh.rustup.rs) -y --default-toolchain stable --profile minimal --no-modify-path
  ```
  然后在每个运行 cargo 的 shell 里 source：`. scripts/env.sh`
- [Tauri v2 系统依赖](https://v2.tauri.app/start/prerequisites/)
  - macOS：Xcode Command Line Tools
  - Linux：`webkit2gtk-4.1`、`libappindicator`、`librsvg` 等
  - Windows：WebView2 + MSVC 构建工具

### 构建

```bash
git submodule update --init --recursive   # 拉取固定的 harness 源码
pnpm install                               # 安装 @tauri-apps/cli
pnpm run icons                             # 由 logo.svg 生成 src-tauri/icons/*
. scripts/env.sh                           # 把工作区本地 Rust 工具链加入 PATH
pnpm run sidecar                           # 部署 harness 闭包 + 拉取 Node
pnpm run build                             # tauri build → .app / .exe / Linux bundle
pnpm run make-dmg                          # 仅 macOS：由 .app 制作 .dmg
```

`pnpm run sidecar`（即 `node scripts/build-sidecar.mjs`）支持覆盖参数：

```bash
node scripts/build-sidecar.mjs --targets=macos-arm64,linux-x64   # 默认宿主平台
HARNESS=/path/to/deepseek-harness node scripts/build-sidecar.mjs --skip-build

# 发布时把真实下载 URL 写入 runtime-manifest.json。
# （版本默认自动取 harness 版本；RUNTIME_VERSION 仅用于在应用版本之外强制运行时重新下载。）
RUNTIME_BASE_URL=https://your-cdn.example.com/deepseek-desktop \
  pnpm run sidecar
```

其他参数：`--no-strip`（不裁剪 node 二进制）、`--dry-run`（只打印命令不执行）、`--help`。

### 外壳工作流程

1. `lib.rs` 调用 `ensure_runtime()`——首次启动（或版本升级）时下载、校验、解压 harness 闭包，并通过 `runtime-progress` 事件向加载页上报进度。
2. 随后以该闭包启动内置 Node sidecar：
   `node <app-data>/runtime/node-app/lib/bin.js --profile web --host 127.0.0.1 --port 0`，
   并把 `DSH_HOME` 指向系统应用数据目录（`<app-data>/dsh`）。
3. harness 在 `127.0.0.1` 上绑定 OS 分配的端口，并打印 `dsh web: http://127.0.0.1:<port>`。
4. Rust 侧解析该行、保存 URL，并发出 `sidecar-ready`。
5. `dist/index.html` 跳转到该 URL（带 `get_sidecar_url` 轮询兜底）。
6. 退出时 Rust 侧杀掉 sidecar 子进程，避免残留孤儿服务器。

### 注意事项与限制

- **Windows 暂非目标平台**（Harness 文档把 Windows 列为 deploy/单文件 exe 路由的非目标）。支持 macOS 和 Linux（x64/arm64）。
- `tauri build` 产出 `.app`（macOS）、`.exe`/`.msi`（Windows）或 `.deb`/`.rpm`/`.AppImage`（Linux）。macOS 的 `.dmg` 由 `pnpm run make-dmg` 制作，因为 Tauri 自带的 `create-dmg` 助手在这个大型应用上会与 Spotlight 竞争、无法卸载其可写卷（"Resource busy"）。
- **macOS 26.5.x 坑**：当 `-o` 输出路径含空格时，`hdiutil convert` 会 PAC 崩溃，所以 `make-dmg.sh` 先转换到不含空格的临时名（`.final.dmg`）再重命名；并使用 `-format ULFO`（lzfse），比 `UDZO` 更快更小。
- DMG 使用标准的「拖拽安装」布局：`make-dmg.sh` 挂载可写暂存镜像，在应用旁添加一个 `Applications -> /Applications` 符号链接。自定义卷图标通过写入 `.VolumeIcon.icns` 并执行 `SetFile -a C` 内置；该文件随后被标记为隐藏（`chflags hidden`），以免弄乱窗口。
- macOS 上 `node-pty` 需要 `-spawn-helper` 同级文件（terminal/bash 工具用到）。harness 自带的安装会构建它；打包前请确保 `node_modules/node-pty` 已就绪（`pnpm deploy` 的 postinstall 会处理）。
- 首次运行状态（`$DSH_HOME`）默认在系统应用数据目录；harness 会在首次启动时创建其 `profiles/` 树。
- `runtime-manifest.json` 的 URL 必须公网可达才能用于分发构建。未设置 `RUNTIME_BASE_URL` 时会写入占位符——应用首次启动下载会失败，直到内置真实 URL。
- 发布分发需要代码签名 / 公证（macOS hardened runtime + notarytool、Windows 代码签名证书）——本地构建不在范围内。当同步到 `0.1.0-rc.5` 这类预发布 harness 版本时，macOS 的 `CFBundleVersion` 会原样继承（含 `-rc.5` 后缀）；App Store 提交要求纯数字构建号，该场景下请设置 `bundle.macOS.bundleVersion`。本地未公证构建不受影响。

---

## English

Cross-platform desktop shell for [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness),
built with **Tauri v2**.

The Harness is a Node.js host that serves its browser UI over a loopback HTTP
server (`dsh web`). This project wraps that host in a native desktop window: a
Tauri (Rust) shell spawns the harness as a **sidecar** (the official Node binary
bundled inside the app), waits for its readiness URL, then points the webview at
`http://127.0.0.1:<port>`.

The harness dependency closure is **not** bundled inside the app. It ships as a
separate `node-app-<triple>.tar.gz` and is downloaded, verified and extracted on
first launch (see [Runtime download](#runtime-download)). Installing the app
installs everything — no system Node, pnpm, or Python is required at runtime.

## Architecture

```
deepseek-desktop/
├── harness/                            # git submodule → deepseek-ai/deepseek-harness (pinned)
├── scripts/
│   ├── build-sidecar.mjs               # deploy the @deepseek-ai/dsh closure + fetch Node
│   ├── make-dmg.sh                     # macOS .dmg from the built .app
│   └── env.sh                          # source to put the workspace-local Rust on PATH
├── src-tauri/
│   ├── src/main.rs                     # calls deepseek_desktop_lib::run()
│   ├── src/lib.rs                      # ensures the runtime, spawns the sidecar, parses the URL line
│   ├── src/runtime.rs                  # download / sha256-verify / extract the closure on first launch
│   ├── runtime-manifest.json           # {version, url, sha256} baked in at compile time (build output)
│   ├── tauri.conf.json                 # externalBin = binaries/node
│   └── binaries/
│       ├── node-<triple>               # bundled Node sidecar (build output)
│       └── node-app-<triple>.tar.gz    # harness closure for first-launch download (build output)
└── dist/index.html                     # loading page; navigates to the sidecar URL once ready
```

> `src-tauri/resources/` is intentionally empty in the current layout. Earlier
> revisions bundled the harness closure there; it now ships as a downloadable
> tarball instead.

### Why a "node carrier" (not a single pkg --sea exe)

The Harness's `web` profile is built on a **profile system** that, at runtime,
resolves bare plugin specifiers from the on-disk `$DSH_HOME/profiles/` directory
via `healProfilesModuleFallback` symlinks, and plugins like
`directory-picker-auto` perform nested `ctx.loader.create({ name: … })`. Those
resolve against real filesystem paths. A `@yao-pkg/pkg --sea` VFS cannot satisfy
them (its symlinks dangle, and its resolver only handles imports from inside the
VFS). The SDK's single-file exe avoids this because it boots a flat config with
`bareModuleBaseUrl`, not the profile system.

The reliable route — and exactly how `dsh web` runs in production — is to ship
the flat dependency closure plus the official Node binary on disk. The
`build-sidecar.mjs` script produces that closure via the Harness's own
`pnpm deploy --legacy` route (plus a workspace-closure completion pass for
transitive `workspace:` packages that legacy deploy drops).

## Runtime download

The harness closure (30k files) is too large and fragile to bundle inside the
`.app`, so the shell fetches it on first launch:

1. `build-sidecar.mjs` packages the staged closure as
   `binaries/node-app-<triple>.tar.gz` (top-level dir `node-app`) and writes
   `src-tauri/runtime-manifest.json`:
   ```json
   {
     "version": "0.1.0",
     "url": "https://your-cdn.example.com/deepseek-desktop/node-app-aarch64-apple-darwin.tar.gz",
     "sha256": "…"
   }
   ```
2. `runtime.rs` bakes that manifest into the binary (`include_str!`).
3. On first launch `ensure_runtime()` downloads `url` into
   `<app-data>/runtime/`, verifies the SHA-256, extracts to
   `<app-data>/runtime/node-app`, and writes a `.installed` version marker.
   A version bump re-downloads; an unchanged version is reused.

The download URL comes from `RUNTIME_BASE_URL` at sidecar-build time (see
[Build](#build)). For local testing you can serve the `binaries/` directory:

```bash
cd src-tauri/binaries && python3 -m http.server 8000
RUNTIME_BASE_URL=http://127.0.0.1:8000 pnpm run sidecar
```

## Version sync

The desktop app version is **not** hardcoded — it is derived from the harness
so the packaged version always matches the harness it wraps. The single source
of truth is the `@deepseek-ai/dsh` package version (`harness/apps/cli/package.json`).

On every `pnpm run sidecar`, `build-sidecar.mjs`:

1. reads the deployed `@deepseek-ai/dsh` version from the staging tree;
2. writes it to `package.json`, `src-tauri/tauri.conf.json` and
   `src-tauri/Cargo.toml`;
3. records it in `runtime-manifest.json` (the runtime's download/version marker,
   defaulting to the harness version; `RUNTIME_VERSION` can override it to force
   a re-download).

`tauri build` then reads the version from `tauri.conf.json`, so the shipped
`.app`/`.dmg` reports the harness version (e.g. `CFBundleShortVersionString`),
and `make-dmg.sh` derives the `.dmg` filename from the built app's
`CFBundleShortVersionString`. Result: bumping the harness submodule and re-running
the build is enough to keep every version field in lockstep.

## Prerequisites

- Node.js ≥ 22 and pnpm ≥ 10
- Rust (stable). The build uses a workspace-local toolchain so you don't need a
  system install:
  ```bash
  # one-time: install the toolchain into the workspace (uses the rsproxy mirror)
  RUSTUP_DIST_SERVER=https://rsproxy.cn RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
    CARGO_HOME="$PWD/.cargo" RUSTUP_HOME="$PWD/.rustup" \
    sh <(curl -fsSL https://sh.rustup.rs) -y --default-toolchain stable --profile minimal --no-modify-path
  ```
  Then source `scripts/env.sh` in every shell that runs cargo:
  `. scripts/env.sh`
- The [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS
  - macOS: Xcode Command Line Tools
  - Linux: `webkit2gtk-4.1`, `libappindicator`, `librsvg`, etc.
  - Windows: WebView2 + MSVC build tools

## Build

```bash
git submodule update --init --recursive   # fetch the pinned harness source
pnpm install                               # installs @tauri-apps/cli
pnpm run icons                             # generate src-tauri/icons/* from logo.svg
. scripts/env.sh                           # workspace-local Rust toolchain on PATH
pnpm run sidecar                           # deploy the harness closure + fetch Node
pnpm run build                             # tauri build → .app / .exe / Linux bundle
pnpm run make-dmg                          # macOS only: .dmg from the .app
```

`pnpm run sidecar` (i.e. `node scripts/build-sidecar.mjs`) accepts overrides:

```bash
node scripts/build-sidecar.mjs --targets=macos-arm64,linux-x64   # host platform by default
HARNESS=/path/to/deepseek-harness node scripts/build-sidecar.mjs --skip-build

# Bake a real download URL into runtime-manifest.json at release time.
# (The version defaults to the harness version automatically; RUNTIME_VERSION
#  is only for forcing a runtime re-download independently of the app version.)
RUNTIME_BASE_URL=https://your-cdn.example.com/deepseek-desktop \
  pnpm run sidecar
```

Other flags: `--no-strip` (ship the node binary unstripped), `--dry-run`
(print commands without executing), `--help`.

## How the shell works

1. `lib.rs` calls `ensure_runtime()` — downloads, verifies and extracts the
   harness closure on first launch (or on a version bump), reporting progress to
   the loading page via `runtime-progress` events.
2. It then spawns the bundled Node sidecar against that closure:
   `node <app-data>/runtime/node-app/lib/bin.js --profile web --host 127.0.0.1 --port 0`,
   with `DSH_HOME` pointed at the OS app-data directory (`<app-data>/dsh`).
3. The harness binds `127.0.0.1` on an OS-assigned port and prints
   `dsh web: http://127.0.0.1:<port>`.
4. The Rust side parses that line, stores the URL, and emits `sidecar-ready`.
5. `dist/index.html` navigates to the URL (with a `get_sidecar_url` poll
   fallback).
6. On exit the Rust side kills the sidecar child so no orphan server remains.

## Notes & limitations

- **Windows is not yet a target** for the closure build (the Harness documents
  Windows as a non-goal for its deploy/single-exe route). macOS and Linux
  (x64/arm64) are supported.
- `tauri build` produces the `.app` (macOS), `.exe`/`.msi` (Windows), or
  `.deb`/`.rpm`/`.AppImage` (Linux). The macOS `.dmg` is produced by
  `pnpm run make-dmg` because Tauri's bundled `create-dmg` helper races
  Spotlight on this large app and fails to unmount its writable volume
  ("Resource busy").
- **macOS 26.5.x gotcha**: `hdiutil convert` PAC-crashes when the `-o` output
  path contains a space, so `make-dmg.sh` converts to a space-free temp name
  (`.final.dmg`) and renames. It also uses `-format ULFO` (lzfse), which is
  faster and smaller than `UDZO` here.
- The DMG uses the standard drag-to-install layout: `make-dmg.sh` mounts the
  writable staging image and adds an `Applications -> /Applications` symlink
  next to the app. The custom volume icon is baked in by writing
  `.VolumeIcon.icns` and running `SetFile -a C`; that file is then marked
  hidden (`chflags hidden`) so it doesn't clutter the window.
- `node-pty` on macOS needs the `-spawn-helper` sibling (used by terminal/bash
  tools). The harness's own install builds it; ensure `node_modules/node-pty`
  is populated before packaging (the `pnpm deploy` postinstall handles it).
- First-run state (`$DSH_HOME`) defaults to the OS app-data directory; the
  harness creates its `profiles/` tree there on first launch.
- The `runtime-manifest.json` URL must be publicly reachable for distributed
  builds. A placeholder is written when `RUNTIME_BASE_URL` is unset — the app
  will fail its first-launch download until a real URL is baked in.
- Release distribution needs code signing / notarization (macOS hardened
  runtime + notarytool, Windows code-signing cert) — out of scope for a local
  build. When a prerelease harness version like `0.1.0-rc.5` is synced, macOS
  `CFBundleVersion` inherits it (with the `-rc.5` suffix); App Store submission
  wants a numeric build number, so set `bundle.macOS.bundleVersion` for that
  path. Local, non-notarized builds are unaffected.
