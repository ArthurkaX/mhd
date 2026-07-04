# Changelog

All notable changes to this project will be documented in this file.

## 0.7.5 - 2026-07-04

- **LLM Trim settings page**: trim controls moved out of the LLM Proxy tab into a dedicated page — a searchable model picker, grouped `tool_result_head` budget rows sharing a dropdown, a **Calculate** button backed by live Tune data, and a Free-tier (light-trim) picker in the model-selection overlay.
- **Tune advisor**: a live advisor overlay that runs a stratified sweep over `tool_result_head`, picks the knee, and reports an honest verdict, with a per-bucket breakdown (Native / CcGateway / OtherOpenai).
- **mhd-inspector**: a standalone egui request/trim inspector; clicking a Proxy Trace row opens it.
- **Per-group head budgets**: separate `tool_result_head` for Native-big / Haiku / harness targets.
- **Fixes**:
  - **Source-code reads protected from elision**: the native trim engine no longer elides the middle of source-file reads (`.py`, `.js`, `.rs`, and other code/config extensions). Layer-1 provenance protection previously covered only documentation extensions, so large code reads were compressed; the model then reconstructed an `old_string` that no longer matched the on-disk file, breaking the Edit tool (observed 83% edit failures on a 30 KB file).
  - **Deterministic bench task**: the 3-arm live measure now runs a deterministic file-chain workload that forces identical turn counts across arms, so the trim A/B cost is reproducible (was swinging by more than 180 points between runs).
  - Fixed a `render_done` self-deadlock (reentrant mutex) in the Tune panel.
- **Proxy reliability**:
  - Retry-with-backoff on 429/529, a CANCELLED-on-200 fix, and collapsed error rows.
  - Outbound token-bucket rate limiter (throttle) on the native path; a probe lane for `max_tokens=1` background probes.
- **Other**:
  - The native trim engine is now the only engine; the legacy `llmtrim` fallback is removed.
  - Proxy Trace: mouse-wheel scroll with a thin scrollbar; trim % shown only on gateway targets.

## 0.7.0 - 2026-07-01

- **Native trim engine**: a clean-room request-compression engine replaces `llmtrim` as the live default.
  - Deterministic, zero-extra-LLM-call compression that trims verbose tool outputs, logs, diffs, fat JSON, and tool-description bloat while leaving any `cache_control`-frozen prefix byte-identical, so the prompt cache keeps hitting.
  - Model-agnostic: it cuts by wire-shape (Anthropic vs OpenAI JSON) and content, not by model, so Claude Code and OpenAI-compatible clients share the same engine and tuning.
  - Beats the previous engine on the frozen backtest corpus (Anthropic 32.6% vs 22.7%, OpenAI 33.2% vs 10.5%). The legacy `llmtrim` engine is kept behind the `trim_engine` toggle as a fallback and comparison baseline.
  - **PROTECTED detector** guards real diagrams and structured content by density, not by absolute glyph counts: provenance, then fenced code, then box-glyph density, then arrow density. The arrow route is density-gated (`trim_arrow_density_min`, default 0.01) so a 35 MB log full of stray arrows is no longer falsely protected.
  - **Fence-gate code-gating** (`trim_fence_requires_code`): a code fence only protects content that actually looks like code, so clients that fence their entire tool output no longer waste the budget.
  - **strip_thinking**: strips the thinking blocks of older completed turns on the native Anthropic path (target-gated; never applied upstream, where the reasoning content must round-trip).
  - Live-tunable via `settings.json` with a file watcher (no restart): `trim_engine`, `trim_tool_desc_chars`, `trim_toolresult_head`/`_tail`, `trim_ws_enabled`, `trim_strip_thinking`, `trim_fence_requires_code`, `trim_arrow_density_min`.
- **OpenAI-compatible native path**: the native engine is wired into the live OpenAI surface (Zed, opencode, pi), with the arrow-density gate bringing OpenAI trim to near-parity with Anthropic (+12.6pp).
- **Request-body storage**: request bodies are compressed with zstd (BLOB) and capped by a corpus retention limit (`corpus_max_rows`, default 5000).
- **Proxy Trace**: honest cache-state taxonomy (COLD / EXPIRED / MISS / HIT, prefix-hash aware) and a live 5h/7d quota mini-bar line.
- **Trim Quota Bench** (power users): an in-app A/B measurement panel that runs the same workload three ways (ECO / native-ON / native-OFF) and reports the weighted quota cost, per-arm time, and the realized share of the live 5-hour quota window consumed. **This spends real quota against the running account** — use a throwaway session, not your primary account.

## 0.6.1 - 2026-06-29

- **Anthropic Trim tuning**: applied the combined Trim tuning to Anthropic requests.
- **Cache token accounting**: capture upstream cache-creation and cache-read tokens in the Proxy Trace.
- **Proxy Trace polish**: taskbar minimize support, trim preset cleanup, and model-selection persistence.

## 0.6.0 - 2026-06-28

- **Request Compression (Trim)**: optional, deterministic, zero-extra-LLM-call compression of outgoing requests, powered by `llmtrim-core`.
  - Trims verbose tool outputs, logs, diffs, duplicate lines, and fat JSON while leaving any `cache_control`-frozen prefix byte-identical, so the Anthropic prompt cache keeps hitting.
  - Fail-open: any error, a body below the size threshold, or a result that does not shrink forwards the original request untouched — Trim never breaks a request.
  - A single tray toggle (**Settings -> LLM Proxy -> Request Compression**) governs both Claude Code (`/v1/messages`) and OpenAI-compatible (`/v1/chat/completions`) traffic.
  - Preset defaults to `auto`, which picks the per-request strategy (agent / code / rag / aggressive) from the request shape, adapting to mixed clients.
- **OpenAI-compatible clients** (e.g. Zed): the proxy now trims, streams, and serves a model list on its OpenAI surface.
  - `POST /v1/chat/completions` supports both streaming (SSE forwarded unchanged) and non-streaming responses, each passing through Trim under the same toggle.
  - `GET /v1/models` returns the configured models in OpenAI list format.
  - Requests are accepted with any API key or none; the proxy uses its own configured upstream key.
- **Proxy Trace**: OpenAI-compatible requests now appear in the trace overlay alongside Claude Code, showing the model, token usage, and per-request trim savings. Widened the trace window and the Reason column so savings fit.

## 0.5.0 - 2026-06-20

- **Breathe pacer**: new paced breathing overlay with an expanding/contracting sphere visualisation.
  - Three presets at 6 breaths per minute: `balanced` (10 min, 5-5), `calm` (15 min, 4-6), `extended` (20 min, 4-6).
  - Preset selection flow: click a preset to highlight, click again to start. Changing presets requires reopening.
  - Cosine-based sphere easing for smooth transitions (no endpoint freezing).
  - Continuous color interpolation between phase colours (no abrupt jumps on transition).
  - Pause/resume snaps back to the beginning of inhale.
  - Session progress bar, breath counter, and phase labels in the native overlay.
  - `breathe` action with optional `preset` field, and a tray menu item.
  - Blackbox logging of start/complete/abandon events when the blackbox feature is enabled.
  - No audio cues (silent — removed `Beep` FFI, synthesis, and mute logic).
- **New files**: `mhd-daemon/src/overlays/breathe.rs`.

## 0.4.0 - 2026-06-18

- **KeyCast overlay**: new keystroke visualizer for recordings and streams.
  - Rounded pill carousel shows pressed shortcuts (e.g. `Ctrl+Alt+T`), mouse clicks, and modifier combos.
  - Configurable position (6 presets) and display duration via Settings → General or `[keycast]` in config.
  - `toggle_keycast` action and tray toggle with checkmark state.
  - Toggle the overlay or bind it to any key combination.
- **Typing block** (optional, off by default): single printable characters (letters, digits, space) appear in a fixed-width block with a mini-carousel animation.
  - Characters are resolved via `ToUnicodeEx` against the **foreground window's keyboard layout**, so Russian, English, and other layouts show the correct glyph.
  - Settings: Show typing toggle, width stepper, typing duration stepper.
- **Key Shortcuts on/off**: tray item renamed from "mhd on/off" to clarify it suspends/resumes hotkey processing without stopping the daemon.
- **Settings editor**: scroll area height now correctly matches the expanded General page layout.
- **New files**: `mhd-daemon/src/overlays/keycast.rs`.

## 0.3.0 - 2026-06-14

- **LLM Proxy**: promoted the proxy into a full user-facing workflow for Claude Code.
  - Configure OpenAI-compatible providers, API keys, and models from the native Settings -> LLM Proxy page.
  - Switch `opus`, `sonnet`, and `haiku` routing live through the model selector without restarting Claude Code.
  - Inspect recent proxy routing in the Proxy Trace overlay, including token counts and downgrade decisions.
  - Store proxy settings in dedicated JSON files under `%USERPROFILE%\.config\mhd\llm-proxy\`.
- **Documentation**: added concise LLM Proxy setup docs, user-facing routing diagrams, and clarified the optional local-only `blackbox` developer build.
- **Release cleanup**: removed generated PDF/test artifacts from the tracked tree and ignored future PDF outputs.

## 0.2.0 - 2026-06-08

- **LLM Proxy**: new built-in proxy server that intercepts Claude Code API calls and routes them to an OpenAI-compatible gateway.
  - Per-tier model remapping (`opus`, `sonnet`, `haiku`) — each can be `"native"` (real Anthropic) or a gateway model.
  - Runtime model selector overlay (`show_llm_models`) — switch proxy models on the fly without restart.
  - Toggle proxy on/off at runtime (`toggle_llm_proxy`).
  - Additional models registered via `[[llm_proxy.model]]` in config.
  - `claude-mhd.bat` launcher with instructions for users with/without an Anthropic subscription.
- **Settings editor**: LLM Proxy actions now display correctly in the shortcuts editor (were showing as "Quit mhd" due to missing editor action names).
- **Blackbox**: fixed SQLite nested-transaction spam and `ensure_app_category` "0 rows changed" errors in console output.
- **New files**: `llm-proxy/` crate, `mhd-daemon/src/core/llm_proxy.rs`, `mhd-daemon/src/overlays/llm_models.rs`.

## 0.1.1 - 2026-06-08

- Public Windows x64 release package.
- Settings, diagnostics, and utility overlay documentation updates.
- Privacy handling improvements for optional developer diagnostics.

## 0.1.0 - 2026-05-31

- Initial public release preparation.
- Portable Windows x64 release package.
- Public-release documentation and repository cleanup.
- Optional developer-only `blackbox` build mode kept out of the distributed release.
- Pomodoro, tray, autostart, themes, and README polish.
