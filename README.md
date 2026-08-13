# deepseek-desktop

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

# Bake a real download URL + version into runtime-manifest.json at release time:
RUNTIME_BASE_URL=https://your-cdn.example.com/deepseek-desktop \
RUNTIME_VERSION=0.1.0 \
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
  faster and smaller than `UDZO` here. The custom volume icon is baked in by
  mounting the writable staging image, writing `.VolumeIcon.icns`, and running
  `SetFile -a C` before converting.
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
  build.
