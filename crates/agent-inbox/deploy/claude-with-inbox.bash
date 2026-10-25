#!/bin/bash
# Launch from any directory; keep positional prompts out of the variadic flag.
set -euo pipefail
: "${AGENT_INBOX_CLAUDE_BIN:?Set AGENT_INBOX_CLAUDE_BIN to the absolute real Claude executable}"
case "$AGENT_INBOX_CLAUDE_BIN" in
  /*) ;;
  *) printf '%s\n' 'AGENT_INBOX_CLAUDE_BIN must be absolute' >&2; exit 2 ;;
esac
if [[ ! -x "$AGENT_INBOX_CLAUDE_BIN" || "$AGENT_INBOX_CLAUDE_BIN" -ef "$0" ]]; then
  printf '%s\n' 'Choose the real Claude executable, not this wrapper' >&2
  exit 2
fi
args=()
has_channel=0
while (( $# )); do
  case "$1" in
    --) break ;;
    --dangerously-load-development-channels)
      args+=("$1" "server:agent-inbox-join")
      has_channel=1
      ;;
    --dangerously-load-development-channels=*)
      args+=("--dangerously-load-development-channels" "server:agent-inbox-join" "${1#*=}")
      has_channel=1
      ;;
    *) args+=("$1") ;;
  esac
  shift
done
if (( ! has_channel )); then
  args+=("--dangerously-load-development-channels" "server:agent-inbox-join")
fi
exec "$AGENT_INBOX_CLAUDE_BIN" "${args[@]}" "$@"
