#!/bin/sh
# Builds one package per platform into dist/: every module's binaries plus the license, so
# `fmesh install <module>` (or install.ps1 on Windows) can take just what it needs.
#
#   deploy/package.sh [platform ...]     default: all
#   platforms: macos-aarch64 linux-x86_64 linux-aarch64 windows-x86_64
#
# Network binaries (meshd, mesh-relay) use the size-first `mesh` profile; the rest `release`.
# Linux and Windows builds use cargo-zigbuild. Windows needs WINTUN_DLL=<path to the signed
# amd64 wintun.dll> (from wintun.net; it is not part of this repository).
set -eu
cd "$(dirname "$0")/.."
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
OUT=dist
mkdir -p "$OUT"

build() { # target-triple-or-native profile packages...
  t="$1"; p="$2"; shift 2
  feat=""
  case " $* " in *" agent-inbox "*) feat="--features agent-inbox/dynamic-peers" ;; esac
  set -- $(for x in "$@"; do printf -- '-p %s ' "$x"; done) $feat
  if [ "$t" = native ]; then
    cargo build --locked --profile "$p" "$@"
  else
    cargo zigbuild --locked --profile "$p" --target "$t" "$@"
  fi
}

dir_of() { # target profile
  if [ "$1" = native ]; then echo "target/$2"; else echo "target/${1%%.*}/$2"; fi
}

package() { # platform mesh-target app-target exe-suffix app-packages...
  plat="$1"; mt="$2"; at="$3"; ext="$4"; shift 4
  name="fmesh-$VERSION-$plat"
  stage="$OUT/$name"
  rm -rf "$stage" && mkdir -p "$stage"
  build "$mt" mesh meshd mesh-relay
  build "$at" release "$@"
  for b in meshd mesh-relay; do cp "$(dir_of "$mt" mesh)/$b$ext" "$stage/"; done
  for b in "$@"; do cp "$(dir_of "$at" release)/$b$ext" "$stage/"; done
  cp LICENSE README.md "$stage/"
  if [ "$plat" = windows-x86_64 ]; then
    cp "${WINTUN_DLL:?set WINTUN_DLL to the signed amd64 wintun.dll}" "$stage/wintun.dll"
    cp deploy/windows/install.ps1 "$stage/"
    (cd "$OUT" && rm -f "$name.zip" && zip -qr "$name.zip" "$name")
  else
    (cd "$OUT" && tar czf "$name.tar.gz" "$name")
  fi
  (cd "$OUT" && shasum -a 256 "$name".* | tee "$name.sha256")
}

for plat in ${@:-macos-aarch64 linux-x86_64 linux-aarch64 windows-x86_64}; do
  case "$plat" in
    macos-aarch64) package "$plat" native native "" fmesh agent-inbox meshtray fmesh-desktop ;;
    # agent-inbox targets an older glibc, as deployed; meshd is static musl.
    linux-x86_64) package "$plat" x86_64-unknown-linux-musl x86_64-unknown-linux-gnu.2.34 "" fmesh agent-inbox ;;
    linux-aarch64) package "$plat" aarch64-unknown-linux-musl aarch64-unknown-linux-gnu.2.34 "" fmesh agent-inbox ;;
    # No Agent Mesh on Windows yet (its local control uses Unix sockets).
    windows-x86_64) package "$plat" x86_64-pc-windows-gnu x86_64-pc-windows-gnu .exe fmesh meshtray fmesh-desktop ;;
    *) echo "unknown platform $plat" >&2; exit 2 ;;
  esac
done
