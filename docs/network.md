# Network Mesh design

Network Mesh is the optional networking module of fMesh: a small self-hosted WireGuard mesh with
hole punching, relay fallback and `.mesh` names. Programs: the node daemon `meshd`, the relay
`mesh-relay`, the shared protocol crate `meshproto` and the desktop tray `meshtray`.

Code is the reference for every wire format: `crates/meshproto/src/{wire,roster,release,service,crypto}.rs`
and `crates/meshd/src/update.rs` (self-update and automatic rollback).

## 1. Goals

- Light: the motivation was a WireGuard mesh whose resident cost is a few MB, not a Go runtime plus a
  userspace stack plus features nobody uses. Rust, no async runtime, every dependency justified.
- No expiry: a node enrolled once stays valid; no third-party account.
- Latency: direct paths at least as good as other mesh products; failed hole punching falls back to a
  nearby relay.
- Visible failure: a node going offline is detectable within a minute.

| Item | Target | How measured |
|---|---|---|
| Resident memory | <= 15 MB per node process, idle, connected to all peers | macOS `footprint`; Linux `RssAnon` in `/proc/<pid>/status` |
| Binary | <= 10 MB | `ls -l` |

Measured on the current code: `meshd` footprint 2.6-3.8 MB on macOS, `RssAnon` 1.5-2 MB on Linux,
about 12 MB private on Windows; tray 9-15 MB footprint on macOS.

Out of scope for now: fine-grained ACL rules, SSO, exit nodes, subnet routes, file transfer, public
exposure of services, mobile.

## 2. Architecture

There is no central server. Only two programs exist: relays and nodes.

```
   +-------------+     +-------------+     +-------------+
   | mesh-relay  |     | mesh-relay  |     | mesh-relay  |   equal peers, same program,
   +------+------+     +------+------+     +------+------+   each holds the signed roster
          |  rendezvous / relay fallback (ciphertext only) / STUN / heartbeat
          +-------------------+--------------------+
                              |
             +----------------+----------------+
             |                                 |
        +----+----+    WireGuard direct    +----+----+
        |  meshd  |<---------------------->|  meshd  |
        +---------+                        +---------+
```

### What a relay does

1. Rendezvous: a node reports its public and LAN addresses; the relay tells it the peer's addresses and
   both sides send packets at once to punch a hole.
2. Relay fallback: when punching fails the relay forwards ciphertext. Relays cannot decrypt node to node
   traffic.
3. STUN: tells the node which address it appears as from outside.
4. Heartbeat: nodes report periodically; a relay can raise an offline alert.

### Roster instead of a database

- One signed text file: every node's id, name, public key and virtual IP; every relay's address and key;
  a version number.
- An administrator Ed25519 key signs it. Nodes fetch it from any relay, verify it and accept only a
  higher version. Ids are never reused; parsing rejects duplicate ids, names, IPs or keys.
- Nodes cache the roster. With every relay down, existing direct paths keep working; only new
  connections and network changes are affected.

### Nodes never relay

Nodes are for machines without a public address. Only dedicated relay servers relay.

### Names

`<name>.mesh` is answered locally by `meshd` from the roster, never by external DNS. macOS uses
`/etc/resolver/mesh`, Windows an NRPT rule, Linux a systemd-resolved routing domain (or a marked block
in `/etc/hosts` where resolved is absent). Relay entries carry a domain plus the last known IP, so a
proxy that rewrites DNS to fake IPs cannot break relay discovery.

### Userspace data plane

Hole punching and endpoint probes must use the same UDP port as WireGuard, which kernel WireGuard does
not allow. macOS has no kernel WireGuard anyway. So `meshd` owns its socket and demultiplexes WireGuard,
control and STUN packets itself, using boringtun only for the Noise handshake and the cipher. A Linux
kernel path is a possible later optimization.

### Platforms

| Platform | Service | Tunnel |
|---|---|---|
| Linux | systemd, root | TUN |
| macOS | launchd, root | utun (no NetworkExtension needed) |
| Windows | scheduled task running `meshd supervise` as SYSTEM | wintun (signed driver, `wintun.dll` next to `meshd.exe`) |

## 3. Protocol

All packets use the node's single UDP port (default 41820). A first byte of 1-4 followed by three zero
bytes is WireGuard; a first byte `0xFA` is fMesh control, the second byte is the type.

**Cryptography** (ring and boringtun's x25519 only):

- Roster and release manifest signatures: Ed25519.
- Control encryption: X25519(own private, peer public) -> HKDF-SHA256 -> ChaCha20-Poly1305, random
  12-byte nonce per packet. Nodes use their WireGuard key pair; relays have their own X25519 key pair
  published in the roster. A relay can tell which node is talking and a node can tell a real relay.

**Relay messages**:

| Type | Direction | Body | Purpose |
|---|---|---|---|
| `FA 01` HELLO | node -> relay | `[id u16][nonce][enc{timestamp, roster version}]` | check-in every 25 s (also NAT keepalive); timestamps more than 60 s off are dropped |
| `FA 02` HELLO_ACK | relay -> node | `[nonce][enc{echoed timestamp, node's public address, relay roster version, release version}]` | STUN and RTT |
| `FA 03` FWD | node -> relay | `[dest id u16][payload]` | forward; the relay identifies the sender by the checked-in source address, never by a field |
| `FA 04` FWD_FROM | relay -> node | `[source id u16][payload]` | accepted only from roster relay addresses |
| `FA 05` / `FA 06` | node <-> relay | chunked roster fetch | answered only to checked-in addresses |
| `FA 07` NO_ROUTE | relay -> node | `[dest id u16]` | destination not checked in here; try the next relay |

Relay forwarding adds 4 bytes, so the largest relayed packet is 1484 bytes and IPv4 never fragments.
Relays and node sockets are IPv4 only for now (an IPv6 direct path would exceed common PPPoE MTUs).

**Replay and relay choice**: a relay accepts only a strictly larger HELLO timestamp per node, inside a
60 s window, so a captured HELLO cannot move a node's registered address. A node accepts only a
HELLO_ACK echoing its latest timestamp. Per peer, the relay path defaults to the relay that last
delivered that peer's packets, else the lowest RTT; NO_ROUTE moves to the next relay.

**Node to node control** `FA 10 [source id u16][nonce][enc{kind, body}]`, sent directly or inside FWD:

- CALL: candidate addresses (public addresses seen by relays plus local interface addresses, excluding
  tunnels, loopback, CGNAT 100.64/10 and fake-IP 198.18/15). Sent via relay.
- PING `{txid}` to each candidate; the receiver answers PONG and treats the source as a candidate too.
- PONG `{txid, observed source}` confirms a direct path; data moves to it.
- No direct path: retry every 5 s. On a direct path: PING every 10 s, fall back after 3 misses.
- Service announcements: DISCO kind 4, see section 5.

**Peer identification**: direct packets map by source address. Unknown-source WireGuard packets pass a
shared `RateLimiter::verify_packet` (mac1, cookie under load) before `parse_handshake_anon`, then the
receiver index (`id << 8`) identifies the peer. Peer endpoints change only after WireGuard
authentication succeeds or a PONG verifies. Since release v7 the decrypted inner source IP must equal
the peer's roster IP (IPv6 dropped), checked by the receiver.

**Roster format** (the signature covers every byte before the `sig` line):

```
flytomesh-roster 1
version 7
relay r1 203.0.113.10:41900 <X25519 public key>
node 1 laptop1 10.249.0.1 <WireGuard public key>
sig <Ed25519 signature>
```

**Threads**: a node has one send thread (TUN -> lookup -> encrypt -> send) and one receive thread
(poll every socket, 250 ms timer tick). A relay is single threaded.

**Self-update**: relays announce the release version in HELLO_ACK; nodes fetch the signed manifest and
binary in chunks over the same UDP port, verify the signature and hash, swap the binary atomically
(write new file, then rename; never overwrite a signed macOS binary in place) and exit for the service
manager to restart. A new release that fails to reach any relay on three starts is rolled back
automatically. Rollback of a bad release is done by re-signing the old binary under a higher version.

The virtual range is `10.249.0.0/16`, chosen not to clash with CGNAT 100.64/10, fake-IP 198.18/15 or
common cloud VPC ranges.

## 4. Desktop tray

- Native tray only (`tray-icon` + `tao`): about 0.8 MB binary, 9-15 MB footprint. A minimal gpui window
  measured 20 MB footprint, so any richer window must be launched on demand and exit when closed.
- The menu is fixed size: this node, connection (relays x/y), other devices online x/y, Agent Mesh
  availability, then "all devices", copy address, copy diagnostics, logs, quit. Every click shows its
  result in the menu.
- The full device and agent list must be a native window (see `docs/ui.md` when written). The current
  HTML snapshot in `meshtray` is an interim reference and will be replaced, not wrapped in a WebView.
- A stale status file (older than 10 s) means `meshd` is not running; never show old counts as live.
- Headless machines use `meshd status --summary`.
- Wording: no announcement is "not discovered", not "not installed". A node name is a device, not a
  person. Ownership labels are explicit display data and never authorization.

## 5. Local interfaces (stable)

### Status file

The only local interface with a compatibility promise. `/var/run/flytomesh.status` (macOS, Linux; root
writes, world readable), `C:\ProgramData\flytoMesh\status` (Windows). Rewritten every 2 s via
`.tmp` + rename; older than 10 s means `meshd` is not running.

```text
node <name> <mesh IP> id <N> roster v<N> release v<N> (<platform>) [downloading <a>/<b>] key <pubkey>
relay <name> <ip:port> up|down rtt <N>ms|- sees-us <ip:port>|-
peer <name> <mesh IP> <path> handshake <N>s|never tx <N> rx <N> id <N> key <pubkey>
service <node> <mesh IP> id <N> name <service> port <N> ident <x>|- expires <N>s [uid <N>|-]
```

- `<path>` is `direct <ip:port>`, `direct <ip:port> (unconnected)`, `relay <name>` or `none`.
- Parse fields by keyword (`key`, `id`), not position.
- A peer is online when `handshake` < 180 s. The path alone does not mean online.
- Compatibility: only new line types and fields appended at the end. Readers ignore unknown lines
  and extra fields.
- A roster update restarts `meshd` (a few seconds without the mesh IP). Listeners on the mesh IP must
  retry bind.
- A mesh source address is not an identity. Applications authenticate themselves.

### Public roster

After verifying the roster at start, `meshd` publishes the admin public key and the roster text
(`/var/run/flytomesh.admin.pub`, `/var/run/flytomesh.roster`; Windows under
`C:\ProgramData\flytoMesh\public\`). Both are world readable and contain no secrets. Readers may verify
the signature themselves and should pin the admin key on first read: trust stops at local root.

### Service announcements

A local program registers a service; `meshd` tells online peers; their local programs read it from the
status file. **An announcement is a discovery candidate, never authorization.**

- Register: a file named after the service (`[a-z0-9-]{1,32}`) in `/var/run/flytomesh.services/`
  (Windows `C:\ProgramData\flytoMesh\services\`), one line `port <N>` or `port <N> ident <x>`. Register
  after the listener is up.
- Heartbeat: file mtime, refreshed at least every 30 s; stale after 90 s; delete on clean exit. At most
  8 services, 256 bytes each; symlinks and hard links are ignored.
- Unix directory mode 1733: anyone can create, nobody can list or replace another user's file. The
  owner uid is shown locally, never sent to peers. On Windows the directory is currently listable and
  no uid is reported (open item).
- Propagation: DISCO kind 4 over the existing node to node control encryption, sender identified by the
  authenticated header. Own services only; resent every 30 s and within 2 s of a peer coming online.
- Receiver: timestamp must be newer than the last one from that peer and within 120 s of local time;
  kept at most 300 s; cleared when the peer goes offline.
- Boundary: an announcement proves "someone on node X registered this", nothing about which process
  listens or whether `ident` is genuine. Root or any local user on X can register for X.

## 6. Operations

- Clocks must be in sync: relays reject HELLO skewed more than 60 s, which looks like every relay down.
- Windows: the scheduled task needs `ExecutionTimeLimit` 0 (the default stops it after 72 h); the mesh
  adapter should be a Private network; a running exe can only be renamed, not deleted.
- When a system VPN owns the default route, the WAN socket is pinned to the physical interface. Some
  VPNs drop UDP replies from their own infrastructure: do not co-locate relays with a VPN provider's
  servers that its clients route through.
- Before removing another mesh product from a host, check host and container `resolv.conf` for its DNS.
- Scripts: `deploy/mesh/push-roster.sh`, `push-release.sh`, `rollback-release.sh`. They expect the
  operator's private material in a local config directory outside the repository.

## 7. Security posture and open items

Implemented: signed roster and release manifests, authenticated control plane, HELLO replay window,
source-address binding of decrypted packets (v7), rate-limited unknown handshakes, signed self-update
with automatic rollback.

Not done; v8 is **not** a closed security review:

- The admin signing key is root of every node (it signs updates). It must be passphrase, keychain or
  hardware protected; it is never in this repository.
- No access control inside the mesh: it is a flat network except per-node inbound allowlists
  (`--inbound tcp:22,...`).
- Relay amplification through ROSTER_GET / BLOB_GET needs review.
- NO_ROUTE is unauthenticated.
- A too-short HELLO_ACK plaintext can panic (`&plain[8..]`).
- DISCO messages can be replayed inside their window; static DH gives no forward secrecy for control.
- Enrollment pre-generates node keys centrally; a "generate locally, request, admin approves" flow is
  needed for fleet rollout.
- The tray is not covered by self-update.
- An external protocol review is required before claiming production-grade security.
