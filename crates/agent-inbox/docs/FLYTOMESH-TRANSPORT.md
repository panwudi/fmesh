# FlytoMesh peer transport

The host relay can use FlytoMesh for cross-host agent messages while keeping the
same local sender commands and agent names. Same-host messages always use the
local relay. It supports two configurations: an existing hub-backed relay can
use direct mesh sends with hub fallback, while a peer-only relay omits the hub
entirely. In peer-only mode, no host has a directory or routing authority over
the others.

FlytoMesh supplies only the host network path and node status. Agent discovery,
authentication, queueing and runtime delivery remain in agent-inbox. No code or
private roster file from FlytoMesh is loaded into agent-inbox.

## Configure a host pair

Add a `flytomesh` section to each host relay TOML. The example below is the
`alpha` side of a pair with `beta`:

```toml
[flytomesh]
# Defaults to /var/run/flytomesh.status on macOS and Linux.
status_file = '/var/run/flytomesh.status'
# Defaults to the relay host name. Override only if the mesh node name differs.
node_name = 'alpha'
listen_port = 8832

[[flytomesh.peers]]
host = 'beta'
# Defaults to the peer host name. Override only if the mesh node name differs.
mesh_name = 'beta'
# Defaults to the local listen_port when omitted.
port = 8832
token_file = 'tokens/alpha-beta.mesh.token'
```

Configure `beta` with `host = 'alpha'` and the same pair token. Use a fresh,
mode-600 token file for each host pair. Do not reuse the hub host token, an inbox
token or an agent sender token. The pair token authenticates only this pair; each
relay maps it to the other host's name prefix. Distribute token files through an
owner-controlled channel without printing their contents.

FlytoMesh must allow inbound TCP on the chosen port. Nodes started with
`meshd --inbound` need that port added to their allowlist. Bind is restricted to
the node IP in FlytoMesh's status file, never all network interfaces. On Windows,
the agent-inbox host relay is not yet available; FlytoMesh itself supports
Windows, but this plugin currently runs with the Unix relay.

Restart the host relay after adding or removing the plugin, or changing its
status path, node name or listener port. Peer token and name changes in an
already enabled plugin follow the relay's normal config reload. A missing or
stale status file does not prevent the relay from starting.

## Experimental peer-only relay

Omit both `hub` and `host_token_file` from relay TOML, configure at least one
FlytoMesh peer, and start the relay with `--no-tunnel`. For the desktop daemon,
set `no_tunnel = true` under `[relay]`. The relay still listens on loopback for
local agents and binds its peer listener only to its current FlytoMesh IP.
Each host has the same role. A host name in a destination address must match a
configured peer; the mesh node name can differ through `mesh_name`.
This static mode has passed fake-peer tests and a live two-host mesh test.
[Dynamic discovery and membership](DYNAMIC-PEERS.md) supersede it for more than
a few hosts; the static peer table remains a test and rollback mechanism.

`who` combines the local agent list with fresh lists from reachable configured
peers. It does not require a central report or cache a peer's agent roster when
that peer is offline. A configured peer that did not answer appears as
`<host>.*` with `offline(host)`; this is a host marker, not a sendable agent.
The GUI shows its computer group as unavailable. This is a view from the
querying host, not a globally consistent directory. Dynamic chat-route
registration still belongs to hub mode;
peer-only Feishu forwarding can use the watcher's static routes and sender
filter while chat-route migration is developed.

For each message, the source relay journals it and saves an outgoing record
before returning HTTP 202. It assigns a persistent increasing sequence for
each sender-to-target pair and retries with the same random message ID and
sequence after timeouts or a source restart. Peer retry backs off from one to
30 seconds while a peer is unavailable. The destination checks the
authenticated peer's host prefix, journals and saves the incoming record and
its high-water sequence before acknowledging it. An older sequence cannot be
delivered again even after recent receipt details roll over. A source that
explicitly drops a rejected message sends a skip record for its sequence so
later messages to the same target can continue. Messages preserve order for
each sender-to-target pair; separate targets can progress independently.
The destination then delivers to the local agent's inbox. A definite local
connection refusal is retried. A possible delivery with no reliable receipt
is marked `unconfirmed` and requires the owning local agent to inspect it
before an explicit resend or drop. The relay also pushes a notice to that
agent's inbox when it becomes reachable, without the original message text.
This prevents automatic duplicate work.

Inspect or resolve the local relay's queue with a current local sender token:

```sh
agent-inbox peer-queue 127.0.0.1:8831 --token-file /path/to/current.sender.token
agent-inbox peer-queue 127.0.0.1:8831 --token-file /path/to/current.sender.token --resend <id>
agent-inbox peer-queue 127.0.0.1:8831 --token-file /path/to/current.sender.token --drop <id>
```

Only the sender can resolve its outgoing record, and only the addressed local
agent can resolve an incoming record. Status contains IDs and routing metadata,
never message text or token values. The private mailbox file lives under the
relay's state directory. Keep it and the relay journal across upgrades.
Dropping an outgoing record advances the sender's sequence with a skip; it
cannot retract a message the destination already saved or delivered. Inspect
the destination before using `--drop` on an uncertain record.
The mailbox accepts at most 256 pending messages across incoming and outgoing
queues, 4096 sequence pairs, and 24 MiB of private state. At a cap it rejects
new acceptance with HTTP 503 after journaling the message; it never discards
the oldest queued message. The last 1024 completed receipt details are kept
for diagnostics and payload-conflict checks. Persistent high-water sequences,
not that bounded receipt history, prevent an old source retry from redelivering.

## Route selection

The plugin reads FlytoMesh's atomically replaced status file. It accepts a
status file updated within ten seconds, uses the `node` and `peer` name/IP fields,
and ignores unknown line types or extra fields. A peer must have a handshake no
older than 180 seconds. Each send also probes the peer's agent-inbox listener.
In hub-backed mode, a failed probe uses the existing hub route. After a mesh
POST begins, an error is reported as uncertain delivery; the relay does not
automatically POST the same message through the hub. Peer-only mode uses its
durable mailbox instead: it retries the same ID until the peer saves it, then
keeps destination-side uncertain agent delivery for explicit resolution.

The remote mesh listener accepts only configured pair tokens. It verifies that
the reported sender is inside the authenticated peer host prefix and that the
destination belongs to the local host. A mesh source IP is never treated as
authentication.

When the central hub directory is unavailable, `who` asks reachable configured
mesh peers for their local agent lists. The result contains this host plus the
peers that answered. Group chat routes still use the central hub.

In hub-backed mode, the existing relay journal keeps the original message when
a route fails or becomes uncertain. It does not automatically replay messages
across the hub and mesh paths; inspect the destination before a manual resend.
Peer-only messages use the durable mailbox and stable message IDs described
above.
