use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

/// The manifest written by `scripts/build-sidecar.mjs` and baked in at compile
/// time. It pins the exact download URL + SHA-256 of the harness closure so the
/// shell can fetch it on first launch instead of shipping it inside the bundle.
const MANIFEST_JSON: &str = include_str!("../runtime-manifest.json");

#[derive(Debug, Deserialize)]
pub struct RuntimeManifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

/// Progress/status events emitted to the loading page as `runtime-progress`.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeProgress {
    phase: &'static str, // check | download | extract | ready | error
    message: String,
    percent: Option<u32>,
}

fn emit(handle: &AppHandle, phase: &'static str, message: String, percent: Option<u32>) {
    let _ = handle.emit("runtime-progress", RuntimeProgress { phase, message, percent });
}

/// Parse the compile-time manifest (written by `scripts/build-sidecar.mjs`).
pub fn baked_manifest() -> Result<RuntimeManifest, String> {
    serde_json::from_str(MANIFEST_JSON).map_err(|e| format!("bad runtime manifest: {e}"))
}

pub fn runtime_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("runtime")
}

fn node_app_entry(runtime: &Path) -> PathBuf {
    runtime.join("node-app").join("lib").join("bin.js")
}

fn installed_version(runtime: &Path) -> Option<String> {
    fs::read_to_string(runtime.join(".installed"))
        .ok()
        .map(|s| s.trim().to_string())
}

fn is_installed(runtime: &Path, manifest: &RuntimeManifest) -> bool {
    node_app_entry(runtime).is_file()
        && installed_version(runtime).as_deref() == Some(manifest.version.as_str())
}

/// Stream a URL to `dest`, reporting percent complete (0..=100) via `on_percent`.
fn download(url: &str, dest: &Path, on_percent: &mut dyn FnMut(u32)) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new().build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok());

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let mut file = File::create(dest).map_err(|e| format!("create file: {e}"))?;
    let mut reader = resp.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_percent: Option<u32> = None;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| format!("write: {e}"))?;
        downloaded += n as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            let percent = ((downloaded * 100) / total).min(100) as u32;
            if last_percent != Some(percent) {
                last_percent = Some(percent);
                on_percent(percent);
            }
        }
    }
    file.flush().map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn extract(tarball: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(tarball).map_err(|e| format!("open: {e}"))?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    archive.unpack(dest).map_err(|e| format!("extract: {e}"))?;
    Ok(())
}

/// Ensure the harness closure is present under `<app_data>/runtime/node-app`,
/// downloading + verifying + extracting it on first launch (or on version bump).
///
/// Blocking (network + file I/O); call from `spawn_blocking`.
pub fn ensure_runtime(
    app_data_dir: &Path,
    manifest: &RuntimeManifest,
    handle: &AppHandle,
) -> Result<PathBuf, String> {
    let runtime = runtime_dir(app_data_dir);

    if is_installed(&runtime, &manifest) {
        emit(handle, "ready", "运行时已就绪".to_string(), None);
        return Ok(runtime);
    }

    fs::create_dir_all(&runtime).map_err(|e| format!("mkdir runtime: {e}"))?;
    let staging = runtime.join(".install");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("mkdir staging: {e}"))?;
    let tarball = staging.join("node-app.tar.gz");

    emit(handle, "download", "正在下载运行时…".to_string(), Some(0));
    {
        let h = handle.clone();
        let mut on_percent = |percent: u32| {
            emit(&h, "download", format!("正在下载运行时… {percent}%"), Some(percent));
        };
        download(&manifest.url, &tarball, &mut on_percent)?;
    }

    let actual = sha256_file(&tarball)?;
    if !actual.eq_ignore_ascii_case(&manifest.sha256) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "运行时校验失败（sha256 不匹配）：期望 {} 实际 {}",
            manifest.sha256, actual
        ));
    }

    emit(handle, "extract", "正在解压运行时…".to_string(), None);
    extract(&tarball, &staging)?;
    let extracted = staging.join("node-app");
    if !extracted.join("lib").join("bin.js").is_file() {
        let _ = fs::remove_dir_all(&staging);
        return Err("运行时压缩包缺少 lib/bin.js".to_string());
    }

    // Swap the freshly extracted tree into place, then record the version.
    let node_app = runtime.join("node-app");
    if node_app.exists() {
        fs::remove_dir_all(&node_app).map_err(|e| format!("remove old runtime: {e}"))?;
    }
    fs::rename(&extracted, &node_app).map_err(|e| format!("install runtime: {e}"))?;
    fs::write(runtime.join(".installed"), manifest.version.as_bytes())
        .map_err(|e| format!("write marker: {e}"))?;
    let _ = fs::remove_dir_all(&staging);

    emit(handle, "ready", "运行时已就绪".to_string(), None);
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Build a tiny tarball with the same layout the real closure uses
    /// (`node-app/lib/bin.js`), so extract + entry checks are realistic.
    fn make_tarball(path: &Path) {
        let file = File::create(path).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let data = b"#!/usr/bin/env node\nconsole.log('hi');\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "node-app/lib/bin.js", &data[..])
            .unwrap();
        builder.finish().unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dsh-runtime-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_and_extract_roundtrip() {
        let dir = temp_dir("extract");
        let tarball = dir.join("node-app.tar.gz");
        make_tarball(&tarball);

        let digest = sha256_file(&tarball).unwrap();
        assert_eq!(digest.len(), 64);

        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();
        extract(&tarball, &out).unwrap();
        assert!(out.join("node-app").join("lib").join("bin.js").is_file());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_from_local_http_reports_progress() {
        let dir = temp_dir("download");
        let tarball = dir.join("node-app.tar.gz");
        make_tarball(&tarball);
        let bytes = fs::read(&tarball).unwrap();
        let server_bytes = bytes.clone();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0u8; 2048];
            let _ = stream.read(&mut req);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                server_bytes.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&server_bytes);
        });

        let url = format!("http://{addr}/node-app.tar.gz");
        let dest = dir.join("dl.tar.gz");
        let mut last_percent = None;
        download(&url, &dest, &mut |p| last_percent = Some(p)).unwrap();
        server.join().unwrap();

        assert_eq!(fs::read(&dest).unwrap(), bytes);
        assert_eq!(last_percent, Some(100));
        let _ = fs::remove_dir_all(&dir);
    }
}
