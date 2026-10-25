#!/bin/sh
# Build meshd for every platform, sign a release manifest and publish it on every relay listed
# in ~/.config/flytomesh/relays. Nodes see the new version in their next HELLO_ACK (within
# 25 seconds), download their platform's binary, verify it and restart into it.
# The release number lives in ~/.config/flytomesh/release.version and goes up by one per run.
set -e
cd "$(dirname "$0")/../.."
A="$HOME/.config/flytomesh"
V=$(( $(cat "$A/release.version" 2>/dev/null || echo 0) + 1 ))
OUT="$A/release.out"
rm -rf "$OUT" && mkdir -p "$OUT"

cargo build --profile mesh -p meshd >/dev/null 2>&1
cargo zigbuild --profile mesh -p meshd --target x86_64-unknown-linux-musl >/dev/null 2>&1
cargo zigbuild --profile mesh -p meshd --target aarch64-unknown-linux-musl >/dev/null 2>&1
cargo zigbuild --profile mesh -p meshd --target x86_64-pc-windows-gnu >/dev/null 2>&1

BODY="$OUT/body"
printf 'flytomesh-release 1\nversion %s\n' "$V" > "$BODY"
add() { # platform path
    h=$(shasum -a 256 "$2" | awk '{print $1}')
    cp "$2" "$OUT/$h"
    printf 'meshd %s %s %s\n' "$1" "$h" "$(wc -c < "$2" | tr -d ' ')" >> "$BODY"
}
add macos-aarch64 target/mesh/meshd
add linux-x86_64 target/x86_64-unknown-linux-musl/mesh/meshd
add linux-aarch64 target/aarch64-unknown-linux-musl/mesh/meshd
add windows-x86_64 target/x86_64-pc-windows-gnu/mesh/meshd.exe
target/mesh/meshd release-sign --admin-key "$A/admin.key" < "$BODY" > "$OUT/manifest"
rm "$BODY"

grep -v '^#' "$A/relays" | while read -r h; do
    [ -n "$h" ] || continue
    ssh -n "$h" 'mkdir -p /etc/flytomesh/release.new && rm -f /etc/flytomesh/release.new/*'
    scp -q "$OUT"/* "$h:/etc/flytomesh/release.new/"
    # Binaries first, manifest last: a relay never sees a manifest without its binaries.
    ssh -n "$h" 'cd /etc/flytomesh && mkdir -p release && find release.new -type f ! -name manifest -exec mv {} release/ \; && mv release.new/manifest release/manifest.tmp && mv release/manifest.tmp release/manifest && rmdir release.new && cd release && for f in *; do [ "$f" = manifest ] || grep -q "$f" manifest || rm -f "$f"; done'
    echo "release v$V pushed to $h"
done
echo "$V" > "$A/release.version"
