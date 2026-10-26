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

# The development-channel flag only works for claude.ai sign-in. Other auth
# methods (an OAuth token, an API key, a custom ANTHROPIC_BASE_URL gateway or
# local model) do not offer channels, so the flag is ignored and Claude prints
# a confusing "Channels are not currently available" notice. The native inbox
# transport does not depend on this flag, so delivery still works there; we just
# drop the dead flag. AGENT_INBOX_CHANNELS=on|off forces the choice; the default
# "auto" asks the real binary for the current auth method.
channels_supported() {
  local out
  out=$("$AGENT_INBOX_CLAUDE_BIN" auth status 2>/dev/null) || return 1
  printf '%s' "$out" | grep -Eq '"authMethod"[[:space:]]*:[[:space:]]*"claude\.ai"'
}

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
# An explicit flag from the caller always wins. Otherwise add it only when the
# current sign-in can use channels, so local-model and API-key sessions launch
# without the ignored-flag notice while still receiving over the native inbox.
if (( ! has_channel )); then
  case "${AGENT_INBOX_CHANNELS:-auto}" in
    on) args+=("--dangerously-load-development-channels" "server:agent-inbox-join") ;;
    off) ;;
    *) if channels_supported; then
         args+=("--dangerously-load-development-channels" "server:agent-inbox-join")
       fi ;;
  esac
fi
exec "$AGENT_INBOX_CLAUDE_BIN" "${args[@]}" "$@"
