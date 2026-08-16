@echo off
REM Claude Code via mhd-llm-proxy.
REM
REM Have an Anthropic subscription? Use as-is — your OAuth is forwarded.
REM Don't have one? Uncomment the line below so the SDK skips auth.
REM
REM Usage: claude-mhd [args...]

setlocal
set "ANTHROPIC_BASE_URL=http://127.0.0.1:8317"
set "CLAUDE_CODE_SUBAGENT_MODEL=claude-sonnet-4-6"

REM set "ANTHROPIC_AUTH_TOKEN=unused"

call claude %*
