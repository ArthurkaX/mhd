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

/// GitHub repo for sherpa-onnx.
const SHERPA_ONNX_REPO: &str = "k2-fsa/sherpa-onnx";
/// Fallback archive name (used if GitHub API fails).
/// This is the v1.10.30 naming pattern (old style).
const FALLBACK_ARCHIVE: &str = "sherpa-onnx-v1.10.30-win-x64-shared.tar.bz2";
const FALLBACK_VERSION: &str = "v1.10.30";

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

    // Determine the best archive URL
    let (version, archive_name) = get_windows_archive_info().unwrap_or_else(|_| {
        (FALLBACK_VERSION.to_string(), FALLBACK_ARCHIVE.to_string())
    });

    let url = format!(
        "https://github.com/{SHERPA_ONNX_REPO}/releases/download/{version}/{archive_name}"
    );

    eprintln!("mhd: downloading sherpa-onnx-ws ({version})...");
    eprintln!("mhd: url: {url}");

    let archive_path = bin_dir.join(&archive_name);

    // Download using curl.exe (available on Windows 10+)
    let status = Command::new("curl.exe")
        .args(["-L", "-o", &archive_path.to_string_lossy(), &url])
        .status()
        .map_err(|e| format!("cannot run curl.exe: {e}"))?;

    if !status.success() {
        return Err(format!("curl download failed (exit: {status:?})"));
    }

    // Extract using tar.exe (available on Windows 10+)
    eprintln!("mhd: extracting...");
    let status = Command::new("tar.exe")
        .args(["-xf", &archive_path.to_string_lossy(), "-C", &bin_dir.to_string_lossy()])
        .status()
        .map_err(|e| format!("cannot run tar.exe: {e}"))?;

    if !status.success() {
        return Err(format!("tar extraction failed (exit: {status:?})"));
    }

    // Find the binary in the extracted tree
    let found = find_and_move_exe(bin_dir, &exe_path);

    // Remove archive
    let _ = fs::remove_file(&archive_path);

    if found {
        eprintln!("mhd: sherpa-onnx-ws ready at {}", exe_path.display());
        Ok(exe_path)
    } else {
        Err(format!(
            "sherpa-onnx-ws.exe not found after extraction in {}",
            bin_dir.display()
        ))
    }
}

/// Recursively search for sherpa-onnx-ws.exe in `dir`, move to `dest`.
fn find_and_move_exe(dir: &Path, dest: &Path) -> bool {
    // First check common paths
    let candidates = [
        dir.join("bin").join("sherpa-onnx-ws.exe"),
        dir.join("sherpa-onnx-ws.exe"),
        dir.join("build").join("bin").join("sherpa-onnx-ws.exe"),
        dir.join("Release").join("bin").join("sherpa-onnx-ws.exe"),
    ];

    for c in &candidates {
        if c.exists() {
            let _ = fs::rename(c, dest);
            // Clean up extracted dir
            if let Some(parent) = c.parent() {
                if let Some(grand) = parent.parent() {
                    let _ = fs::remove_dir_all(grand);
                }
            }
            return dest.exists();
        }
    }

    // Fallback: walk the directory tree
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path != dest.parent().unwrap() {
                if find_and_move_exe(&path, dest) {
                    return true;
                }
            }
        }
    }
    false
}

/// Determine the correct Windows release archive name.
///
/// Tries the GitHub API to get the latest release, then finds a suitable
/// Windows x64 shared archive asset.
fn get_windows_archive_info() -> Result<(String, String), String> {
    let url = format!("https://api.github.com/repos/{SHERPA_ONNX_REPO}/releases/latest");
    let output = Command::new("curl.exe")
        .args(["-s", "-L", "-H", "Accept: application/json", &url])
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;

    if !output.status.success() {
        return Err("GitHub API request failed".into());
    }

    let body = String::from_utf8_lossy(&output.stdout);

    // Extract tag_name
    let version = extract_json_string(&body, "tag_name")
        .ok_or_else(|| "no tag_name in response".to_string())?;

    // Find a Windows x64 shared archive (non-cuda, non-debug, non-jni)
    // Patterns in the assets list:
    //   "sherpa-onnx-v{ver}-win-x64-shared.tar.bz2"           (old naming)
    //   "sherpa-onnx-v{ver}-win-x64-shared-MD-Release.tar.bz2" (new naming)
    //   "sherpa-onnx-v{ver}-win-x64-shared-MT-Release.tar.bz2"
    //   "sherpa-onnx-v{ver}-win-x64-static-MD-Release.tar.bz2"
    //   etc.
    //
    // We prefer: shared (not static), not cuda, not debug, not jni, not no-tts

    let asset_names = extract_asset_names(&body);
    let preferred = asset_names.iter().find(|name| {
        name.contains("win-x64")
            && name.contains("shared")
            && !name.contains("static")
            && !name.contains("cuda")
            && !name.contains("Debug")
            && !name.contains("RelWithDebInfo")
            && !name.contains("MinSizeRel")
            && !name.contains("jni")
            && !name.contains("-lib")
            && !name.contains("-no-tts")
            && name.ends_with(".tar.bz2")
    });

    if let Some(archive_name) = preferred {
        return Ok((version, archive_name.clone()));
    }

    // Fallback: any win-x64 shared archive
    let fallback = asset_names.iter().find(|name| {
        name.contains("win-x64")
            && name.contains("shared")
            && !name.contains("cuda")
            && name.ends_with(".tar.bz2")
    });

    if let Some(archive_name) = fallback {
        return Ok((version, archive_name.clone()));
    }

    Err("no suitable Windows archive found in release assets".to_string())
}

/// Extract a JSON string field by key (no serde dependency).
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let search = format!(r#""{key}":""#);
    if let Some(start) = body.find(&search) {
        let val_start = start + search.len();
        if let Some(end) = body[val_start..].find('"') {
            return Some(body[val_start..val_start + end].to_string());
        }
    }
    None
}

/// Extract all asset names from a GitHub releases JSON body.
fn extract_asset_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let search = r#""name":""#;
    let mut pos = 0;
    while let Some(start) = body[pos..].find(search) {
        let val_start = pos + start + search.len();
        if let Some(end) = body[val_start..].find('"') {
            names.push(body[val_start..val_start + end].to_string());
            pos = val_start + end + 1;
        } else {
            break;
        }
    }
    names
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

        let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");

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
