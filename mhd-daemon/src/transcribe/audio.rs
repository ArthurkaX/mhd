//! WASAPI microphone capture.
//!
//! Captures from the default input endpoint, converts to 16 kHz mono f32.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Audio chunk produced by the capture loop.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Monotonically increasing sequence ID.
    pub sequence_id: u64,
    /// Sample rate (always 16000 after conversion).
    pub sample_rate: u32,
    /// PCM f32 samples, mono.
    pub samples: Vec<f32>,
    /// Timestamp (ms) when this chunk was produced.
    pub started_at_ms: u64,
    /// Duration of this chunk in ms.
    pub duration_ms: u64,
}

/// Start capturing from the default microphone.
///
/// Spawns a capture thread that sends audio chunks through the returned
/// receiver. The thread stops when `running` is set to `false`.
pub fn start_capture(
    running: std::sync::Arc<AtomicBool>,
) -> Result<mpsc::Receiver<AudioChunk>, String> {
    let (tx, rx) = mpsc::channel();

    let _handle = thread::Builder::new()
        .name("wasapi-capture".into())
        .spawn(move || {
            if let Err(e) = capture_loop(running, tx) {
                eprintln!("mhd: audio capture error: {e}");
            }
        })
        .map_err(|e| format!("cannot spawn capture thread: {e}"))?;

    Ok(rx)
}

/// The capture loop: initialises COM, sets up WASAPI, reads audio until
/// `running` goes false.
fn capture_loop(
    running: std::sync::Arc<AtomicBool>,
    tx: mpsc::Sender<AudioChunk>,
) -> Result<(), String> {
    unsafe {
        // 1. Initialise COM
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() {
            return Err(format!("CoInitializeEx failed: {hr:?}"));
        }


        let result = wasapi_capture(&running, &tx);

        CoUninitialize();
        result
    }
}

unsafe fn wasapi_capture(
    running: &AtomicBool,
    tx: &mpsc::Sender<AudioChunk>,
) -> Result<(), String> {
    // 2. Create device enumerator via CoCreateInstance
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(
        &MMDeviceEnumerator,
        None,
        CLSCTX_ALL,
    )
    .map_err(|e| format!("CoCreate IMMDeviceEnumerator: {e}"))?;

    // 3. Get default capture endpoint
    let device = enumerator
        .GetDefaultAudioEndpoint(eCapture, eConsole)
        .map_err(|e| format!("GetDefaultAudioEndpoint: {e}"))?;

    // 4. Activate IAudioClient
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("Activate IAudioClient: {e}"))?;

    // 5. Get mix format
    let mix_ptr = client.GetMixFormat()
        .map_err(|e| format!("GetMixFormat: {e}"))?;
    let mix = &*mix_ptr;

    let device_sr = mix.nSamplesPerSec;
    let device_channels = mix.nChannels;

    // 6. Try to initialise with our desired format: 16 kHz mono float
    let mut our_fmt = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 1,
        nSamplesPerSec: 16000,
        nAvgBytesPerSec: 16000 * 4, // 16 kHz * 4 bytes (float)
        nBlockAlign: 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };

    let init_result = client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        100_0000, // 100 ms buffer (in 100-ns units)
        0,
        &our_fmt,
        None,
    );

    if init_result.is_err() {
        // Fall back to device's mix format. Need Reset first (or create new client).
        // Shared mode doesn't support Reset, so we try with mix format directly.
        let _ = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            100_0000,
            0,
            &*mix_ptr,
            None,
        ).map_err(|e| format!("Initialize (mix format): {e}"))?;
    }

    // 7. Get capture client via GetService
    let capture: IAudioCaptureClient = client.GetService()
        .map_err(|e| format!("GetService IAudioCaptureClient: {e}"))?;

    // 8. Create event handle for buffer notifications
    let event = CreateEventW(None, false, false, None)
        .map_err(|e| format!("CreateEvent: {e}"))?;
    client.SetEventHandle(event)
        .map_err(|e| format!("SetEventHandle: {e}"))?;

    // 9. Start capture
    client.Start().map_err(|e| format!("Start: {e}"))?;

    // 10. Capture loop
    let mut seq: u64 = 0;
    let mut buffer: Vec<f32> = Vec::new();
    let chunk_samples_50ms = 800; // 50 ms @ 16 kHz

    while running.load(Ordering::Relaxed) {
        // Wait for event or timeout (check running every 200ms)
        let wait = WaitForSingleObject(event, 200);
        if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
            break;
        }
        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Read all available packets
        loop {
            let packet_size = match capture.GetNextPacketSize() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("mhd: GetNextPacketSize: {e}");
                    break;
                }
            };
            if packet_size == 0 {
                break;
            }

            let mut flags: u32 = 0;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;

            if let Err(e) = capture.GetBuffer(
                &mut data_ptr as *mut *mut u8,
                &mut frames,
                &mut flags as *mut u32,
                None,
                None,
            ) {
                eprintln!("mhd: GetBuffer: {e}");
                break;
            }

            let is_silent = flags & 2 != 0; // AUDCLNT_BUFFERFLAGS_SILENT = 2

            if frames > 0 && !data_ptr.is_null() {
                let is_float = mix.wFormatTag == 3; // WAVE_FORMAT_IEEE_FLOAT
                let is_pcm = mix.wFormatTag == 1;    // WAVE_FORMAT_PCM

                let total_samples = frames as usize * device_channels as usize;
                let mono_samples = frames as usize;

                if is_float {
                    let slice = std::slice::from_raw_parts(data_ptr as *const f32, total_samples);
                    let mono: Vec<f32> = if device_channels == 1 {
                        slice.to_vec()
                    } else {
                        slice.chunks(device_channels as usize)
                            .map(|ch| ch.iter().sum::<f32>() / device_channels as f32)
                            .collect()
                    };
                    if is_silent {
                        buffer.extend(std::iter::repeat(0.0f32).take(mono_samples));
                    } else {
                        resample_into(&mut buffer, &mono, device_sr, 16000);
                    }
                } else if is_pcm {
                    let slice = std::slice::from_raw_parts(data_ptr as *const i16, total_samples);
                    if device_channels == 1 {
                        let mono: Vec<f32> = slice.iter().map(|&s| s as f32 / 32768.0).collect();
                        if is_silent {
                            buffer.extend(std::iter::repeat(0.0f32).take(mono_samples));
                        } else {
                            resample_into(&mut buffer, &mono, device_sr, 16000);
                        }
                    } else {
                        let mono: Vec<f32> = slice.chunks(device_channels as usize)
                            .map(|ch| ch.iter().map(|&s| s as f32).sum::<f32>() / (device_channels as f32 * 32768.0))
                            .collect();
                        if is_silent {
                            buffer.extend(std::iter::repeat(0.0f32).take(mono_samples));
                        } else {
                            resample_into(&mut buffer, &mono, device_sr, 16000);
                        }
                    }
                }
                // Other formats (24-bit, etc.) — skip for MVP
            }

            let _ = capture.ReleaseBuffer(frames);
        }

        // Emit 50ms chunks
        while buffer.len() >= chunk_samples_50ms {
            let chunk_samples: Vec<f32> = buffer.drain(..chunk_samples_50ms).collect();
            let chunk = AudioChunk {
                sequence_id: seq,
                sample_rate: 16000,
                samples: chunk_samples,
                started_at_ms: seq * 50,
                duration_ms: 50,
            };
            seq += 1;

            if tx.send(chunk).is_err() {
                // Receiver dropped — stop
                break;
            }
        }
    }

    // Flush remaining samples
    if !buffer.is_empty() {
        let chunk = AudioChunk {
            sequence_id: seq,
            sample_rate: 16000,
            samples: buffer,
            started_at_ms: seq * 50,
            duration_ms: 50,
        };
        let _ = tx.send(chunk);
    }

    // Stop
    let _ = client.Stop();
    let _ = CloseHandle(event);

    Ok(())
}

/// Simple linear resample from `src_sr` to 16000 Hz, appends to `out`.
fn resample_into(out: &mut Vec<f32>, src: &[f32], src_sr: u32, dst_sr: u32) {
    if src_sr == dst_sr {
        out.extend_from_slice(src);
        return;
    }
    let ratio = dst_sr as f64 / src_sr as f64;
    let out_len = (src.len() as f64 * ratio).ceil() as usize;
    for i in 0..out_len {
        let src_idx = (i as f64 / ratio).min((src.len() - 1) as f64);
        let lo = src_idx.floor() as usize;
        let hi = (lo + 1).min(src.len() - 1);
        let frac = src_idx - lo as f64;
        let s = src[lo] * (1.0 - frac as f32) + src[hi] * frac as f32;
        out.push(s);
    }
}
