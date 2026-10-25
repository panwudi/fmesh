# Service discovery and shared nodes

## Implementation boundary and member entry rule

This is partly a design, not an implemented team member authorization system.
The current code authenticates hosts and agent sender credentials. It does not
bind those senders to independently verified team members or enforce
per-person capabilities at an isolated tool gateway.

Shared server agents are to be contacted through a member's own agent,
never through a direct member chat entry in the device viewer. A local
`shared_hosts` display label is not an access-control policy. A sender name,
agent description or owner label must not establish member authority.
Enforcement requires person-bound identities and a server-side policy, with
keys and privileged tools inaccessible to the shared worker account. Existing
direct Feishu routes require an explicit sender-to-personal-agent mapping
before migration; do not silently infer that mapping or reclassify the bot as
a member's personal agent.

Do not advertise enforced member sharing until those boundaries are implemented
and negative tests prove that unauthorized senders, direct entry paths,
revoked identities and shared-UID credential access cannot bypass them.

## Design goals

This design separates three decisions: where an inbox service is listening,
who sent a request, and what that sender may ask a receiving agent to do.
FlytoMesh supplies the first fact. Agent-inbox authenticates the second and
enforces the third. None of these facts may be inferred from a message body,
tmux window title, host name or an open TCP port.

## Service discovery

After its mesh TCP listener has bound successfully, agent-inbox writes the
`agent-inbox` file under `/var/run/flytomesh.services/`. The one-line record
is `port <actual TCP port> ident <device-key fingerprint>`. Agent-inbox
refreshes it every 25 seconds while the listener is healthy and removes it
on a clean stop. FlytoMesh adds the node ID, mesh IP and expiry to its local
status view; those fields are not supplied by the registering process.
FlytoMesh expires it when renewals stop and distributes the service state with
its existing peer-presence information. Other inbox instances read the local
FlytoMesh service view; they do not scan other nodes' TCP ports. A service
record is only a discovery hint. A peer still must pass inbox membership and
message authentication before it is trusted.

Try TCP 8832 first. If that address and port are occupied, bind a free TCP
port on the same mesh address and register the actual port. A port change
must replace the service record before peers are told the old endpoint is
healthy. Without FlytoMesh, a loopback-only relay continues to route among
local agents and must not claim cross-host reachability.

The FlytoMesh contract is in `docs/network.md` section 5. A status row is
`service <node> <mesh IP> id <N> name agent-inbox port <N> ident <x> expires <N>s`.
Only the local row includes `uid <N>`; the relay checks file ownership when it
registers, then checks that its own record is still intact on each loop. The
remote row is accepted as an endpoint only after its node ID, IP and key match
the signed FlytoMesh roster and the target is in signed inbox membership.
On a shared Unix account, another process under the same UID can tamper with
this local record. That affects discovery availability, not peer trust; the
record must never grant ownership or message authority.

## Device ownership and local sessions

Each inbox host has a long-lived device identity. A signed ownership record
binds its device public key and FlytoMesh node ID to a person identifier.
The first owner device is established through an explicit trusted bootstrap.
A later owner device is paired once from an already trusted owner device;
the receiver verifies the pairing record before recognizing it as another
device of the same person. Mesh membership alone never establishes ownership.
The bootstrap signing identity is a trust root. Its custody and recovery must
be reviewed with the same care as the mesh membership authority key; an
unprotected key in a shared UID or one-admin-domain fleet would let that
domain forge ownership records.

Agents started by the same verified OS user on a registered single-owner
personal computer may coordinate without another owner prompt. The local relay attaches the
verified session and user identity; forwarding external text does not change
that text's original sender. Different OS users on one host remain distinct.
If several people share one Unix account, their tmux windows cannot be
securely distinguished by window names or file-backed tokens under that
account. Such sessions cannot receive automatic owner privileges. In
`shared-node` mode, same UID is never sufficient evidence of the same person.

## Shared-node mode

An inbox service may be configured as `shared-node`. It accepts signed tasks
from enrolled people according to a revisioned local policy. The owner is a
service administrator, but other enrolled people may use the shared server's
ordinary network and project capabilities without per-task owner approval.
The policy maps person IDs to allowed tool capabilities, project workspaces,
quotas and administrative actions. Unknown identities are denied.

The owner manages this policy through the local agent-inbox GUI on an owner
device. The GUI sends an authenticated revisioned update to the shared node.
The server stores the effective policy and an audit record; the GUI shows a
visible result and can preview whether a sample task would be allowed.
Policy checks must occur at the tool gateway, outside the language model.
A shared agent with an unrestricted shell under a broad VPN account cannot
provide meaningful per-person file or command isolation merely by checking
message signatures. The gateway is an enforcement boundary only if workers
cannot invoke the same tools directly or read the tool credentials outside
it. Exclusive access to tools and credentials requires OS-enforced isolation;
a gateway beside an unrestricted same-UID shell is advisory.

If a server runs several people's sessions under one Unix account, processes
under that UID can read each other's mode-600 token,
key, policy, audit and state files, access its local sockets and interfere
with other processes. Policy and audit data stored under that UID are not
tamper-proof against those processes.
Application signatures and grants cannot stop that same-UID bypass. On such a
server the entire account is one security principal, so per-person gates
would be advisory only. Do not enable enforced person/project/tool separation
there until agent workers and credentials are isolated with separate UIDs or
equivalent OS-enforced sandboxes and a privileged broker. A VPN-capable shared
worker may provide network egress, but must not inherit unrestricted access
to the private workspaces of every requester.
The broker must own each principal's signing key and sender token under a
different UID from the shared workers; those secrets cannot remain readable
in the shared account's files. It must authenticate callers through their
separate OS identities or another credential that a shared-UID process cannot
copy. A broker in front of secrets still readable by the shared account is
not an isolation boundary.

Where only one Linux account has internet access, that account can host a
shared internet-capable agent. Requests arrive from agents associated with
separate people and retain their signed origin. Per-person file or command
work must run under separate OS identities or isolated workers, with network
egress brokered by the VPN account. Direct tmux sessions opened by different
people under one shared VPN Unix account cannot be assigned reliable human
identities without another isolated authentication boundary.

## Time-limited delegation

The owner may authorize a named colleague to direct specified owner agents
for a bounded time and scope. The owner signs a grant containing the delegate
identity, target agents, allowed capabilities, start and expiry times,
grant ID and revocation version. The delegate signs each task with its own
identity and attaches the grant; it never receives the owner's signing key.
The receiver checks both signatures, target, scope, time and replay state.
An expired or revoked grant fails closed. A grant cannot expand local OS or
tool permissions and cannot be redelegated by default.

A trusted local agent may translate a clear owner instruction into a typed
grant creation request. Agent-inbox enforces the typed record; it does not
grant authority by interpreting arbitrary natural-language messages. The
owner sees a receipt when the grant is saved. Ambiguous scope requires one
clarification before activation, not a second approval for every later task.

## Current implementation boundary

The repository currently has local relay/session transport and host-level
dynamic mesh authentication with FlytoMesh service discovery.
It does not yet have signed person-device ownership, signed delegation,
shared-node policy storage, tool-gateway enforcement or the corresponding
administration UI. Until these are implemented and verified, discovery must
not be mistaken for ownership, and colleagues' or shared-account sessions
must not be auto-trusted as the owner.
