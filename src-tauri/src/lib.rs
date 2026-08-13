mod runtime;

use std::sync::Mutex;

use tauri::{Emitter, Manager, RunEvent, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// The readiness line the harness prints once its web server is bound:
/// `dsh web: http://127.0.0.1:<port>` (optionally with a ` (LAN: …)` suffix).
const READY_PREFIX: &str = "dsh web: ";

/// Owns the sidecar child process (killed on exit) and the resolved UI URL.
struct SidecarState {
    child: Mutex<Option<CommandChild>>,
    url: Mutex<Option<String>>,
}

impl Default for SidecarState {
    fn default() -> Self {
        Self {
            child: Mutex::new(None),
            url: Mutex::new(None),
        }
    }
}

/// Extract the canonical loopback URL from a `dsh web: …` readiness line.
fn parse_ready_url(line: &str) -> Option<String> {
    let rest = line.strip_prefix(READY_PREFIX)?;
    let url = rest.split_whitespace().next()?.trim();
    if url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:") {
        Some(url.to_string())
    } else {
        None
    }
}

/// Polled by the loading page as a fallback in case the readiness event fired
/// before its listener registered.
#[tauri::command]
fn get_sidecar_url(state: State<'_, SidecarState>) -> Option<String> {
    state.url.lock().ok().and_then(|u| u.clone())
}

/// Emit a fatal bootstrap error to the loading page (phase `error`).
fn emit_error(handle: &tauri::AppHandle, message: String) {
    eprintln!("[runtime] {message}");
    let _ = handle.emit(
        "runtime-progress",
        serde_json::json!({ "phase": "error", "message": message }),
    );
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(SidecarState::default())
        .invoke_handler(tauri::generate_handler![get_sidecar_url])
        .setup(|app| {
            let handle = app.handle().clone();
            let app_data_dir = app.path().app_data_dir()?;

            tauri::async_runtime::spawn(async move {
                // 1. Ensure the harness closure is present in the data dir —
                //    downloads + verifies + extracts it on first launch.
                let manifest = match runtime::baked_manifest() {
                    Ok(manifest) => manifest,
                    Err(err) => {
                        emit_error(&handle, err);
                        return;
                    }
                };
                let data = app_data_dir.clone();
                let bootstrap_handle = handle.clone();
                let runtime_dir = match tauri::async_runtime::spawn_blocking(move || {
                    runtime::ensure_runtime(&data, &manifest, &bootstrap_handle)
                })
                .await
                {
                    Ok(Ok(dir)) => dir,
                    Ok(Err(err)) => {
                        emit_error(&handle, err);
                        return;
                    }
                    Err(err) => {
                        emit_error(&handle, format!("runtime bootstrap panicked: {err}"));
                        return;
                    }
                };

                // 2. Spawn the sidecar (bundled Node) against the runtime closure.
                let bin_path = runtime_dir.join("node-app").join("lib").join("bin.js");
                let args = vec![
                    bin_path.to_string_lossy().into_owned(),
                    "--profile".to_string(),
                    "web".to_string(),
                    "--host".to_string(),
                    "127.0.0.1".to_string(),
                    "--port".to_string(),
                    "0".to_string(),
                ];

                let command = match handle.shell().sidecar("node") {
                    Ok(command) => command,
                    Err(err) => {
                        emit_error(&handle, format!("failed to locate node sidecar: {err}"));
                        return;
                    }
                }
                .env("DSH_HOME", app_data_dir.join("dsh"))
                .args(args);

                let (mut rx, child) = match command.spawn() {
                    Ok(pair) => pair,
                    Err(err) => {
                        emit_error(&handle, format!("failed to spawn sidecar: {err}"));
                        return;
                    }
                };

                if let Some(state) = handle.try_state::<SidecarState>() {
                    if let Ok(mut guard) = state.child.lock() {
                        *guard = Some(child);
                    }
                }

                while let Some(event) = rx.recv().await {
                    match event {
                        CommandEvent::Stdout(line) => {
                            let text = String::from_utf8_lossy(&line);
                            if let Some(url) = parse_ready_url(&text) {
                                if let Some(state) = handle.try_state::<SidecarState>() {
                                    if let Ok(mut guard) = state.url.lock() {
                                        *guard = Some(url.clone());
                                    }
                                }
                                let _ = handle.emit(
                                    "sidecar-ready",
                                    serde_json::json!({ "url": url }),
                                );
                            }
                        }
                        CommandEvent::Stderr(line) => {
                            eprint!("{}", String::from_utf8_lossy(&line));
                        }
                        CommandEvent::Error(err) => {
                            eprintln!("[sidecar] error: {err}");
                        }
                        CommandEvent::Terminated(payload) => {
                            eprintln!("[sidecar] terminated: code={:?}", payload.code);
                            break;
                        }
                        _ => {}
                    }
                }
            });

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the DeepSeek Harness desktop app")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<SidecarState>() {
                    if let Ok(mut guard) = state.child.lock() {
                        if let Some(child) = guard.take() {
                            let _ = child.kill();
                        }
                    }
                }
            }
        });
}
