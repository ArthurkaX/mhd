@echo off
REM Codex via mhd-llm-proxy.
REM
REM ChatGPT OAuth is forwarded by mHD to the official Codex backend.
REM mHD must be running on 127.0.0.1:3456.
REM
REM Usage: codex-mhd [args...]

setlocal
set "MHD_CODEX_BASE_URL=http://127.0.0.1:3456/v1"

REM Keep the override per-process; the user's global Codex config is untouched.
call codex -c "openai_base_url=%MHD_CODEX_BASE_URL%" -c "features.apps=false" %*
