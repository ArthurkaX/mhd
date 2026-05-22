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

/// GitHub release info for sherpa-onnx.
const SHERPA_ONNX_REPO: &str = "k2-fsa/sherpa-onnx";
/// Hardcoded fallback version (will try GitHub API first).
const SHERPA_ONNX_VERSION: &str = "v1.10.30";

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

/// Canonical transcribe directory under `~/.config/mhd/`.
fn transcribe_dir() -> Result<PathBuf, String> {
    let base = get_config_dir()?;
    Ok(base.join("transcribe"))
}

/// Path to sherpa-onnx-ws.exe.
pub fn sherpa_onnx_path() -> Result<PathBuf, String> {
    Ok(transcribe_dir()?.join("bin").join("sherpa-onnx-ws.exe"))
}

/// Directory for model files.
pub fn models_dir() -> Result<PathBuf, String> {
    Ok(transcribe_dir()?.join("models"))
}

/// Get `~/.config/mhd` directory, creating if needed.
fn get_config_dir() -> Result<PathBuf, String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "cannot determine home directory".to_string())?;
    let dir = Path::new(&home).join(".config").join("mhd");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create config dir: {e}"))?;
    Ok(dir)
}

/// Ensure `sherpa-onnx-ws.exe` exists locally.
/// If missing, downloads from GitHub releases.
pub fn ensure_sherpa_onnx() -> Result<PathBuf, String> {
    let exe_path = sherpa_onnx_path()?;
    if exe_path.exists() {
        return Ok(exe_path);
    }

    // Create bin directory
    let bin_dir = exe_path.parent().unwrap();
    fs::create_dir_all(bin_dir)
        .map_err(|e| format!("cannot create bin dir: {e}"))?;

    // Try to get latest version from GitHub API
    let version = get_latest_sherpa_version().unwrap_or_else(|_| SHERPA_ONNX_VERSION.to_string());

    // Build download URL
    let version_stripped = version.trim_start_matches('v');
    let archive_name = format!("sherpa-onnx-win64-cpu-v{version_stripped}.tar.bz2");
    let url = format!(
        "https://github.com/{SHERPA_ONNX_REPO}/releases/download/{version}/{archive_name}"
    );
    let archive_path = bin_dir.join(&archive_name);

    eprintln!("mhd: downloading sherpa-onnx-ws ({version})...");

    // Download using curl.exe (available on Windows 10+)
    let status = Command::new("curl.exe")
        .args(["-L", "-o", &archive_path.to_string_lossy(), &url])
        .status()
        .map_err(|e| format!("cannot run curl.exe: {e}"))?;

    if !status.success() {
        return Err(format!("curl download failed (exit: {status:?})"));
    }

    // Extract using tar.exe (available on Windows 10+)
    eprintln!("mhd: extracting sherpa-onnx-ws...");
    let status = Command::new("tar.exe")
        .args(["-xf", &archive_path.to_string_lossy(), "-C", &bin_dir.to_string_lossy()])
        .status()
        .map_err(|e| format!("cannot run tar.exe: {e}"))?;

    if !status.success() {
        return Err(format!("tar extraction failed (exit: {status:?})"));
    }

    // Move the binary from the extracted subdirectory to bin/
    let extracted_dir = bin_dir.join(format!("sherpa-onnx-win64-cpu-v{version_stripped}"));
    let extracted_exe = extracted_dir.join("bin").join("sherpa-onnx-ws.exe");
    if extracted_exe.exists() {
        fs::rename(&extracted_exe, &exe_path)
            .map_err(|e| format!("cannot move sherpa-onnx-ws.exe: {e}"))?;
        // Clean up extracted directory
        let _ = fs::remove_dir_all(&extracted_dir);
    } else {
        // Maybe the binary is directly in the archive root
        let alt_exe = extracted_dir.join("sherpa-onnx-ws.exe");
        if alt_exe.exists() {
            fs::rename(&alt_exe, &exe_path)
                .map_err(|e| format!("cannot move sherpa-onnx-ws.exe: {e}"))?;
            let _ = fs::remove_dir_all(&extracted_dir);
        }
    }

    // Remove archive
    let _ = fs::remove_file(&archive_path);

    if exe_path.exists() {
        eprintln!("mhd: sherpa-onnx-ws ready at {}", exe_path.display());
        Ok(exe_path)
    } else {
        Err("sherpa-onnx-ws.exe not found after extraction".to_string())
    }
}

/// Get the latest release version from GitHub API.
fn get_latest_sherpa_version() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{SHERPA_ONNX_REPO}/releases/latest");
    let output = Command::new("curl.exe")
        .args(["-s", "-L", "-H", "Accept: application/json", &url])
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;

    if !output.status.success() {
        return Err("GitHub API request failed".into());
    }

    let body = String::from_utf8_lossy(&output.stdout);
    // Parse "tag_name" from JSON (simple, no serde dependency)
    if let Some(tag_start) = body.find(r#""tag_name":""#) {
        let start = tag_start + r#""tag_name":""#.len();
        if let Some(end) = body[start..].find('"') {
            return Ok(body[start..start + end].to_string());
        }
    }
    Err("could not parse tag_name from GitHub API response".to_string())
}

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
    // Check that all required files exist
    for file in MODEL_FILES {
        if !model_dir.join(file).exists() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Download a Parakeet model from HuggingFace.
/// Reports progress via a callback (can be used for UI).
pub fn download_model<F>(model_name: &str, progress: F) -> Result<PathBuf, String>
where
    F: Fn(usize, usize), // (current_file, total_files)
{
    // Find model info
    let repo = PARAKEET_MODELS
        .iter()
        .find(|(name, _)| *name == model_name)
        .map(|(_, repo)| *repo)
        .ok_or_else(|| format!("unknown model: {model_name}"))?;

    let model_dir = models_dir()?.join(model_name);
    fs::create_dir_all(&model_dir)
        .map_err(|e| format!("cannot create model dir: {e}"))?;

    let total_files = MODEL_FILES.len();

    for (i, file) in MODEL_FILES.iter().enumerate() {
        let dest = model_dir.join(file);
        if dest.exists() {
            progress(i, total_files);
            continue;
        }

        let url = format!(
            "https://huggingface.co/{repo}/resolve/main/{file}"
        );

        eprintln!("mhd: downloading model file {}/{}: {file}", i + 1, total_files);
        let status = Command::new("curl.exe")
            .args(["-L", "-o", &dest.to_string_lossy(), &url])
            .status()
            .map_err(|e| format!("cannot run curl.exe for {file}: {e}"))?;

        if !status.success() {
            return Err(format!("download failed for {file} (exit: {status:?})"));
        }

        progress(i + 1, total_files);
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

/// Parse the config to resolve model path:
/// - If `config.model` is an absolute path, use it as-is.
/// - If `config.models_dir` is set, use it.
/// - Otherwise, resolve `transcribe/models/<model_name>`.
pub fn resolve_model_path(config: &crate::transcribe::config::TranscribeConfig) -> PathBuf {
    let model_path = Path::new(&config.model);
    if model_path.is_absolute() {
        return model_path.to_path_buf();
    }
    if !config.models_dir.is_empty() {
        return Path::new(&config.models_dir).join(&config.model);
    }
    // Default: ~/.config/mhd/transcribe/models/<model_name>
    models_dir()
        .unwrap_or_default()
        .join(&config.model)
}
