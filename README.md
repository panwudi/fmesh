# fMesh

A small, self-hosted mesh for people and their AI agents. Install only the parts you need:

| Module | What it does | Crates |
|---|---|---|
| Agent Mesh | Push coordination messages into running Claude Code, Codex and Grok sessions, on one machine or across hosts | `agent-inbox` |
| Network Mesh | WireGuard mesh with hole punching, relay fallback and `.mesh` names; a few MB per node | `meshproto`, `meshd`, `mesh-relay` |
| Desktop | Tray summary and native device and agent lists (macOS, Windows) | `meshtray` |

Agent Mesh runs on a single machine with no network module, no root and no GUI. Every desktop view has
a CLI equivalent for headless servers.

fMesh continues two earlier projects: [agent-inbox](https://github.com/panwudi/agent-inbox) (superseded
by fMesh; the `agent-inbox` command stays compatible) and an internal mesh network.

## Status

Early integration. The modules work as they did in their original projects; unification of the CLI,
installers and native lists is in progress. Security today is host level only: see
[AGENTS.md](AGENTS.md#security-wording) and [docs/network.md](docs/network.md#7-security-posture-and-open-items)
before relying on it for shared servers.

## Build

```sh
cargo build --locked --release -p agent-inbox --features dynamic-peers
cargo build --locked --profile mesh -p meshd -p mesh-relay
cargo test --locked --workspace --features agent-inbox/dynamic-peers
```

Agent Mesh usage: [crates/agent-inbox/README.md](crates/agent-inbox/README.md).
Network design and protocol: [docs/network.md](docs/network.md).

## License

[AGPL-3.0-only](LICENSE). Releases of agent-inbox published before fMesh remain under MIT.
