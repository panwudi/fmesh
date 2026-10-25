#!/bin/sh
# The Rust supervisor shares a kernel dial lock across this host's relay sessions.
set -eu
inbox_script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$inbox_script_dir/tunnel-common.sh"
if [ "${1:-}" = '--help' ]; then
    printf '%s\n' 'agent-inbox-tunnels.sh --config /absolute/path/tunnel.env'
    exit 0
fi
[ "$#" -eq 2 ] && [ "$1" = '--config' ] || inbox_fail "use --config /absolute/path/tunnel.env"
inbox_load "$2"
inbox_require "${INBOX_DIAL_LOCK_FILE:-}" INBOX_DIAL_LOCK_FILE
inbox_require "${INBOX_SSH_CONFIG:-}" INBOX_SSH_CONFIG
[ -r "$INBOX_SSH_CONFIG" ] || inbox_fail "SSH config is not readable"
for inbox_port_field in LOCAL_INBOX_PORT REVERSE_PORT LOCAL_SEND_PORT HUB_PORT; do
    case "$inbox_port_field" in
        LOCAL_INBOX_PORT) inbox_port "${INBOX_LOCAL_INBOX_PORT:-}" INBOX_LOCAL_INBOX_PORT ;;
        REVERSE_PORT) inbox_port "${INBOX_REVERSE_PORT:-}" INBOX_REVERSE_PORT ;;
        LOCAL_SEND_PORT) inbox_port "${INBOX_LOCAL_SEND_PORT:-}" INBOX_LOCAL_SEND_PORT ;;
        HUB_PORT) inbox_port "${INBOX_HUB_PORT:-}" INBOX_HUB_PORT ;;
    esac
done
exec "$INBOX_BIN" tunnel-supervise
