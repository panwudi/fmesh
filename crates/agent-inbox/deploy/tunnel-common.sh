#!/bin/sh
# Shared helpers for owner-managed, outbound-only SSH tunnels.

inbox_fail() { printf 'agent-inbox tunnels: %s\n' "$1" >&2; exit 1; }
inbox_require() { [ -n "$1" ] || inbox_fail "missing $2 in configuration"; }
inbox_number() {
    case "$1" in ''|*[!0-9]*) inbox_fail "$2 must be a positive integer" ;; esac
    [ "$1" -gt 0 ] && [ "$1" -le 65535 ] || inbox_fail "$2 is out of range"
}
inbox_port() { inbox_number "$1" "$2"; }

inbox_load() {
    inbox_require "$1" config
    case "$1" in /*) ;; *) inbox_fail "configuration path must be absolute" ;; esac
    [ -r "$1" ] || inbox_fail "cannot read configuration"
    # This is a trusted owner-created shell assignment file, not network input.
    set -a
    . "$1"
    inbox_require "${INBOX_SSH_USER:-}" INBOX_SSH_USER
    inbox_require "${INBOX_SSH_KEY:-}" INBOX_SSH_KEY
    inbox_require "${INBOX_KNOWN_HOSTS:-}" INBOX_KNOWN_HOSTS
    inbox_require "${INBOX_HOST_KEY_ALIAS:-}" INBOX_HOST_KEY_ALIAS
    [ -r "$INBOX_SSH_KEY" ] || inbox_fail "SSH identity file is not readable"
    [ -r "$INBOX_KNOWN_HOSTS" ] || inbox_fail "known-hosts file is not readable"
    : "${INBOX_SSH_BIN:=ssh}"
    : "${INBOX_NC_BIN:=nc}"
    : "${INBOX_BIN:=agent-inbox}"
    : "${INBOX_OUTER_CONNECT_TIMEOUT:=45}"
    : "${INBOX_DIAL_TIMEOUT:=45}"
    : "${INBOX_ALIVE_INTERVAL:=15}"
    : "${INBOX_ALIVE_COUNT:=3}"
    inbox_number "$INBOX_OUTER_CONNECT_TIMEOUT" INBOX_OUTER_CONNECT_TIMEOUT
    inbox_number "$INBOX_DIAL_TIMEOUT" INBOX_DIAL_TIMEOUT
    inbox_number "$INBOX_ALIVE_INTERVAL" INBOX_ALIVE_INTERVAL
    inbox_number "$INBOX_ALIVE_COUNT" INBOX_ALIVE_COUNT
    set +a
}
