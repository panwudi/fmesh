# fMesh

A small, self-hosted mesh for people and their AI agents. Install only the parts you need:

| Module | What it does | Crates |
|---|---|---|
| Agent Mesh | Push coordination messages into running Claude Code, Codex and Grok sessions, on one machine or across hosts | `agent-inbox` |
| Network Mesh | WireGuard mesh with hole punching, relay fallback and `.mesh` names; a few MB per node | `meshproto`, `meshd`, `mesh-relay` |
| Desktop | Tray summary and a native device and agent list (macOS, Windows) | `meshtray`, `fmesh-desktop` |
| CLI | `fmesh status / devices / agents / install` for every machine, desktop or headless | `fmesh`, `fmesh-core` |

Agent Mesh runs on a single machine with no network module, no root and no GUI. Every desktop view has
a CLI equivalent for headless servers.

fMesh continues two earlier projects: [agent-inbox](https://github.com/panwudi/agent-inbox) (superseded
by fMesh; the `agent-inbox` command stays compatible) and an internal mesh network.

## Install

Download the package for your platform (or build it with `deploy/package.sh`), unpack it and run:

```sh
./fmesh install agent                 # Agent Mesh for this user: no root, no network module
sudo ./fmesh install network --name laptop9   # Network Mesh node: key generated here, prints a join request
sudo ./fmesh install desktop          # macOS tray and device list
```

Add `--dry-run` to see the plan first. Existing configuration is never overwritten. On Windows use
`install.ps1 -Module network|desktop` from an elevated PowerShell.

Then `fmesh status`, `fmesh devices`, `fmesh agents` (add `--json` for scripts) on any machine, or
the tray's device list on a desktop.

| | macOS | Linux | Windows |
|---|---|---|---|
| Agent Mesh | yes | yes | not yet (local control uses Unix sockets) |
| Network Mesh | yes | yes | yes |
| Desktop (tray, list) | yes | CLI only | yes |
| `fmesh` CLI | yes | yes | yes |

Verified end to end so far: `fmesh install agent` (an automated test installs into an empty HOME and
delivers a message between two sessions), the device list and CLI on macOS and Windows, and the network modules in production. The
service setup of `install network` / `install desktop`, the Linux user service and `install.ps1`
have only had dry runs; report what you find.

## Status

Early integration of two working projects. Security today is host level only: read
[docs/security/README.md](docs/security/README.md) before using it on a shared server.

## Build

```sh
cargo test --locked --workspace --features agent-inbox/dynamic-peers
deploy/package.sh macos-aarch64       # or linux-x86_64, linux-aarch64, windows-x86_64
```

Agent Mesh usage: [crates/agent-inbox/README.md](crates/agent-inbox/README.md).
Network design and protocol: [docs/network.md](docs/network.md).

## License

[AGPL-3.0-only](LICENSE). Releases of agent-inbox published before fMesh remain under MIT.
