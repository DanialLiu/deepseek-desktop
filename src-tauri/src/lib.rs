mod runtime;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{Emitter, Manager, RunEvent, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// The readiness line the harness prints once its web server is bound:
/// `dsh web: http://127.0.0.1:<port>` (optionally with a ` (LAN: …)` suffix).
/// Kept as a fast-path signal only — see `probe_http_ready` for why readiness
/// can't depend on this line alone.
const READY_PREFIX: &str = "dsh web: ";

/// Ceiling on how long the direct HTTP readiness probe (see `probe_http_ready`)
/// keeps retrying before giving up. Generous because first launch may still be
/// warming up profile plugins.
const PROBE_TIMEOUT: Duration = Duration::from_secs(600);

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

/// Reserve a free loopback port by binding then immediately releasing it, so
/// the shell knows the port up front instead of parsing it out of the
/// sidecar's stdout. Small TOCTOU race (something else could grab the port
/// before the sidecar binds it), acceptable for a local single-user app.
fn reserve_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.local_addr().map(|addr| addr.port())
}

/// Parse the HTTP status code out of a response's first line (`HTTP/1.1 200
/// OK\r\n...`). Deliberately strict — the harness's own server accepts
/// connections and answers with a 4xx for a stretch before its routes are
/// actually mounted (observed: ~30s of `400` immediately after the port
/// binds, then `200` once ready), so "got *an* HTTP response" is not a valid
/// readiness signal on its own.
fn response_status_code(bytes: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(bytes).ok()?;
    let line = text.lines().next()?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Poll the harness's own HTTP server directly instead of relying on it
/// printing a readiness banner to stdout. The banner is only printed after
/// the harness's *entire* plugin-loader tree settles (see
/// `@deepseek-ai/dsh-web-app`'s `printUrl`), and that settle can stall on a
/// plugin unrelated to serving HTTP (e.g. the client-HMR reload chain) even
/// though the web server itself is already bound and serving — observed in
/// practice: the port accepts real requests while the banner never prints.
/// A direct probe of the port we ourselves reserved is a strictly more
/// reliable readiness signal.
fn probe_http_ready(addr: SocketAddr, deadline: Instant, give_up: &AtomicBool) -> bool {
    while Instant::now() < deadline {
        if give_up.load(Ordering::SeqCst) {
            return false;
        }
        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let request = format!("GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", addr);
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf) {
                    if let Some(code) = response_status_code(&buf[..n]) {
                        if (200..300).contains(&code) {
                            return true;
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Report the sidecar as ready exactly once, however readiness was detected
/// (stdout banner or the direct HTTP probe racing it).
fn announce_ready(handle: &tauri::AppHandle, announced: &AtomicBool, url: String) {
    if announced.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(state) = handle.try_state::<SidecarState>() {
        if let Ok(mut guard) = state.url.lock() {
            *guard = Some(url.clone());
        }
    }
    let _ = handle.emit("sidecar-ready", serde_json::json!({ "url": url }));
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

                // 2. Reserve the loopback port ourselves (rather than letting
                //    the harness pick with --port 0) so readiness can be
                //    probed directly instead of depending solely on the
                //    harness printing it back to stdout.
                let port = match reserve_port() {
                    Ok(port) => port,
                    Err(err) => {
                        emit_error(&handle, format!("failed to reserve a loopback port: {err}"));
                        return;
                    }
                };

                // 3. Spawn the sidecar (bundled Node) against the runtime closure.
                let bin_path = runtime_dir.join("node-app").join("lib").join("bin.js");
                let args = vec![
                    bin_path.to_string_lossy().into_owned(),
                    "--profile".to_string(),
                    "web".to_string(),
                    "--host".to_string(),
                    "127.0.0.1".to_string(),
                    "--port".to_string(),
                    port.to_string(),
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

                // Readiness is reported by whichever fires first: the stdout
                // banner (fast path) or the direct HTTP probe (reliable path,
                // see `probe_http_ready`). `announced` guards against both
                // firing.
                let announced = Arc::new(AtomicBool::new(false));
                let probe_handle = handle.clone();
                let probe_announced = announced.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
                    let deadline = Instant::now() + PROBE_TIMEOUT;
                    if probe_http_ready(addr, deadline, &probe_announced) {
                        announce_ready(&probe_handle, &probe_announced, format!("http://127.0.0.1:{port}"));
                    }
                });

                // Tail of stderr, kept only to enrich the error message if the
                // sidecar dies before ever becoming ready (nothing else surfaces
                // it, since the packaged GUI app has no visible console).
                let mut stderr_tail: Vec<String> = Vec::new();

                while let Some(event) = rx.recv().await {
                    match event {
                        CommandEvent::Stdout(line) => {
                            let text = String::from_utf8_lossy(&line);
                            if let Some(url) = parse_ready_url(&text) {
                                announce_ready(&handle, &announced, url);
                            }
                        }
                        CommandEvent::Stderr(line) => {
                            let text = String::from_utf8_lossy(&line).into_owned();
                            eprint!("{text}");
                            stderr_tail.push(text);
                            if stderr_tail.len() > 20 {
                                stderr_tail.remove(0);
                            }
                        }
                        CommandEvent::Error(err) => {
                            eprintln!("[sidecar] error: {err}");
                            // Also doubles as the probe thread's give-up signal.
                            if !announced.swap(true, Ordering::SeqCst) {
                                emit_error(&handle, format!("harness sidecar error: {err}"));
                            }
                        }
                        CommandEvent::Terminated(payload) => {
                            eprintln!("[sidecar] terminated: code={:?}", payload.code);
                            // Also doubles as the probe thread's give-up signal.
                            if !announced.swap(true, Ordering::SeqCst) {
                                let tail = stderr_tail.join("").trim().to_string();
                                let detail = if tail.is_empty() {
                                    format!("harness sidecar exited before starting (code={:?})", payload.code)
                                } else {
                                    format!(
                                        "harness sidecar exited before starting (code={:?}): {tail}",
                                        payload.code
                                    )
                                };
                                emit_error(&handle, detail);
                            }
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
