#!/bin/sh
# Re-publish the binaries of an earlier release under a new, higher release number, so every
# node moves back to them on its own (nodes only ever take a higher version). Use it when a
# release misbehaves but still reaches the relays (a release that cannot reach a relay is
# rolled back on the node by itself).
#
#   deploy/rollback-release.sh ~/.config/flytomesh/release.out.v7            # publish
#   deploy/rollback-release.sh ~/.config/flytomesh/release.out.v7 --dry-run  # sign and check only
#
# The directory is a saved copy of release.out from deploy/push-release.sh (binaries named by
# SHA-256, plus manifest). Keep one per release: push-release.sh replaces release.out.
set -e
cd "$(dirname "$0")/../.."
SRC=${1:?usage: rollback-release.sh <saved release.out dir> [--dry-run]}
A="$HOME/.config/flytomesh"
V=$(( $(cat "$A/release.version") + 1 ))
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT
M=target/mesh/meshd
ADMIN_PUB=$(cat "$A/admin.pub")

# The saved manifest must verify, and every binary it lists must be there with that hash.
$M release-check --admin-pub "$ADMIN_PUB" < "$SRC/manifest" >/dev/null
printf 'flytomesh-release 1\nversion %s\n' "$V" > "$OUT/body"
grep '^meshd ' "$SRC/manifest" | while read -r _ platform hash size; do
    [ "$(shasum -a 256 "$SRC/$hash" | awk '{print $1}')" = "$hash" ] || { echo "missing or changed binary $hash" >&2; exit 1; }
    cp "$SRC/$hash" "$OUT/$hash"
    printf 'meshd %s %s %s\n' "$platform" "$hash" "$size" >> "$OUT/body"
done
$M release-sign --admin-key "$A/admin.key" < "$OUT/body" > "$OUT/manifest"
rm "$OUT/body"
$M release-check --admin-pub "$ADMIN_PUB" < "$OUT/manifest"

if [ "$2" = "--dry-run" ]; then
    echo "dry run: release v$V with the binaries of $SRC signed and verified, not published"
    exit 0
fi
grep -v '^#' "$A/relays" | while read -r h; do
    [ -n "$h" ] || continue
    ssh -n "$h" 'mkdir -p /etc/flytomesh/release.new && rm -f /etc/flytomesh/release.new/*'
    scp -q "$OUT"/* "$h:/etc/flytomesh/release.new/"
    ssh -n "$h" 'cd /etc/flytomesh && mkdir -p release && find release.new -type f ! -name manifest -exec mv {} release/ \; && mv release.new/manifest release/manifest.tmp && mv release/manifest.tmp release/manifest && rmdir release.new && cd release && for f in *; do [ "$f" = manifest ] || grep -q "$f" manifest || rm -f "$f"; done'
    echo "release v$V (binaries of $SRC) pushed to $h"
done
echo "$V" > "$A/release.version"
