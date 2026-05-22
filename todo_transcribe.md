# mhd Transcribe Module TODO

> Last updated: 2026-05-22 — Phase 1 ✅ Phase 2 ✅

Goal: add a short-lived dictation/transcription module to `mhd`.

The daemon must stay lightweight at idle. Transcription resources are created only when the action is invoked, then released after the session finishes:

- microphone capture stops and drops buffers
- preview overlay is destroyed/hidden
- transcription queue drains
- `sherpa-onnx-ws.exe` sidecar is stopped unless explicitly configured to stay warm
- model/audio temp files are closed and cleaned

## Status Overview

| Phase | Description | Status |
|-------|-------------|--------|
| 1 | Single-shot MVP (config, action, sidecar, WS, clipboard) | ✅ Done |
| 2 | WASAPI microphone capture | ✅ Done |
| 3 | Silence chunking + live transcription pipeline | ❌ Not started |
| 4 | Live preview overlay | ❌ Not started |
| 5 | Model registry / download | ❌ Not started |
| 6 | Polish (paste_on_blur, cancellation, etc.) | ❌ Not started |

## Implemented Modules

| File | Description | Phase |
|------|-------------|-------|
| `src/transcribe/mod.rs` | `TranscribeController`, `parse_transcribe_config()` | 1 |
| `src/transcribe/config.rs` | `TranscribeConfig`, `OutputMode`, validation | 1 |
| `src/transcribe/session.rs` | `SessionState` (Idle→Recording→Done) | 1 |
| `src/transcribe/parakeet.rs` | `Sidecar` (sherpa-onnx-ws start/stop), `WsClient` (RFC 6455), `transcribe_wav_file()`, WAV parser | 1 |
| `src/transcribe/clipboard.rs` | `set_clipboard_text()`, `send_paste()` (Ctrl+V) | 1 |
| `src/transcribe/audio.rs` | WASAPI capture → 16 kHz mono f32 chunks (50 ms) | 2 |

## Integration Points

| Integration | Done |
|-------------|------|
| `Action::Transcribe` in enum + parser + descriptor | ✅ |
| `[transcribe]` config section in `config/raw.rs` + `config/mod.rs` | ✅ |
| Worker dispatch in `core/worker.rs` | ✅ |
| Editor indices in `config/editor.rs` | ✅ |
| New deps: `sha1`, `base64` (WS handshake) | ✅ |

## Target UX

- Hotkey starts a transcribe session.
- A small topmost preview overlay appears.
- Audio is segmented by pauses.
- Completed chunks are transcribed on CPU through Parakeet/sherpa-onnx.
- Recognized chunk text appears in the overlay as it becomes available.
- Hotkey stops the session.
- Final text is assembled in chunk order.
- Output mode decides what happens next:
  - `clipboard`: copy final text to clipboard.
  - `paste`: copy final text and immediately send paste.
  - `paste_on_blur`: hide preview, restore original target window, then paste.

## Non-Goals For First Version

- No NVIDIA cloud API.
- No CUDA/GPU dependency.
- No long-running background transcription service at idle.
- No meeting transcription.
- No diarization.
- No semantic cleanup/LLM rewrite.
- No model fine-tuning.

## Architecture

Proposed modules:

- `src/transcribe/mod.rs`
- `src/transcribe/config.rs`
- `src/transcribe/session.rs`
- `src/transcribe/audio.rs`
- `src/transcribe/segmenter.rs`
- `src/transcribe/parakeet.rs`
- `src/transcribe/model_registry.rs`
- `src/transcribe/clipboard.rs`
- `src/overlays/transcribe_preview.rs`

Runtime flow:

```text
Action::Transcribe
  -> TranscribeController::toggle()
  -> TranscribeSession::start()
  -> AudioRecorder starts WASAPI capture
  -> Segmenter emits ordered audio chunks
  -> ParakeetWorker transcribes chunks through sherpa-onnx-ws
  -> Preview overlay receives chunk updates
  -> TranscribeSession::stop()
  -> flush current chunk
  -> wait for pending chunks
  -> assemble final text by sequence_id
  -> output clipboard/paste/paste_on_blur
  -> shutdown session resources
```

## Config

Add top-level `[transcribe]` section.

Example:

```toml
[transcribe]
enabled = true
model = "parakeet-tdt-0.6b-v3"
models_dir = "%USERPROFILE%\\.cache\\mhd\\parakeet-models"
sherpa_onnx_ws = "%USERPROFILE%\\.cache\\mhd\\bin\\sherpa-onnx-ws-win32-x64.exe"
output = "paste_on_blur"
show_preview = true
keep_sidecar_warm = false
threads = 4
silence_ms = 700
min_chunk_ms = 500
max_chunk_ms = 15000
overlap_ms = 250
speech_rms_threshold = 0.001
join_separator = " "

[[binding]]
trigger = "ctrl+alt+space"
action = "transcribe"
```

Validation:

- `model` must be in the known registry or match a downloaded model directory.
- `output` must be one of `clipboard`, `paste`, `paste_on_blur`.
- `threads` must be at least `1`.
- `silence_ms`, `min_chunk_ms`, `max_chunk_ms` must be positive.
- `max_chunk_ms` must be greater than `min_chunk_ms`.
- `overlap_ms` must be smaller than `min_chunk_ms`.

## Action Integration

- Add `Action::Transcribe`.
- Add parser entry for `action = "transcribe"`.
- Add descriptor in `ALL_ACTIONS`.
- Route execution in `core/worker.rs`.
- Avoid running transcription directly on the existing action worker if it can block other actions for too long.
- Use a dedicated `TranscribeController` or session thread.

Open question:

- Should repeated hotkey toggle start/stop, or should separate `transcribe_start` and `transcribe_stop` actions also exist?

Recommended first version:

- `transcribe` toggles.
- Later add explicit start/stop if push-to-talk needs key-up support.

## Audio Capture

Preferred Windows capture path:

- WASAPI capture through Win32/Core Audio APIs.
- Capture default input endpoint first.
- Later add configured device ID/name.

Output format inside module:

- PCM mono
- 16 kHz
- `f32` samples for sherpa WebSocket protocol
- Optional WAV writer only for debug/temp fallback

Tasks:

- Implement `AudioRecorder`.
- Start capture on session start.
- Convert input sample format to `f32`.
- Downmix stereo/multichannel to mono.
- Resample to 16 kHz if device sample rate differs.
- Emit fixed-size frames to segmenter.
- Stop cleanly and unblock capture thread.
- Handle no microphone / permission / device lost errors.

Rust dependency decision:

- Option A: use `cpal` for faster MVP.
- Option B: use `windows` WASAPI directly for minimal dependency footprint.

Recommendation:

- Use direct WASAPI if keeping `mhd` minimal is more important.
- Use `cpal` only if implementation speed matters more than binary/dependency size.

## Silence Segmenter

Segmenter responsibilities:

- Measure RMS/peak over small windows.
- Detect speech start.
- Detect phrase end after `silence_ms`.
- Enforce `min_chunk_ms`.
- Force-flush at `max_chunk_ms`.
- Add optional `overlap_ms` between adjacent chunks.
- Assign monotonically increasing `sequence_id`.

Chunk type:

```text
AudioChunk {
  sequence_id: u64,
  sample_rate: 16000,
  samples: Vec<f32>,
  started_at_ms: u64,
  duration_ms: u64,
  is_final_flush: bool,
}
```

MVP:

- RMS threshold only.
- No VAD model.

Later:

- Add WebRTC VAD or Silero VAD only if RMS gate is not reliable enough.

## Parakeet / sherpa-onnx Integration

Use `sherpa-onnx-ws.exe` as a short-lived sidecar.

Start args:

```text
--tokens=<model_dir>\\tokens.txt
--encoder=<model_dir>\\encoder.int8.onnx
--decoder=<model_dir>\\decoder.int8.onnx
--joiner=<model_dir>\\joiner.int8.onnx
--port=<free_local_port>
--num-threads=<threads>
```

WebSocket request protocol used by OpenWhispr:

```text
[int32_le sample_rate][int32_le num_audio_bytes][float32 samples...]
```

Tasks:

- Locate `sherpa-onnx-ws.exe`.
- Find free local port.
- Spawn sidecar hidden.
- Wait until stderr contains readiness marker, such as `Listening on:`.
- Warm up with 1 second silence or skip warmup for strict lowest memory lifetime.
- For each chunk, open WS connection and send binary packet.
- Parse result as JSON if possible, otherwise plain text.
- Stop sidecar after session finishes when `keep_sidecar_warm = false`.
- Kill/cleanup sidecar on daemon shutdown.

Important:

- CPU-only path uses INT8 ONNX models.
- Do not require CUDA.
- Do not keep model resident at idle by default.

## Model Registry

Primary source:

```text
https://api.github.com/repos/k2-fsa/sherpa-onnx/releases/tags/asr-models
```

Registry filter:

- asset name starts with `sherpa-onnx-nemo-parakeet`
- asset name ends with `.tar.bz2`
- prefer assets containing `int8`
- for MVP prefer non-streaming models

Known useful models:

- `sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8`
- `sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`
- `sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming`

Tasks:

- Implement `list_available_models`.
- Implement `download_model`.
- Verify expected extracted files:
  - `encoder.int8.onnx`
  - `decoder.int8.onnx`
  - `joiner.int8.onnx`
  - `tokens.txt`
- Store model metadata locally.
- Handle interrupted downloads with `.partial` files.
- Validate archive extraction path to avoid path traversal.

Archive handling:

- Need `.tar.bz2` extraction support.
- Either add Rust crates for tar/bzip2 or shell out to a bundled helper.

Recommendation:

- Add Rust extraction dependencies only if acceptable.
- Otherwise require manual model install for MVP and add downloader later.

## Preview Overlay

Overlay behavior:

- Native Win32/GDI layered window, matching existing mhd style.
- Small topmost preview near active text target or center-bottom.
- Shows current session state:
  - recording
  - processing chunk
  - finalizing
  - copied/pasted
  - error
- Shows finalized chunk text.
- Shows current processing indicator for latest chunk.
- Hides on stop/final output.

Tasks:

- Add `overlays/transcribe_preview.rs`.
- Reuse theme colors.
- Provide thread-safe update channel.
- Keep layout stable with bounded text area.
- Auto-scroll latest text.
- Provide compact mode if text exceeds max height.

## Output / Clipboard / Paste

Clipboard:

- Reuse Win32 clipboard code pattern from `config/editor.rs`.
- Write `CF_UNICODETEXT`.
- Ensure allocation ownership is transferred correctly after `SetClipboardData`.

Paste:

- Use existing `platform::send_keys` or `SendInput` helper for `Ctrl+V`.
- Preserve current clipboard only if configured later.

`paste_on_blur` behavior:

- On session start, record `target_hwnd = GetForegroundWindow()`.
- Show overlay.
- On stop:
  - hide overlay
  - call `SetForegroundWindow(target_hwnd)`
  - wait `30-80 ms`
  - write final text to clipboard
  - send paste

Risks:

- Foreground restrictions may block focus restore in some apps.
- UAC/elevated windows may reject synthetic paste from non-elevated daemon.
- Some apps handle paste slowly; configurable delay may be needed.

## Session State

States:

```text
Idle
Starting
Recording
Stopping
Finalizing
Outputting
Done
Error
```

Rules:

- Only one active transcribe session at a time.
- `toggle` in `Idle` starts a new session.
- `toggle` in `Recording` requests stop.
- `toggle` in `Starting`, `Stopping`, `Finalizing`, `Outputting` is ignored or treated as cancel.
- On error, cleanup all resources and return to `Idle`.

Potential commands:

- `start`
- `stop`
- `cancel`
- `force_cleanup`

## Chunk Queue

MVP:

- Single worker processes chunks sequentially.
- Keeps sidecar interaction simple.

Later:

- Parallel transcription workers are possible but may contend heavily on CPU.
- Preserve final order by `sequence_id`.

Result type:

```text
ChunkResult {
  sequence_id: u64,
  text: String,
  duration_ms: u64,
  elapsed_ms: u64,
}
```

Final assembly:

- Sort by `sequence_id`.
- Drop empty chunks.
- Trim each chunk.
- Join with configured `join_separator`.

Later:

- Add overlap-aware deduplication.
- Add punctuation/spacing cleanup.

## Resource Lifetime

Default policy:

- Keep nothing loaded at idle.
- Spawn sidecar on session start.
- Stop sidecar after final output.

Optional policy:

```toml
keep_sidecar_warm = true
warm_idle_timeout_seconds = 300
```

If added later:

- Sidecar can remain warm for a short timeout.
- Idle timeout should stop process and free model memory.
- Must still stop on daemon shutdown.

Cleanup checklist:

- Stop audio capture.
- Join capture thread.
- Flush segmenter.
- Drain or cancel transcription queue.
- Close all WebSocket connections.
- Stop sidecar.
- Drop audio buffers.
- Remove temp files.
- Hide/destroy preview overlay.
- Reset session state to `Idle`.

## Error Handling

User-visible errors:

- microphone unavailable
- audio capture failed
- model not installed
- `sherpa-onnx-ws.exe` not found
- sidecar failed to start
- transcription timed out
- clipboard write failed
- paste failed

Timeouts:

- sidecar startup: 60 seconds
- per-chunk transcription: 300 seconds
- finalization: configurable or no hard limit for MVP

Logging:

- Keep normal daemon quiet unless `quiet = false`.
- Print concise errors to stderr.
- Avoid logging raw transcript by default.

## Security / Privacy

- Audio never leaves the machine.
- Download only models from explicit registry URLs.
- Validate archive paths before extraction.
- Do not log audio content or transcript unless debug flag is explicitly enabled.
- Clean temp audio files.

## Dependencies To Evaluate

Audio:

- Direct `windows` WASAPI
- `cpal`

WebSocket client:

- `tungstenite`
- `tokio-tungstenite` only if async runtime is introduced

HTTP/model listing/download:

- `ureq` for simple blocking HTTP
- `reqwest` only if async/runtime is accepted

Archive extraction:

- `tar`
- `bzip2`

Resampling:

- simple linear resampler for MVP
- `rubato` if quality/robustness is needed

## Implementation Phases

### Phase 1: Single-shot MVP ✅

- [x] Add `Action::Transcribe`.
- [x] Add config parsing.
- [x] Implement manual model path config.
- [x] Implement `sherpa-onnx-ws` sidecar start/stop.
- [x] Implement WebSocket transcription from a WAV/test file.
- [x] Implement clipboard output.
- [ ] Verify CPU-only Parakeet inference (needs model + sidecar downloaded).

### Phase 2: Microphone Recording ✅

- [x] Implement WASAPI/recording.
- [ ] Record until second hotkey press (needs session wiring in controller).
- [x] Convert to Parakeet input (16 kHz mono f32 chunks).
- [ ] Transcribe whole recording (needs Phase 3 pipeline).
- [x] Copy/paste output helpers.

### Phase 3: Silence Chunking ❌

- [ ] Add RMS segmenter (`src/transcribe/segmenter.rs`).
- [ ] Emit chunks on pauses (silence_ms config).
- [ ] Transcribe chunks while recording continues.
- [ ] Assemble final transcript in order.

### Phase 4: Live Preview Overlay ❌

- [ ] Add preview overlay (`src/overlays/transcribe_preview.rs`).
- [ ] Show chunk text as each result completes.
- [ ] Show recording/processing/finalizing states.
- [ ] Hide on final output.

### Phase 5: Model Registry / Download ❌

- [ ] List Parakeet models from GitHub release assets.
- [ ] Download selected `.tar.bz2`.
- [ ] Extract and validate required files.
- [ ] Store in mhd cache directory.

### Phase 6: Polish ❌

- [ ] `paste_on_blur` — hide overlay, restore target, paste.
- [ ] Target window restore.
- [ ] Configurable paste delay.
- [ ] Cancellation (force_cleanup).
- [ ] Debug diagnostics.
- Add cancellation.
- Add better no-audio detection.
- Add debug diagnostics.

## Testing

Unit tests:

- config validation
- model asset filter
- chunk segmentation by synthetic RMS patterns
- transcript assembly order
- archive path validation

Integration tests:

- transcribe known WAV file with installed model
- missing model error
- missing sidecar error
- sidecar startup timeout
- clipboard write smoke test on Windows

Manual tests:

- Notepad paste.
- Browser text field paste.
- Elevated/non-elevated app behavior.
- Long dictation with pauses.
- Silence/no-audio session.
- Repeated start/stop sessions and memory observation.
- Daemon idle memory before and after transcription.

## Open Decisions

- Use direct WASAPI or add `cpal`.
- Bundle `sherpa-onnx-ws.exe` or download it separately.
- Manual model install first or model downloader first.
- Toggle-only action or separate start/stop actions.
- Keep sidecar strictly short-lived or allow optional warm timeout.
- Whether to preserve previous clipboard contents after paste.
