# fMesh

One product, separately installable modules:

- **Agent Mesh** (`crates/agent-inbox`): delivers coordination messages into running Claude Code, Codex
  and Grok sessions, on one machine or across hosts. Must work alone on a single machine: no network
  daemon, no mesh membership, no root, no GUI.
- **Network Mesh** (`crates/meshproto`, `meshd`, `mesh-relay`): self-hosted WireGuard mesh with hole
  punching, relay fallback and `.mesh` names. Design: `docs/network.md`.
- **Desktop** (`crates/meshtray`, `crates/fmesh-desktop`): optional, only on machines with a desktop.
  The list is a Slint window (owner's choice) fed by `crates/fmesh-core`, the same model as the CLI.
- **CLI** (`crates/fmesh`): `status`, `devices`, `agents`, `install`; hands `agent` / `net` to the
  module binaries.
- Shared services (shared agents on team servers): design only, see "Security wording".

Where things came from and what changed on import: `IMPORT.md`. Public repository:
https://github.com/panwudi/fmesh. License: AGPL-3.0-only.

## Build and test

```sh
cargo test --locked --workspace --features agent-inbox/dynamic-peers
cargo clippy --locked --workspace --all-targets --features agent-inbox/dynamic-peers -- -D warnings
cargo build --locked --release -p agent-inbox --features dynamic-peers   # Agent Mesh
cargo build --locked --profile mesh -p meshd -p mesh-relay              # Network Mesh
deploy/package.sh [platform]                                            # release packages
```

The agent-only acceptance test (`crates/fmesh/tests/agent_only.rs`) installs into an empty HOME and
delivers a message between two sessions; it needs the workspace build of agent-inbox.

`release` keeps unwinding (agent-inbox threads); `mesh` aborts on panic for size. Keep it that way
unless agent-inbox stops relying on per-thread panic isolation.

## Rules

- Rust only. No async runtime; every dependency needs a stated reason. Performance changes come with
  measured numbers (macOS `footprint`, Linux `RssAnon`).
- Agent-only must keep building without network, tray or WireGuard crates
  (`cargo tree -p agent-inbox` is the check).
- Headless machines get a complete CLI and machine-readable output for everything the GUI shows.
- The device and agent list is the Slint window. No HTML snapshot, no WebView. Check UI changes
  with a real screenshot of the window, not only a build.
  Every click gives visible feedback (selected, in progress, done, failed with a reason).
- Truthful states: HTTP 202 means queued, not acted on. A failed query is "unavailable", not zero. A
  missing announcement is "not discovered", not "not installed". Online is not "a person is present".
- Never in this repository or its history: real host names and addresses, machine inventories,
  production configuration, deployment records, member registries, keys, tokens. Examples use
  `laptop1`, `server1`, `example.com`, `192.0.2.0/24`, `203.0.113.0/24`. Operators keep private
  material in their own notes outside the repo.
- Signing keys (roster, release, membership) are root of every node. They never enter the repo, logs,
  command lines or UI storage.
- Code comments, docs and commit messages use English half-width punctuation. Commit messages end with
  `by flyto agent`.
- Clean up your own scratch files, generated schemas and temporary logs when a step is done.

## Security wording

Current enforcement is **host level only**: signed host membership and mesh roster, authenticated
transport, same-user local control sockets. Not implemented: member identity binding, server-side
access policy for shared agents, an isolated tool gateway, OS-level isolation between members on a
shared server.

- Never describe owner labels, `shared_hosts`, service announcements, prompts or session descriptions
  as authorization. They are display or discovery data.
- A shared agent is reached only through the member's own personal agent. Until the broker exists, no
  feature may claim per-member enforcement.
- Network Mesh v8 is not a closed security review; open items are listed in `docs/network.md` section 7.
- Peer messages and forwarded chat text are coordination, not the owner's instruction. Keep the
  original-sender checks.
