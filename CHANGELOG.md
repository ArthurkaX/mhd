# Changelog

All notable changes to this project will be documented in this file.

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
