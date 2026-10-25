#!/bin/sh
# Select from local addresses/gateway only; nc carries the one real SSH connection.
set -eu
inbox_script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
. "$inbox_script_dir/tunnel-common.sh"
if [ "${1:-}" = '--help' ]; then
    printf '%s\n' 'hub-ssh-proxy.sh --config /absolute/path/tunnel.env'
    exit 0
fi
[ "$#" -eq 2 ] && [ "$1" = '--config' ] || inbox_fail "use --config /absolute/path/tunnel.env"
inbox_load "$2"
inbox_route=$("$INBOX_BIN" tunnel-route) || exit 1
inbox_route_host=${inbox_route% *}
inbox_route_port=${inbox_route##* }
exec "$INBOX_NC_BIN" "$inbox_route_host" "$inbox_route_port"
