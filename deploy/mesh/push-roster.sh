#!/bin/sh
# Sign ~/.config/flytomesh/roster.body and push it to every relay listed (one ssh destination per
# line) in ~/.config/flytomesh/relays. Relays re-read it within 5 seconds; nodes fetch it from
# them on their next HELLO, within 25 seconds, and restart. Hosts and addresses stay out of the
# repo: they live in the admin's ssh config and in that file.
# Bump the `version` line in roster.body first: relays and nodes ignore a roster that is not newer.
set -e
A="$HOME/.config/flytomesh"
MESHD="$(dirname "$0")/../../target/mesh/meshd"
"$MESHD" roster-sign --admin-key "$A/admin.key" < "$A/roster.body" > "$A/roster.new"
"$MESHD" roster-check --admin-pub "$(cat "$A/admin.pub")" < "$A/roster.new"
mv "$A/roster.new" "$A/roster"
grep -v '^#' "$A/relays" | while read -r h; do
    [ -n "$h" ] || continue
    scp -q "$A/roster" "$h:/etc/flytomesh/roster.tmp"
    ssh -n "$h" 'chmod 644 /etc/flytomesh/roster.tmp && mv /etc/flytomesh/roster.tmp /etc/flytomesh/roster'
    echo "pushed to $h"
done
