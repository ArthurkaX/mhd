//! Automatic downloader for sherpa-onnx-ws and Parakeet models.
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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;

/// Known working Windows release archives.
/// (version, archive_name, url)
const WINDOWS_RELEASES: &[(&str, &str, &str)] = &[
    // Latest (May 2026)
    ("v1.13.2", "sherpa-onnx-v1.13.2-win-x64-shared-MD-Release.tar.bz2",
     "https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.13.2/sherpa-onnx-v1.13.2-win-x64-shared-MD-Release.tar.bz2"),
    // Old naming (v1.10.x)
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
    let base = get_config_dir()?;
    Ok(base.join("transcribe"))
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
    fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create config dir: {e}"))?;
    Ok(dir)
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
        let status = Command::new("curl.exe")
            .args(["-L", "-o", &archive_path.to_string_lossy(), url])
            .status()
            .map_err(|e| format!("cannot run curl.exe: {e}"))?;

        if !status.success() {
            eprintln!("mhd:   download failed, trying next...");
            let _ = fs::remove_file(&archive_path);
            continue;
        }

        // Reject error pages (less than 1 MB)
        if fs::metadata(&archive_path).ok().map(|m| m.len()).unwrap_or(0) < 1024 * 1024 {
            eprintln!("mhd:   archive too small (probably error page), trying next...");
            let _ = fs::remove_file(&archive_path);
            continue;
        }

        // Extract
        eprintln!("mhd:   extracting ...");
        let status = Command::new("tar.exe")
            .args(["-xf", &archive_path.to_string_lossy(), "-C", &bin_dir.to_string_lossy()])
            .status()
            .map_err(|e| format!("cannot run tar.exe: {e}"))?;

        if !status.success() {
            eprintln!("mhd:   extraction failed, trying next...");
            let _ = fs::remove_file(&archive_path);
            continue;
        }

        let _ = fs::remove_file(&archive_path);

        // Find sherpa-onnx-ws.exe in the extracted tree.
        // The archive extracts to a subdirectory like:
        //   sherpa-onnx-v1.13.2-win-x64-shared-MD-Release/bin/sherpa-onnx-ws.exe
        if let Some(extracted) = find_subdir(bin_dir, "sherpa-onnx") {
            let candidate = extracted.join("bin").join("sherpa-onnx-ws.exe");
            if candidate.exists() {
                eprintln!("mhd:   found at {}", candidate.display());
                let _ = fs::rename(&candidate, &exe_path);
                let _ = fs::remove_dir_all(&extracted);
                eprintln!("mhd: sherpa-onnx-ws ready at {}", exe_path.display());
                return Ok(exe_path);
            }
            // Search recursively
            let mut found = Vec::new();
            collect_files(&extracted, &mut found);
            for f in &found {
                if f.ends_with("sherpa-onnx-ws.exe") {
                    eprintln!("mhd:   found at {}", f.display());
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

/// Find first subdirectory starting with `prefix` inside `dir`.
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

/// Recursively collect all file paths under `dir`.
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

// ── Model helpers ────────────────────────────────────────────────────

pub fn available_models() -> Vec<&'static str> {
    PARAKEET_MODELS.iter().map(|(name, _)| *name).collect()
}

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

pub fn download_model<F>(model_name: &str, progress: F) -> Result<PathBuf, String>
where
    F: Fn(usize, usize),
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

        let status = Command::new("curl.exe")
            .args(["-L", "-o", &dest.to_string_lossy(), &url])
            .status()
            .map_err(|e| format!("cannot run curl.exe for {file}: {e}"))?;

        if !status.success() {
            return Err(format!("download failed for {file} (exit: {status:?})"));
        }
        progress(i + 1, MODEL_FILES.len());
    }

    eprintln!("mhd: model '{model_name}' ready at {}", model_dir.display());
    Ok(model_dir)
}

pub fn ensure_model(model_name: &str) -> Result<PathBuf, String> {
    if is_model_downloaded(model_name)? {
        return Ok(models_dir()?.join(model_name));
    }
    download_model(model_name, |_, _| {})
}

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
