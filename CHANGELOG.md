# Changelog

All notable changes to this project will be documented in this file.

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
