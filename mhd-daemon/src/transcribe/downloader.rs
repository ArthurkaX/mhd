//! Automatic downloader for sherpa-onnx-ws and Parakeet models.
//!
//! Uses pure Rust crates (ureq + tar + bzip2) — no dependency on curl.exe
//! or tar.exe. This module is loaded/unloaded with the transcribe feature.
//!
//! Directory layout:
//!   ~/.config/mhd/transcribe/
//!     bin/
//!       sherpa-onnx-ws.exe
//!     models/
//!       parakeet-tdt-0.6b-v3/
//!         tokens.txt
//!         encoder.int8.onnx
//!         decoder.int8.onnx
//!         joiner.int8.onnx
//!       ...

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ── sherpa-onnx-ws releases ──────────────────────────────────────────

const WINDOWS_RELEASES: &[(&str, &str, &str)] = &[
    ("v1.13.2", "sherpa-onnx-v1.13.2-win-x64-shared-MD-Release.tar.bz2",
     "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.2/sherpa-onnx-v1.13.2-win-x64-shared-MD-Release.tar.bz2"),
    ("v1.10.30", "sherpa-onnx-v1.10.30-win-x64-shared.tar.bz2",
     "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.10.30/sherpa-onnx-v1.10.30-win-x64-shared.tar.bz2"),
];

// ── Model registry ───────────────────────────────────────────────────
//
// Models are distributed as .tar.bz2 archives on GitHub releases
// (tag "asr-models"). Inside each archive there is a directory
// containing tokens.txt + encoder/decoder/joiner .onnx files.

const MODEL_REGISTRY: &[ModelEntry] = &[
    ModelEntry {
        name: "parakeet-tdt-0.6b-v3",
        archive: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        size_hint: "465 MB",
    },
    ModelEntry {
        name: "parakeet-tdt-0.6b-v2",
        archive: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
        size_hint: "465 MB",
    },
    ModelEntry {
        name: "zipformer-en",
        archive: "sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2",
        size_hint: "38 MB",
    },
];

struct ModelEntry {
    name: &'static str,
    archive: &'static str,
    url: &'static str,
    size_hint: &'static str,
}

/// Required model files (checked after extraction).
const MODEL_FILES: &[&str] = &[
    "tokens.txt",
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
];

// ── Path helpers ─────────────────────────────────────────────────────

fn transcribe_dir() -> Result<PathBuf, String> {
    Ok(get_config_dir()?.join("transcribe"))
}

pub fn sherpa_onnx_path() -> Result<PathBuf, String> {
    Ok(transcribe_dir()?.join("bin").join("sherpa-onnx-ws.exe"))
}

pub fn models_dir() -> Result<PathBuf, String> {
    Ok(transcribe_dir()?.join("models"))
}

fn get_config_dir() -> Result<PathBuf, String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "cannot determine home directory".to_string())?;
    let dir = Path::new(&home).join(".config").join("mhd");
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create config dir: {e}"))?;
    Ok(dir)
}

// ── HTTP download ───────────────────────────────────────────────────

/// Download a URL to a file. Returns the number of bytes downloaded.
fn download_to_file(url: &str, dest: &Path) -> Result<u64, String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("HTTP {url}: {e}"))?;

    let body: ureq::Body = response.into_body();
    let mut reader = body.into_reader();
    let mut file = fs::File::create(dest)
        .map_err(|e| format!("cannot create file: {e}"))?;

    let downloaded = io::copy(&mut reader, &mut file)
        .map_err(|e| format!("download error: {e}"))?;

    Ok(downloaded)
}

// ── Archive extraction ──────────────────────────────────────────────

fn extract_tar_bz2(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let file = fs::File::open(archive_path)
        .map_err(|e| format!("cannot open archive: {e}"))?;
    let bz_reader = bzip2::read::BzDecoder::new(file);
    let mut archive = tar::Archive::new(bz_reader);
    archive.unpack(dest_dir)
        .map_err(|e| format!("cannot extract archive: {e}"))?;
    Ok(())
}

/// Find first subdirectory inside `dir` (non-recursive).
fn first_subdir(dir: &Path) -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    None
}

// ── sherpa-onnx-ws download ─────────────────────────────────────────

pub fn ensure_sherpa_onnx() -> Result<PathBuf, String> {
    let exe_path = sherpa_onnx_path()?;
    if exe_path.exists() {
        eprintln!("mhd: sherpa-onnx-ws found at {}", exe_path.display());
        return Ok(exe_path);
    }

    let bin_dir = exe_path.parent().unwrap();
    fs::create_dir_all(bin_dir)
        .map_err(|e| format!("cannot create bin dir: {e}"))?;

    for &(version, archive_name, url) in WINDOWS_RELEASES {
        eprintln!("mhd: trying sherpa-onnx-ws {version} ...");
        let archive_path = bin_dir.join(archive_name);

        eprintln!("mhd:   downloading {archive_name} ...");
        let size = match download_to_file(url, &archive_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mhd:   download failed: {e}");
                let _ = fs::remove_file(&archive_path);
                continue;
            }
        };

        if size < 1024 * 1024 {
            eprintln!("mhd:   archive too small ({size} bytes)");
            let _ = fs::remove_file(&archive_path);
            continue;
        }
        eprintln!("mhd:   downloaded {size} bytes");

        eprintln!("mhd:   extracting ...");
        if let Err(e) = extract_tar_bz2(&archive_path, bin_dir) {
            eprintln!("mhd:   extraction failed: {e}");
            let _ = fs::remove_file(&archive_path);
            continue;
        }
        let _ = fs::remove_file(&archive_path);

        // Find the WebSocket server binary in the extracted tree
        if let Some(extracted) = find_subdir(bin_dir, "sherpa-onnx") {
            if let Some(exe) = find_ws_server(&extracted) {
                eprintln!("mhd:   found {} -> sherpa-onnx-ws.exe",
                    exe.file_name().unwrap().to_string_lossy());
                let _ = fs::rename(&exe, &exe_path);
                let _ = fs::remove_dir_all(&extracted);
                eprintln!("mhd: sherpa-onnx-ws ready at {}", exe_path.display());
                return Ok(exe_path);
            }
            let _ = fs::remove_dir_all(&extracted);
        }

        eprintln!("mhd:   WebSocket server not found in archive");
    }

    Err("sherpa-onnx-ws.exe could not be downloaded from any known release".to_string())
}

fn find_subdir(dir: &Path, prefix: &str) -> Option<PathBuf> {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name() {
                    if name.to_string_lossy().starts_with(prefix) {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

/// Search for websocket-server binary in an extracted directory tree.
fn find_ws_server(root: &Path) -> Option<PathBuf> {
    // Check common paths first
    let candidates = [
        root.join("bin").join("sherpa-onnx-online-websocket-server.exe"),
        root.join("bin").join("sherpa-onnx-offline-websocket-server.exe"),
        root.join("bin").join("sherpa-onnx-ws.exe"),
        root.join("sherpa-onnx-online-websocket-server.exe"),
        root.join("sherpa-onnx-offline-websocket-server.exe"),
        root.join("sherpa-onnx-ws.exe"),
    ];
    for c in &candidates {
        if c.exists() {
            return Some(c.clone());
        }
    }
    // Recursive search
    let mut found = Vec::new();
    collect_files(root, &mut found);
    for f in &found {
        let name = f.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        if name.contains("websocket-server") || name == "sherpa-onnx-ws.exe" {
            return Some(f.clone());
        }
    }
    None
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, files);
            } else {
                files.push(path);
            }
        }
    }
}

// ── Model download ──────────────────────────────────────────────────

/// Return list of available model names.
pub fn available_models() -> Vec<&'static str> {
    MODEL_REGISTRY.iter().map(|m| m.name).collect()
}

/// Check if a specific model is already downloaded.
pub fn is_model_downloaded(model_name: &str) -> Result<bool, String> {
    let model_dir = models_dir()?.join(model_name);
    if !model_dir.exists() {
        return Ok(false);
    }
    for file in MODEL_FILES {
        if !model_dir.join(file).exists() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Download a model by name from GitHub releases.
pub fn download_model(model_name: &str) -> Result<PathBuf, String> {
    let entry = MODEL_REGISTRY
        .iter()
        .find(|m| m.name == model_name)
        .ok_or_else(|| format!("unknown model: {model_name}"))?;

    let models_root = models_dir()?;
    let model_dir = models_root.join(model_name);
    let archive_path = models_root.join(entry.archive);

    // If already downloaded and extracted, return
    if is_model_downloaded(model_name)? {
        return Ok(model_dir);
    }

    // If archive exists but not extracted, re-extract
    if !archive_path.exists() {
        eprintln!("mhd: downloading model '{model_name}' ({}). This may take a while...", entry.size_hint);
        download_to_file(entry.url, &archive_path)?;

        let size = fs::metadata(&archive_path).ok().map(|m| m.len()).unwrap_or(0);
        eprintln!("mhd:   downloaded {size} bytes");
    }

    // Extract
    eprintln!("mhd:   extracting model ...");
    fs::create_dir_all(&model_dir)
        .map_err(|e| format!("cannot create model dir: {e}"))?;

    // Extract to a temp dir first, then move
    let temp_dir = models_root.join(".extract_tmp");
    let _ = fs::remove_dir_all(&temp_dir);
    fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("cannot create temp dir: {e}"))?;

    extract_tar_bz2(&archive_path, &temp_dir)?;

    // Find the extracted subdirectory and move contents to model_dir
    if let Some(extracted) = first_subdir(&temp_dir) {
        // Move each file from extracted to model_dir
        for file in MODEL_FILES {
            let src = extracted.join(file);
            if src.exists() {
                let dst = model_dir.join(file);
                let _ = fs::rename(&src, &dst);
            }
        }
    }

    // Cleanup
    let _ = fs::remove_dir_all(&temp_dir);
    let _ = fs::remove_file(&archive_path);

    // Verify
    if is_model_downloaded(model_name).unwrap_or(false) {
        eprintln!("mhd: model '{model_name}' ready at {}", model_dir.display());
        Ok(model_dir)
    } else {
        Err(format!("model '{model_name}' extraction incomplete (missing model files)"))
    }
}

/// Ensure a model is downloaded. Returns the model directory path.
pub fn ensure_model(model_name: &str) -> Result<PathBuf, String> {
    if is_model_downloaded(model_name)? {
        return Ok(models_dir()?.join(model_name));
    }
    download_model(model_name)
}
