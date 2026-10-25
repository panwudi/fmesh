# Security status

What is enforced today, what is only designed, and what must not be claimed. Read this before
deploying fMesh on a server that several people share.

## Enforced

| Area | Mechanism |
|---|---|
| Network membership | Ed25519-signed roster; nodes and relays accept only a higher signed version |
| Network transport | WireGuard between nodes; authenticated, encrypted control plane; decrypted source IP bound to the roster (v7) |
| Relay messages | HELLO replay window; sealed and fresh NO_ROUTE (v9); per-node byte budget on roster and release replies (v9) |
| Updates | Signed release manifest, hashed binaries, atomic replace, automatic rollback after three failed starts |
| Agent hosts | Signed host membership (dynamic peers), per-request MAC with replay cache, mesh interface and source checks |
| Local sessions | Same-user Unix control socket with UID check; per-session tokens in mode 0600 files; read-only directory |

## Not implemented

These are designs (see `crates/agent-inbox/docs/SHARED-NODE.md`, `DYNAMIC-PEERS.md`,
`OWNER-SIGNING.md`), not shipped behavior:

- Binding a sender to an enrolled team member (member identity, member devices, personal agents).
- A server-side access broker for shared agents, checked on receipt and before each tool call.
- An isolated tool gateway and OS-level isolation between members on a shared server.
- Member-scoped directory visibility, grants, delegation, revocation and audit.
- Owner-signed instructions (tier 2).

Consequences:

- Host membership is not person ownership. A host signature does not say which member or personal
  agent originated a task.
- If several people's sessions run under one OS account, that account is one security principal:
  its processes can read each other's tokens and state. File modes inside it isolate nobody.
- Owner labels, `shared_hosts`, service announcements, session descriptions and prompts are display
  or discovery data. They are never authorization.
- A shared agent is to be contacted only through a member's own agent. Until the broker exists this
  is a rule for people and agents, not something the software enforces. The UI and CLI say so.

## Network open items

Listed in `docs/network.md` section 7: admin key protection, no access control inside the mesh,
bounded (not removed) relay reflection, replayable DISCO inside its window, no forward secrecy for
static-key control messages, central key generation in older enrollments (fixed by
`fmesh install network`, which generates the key on the node). An external protocol review is
still required.

## Reporting

Report vulnerabilities privately to the maintainers rather than in a public issue.
