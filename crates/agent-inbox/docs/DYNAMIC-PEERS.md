# Dynamic peer discovery and membership

Status: implemented behind the `dynamic-peers` feature. It provides host-level
membership only. Person-device ownership, signed delegation, shared-node
enforcement and LAN discovery/TLS are separate work and are not provided by it.

The `dynamic-peers` feature contains signed membership, FlytoMesh
roster/status verification, directed per-request MACs, durable replay
protection, dynamic mesh routing and manifest gossip. The earlier
`flytomesh.peers` table and pair tokens remain a test and rollback mechanism.

## Separate discovery from admission

A transport reports candidate addresses, never authority to send as a host.

- With FlytoMesh installed, verify its public signed roster with a provisioned
  FlytoMesh administrator public key, then read its atomically replaced status
  file. Accept a status file updated within ten seconds. Scan only the mesh
  node IDs in the agent-inbox membership manifest, not every mesh node. A peer
  row supplies an address only when its roster revision matches the verified
  roster, its ID and key match that roster, and its handshake is younger than
  180 seconds. Retry a revision mismatch after the next two-second status
  refresh; mark that member offline and alert if it persists for 30 seconds.
  Bound concurrent probes and timeouts. Do not enumerate host names
  in relay TOML. The public roster, the ID/key status fields and service
  announcements are described in `docs/network.md` section 5. On macOS and
  Linux, the files are `/var/run/flytomesh.roster` and
  `/var/run/flytomesh.admin.pub`. Compare the latter to the provisioned
  FlytoMesh administrator key before using it to verify the roster.
- Without FlytoMesh, advertise and discover candidate addresses on the LAN
  through mDNS. Optional seed addresses can bootstrap discovery across network
  segments. Seeds are addresses to ask, not trusted members or a required hub.
  LAN traffic must use TLS 1.3 with both host certificates bound to the
  signed membership before message text leaves the host; discovery and
  request signatures alone do not provide confidentiality.
- Local agents continue to register with their own host relay regardless of
  the cross-host discovery provider.

The FlytoMesh status file supplies name, IP, handshake age, node ID,
public key and short-lived service rows. Admission requires the `node` row's
`key` and both public roster files. Check these capabilities, not a release
number: a manually installed binary may report an older release number. A
mesh IP or an open listener alone never authorizes a sender. The roster also
includes nodes that do not run agent-inbox.

## Membership without a running authority

Each host creates a private X25519 agent-inbox transport key. Every
installation pins a set of Ed25519 agent-inbox membership authority public
keys and keeps its own transport private key in a mode 600 file. A deployment may use
a single automated authority key with threshold 1; see the custody section
for what that trades away. Both key
types are separate from FlytoMesh's roster signer and WireGuard keys. The
membership signers are used only to admit, revoke or renew members; normal
routing does not contact them.

A quorum-signed, versioned manifest contains a network ID, revision,
`token_epoch`, expiration, authority set and unique records `(host, mesh node
ID, X25519 transport public key)`. A record may omit the mesh node ID for a
LAN-only host; it also needs a pinned TLS authentication key. The FlytoMesh
roster controls that ID's current name, IP and WireGuard public
key, so a normal mesh key rotation does not require a second agent-inbox
manifest change. The inbox transport key remains independently pinned. The
manifest rejects duplicate host names, transport keys, mesh IDs and low-order
X25519 public keys. Every relay verifies
the signature threshold, network ID, validity window and a revision not below
its durably persisted highest accepted revision before replacing its active
manifest. The manifest and highest revision are one atomic persisted state.
An older revision is rejected even if its signatures remain valid. An equal
revision with different bytes is a conflict, not an update.

### Authority custody

An m-of-n policy is independent only if no single administrator credential
can reach m signing keys. If one administrator credential reaches privileged
accounts on every relay host, placing one key on each host under root is still
one failure domain, not m-of-n protection. Audit this before choosing a
threshold.

A threshold-1 automated key avoids human key handling, but compromise of the
signing host's administrator domain can rewrite membership, admit a rogue host
or replace a host transport key. Its private key stays on that signing host
and is never sent to peers. A hardware-backed signer with a human presence check is
an independent alternative; it needs signing-provider code and a deliberate
renewal interval before deployment. These choices affect membership control,
not the peer-to-peer data path.

An authority rotation is a transition manifest signed by a quorum of the
current keys and a quorum of the next keys. Relays persist the new authority
set and revision atomically, then use the next set for later revisions. If a
quorum of current authority keys is lost or compromised before a transition,
recovery requires an owner-controlled out-of-band re-pin on each host; no
network peer may choose a replacement authority for others.

Any existing peer may serve the signed manifest to another peer. Relays gossip
newer revisions and cache the last verified one, so no particular computer is
needed during normal operation. A new installation receives the pinned
authority keys and its first signed manifest through owner-controlled
provisioning, never from an unauthenticated network answer. With no cached
manifest, it serves local agents only and rejects remote queries and writes.
An offline host can resume with its cached valid membership and queue messages
for known peers.

Initial manifest validity is 30 days. Warn at 14, 7 and 1 days remaining;
any two of the three authority key holders can renew without changing members.
On expiry, a relay stops accepting remote queries and writes from every peer
and queues new outgoing remote messages until a current signed revision
arrives. The durable queue drains automatically after renewal. Local messages
and the local directory remain available; `who` marks known remote hosts as
`unavailable(membership expired)` without querying them. Continuing to accept
known keys after expiry would let a revoked peer in an isolated partition keep
access indefinitely. This design chooses a bounded remote pause instead. The
worst-case revocation delay
for a partitioned but connected revoked peer is the remaining validity of its
last accepted manifest, at most 30 days. Connected relays normally receive a
revocation at gossip speed, in seconds or minutes; the 30-day ceiling applies
only to a relay partitioned from the update for the entire remaining period.

Joining requires one deliberate admission: install FlytoMesh or enable LAN
discovery, generate the host key, verify the host's network identity, and have
a quorum sign one new manifest revision. The new host imports that revision
once; existing hosts learn it from any reachable member. Adding a host does
not edit every relay's TOML or distribute pairwise tokens. Revocation removes
its record in a higher signed revision. The operator can inspect the active
revision, remaining validity and propagation status on each host. None of the
authority private keys or host private keys enter messages, logs or manifests.
Increasing `token_epoch` deliberately rotates all pair MAC keys without
changing host key files. During manifest propagation, peers at different
epochs keep outgoing messages queued; they retry after accepting the same
signed revision. Every inbound request checks current membership first, so
removing a host takes effect as soon as the new revision is accepted.

## Authenticate each request

Both members of a pair derive a shared secret from their X25519 keys, then
HKDF-SHA256 derives separate A-to-B and B-to-A HMAC keys. The HKDF context
contains a protocol label, network ID, length-prefixed ordered host names and
public keys, `token_epoch` and direction. A manifest revision alone does not
rotate pair keys; a deliberate epoch or host-key change does. No derived key
or bearer token travels on the wire.

Each request carries only an HMAC-SHA256 tag over method, path, exact body
digest, timestamp, random 128-bit nonce, full sender name and receiver host.
The receiver checks current signed membership before deriving a pair key,
rejects an absent or invalid MAC without downgrade, and derives the sender's
`host` prefix from the verified pair record. Accept at most two minutes of
clock skew. Keep accepted nonces for five minutes in a bounded durable cache,
so a relay restart cannot replay a recently captured non-sequenced request.
The cache holds at most 4096 nonces globally and 256 per peer, so one peer
cannot exhaust every other peer's replay capacity. Each accepted request
fsyncs this state; this is appropriate for coordination traffic, but it is a
throughput limit if message volume grows substantially.
Existing persistent message IDs and sender-to-target high-water sequences
provide an additional delivery dedupe layer. Distinct directional keys stop
reflection of a request into the opposite direction, but both pair members
can derive both keys. They do not cryptographically distinguish A from B
within their pair; the mesh source-IP and interface checks below remain
mandatory for sender attribution.

This is a deliberate tradeoff against asymmetric per-request signatures. A
compromised host transport private key exposes every pair key involving that
host, and static ECDH alone has no application-layer forward secrecy.
FlytoMesh supplies encrypted transport with its own session properties. LAN
discovery must not send message text until mutual TLS is available.

For a FlytoMesh route, verify the public roster signature, compare the
candidate's mesh node ID with its inbox manifest record, and compare the
current WireGuard key with the roster and status row. Inbound sockets accept only
traffic delivered by the FlytoMesh interface. On Linux use `SO_BINDTODEVICE`
for `flytomesh`, or a verified strict reverse-path firewall rule if binding
requires privileges. On macOS resolve the current `utun` interface index from
the local mesh IP and use `IP_BOUND_IF`; verify with a real packet test that it
rejects traffic arriving through another interface. On Windows restrict the
firewall's remote address to the mesh subnet. The new public roster capability
also implies the receiving `meshd` validates each decrypted source IP against
its WireGuard peer before injecting the packet. The socket's source IP must
map to the same roster node ID. These checks gate admission; they are not only
diagnostics. They bind the pair MAC to the actual mesh node while
allowing a FlytoMesh-signed key rotation for the same ID. A stolen inbox key
alone cannot authenticate from another mesh node. The FlytoMesh overlay
provides the encrypted path; agent-inbox pair authentication is still
mandatory. An
untrusted LAN discovery answer cannot impersonate a member because it lacks
the listed private key.

The manifest's date check allows at most two minutes of clock error. A host
whose clock is farther out of sync queues remote work and reports the clock
problem. FlytoMesh itself requires synchronized clocks for its control plane;
LAN-only hosts need a local clock check before remote membership is enabled.

FlytoMesh can recreate its tunnel interface after an upgrade or roster change.
Rebuild the mesh listener when the local roster revision, mesh IP or interface
index changes, even if the old listener has not reported an error. Continue
serving local agents and keeping remote messages queued during the rebind.
The status file must have been refreshed within ten seconds. If meshd stops
updating it, peer routing pauses closed and the relay reports the stale
status; monitor meshd freshness when diagnosing a mesh pause.

## Feishu chat ownership without the hub

Each host relay stores only claims made by its authenticated local agents.
`agent-inbox chat-route register/remove` keeps its existing self-registration
interface. A host serves its own versioned claim snapshot at
`/mesh/chats/snapshot` only to an authenticated mesh host. The receiver binds
that snapshot to the sender host authenticated by the request MAC and mesh
source address; host A cannot publish or erase host B's claims. Other relays
pull snapshots on demand and cache them durably. Host-scoped revisions reject
old or conflicting snapshots. The local `/chats/list` response includes merged
routes, any conflicts and hosts that could not be queried in this pass.

A claim persists while its agent is offline. This preserves groups assigned
to dormant projects, which must queue messages until the agent returns.
Registration by the same agent as an imported route is idempotent. When two
hosts claim the same chat, no route wins by arrival time: the watcher retains
that chat's events in `awaiting_route` and raises a visible conflict. Before
snapshot convergence, two hosts can briefly have different views; the
operator must reconcile a duplicate claim promptly. The normal resolution
is for a claimant to remove its own registration. If that agent or host is
unavailable, an owner-controlled `[[route_overrides]]` entry in the watcher
configuration selects the receiver for that chat without forging a remote
host's claim. The override remains visible as an exception to remove after
the original claim is repaired. If any host cannot be queried, an unknown
chat also stays in `awaiting_route`; the watcher never guesses the trusted
sender fallback while the route table is incomplete. Known, conflict-free
routes from hosts that answered continue to deliver. A cached route from an
unreachable host pauses until that host answers, since its owner may have
withdrawn it. The watcher saves the pending event and cursor
atomically, so a prolonged partition delays but does not discard it.

When migrating from a hub, import its existing routes as per-host seed files,
preserving their owning `host.agent` names. A relay loads a seed only when
creating its chat-route state for the first time; subsequent
self-registration and self-removal update the same local claims. An offline
target is a valid claim. Activate `use_peer_routes=true` and
`use_hub_routes=false` in the Feishu watcher only after every seed claim
appears exactly once without conflicts in a fresh peer query. The old static
table remains a rollback artifact and is ignored while peer routes are
authoritative. Later group creation continues to use self-registration.

## Routing and limits

`host.agent` stays the address format. The host prefix selects a verified
manifest record. The discovery provider supplies current addresses for that
record. The source saves an outgoing message before any network attempt and
keeps it queued when the peer is offline; the destination persists its
incoming record and high-water sequence before acknowledging. `who` queries
reachable authenticated peers and marks known unreachable hosts as
`offline(host)`. Its answer remains a per-host view during partitions.

Bound status scans, handshake concurrency, manifest size, peer count, queue
size and retry backoff. Do not scan every FlytoMesh node for every `who` call:
cache authenticated reachability briefly and refresh candidates in the
background. A signed member that is absent from the current discovery view
remains a known offline host, not an unknown name.
Rate-limit challenge issuance and back off failed candidates per address and
member ID. An untrusted mDNS or seed answer must not cause a hot authentication
loop or unbounded challenge state.

The routing loop is deterministic:

```text
on discovery refresh:
    verify the newest signed membership revision
    for each member except self:
        look up its mesh node ID in the verified FlytoMesh roster
        find the matching ID and key in current FlytoMesh status, or its LAN advertisement
        if its candidate address is fresh, probe and authenticate it
        cache authenticated address and reachability for a short interval

on send to host.agent:
    reject host if absent from the verified membership
    persist message ID, target and sender-to-target sequence locally
    if host is reachable, MAC and POST to its authenticated address
    otherwise keep the message queued with bounded backoff

on who:
    list local agents, query reachable authenticated members in parallel
    mark each known member that did not answer as offline(host)
```

## Experimental configuration

The feature build accepts a `dynamic_mesh` section instead of
`flytomesh.peers`. There are no remote host names or pair token files in this
TOML. Each relay still defines its own local `[[agents]]` rows as needed.

```toml
host = 'alpha'
state_dir = 'state'

[dynamic_mesh]
network = 'team'
host_key_file = 'tokens/alpha.x25519.key'
membership_state_file = 'state/membership.json'
replay_state_file = 'state/mesh-replay.json'
bootstrap_manifest_file = 'tokens/membership.signed.json'
flytomesh_admin_key = '<provisioned FlytoMesh administrator public key>'
listen_port = 8832
service_discovery = true

[dynamic_mesh.authorities]
threshold = 1
keys = ['<owner-approved authority public key>']
```

The `mesh-identity` CLI generates a mode-600 host key and an Ed25519
authority key, then signs a manifest payload. Provision the key according
to the approved custody model:

```sh
agent-inbox mesh-identity host-keygen --file tokens/alpha.x25519.key
agent-inbox mesh-identity authority-keygen --file private/authority-a.key
agent-inbox mesh-identity manifest-sign --payload manifest-payload.json --key private/authority-a.key --output membership.signed.json
agent-inbox mesh-status --hub 127.0.0.1:8831 --token-file tokens/own.sender.token
```

Keep the authority private key only on the owner-controlled signing host. The
paths above are examples, not a provisioning recipe. Each relay compares the public FlytoMesh administrator
key against its provisioned value, verifies the signed roster, then verifies
the agent-inbox manifest. Later manifest revisions can be placed on one
relay; peer gossip carries them to the others without editing each TOML.
`service_discovery = true` requires FlytoMesh's service directory and status
rows (`docs/network.md` section 5). The listener tries `listen_port`, then a free TCP
port on the same mesh interface if that port is occupied. It registers the
actual port before accepting mesh traffic. Remote sends require a fresh
`agent-inbox` service row; an absent row pauses routing to that host. Omitting
`service_discovery` keeps the earlier fixed-port behavior for staged hosts.
The service record is unsigned and supplies only a port on the signed roster
IP. A false or stale record can make that peer unreachable; it cannot select
another IP or authenticate a message. The inbound interface, source-IP and
per-request MAC checks remain mandatory.

### Automatic renewal with one authority key

For an owner-approved threshold-1 deployment, `mesh-identity renew-single`
verifies the current envelope against the sole pinned authority key and a
durable signer revision state. It changes only the revision and validity
window; members and `token_epoch` remain unchanged. It renews when 14 days or
less remain, issuing a new 28-day envelope. The local relay polls its
`bootstrap_manifest_file` and gossips an accepted revision to peers.

```sh
agent-inbox mesh-identity renew-single \
  --key /absolute/private/authority.key \
  --manifest /absolute/host/tokens/membership.signed.json \
  --state /absolute/private/signer-state.json
```

Run this daily with a service manager, at an offset minute rather than on the
hour or half hour. The
signer key and state stay on the owner-controlled signing host; other hosts
receive only the signed envelope. The command refuses an invalid signature,
changed authority set, or a rollback. If the signing host remains offline
until the current envelope expires, normal renewal fails closed and an owner
must recover or replace the trust anchor out of band.

### Mixed-mode canary before cutover

A relay may temporarily keep `hub`, `host_token_file` and its existing tunnel
settings while replacing the static `[flytomesh]` block with `[dynamic_mesh]`.
Set `mesh_canary_only = true` at the top level of that mixed-mode TOML. The
dynamic listener and signed membership run, but ordinary outgoing messages
continue through the existing hub. The flag is rejected unless both a hub
and dynamic mesh are configured, so it cannot accidentally persist into a
hubless cutover. A local authenticated sender can check the actual peer MAC,
interface binding and remote directory response without carrying a user
message:

```sh
agent-inbox mesh-status --hub 127.0.0.1:8831 --token-file tokens/own.sender.token --peer laptop2
```

After every pair passes, remove `mesh_canary_only` and restart the mixed
relays to exercise dynamic egress with hub fallback. A hub-backed relay does
not use the hubless durable peer queue for outgoing messages; a direct POST
with an uncertain result is journaled and reported to its sender, not sent a
second time through the hub. Keep that stage short and inspect failures.
Stop only the Feishu watcher before removing the hub from relays; its event
journal continues collecting messages. After all relay claims reconcile,
start the watcher with `use_hub_routes=false` and `use_peer_routes=true`.
Keep the old hub and per-host rollback files until live traffic has been
observed through the new path.
