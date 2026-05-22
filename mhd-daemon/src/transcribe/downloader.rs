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
use std::time::Duration;

/// Known working Windows release archives for sherpa-onnx.
/// (version, archive_name, download_url)
const WINDOWS_RELEASES: &[(&str, &str, &str)] = &[
    ("v1.13.2", "sherpa-onnx-v1.13.2-win-x64-shared-MD-Release.tar.bz2",
     "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.2/sherpa-onnx-v1.13.2-win-x64-shared-MD-Release.tar.bz2"),
    ("v1.10.30", "sherpa-onnx-v1.10.30-win-x64-shared.tar.bz2",
     "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.10.30/sherpa-onnx-v1.10.30-win-x64-shared.tar.bz2"),
];

/// Known Parakeet models (name → HuggingFace repo).
const PARAKEET_MODELS: &[(&str, &str)] = &[
    ("parakeet-tdt-0.6b-v3",   "k2-fsa/parakeet-tdt-0.6b-v3"),
    ("parakeet-ctc-0.6b-v3",   "k2-fsa/parakeet-ctc-0.6b-v3"),
    ("parakeet-tdt-1.1b-v3",   "k2-fsa/parakeet-tdt-1.1b-v3"),
    ("parakeet-ctc-1.1b-v3",   "k2-fsa/parakeet-ctc-1.1b-v3"),
    ("parakeet-tdt-0.6b-int8", "k2-fsa/parakeet-tdt-0.6b-int8"),
];

/// Required model files for sherpa-onnx-ws.
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

// ── HTTP download helper ────────────────────────────────────────────

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

/// Extract a .tar.bz2 archive into `dest_dir` using bzip2 + tar crates.
fn extract_tar_bz2(archive_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let file = fs::File::open(archive_path)
        .map_err(|e| format!("cannot open archive: {e}"))?;
    let bz_reader = bzip2::read::BzDecoder::new(file);
    let mut archive = tar::Archive::new(bz_reader);
    archive.unpack(dest_dir)
        .map_err(|e| format!("cannot extract archive: {e}"))?;
    Ok(())
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

        // Download
        eprintln!("mhd:   downloading {archive_name} ...");
        if let Err(e) = download_to_file(url, &archive_path) {
            eprintln!("mhd:   download failed: {e}");
            let _ = fs::remove_file(&archive_path);
            continue;
        }

        let size = fs::metadata(&archive_path).ok().map(|m| m.len()).unwrap_or(0);
        if size < 1024 * 1024 {
            eprintln!("mhd:   archive too small ({size} bytes), trying next...");
            let _ = fs::remove_file(&archive_path);
            continue;
        }
        eprintln!("mhd:   downloaded {size} bytes");

        // Extract
        eprintln!("mhd:   extracting ...");
        if let Err(e) = extract_tar_bz2(&archive_path, bin_dir) {
            eprintln!("mhd:   extraction failed: {e}");
            let _ = fs::remove_file(&archive_path);
            continue;
        }

        let _ = fs::remove_file(&archive_path);

        // Find the WebSocket server binary
        if let Some(extracted) = find_subdir(bin_dir, "sherpa-onnx") {
            let bin_subdir = extracted.join("bin");

            let candidates = [
                bin_subdir.join("sherpa-onnx-online-websocket-server.exe"),
                bin_subdir.join("sherpa-onnx-offline-websocket-server.exe"),
                bin_subdir.join("sherpa-onnx-ws.exe"),
            ];

            for c in &candidates {
                if c.exists() {
                    eprintln!("mhd:   found {} -> sherpa-onnx-ws.exe",
                        c.file_name().unwrap().to_string_lossy());
                    let _ = fs::rename(c, &exe_path);
                    let _ = fs::remove_dir_all(&extracted);
                    eprintln!("mhd: sherpa-onnx-ws ready at {}", exe_path.display());
                    return Ok(exe_path);
                }
            }

            // Recursive search
            let mut found = Vec::new();
            collect_files(&extracted, &mut found);
            for f in &found {
                let name = f.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                if name.contains("websocket-server") || name == "sherpa-onnx-ws.exe" {
                    eprintln!("mhd:   found {} -> sherpa-onnx-ws.exe", name);
                    let _ = fs::rename(f, &exe_path);
                    let _ = fs::remove_dir_all(&extracted);
                    eprintln!("mhd: sherpa-onnx-ws ready at {}", exe_path.display());
                    return Ok(exe_path);
                }
            }
            let _ = fs::remove_dir_all(&extracted);
        }

        eprintln!("mhd:   sherpa-onnx-ws.exe not found in archive");
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
    PARAKEET_MODELS.iter().map(|(name, _)| *name).collect()
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

/// Download a Parakeet model from HuggingFace.
/// Reports progress via a callback (can be used for UI).
pub fn download_model<F>(model_name: &str, mut progress: F) -> Result<PathBuf, String>
where
    F: FnMut(usize, usize),
{
    let repo = PARAKEET_MODELS
        .iter()
        .find(|(name, _)| *name == model_name)
        .map(|(_, repo)| *repo)
        .ok_or_else(|| format!("unknown model: {model_name}"))?;

    let model_dir = models_dir()?.join(model_name);
    fs::create_dir_all(&model_dir)
        .map_err(|e| format!("cannot create model dir: {e}"))?;

    for (i, file) in MODEL_FILES.iter().enumerate() {
        let dest = model_dir.join(file);
        if dest.exists() {
            progress(i, MODEL_FILES.len());
            continue;
        }

        let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");
        eprintln!("mhd: downloading model file {}/{}: {file}", i + 1, MODEL_FILES.len());

        download_to_file(&url, &dest)
            .map_err(|e| format!("failed to download {file}: {e}"))?;

        progress(i + 1, MODEL_FILES.len());
    }

    eprintln!("mhd: model '{model_name}' ready at {}", model_dir.display());
    Ok(model_dir)
}

/// Ensure a model is downloaded. Returns the model directory path.
pub fn ensure_model(model_name: &str) -> Result<PathBuf, String> {
    if is_model_downloaded(model_name)? {
        return Ok(models_dir()?.join(model_name));
    }
    download_model(model_name, |_, _| {})
}

/// Resolve model path from config.
pub fn resolve_model_path(config: &crate::transcribe::config::TranscribeConfig) -> PathBuf {
    let model_path = Path::new(&config.model);
    if model_path.is_absolute() {
        return model_path.to_path_buf();
    }
    if !config.models_dir.is_empty() {
        return Path::new(&config.models_dir).join(&config.model);
    }
    models_dir()
        .unwrap_or_default()
        .join(&config.model)
}
