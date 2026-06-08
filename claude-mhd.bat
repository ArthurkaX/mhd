@echo off
REM Claude Code via mhd-llm-proxy.
REM
REM Have an Anthropic subscription? Use as-is — your OAuth is forwarded.
REM Don't have one? Uncomment the line below so the SDK skips auth.
REM
REM Place this file in a folder that is on your PATH, or run it directly.
REM Usage: claude-mhd [args...]

setlocal
set "ANTHROPIC_BASE_URL=http://127.0.0.1:3456"

set "ANTHROPIC_AUTH_TOKEN="
REM set "ANTHROPIC_AUTH_TOKEN=unused"

call claude %*
