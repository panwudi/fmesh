# Import record

fMesh started on 2026-10-25 from file snapshots of two earlier private repositories. Their history was
not imported: it contains internal deployment material, and removing files from a tree does not clean
history. This file records what came in and what was changed on the way.

| Source | Commit | Imported as |
|---|---|---|
| agent-inbox | `4ea5f93d53f1b27aa95e1e5f1d42f0256cb475d2` | `crates/agent-inbox/` (src, tests, deploy, docs, README) |
| flytoMesh | `166af021d9d19086c2c563aa8e17294459aea32d` | `crates/{meshproto,meshd,mesh-relay,meshtray}/`, `deploy/mesh/` |

Snapshots were taken with `git archive <commit>`. The source repositories were only read.

## Left out

- Both projects' `AGENTS.md`: replaced by the top-level one.
- flytoMesh `docs/design.md`: its deployment constraints, progress log, incident notes and machine
  inventory are internal. The design itself was rewritten in general terms as `docs/network.md`.
- agent-inbox `docs/HUB-BRIEF.md`, `docs/HOST-RELAY-UPGRADE-BRIEF.md`: owner briefs for one deployment.
- agent-inbox `deploy/BUILD-VERIFICATION.md`, `deploy/TUNNEL-ACCOUNT-VERIFICATION.md`: records of
  specific production hosts.
- agent-inbox `scripts/publish-github.sh`: the old public-mirror workflow. fMesh is public itself.

## Changed

- Category only, no original values listed here: internal host names, a domain, an intranet address
  range, real relay addresses in test fixtures and vendor-specific VPN wording in comments became
  documentation examples (`laptop1`, `laptop2`, `server1`, `nas`, `desk`, `hub.example.com`,
  `192.0.2.0/24`, `203.0.113.0/24`). Tests were adjusted with the same mapping.
- The hub SSH example was renamed to `deploy/inbox-hub.ssh.example`.
- `docs/OWNER-SIGNING.md` kept with the same mapping; owner pronouns made neutral; the audit of one
  live bot server became a general signer placement requirement.
- `docs/SHARED-NODE.md` and `docs/DYNAMIC-PEERS.md` kept as design. Deployment status, test logs,
  cutover records and statements about specific servers were removed or turned into conditional
  requirements. The security design will move to `docs/security/`.
- agent-inbox `docs/PEER-MIGRATION.md` left out: a cutover runbook for specific hosts.
- One Cargo workspace. The `release` profile keeps unwinding (agent-inbox serves connections on threads);
  the network binaries build with the new `mesh` profile (`panic = "abort"`, same LTO settings).
  `deploy/mesh/*.sh` now build with `--profile mesh` and run from the repository root.
- License: both sources were relicensed by their owner to AGPL-3.0-only for fMesh. Earlier public
  agent-inbox releases remain under MIT.
- `Cargo.lock`: built from the agent-inbox lock plus flytoMesh's pinned versions. Every package version
  appears in one of the two source locks. One unavoidable change: the Linux tray's gtk build
  dependencies pin `toml_datetime` 0.6.3, so `toml` resolves to 0.8.2 for agent-inbox too (was 0.8.23).

## Baseline at import

- `cargo test --workspace --features agent-inbox/dynamic-peers`: all mesh tests pass (meshd 12,
  meshproto 10, mesh-relay 2, meshtray 8). agent-inbox passes except
  `codex_watch::same_directory_pairs_have_distinct_type_tags_and_explicit_worker_names_are_kept`, which
  also fails on the unmodified source snapshot on the importing machine (condition timeout). Under full
  parallel load two `auto_join` fixtures sometimes time out at startup and pass when run alone, as noted
  by the previous maintainers.
- `cargo tree -p agent-inbox` pulls in no network, tray or WireGuard crate: the Agent module builds alone.
