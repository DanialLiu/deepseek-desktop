use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};

/// The manifest written by `scripts/build-sidecar.mjs` and baked in at compile
/// time. It pins the harness runtime version + SHA-256 of the shipped closure
/// so the shell can verify the bundled tarball before extracting it.
const MANIFEST_JSON: &str = include_str!("../runtime-manifest.json");

#[derive(Debug, Deserialize)]
pub struct RuntimeManifest {
    pub version: String,
    /// Optional download URL. Kept only for a future hybrid/update path; the
    /// shipped app extracts the bundled resource instead of downloading, so a
    /// missing `url` is fine.
    #[serde(default)]
    #[allow(dead_code)]
    pub url: Option<String>,
    pub sha256: String,
}

/// Progress/status events emitted to the loading page as `runtime-progress`.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeProgress {
    phase: &'static str, // check | extract | ready | error
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

/// Ensure the harness closure is present under `<app_data>/runtime/node-app`.
///
/// The closure ships as a single bundled resource (a tarball inside the .app),
/// so first launch is fully offline: verify the SHA-256, extract to a staging
/// dir, then atomically swap it into place. Subsequent launches on the same
/// version are a no-op. Blocking (file I/O); call from `spawn_blocking`.
pub fn ensure_runtime(
    app_data_dir: &Path,
    manifest: &RuntimeManifest,
    handle: &AppHandle,
) -> Result<PathBuf, String> {
    let runtime = runtime_dir(app_data_dir);

    if is_installed(&runtime, manifest) {
        emit(handle, "ready", "运行时已就绪".to_string(), None);
        return Ok(runtime);
    }

    // Locate the bundled tarball inside the app's Resources directory.
    let resource_dir = handle
        .path()
        .resource_dir()
        .map_err(|e| format!("无法定位应用资源目录: {e}"))?;
    let tarball = resource_dir.join("node-app-runtime.tar.gz");
    if !tarball.is_file() {
        return Err(format!("应用资源缺少运行时包: {}", tarball.display()));
    }

    fs::create_dir_all(&runtime).map_err(|e| format!("mkdir runtime: {e}"))?;
    let staging = runtime.join(".install");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("mkdir staging: {e}"))?;

    // Integrity self-check against the compile-time SHA-256.
    emit(handle, "check", "正在校验运行时…".to_string(), None);
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
    use std::path::PathBuf;

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
}
