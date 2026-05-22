//! sherpa-onnx-ws sidecar management and WebSocket transcription client.
//!
//! This module spawns the sidecar process, finds a free TCP port, waits
//! for it to be ready, then communicates via a minimal WebSocket client.
//! No external WebSocket crate is needed — we implement the handshake
//! and binary/text framing directly over `TcpStream`.

use std::io::{Read, Write};
use std::net::{TcpStream, TcpListener};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::transcribe::config::TranscribeConfig;

/// A handle to a running sherpa-onnx-ws sidecar.
pub struct Sidecar {
    child: Option<Child>,
    port: u16,
}

impl Sidecar {
    /// Start the sidecar on the given port and wait until TCP accepts connections.
    pub fn start(config: &TranscribeConfig, port: u16) -> Result<Self, String> {
        let exe = &config.sherpa_onnx_ws;

        // Resolve model path
        let model_path = if std::path::Path::new(&config.model).is_absolute() {
            config.model.clone()
        } else {
            let p = std::path::Path::new(&config.models_dir).join(&config.model);
            p.to_string_lossy().to_string()
        };

        let mut child = Command::new(exe)
            .args(&[
                "--tokens", &format!("{}/tokens.txt", model_path),
                "--encoder", &format!("{}/encoder.int8.onnx", model_path),
                "--decoder", &format!("{}/decoder.int8.onnx", model_path),
                "--joiner", &format!("{}/joiner.int8.onnx", model_path),
                "--port", &port.to_string(),
                "--num-threads", &config.threads.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start sherpa-onnx-ws: {e}"))?;

        // Wait until TCP port is listening (up to 60 seconds)
        let start = Instant::now();
        let timeout = Duration::from_secs(60);
        let addr = format!("127.0.0.1:{}", port);

        // Stderr reader thread — we spawn a reader to prevent stderr buffer deadlock
        let mut stderr = child.stderr.take()
            .ok_or_else(|| "no stderr on sidecar".to_string())?;
        let _stderr_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_n) => { /* discard */ }
                }
            }
        });

        loop {
            if start.elapsed() > timeout {
                let _ = child.kill();
                let _ = child.wait();
                return Err("sidecar startup timed out (60s)".into());
            }

            match TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200)) {
                Ok(_) => break,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        Ok(Sidecar { child: Some(child), port })
    }

    /// Stop the sidecar (kill process).
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Get the port number.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Find a free TCP port on localhost.
pub fn find_free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("cannot bind: {e}"))?;
    let port = listener.local_addr()
        .map_err(|e| format!("cannot get addr: {e}"))?
        .port();
    // listener drops here, freeing the port. There's a small race but it's fine.
    Ok(port)
}

// ── Minimal WebSocket client ────────────────────────────────────────────

/// WebSocket opcodes.
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

/// A minimal WebSocket client over TcpStream.
pub struct WsClient {
    stream: TcpStream,
}

impl WsClient {
    /// Connect to a WebSocket server at `ws://host:port/path`.
    pub fn connect(host: &str, port: u16, path: &str) -> Result<Self, String> {
        let addr = format!("{host}:{port}");
        let mut stream = TcpStream::connect(&addr)
            .map_err(|e| format!("WS connect to {addr}: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| format!("set WS timeout: {e}"))?;

        // Generate a random Sec-WebSocket-Key
        let key_bytes: [u8; 16] = rand_key();
        let key_b64 = BASE64.encode(key_bytes);

        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}:{port}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key_b64}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );

        stream.write_all(request.as_bytes())
            .map_err(|e| format!("WS send handshake: {e}"))?;
        stream.flush().map_err(|e| format!("WS flush: {e}"))?;

        // Read response
        let mut response = Vec::new();
        let mut buf = [0u8; 1];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    response.push(buf[0]);
                    if response.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                Err(e) => return Err(format!("WS read handshake: {e}")),
            }
        }

        let resp_str = String::from_utf8_lossy(&response);
        if !resp_str.contains(" 101 ") {
            return Err(format!("WS handshake failed: {resp_str}"));
        }

        // Verify Sec-WebSocket-Accept
        let expected_accept = ws_accept_key(&key_b64);
        if let Some(accept_line) = resp_str.lines().find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-accept:")) {
            let accept_val = accept_line.split(':').nth(1).unwrap_or("").trim();
            if accept_val != expected_accept {
                return Err(format!("WS accept key mismatch: got '{accept_val}', expected '{expected_accept}'"));
            }
        }

        Ok(WsClient { stream })
    }

    /// Send a binary frame (masked, as required by client→server).
    pub fn send_binary(&mut self, payload: &[u8]) -> Result<(), String> {
        send_frame(&mut self.stream, OPCODE_BINARY, payload, true)
    }

    /// Send a text frame (masked).
    #[allow(dead_code)]
    pub fn send_text(&mut self, payload: &str) -> Result<(), String> {
        send_frame(&mut self.stream, OPCODE_TEXT, payload.as_bytes(), true)
    }

    /// Read the next frame. Returns `(opcode, payload)`.
    /// For text frames, payload is UTF-8 bytes.
    pub fn read_frame(&mut self) -> Result<(u8, Vec<u8>), String> {
        read_frame_inner(&mut self.stream)
    }

    /// Send a close frame.
    pub fn close(&mut self) -> Result<(), String> {
        // Close frame with status 1000
        let status = 1000u16;
        let payload = status.to_be_bytes();
        send_frame(&mut self.stream, OPCODE_CLOSE, &payload, true)?;
        // Read close frame back
        let _ = self.read_frame();
        Ok(())
    }
}

fn send_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8], masked: bool) -> Result<(), String> {
    let mut header = Vec::new();
    header.push(0x80 | opcode); // FIN + opcode

    let mask_key = if masked {
        let key: [u8; 4] = rand_key_4();
        let len = payload.len();
        if len < 126 {
            header.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            header.push(0x80 | 126);
            header.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            header.push(0x80 | 127);
            header.extend_from_slice(&(len as u64).to_be_bytes());
        }
        header.extend_from_slice(&key);
        Some(key)
    } else {
        let len = payload.len();
        if len < 126 {
            header.push(len as u8);
        } else if len <= 0xFFFF {
            header.push(126);
            header.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            header.push(127);
            header.extend_from_slice(&(len as u64).to_be_bytes());
        }
        None
    };

    stream.write_all(&header)
        .map_err(|e| format!("WS write header: {e}"))?;

    if let Some(key) = mask_key {
        let masked: Vec<u8> = payload.iter().enumerate()
            .map(|(i, b)| b ^ key[i % 4])
            .collect();
        stream.write_all(&masked)
            .map_err(|e| format!("WS write payload: {e}"))?;
    } else {
        stream.write_all(payload)
            .map_err(|e| format!("WS write payload: {e}"))?;
    }

    stream.flush().map_err(|e| format!("WS flush: {e}"))?;
    Ok(())
}

fn read_frame_inner(stream: &mut TcpStream) -> Result<(u8, Vec<u8>), String> {
    let mut buf = [0u8; 2];
    read_exact(stream, &mut buf)?;

    let fin = (buf[0] & 0x80) != 0;
    let opcode = buf[0] & 0x0F;
    let masked = (buf[1] & 0x80) != 0;
    let mut len = (buf[1] & 0x7F) as u64;

    if len == 126 {
        let mut ext = [0u8; 2];
        read_exact(stream, &mut ext)?;
        len = u16::from_be_bytes(ext) as u64;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        read_exact(stream, &mut ext)?;
        len = u64::from_be_bytes(ext);
    }

    let mut mask_key = [0u8; 4];
    if masked {
        read_exact(stream, &mut mask_key)?;
    }

    let mut payload = vec![0u8; len as usize];
    if len > 0 {
        read_exact(stream, &mut payload)?;
    }

    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask_key[i % 4];
        }
    }

    if !fin {
        // Fragmented frames — read continuation
        let (_, mut rest) = read_frame_inner(stream)?;
        payload.append(&mut rest);
    }

    match opcode {
        OPCODE_PING => {
            // Respond with pong
            send_frame(stream, OPCODE_PONG, &[], true)?;
            read_frame_inner(stream) // recurse
        }
        OPCODE_CLOSE => {
            Ok((opcode, payload))
        }
        _ => Ok((opcode, payload)),
    }
}

fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), String> {
    let mut offset = 0;
    while offset < buf.len() {
        match stream.read(&mut buf[offset..]) {
            Ok(0) => return Err("WS connection closed".into()),
            Ok(n) => offset += n,
            Err(e) => return Err(format!("WS read: {e}")),
        }
    }
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn rand_key() -> [u8; 16] {
    let mut key = [0u8; 16];
    // Use a simple pseudo-random (fast enough for one-off handshake)
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    key[0..8].copy_from_slice(&t.to_le_bytes());
    key[8..16].copy_from_slice(&(!t).to_le_bytes());
    key
}

fn rand_key_4() -> [u8; 4] {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u32;
    t.to_le_bytes()
}

fn ws_accept_key(key: &str) -> String {
    let magic = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let combined = format!("{}{}", key, magic);
    let hash = Sha1::digest(combined.as_bytes());
    BASE64.encode(hash)
}

/// Transcribe audio from a WAV file by sending it through the sidecar.
///
/// This is the Phase-1 entry point: given a 16 kHz mono WAV file path,
/// start the sidecar, send samples, collect transcription, return text.
pub fn transcribe_wav_file(config: &TranscribeConfig, wav_path: &str) -> Result<String, String> {
    // 1. Find a free port
    let port = find_free_port()?;

    // 2. Start sidecar
    let mut sidecar = Sidecar::start(config, port)?;

    // 3. Read WAV file
    let wav_data = std::fs::read(wav_path)
        .map_err(|e| format!("cannot read WAV file '{wav_path}': {e}"))?;

    // Parse WAV header to find PCM data
    let (sample_rate, pcm_data) = parse_wav(&wav_data)?;

    // Convert PCM to f32 samples
    let samples = pcm_to_f32(&pcm_data, sample_rate);

    // 4. Connect WebSocket
    let mut ws = WsClient::connect("127.0.0.1", port, "/")?;

    // 5. Send audio: [int32_le sample_rate][int32_le num_bytes][f32 samples...]
    let sr_bytes = (sample_rate as i32).to_le_bytes();
    let num_bytes = (samples.len() * 4) as i32; // f32 = 4 bytes each
    let num_bytes_le = num_bytes.to_le_bytes();

    let mut msg = Vec::with_capacity(8 + samples.len() * 4);
    msg.extend_from_slice(&sr_bytes);
    msg.extend_from_slice(&num_bytes_le);
    for &s in &samples {
        msg.extend_from_slice(&s.to_le_bytes());
    }

    ws.send_binary(&msg)?;

    // Also send an empty frame to signal end of audio (optional, but good practice)
    // Send a zero-length binary frame
    ws.send_binary(&[])?;

    // 6. Read response(s) — accumulate text frames
    let mut result = String::new();
    loop {
        match ws.read_frame() {
            Ok((opcode, payload)) => {
                if opcode == OPCODE_TEXT {
                    if let Ok(text) = String::from_utf8(payload.clone()) {
                        result.push_str(&text);
                    }
                } else if opcode == OPCODE_BINARY {
                    // Binary response — could be end marker or metadata
                    if payload.len() < 4 && payload.iter().all(|&b| b == 0) {
                        // Empty or zero — end of transcription
                        break;
                    }
                } else if opcode == OPCODE_CLOSE {
                    break;
                }
            }
            Err(e) => {
                // Could be timeout or connection closed — treat as end
                break;
            }
        }
    }

    // 7. Cleanup
    let _ = ws.close();
    sidecar.stop();

    if result.is_empty() {
        Err("transcription returned empty result".into())
    } else {
        Ok(result.trim().to_string())
    }
}

// ── WAV parsing helpers ─────────────────────────────────────────────────

fn parse_wav(data: &[u8]) -> Result<(u32, Vec<i16>), String> {
    if data.len() < 44 {
        return Err("WAV file too short".into());
    }

    // Check RIFF header
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a valid WAV file".into());
    }

    // Find fmt chunk
    let mut pos = 12;
    let mut sample_rate = 16000u32;
    let mut bits_per_sample = 16u16;
    let mut channels = 1u16;

    loop {
        if pos + 8 > data.len() {
            return Err("WAV: no fmt chunk found".into());
        }
        let chunk_id = &data[pos..pos+4];
        let chunk_size = u32::from_le_bytes(data[pos+4..pos+8].try_into().unwrap()) as usize;

        if chunk_id == b"fmt " {
            if chunk_size < 16 || pos + 8 + chunk_size > data.len() {
                return Err("WAV: invalid fmt chunk".into());
            }
            let audio_format = u16::from_le_bytes(data[pos+8..pos+10].try_into().unwrap());
            if audio_format != 1 {
                return Err(format!("WAV: unsupported audio format {audio_format} (only PCM supported)"));
            }
            channels = u16::from_le_bytes(data[pos+10..pos+12].try_into().unwrap());
            sample_rate = u32::from_le_bytes(data[pos+12..pos+16].try_into().unwrap());
            bits_per_sample = u16::from_le_bytes(data[pos+22..pos+24].try_into().unwrap());
            pos += 8 + ((chunk_size + 1) & !1);
            break;
        }
        pos += 8 + ((chunk_size + 1) & !1); // align to 2 bytes
    }

    // Find data chunk
    loop {
        if pos + 8 > data.len() {
            return Err("WAV: no data chunk found".into());
        }
        let chunk_id = &data[pos..pos+4];
        let chunk_size = u32::from_le_bytes(data[pos+4..pos+8].try_into().unwrap()) as usize;
        pos += 8;

        if chunk_id == b"data" {
            let pcm_end = (pos + chunk_size).min(data.len());
            let pcm_raw = &data[pos..pcm_end];

            match bits_per_sample {
                16 => {
                    let samples: Vec<i16> = pcm_raw.chunks(2)
                        .filter(|c| c.len() == 2)
                        .map(|c| i16::from_le_bytes(c.try_into().unwrap()))
                        .collect();
                    // Downmix to mono if stereo
                    if channels == 1 {
                        return Ok((sample_rate, samples));
                    } else {
                        let mono: Vec<i16> = samples.chunks(channels as usize)
                            .map(|ch| ch.iter().map(|&s| s as i32).sum::<i32>() / channels as i32)
                            .map(|s| s as i16)
                            .collect();
                        return Ok((sample_rate, mono));
                    }
                }
                8 => {
                    let samples: Vec<i16> = pcm_raw.iter()
                        .map(|&b| ((b as i16) - 128) << 8)
                        .collect();
                    if channels == 1 {
                        return Ok((sample_rate, samples));
                    } else {
                        let mono: Vec<i16> = samples.chunks(channels as usize)
                            .map(|ch| ch.iter().map(|&s| s as i32).sum::<i32>() / channels as i32)
                            .map(|s| s as i16)
                            .collect();
                        return Ok((sample_rate, mono));
                    }
                }
                other => return Err(format!("unsupported bits per sample: {other}")),
            }
        }
        pos += (chunk_size + 1) & !1; // align
    }
}

fn pcm_to_f32(pcm: &[i16], sample_rate: u32) -> Vec<f32> {
    // Resample to 16 kHz if needed
    if sample_rate == 16000 {
        pcm.iter().map(|&s| (s as f32) / 32768.0).collect()
    } else {
        // Simple linear resample to 16 kHz
        let out_len = (pcm.len() as f64 * 16000.0 / sample_rate as f64) as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let src_idx = (i as f64 * sample_rate as f64 / 16000.0) as usize;
            let s = pcm.get(src_idx).copied().unwrap_or(0) as f32 / 32768.0;
            out.push(s);
        }
        out
    }
}
