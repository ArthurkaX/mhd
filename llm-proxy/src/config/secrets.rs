//! Secrets encryption for the LLM proxy config.
//!
//! On Windows uses **DPAPI** ([`CryptProtectData`] / [`CryptUnprotectData`]) which
//! ties encryption to the current Windows user account — only the same user on
//! the same machine can decrypt. On non-Windows falls back to hex obfuscation
//! (weak but prevents casual reading).
//!
//! # Wire format
//!
//! Secrets are stored in a JSON envelope so the file is self-describing:
//!
//! ```json
//! { "v": 1, "c": "dpapi", "d": "<hex-encoded ciphertext>" }
//! ```
//!
//! `c` is one of `"dpapi"`, `"hex"` (obfuscation), or `"raw"` (plaintext, dev).

use serde::{Deserialize, Serialize};

use std::collections::HashMap;

/// The secrets payload: the actual API keys we protect.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Secrets {
    #[serde(default)]
    pub anthropic_key: String,
    #[serde(default)]
    pub upstream_key: String,
    /// Per-provider API keys, keyed by provider name.
    /// Fallback: if a provider has no entry here, `upstream_key` is used.
    #[serde(default)]
    pub provider_keys: HashMap<String, String>,
}

/// JSON envelope stored on disk.
#[derive(Debug, Serialize, Deserialize)]
struct SecretsFile {
    v: u32,
    c: String, // cipher: "dpapi" | "hex" | "raw"
    d: String, // data: hex-encoded ciphertext or raw JSON
}

const FORMAT_VERSION: u32 = 1;

// ── Public API ─────────────────────────────────────────────────────────

/// Encrypt secrets to the JSON envelope format.
pub fn seal(secrets: &Secrets) -> Vec<u8> {
    let plaintext = serde_json::to_string(secrets).expect("Secrets should always serialize");
    let (cipher, data) = encrypt(&plaintext);
    let envelope = SecretsFile {
        v: FORMAT_VERSION,
        c: cipher.to_string(),
        d: data,
    };
    serde_json::to_vec(&envelope).expect("Envelope should always serialize")
}

/// Decrypt secrets from the JSON envelope format.
pub fn unseal(bytes: &[u8]) -> Option<Secrets> {
    let envelope: SecretsFile = serde_json::from_slice(bytes).ok()?;
    if envelope.v != FORMAT_VERSION {
        return None;
    }
    let plaintext = decrypt(&envelope.c, &envelope.d)?;
    serde_json::from_str(&plaintext).ok()
}

// ── Encryption backends ────────────────────────────────────────────────

fn encrypt(plaintext: &str) -> (&'static str, String) {
    #[cfg(windows)]
    {
        if let Some(ct) = dpapi_encrypt(plaintext.as_bytes()) {
            return ("dpapi", hex_encode(&ct));
        }
    }
    // Fallback: hex-encode the plaintext (weak obfuscation).
    ("hex", hex_encode(plaintext.as_bytes()))
}

fn decrypt(cipher: &str, data: &str) -> Option<String> {
    match cipher {
        "dpapi" => {
            #[cfg(windows)]
            {
                let ct = hex_decode(data)?;
                let pt = dpapi_decrypt(&ct)?;
                String::from_utf8(pt).ok()
            }
            #[cfg(not(windows))]
            {
                let _ = data; // suppress unused
                None
            }
        }
        "hex" => {
            let pt = hex_decode(data)?;
            String::from_utf8(pt).ok()
        }
        "raw" => Some(data.to_string()),
        _ => None,
    }
}

// ── Hex helpers (no base64 dependency needed) ──────────────────────────

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ── Windows DPAPI ──────────────────────────────────────────────────────

#[cfg(windows)]
mod dpapi {
    use std::ptr;

    #[repr(C)]
    struct DATA_BLOB {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            pDataIn: *const DATA_BLOB,
            szDataDescr: *const u16,
            pOptionalEntropy: *const DATA_BLOB,
            pvReserved: *mut std::ffi::c_void,
            pPromptStruct: *mut std::ffi::c_void,
            dwFlags: u32,
            pDataOut: *mut DATA_BLOB,
        ) -> i32;

        fn CryptUnprotectData(
            pDataIn: *const DATA_BLOB,
            ppszDataDescr: *mut *mut u16,
            pOptionalEntropy: *const DATA_BLOB,
            pvReserved: *mut std::ffi::c_void,
            pPromptStruct: *mut std::ffi::c_void,
            dwFlags: u32,
            pDataOut: *mut DATA_BLOB,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(hmem: *mut u8) -> *mut u8;
    }

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x01;

    pub fn encrypt(plaintext: &[u8]) -> Option<Vec<u8>> {
        let input = DATA_BLOB {
            cb_data: plaintext.len() as u32,
            pb_data: plaintext.as_ptr() as *mut u8,
        };
        let mut output = DATA_BLOB {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };

        let ret = unsafe {
            CryptProtectData(
                &input as *const DATA_BLOB,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output as *mut DATA_BLOB,
            )
        };

        if ret == 0 {
            return None;
        }

        let result = if output.cb_data > 0 && !output.pb_data.is_null() {
            let slice =
                unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize) };
            Some(slice.to_vec())
        } else {
            None
        };

        unsafe {
            LocalFree(output.pb_data);
        }

        result
    }

    pub fn decrypt(ciphertext: &[u8]) -> Option<Vec<u8>> {
        let input = DATA_BLOB {
            cb_data: ciphertext.len() as u32,
            pb_data: ciphertext.as_ptr() as *mut u8,
        };
        let mut output = DATA_BLOB {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };

        let ret = unsafe {
            CryptUnprotectData(
                &input as *const DATA_BLOB,
                ptr::null_mut(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output as *mut DATA_BLOB,
            )
        };

        if ret == 0 {
            return None;
        }

        let result = if output.cb_data > 0 && !output.pb_data.is_null() {
            let slice =
                unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize) };
            Some(slice.to_vec())
        } else {
            None
        };

        unsafe {
            LocalFree(output.pb_data);
        }

        result
    }
}

#[cfg(windows)]
use dpapi::{decrypt as dpapi_decrypt, encrypt as dpapi_encrypt};
