# Owner-managed hub deployment

The relay owner reviews and installs these files. The build agent does not
install packages, modify sshd, or start services on the hub host. All addresses and
ports are chosen explicitly; the sample wiring is not a set of defaults.
The current topology uses [one host relay/tunnel](../docs/HOST-RELAY.md), with
host-only central registration and a prefix-scoped agent directory. Per-session
tunnel instructions below remain available for migration.

For desktop startup, use [the unified desktop service](../docs/DESKTOP-SERVICE.md).
It owns the relay and optional Codex watcher through one login entry. The
standalone relay/watcher units below remain available for server and advanced
deployments; do not install them alongside a daemon that owns those components.

For [Claude auto-join](../docs/CLAUDE-AUTO-JOIN.md), ship the user-scope
[MCP entry](claude-user-mcp.json.example) and the all-directory
[shell wrapper](claude-with-inbox.bash) together. Both use the exact MCP name
`agent-inbox-join`; the channel argument is `server:agent-inbox-join`.
The relay must run as the same UID as these sessions; Linux can use the
[user relay unit](agent-inbox-relay.user.service). Dedicated-account system
services need a writable private control path and matching session ownership.

Codex uses the additive [watcher setup](../docs/CODEX-AUTO-JOIN.md),
[config example](codex-watch.toml.example), [launchd template](io.agent-inbox.codex-watch.plist.example)
or [systemd user unit](agent-inbox-codex-watch.service). It reuses the same local
relay without a per-thread MCP entry or manual registration. Online denotes
loaded-thread/inbox state, with the daemon's disconnect grace; idle delivery
can start work without an attached human.

## Files

| File | Purpose |
| --- | --- |
| `agent-inbox-hub.service` | Relay service, loopback listen, `Restart=always`. |
| `hub.env.example` | Explicit relay router listen address. |
| `hub.toml.example` | Stable peers and independent sender token files. |
| `tunnel.env.example` | One session's endpoints, ports, key paths, and timing. |
| `inbox-hub.ssh.example` | SSH host with the endpoint-selection ProxyCommand. |
| `hub-ssh-proxy.sh` | Local-facts route selection for a manual SSH ProxyCommand. |
| `agent-inbox-tunnels.sh` | Launches the Rust supervisor for outbound `-R` and `-L`. |
| `tunnel-common.sh` | Shared config validation; keep beside both scripts. |
| `sshd-agent-tunnel.conf.example` | Nologin tunnel account's sshd Match policy. |
| `agent-tunnel.authorized_keys.example` | Independent session keys with exact port limits. |
| `relay.toml.example` | Local agents, descriptions and sender/inbox token paths. |
| `host-tunnel.env.example` | One host's shared tunnel wiring. |
| `host-tunnel.authorized_keys.example` | One restricted public key per host. |
| `io.agent-inbox.relay.plist.example` | Mac relay restart definition. |
| `agent-inbox-relay.service`, `relay.env.example` | Linux relay restart definition. |
| `hub.legacy.toml.example` | Previous per-session central registration. |

## Build on the Mac

The target is Debian 12 x86-64, glibc 2.36. Compile on the Mac, never on the
relay. With Rust, Zig and cargo-zigbuild installed locally:

```sh
rustup target add x86_64-unknown-linux-gnu
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.36
```

The output is `target/x86_64-unknown-linux-gnu/release/agent-inbox`. The glibc
suffix follows the [cargo-zigbuild target convention](https://github.com/rust-cross/cargo-zigbuild#specify-glibc-version).

For a native Mac copy, use `cargo build --release`. Replace an installed binary
by removing the old file and copying the new one, as described in the main
README, to avoid the macOS code-signature cache issue.

## Relay setup, done by the owner after review

1. Choose a loopback router port and one unused reverse port per host. Register
   only host names in `[[hosts]]`; local agents use `<host>.<agent>`. Claude
   session names and Codex thread IDs remain local inbox configuration.
2. Create the service account `agent-inbox`, or adjust `User` and `Group` in the
   unit to the owner-selected account. Copy the already-built Linux executable
   to `/usr/local/bin/agent-inbox`, with executable permissions.
3. Copy and fill `hub.toml.example` as `/etc/agent-inbox/hub.toml`, and
   `hub.env.example` as `/etc/agent-inbox/hub.env`. Keep the listen address on
   127.0.0.1. Copy only independent host tokens under
   `/etc/agent-inbox/tokens/`; agent inbox/local sender tokens stay on their host.
   Set `directory_file` under `/var/lib/agent-inbox` for persistent metadata.
   All credential files are regular mode-600 files
   owned by the service account; put file paths in TOML, never token values.
   The unit protects home directories, so its credentials must be under `/etc`.
4. Install the unit in `/etc/systemd/system/agent-inbox-hub.service`. The unit
   creates a private `/var/lib/agent-inbox` state directory for the journal.
   The owner runs:

   ```sh
   sudo systemd-analyze verify /etc/systemd/system/agent-inbox-hub.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now agent-inbox-hub.service
   sudo systemctl status agent-inbox-hub.service
   ```

5. Verify the router health on its chosen loopback port and confirm a test message
   appears in the intended peer. HTTP acceptance alone does not prove a Claude
   channel was enabled. Restart the hub after changing routes or token files.

The hub's journal remains the copy of messages sent while a reverse tunnel or
peer is offline. It returns 503 and does not replay those messages automatically.

## Laptop/session tunnels

Use the dedicated `agent-tunnel` SSH account below, separate from the
`agent-inbox` service account and from personal/admin accounts. Each session has
its own SSH key and authorized_keys line; a shared admin key is not used.

Each session has one local inbox and one sender hub token. Copy the deployment
scripts together locally. Create a trusted owner-controlled mode-600
`tunnel.env` from the example, using absolute paths, and fill an SSH config from
`inbox-hub.ssh.example`. Set its ProxyCommand to the actual script and that
session's environment file. The supervisor passes the user, identity, known
hosts and host-key alias from the same environment file to the outer SSH client.
Set `INBOX_BIN` to the updated binary. All sessions under this OS user targeting
the same relay must share the exact absolute `INBOX_DIAL_LOCK_FILE` path,
regardless of their session names, keys or ports. The supervisor creates a
persistent mode-600 file and uses the kernel file lock on macOS and Linux.
Keep that file in place, including while tunnels run; deleting or replacing it
would give different processes different lock inodes.

Pin the hub host's verified host key under `INBOX_HOST_KEY_ALIAS` in
`INBOX_KNOWN_HOSTS`. The intranet and public endpoints must authenticate to the
same relay and pinned key. Batch mode is used; load a passphrase-protected key
in the SSH agent before starting. No script changes host keys or disables their
verification.

The sample addresses are an intranet endpoint
`192.0.2.10:22` and public endpoint `hub.example.com:2222`. They are
configuration values, and can be changed for another relay.

Start the inbox first, then keep the supervisor running:

```sh
/absolute/path/to/deploy/agent-inbox-tunnels.sh \
  --config /absolute/path/to/session/tunnel.env
```

Use separate environment files, reverse ports, local inbox ports and local send
ports for separate sessions. Both forwards share one outbound SSH connection:

- `-R 127.0.0.1:<reverse-port>:127.0.0.1:<local-inbox-port>` lets the hub reach
  the receiver's existing loopback inbox.
- `-L 127.0.0.1:<local-send-port>:127.0.0.1:<router-port>` lets that session call
  the hub on a stable local address.

The supervisor does not daemonize. Keep its terminal open or run it under the
owner's launch agent/process supervisor. HUP, INT and TERM stop its current SSH
or backoff child promptly. It does not kill remote processes to reclaim ports.

## Route selection, retries and stale reverse ports

Each attempt reads local IPv4 addresses and the default gateway. An address in
`INBOX_INTR_PREFIX` (for example `192.0.2.0/24`) or a default gateway equal
to optional `INBOX_INTR_GATEWAY` selects the intranet. Otherwise it selects the
public endpoint directly. macOS uses local `ifconfig`/`route` output; Linux uses
local `ip` output. Unavailable facts select public; malformed configured IPv4
CIDR/gateway values fail. No remote SSH or TCP availability probe is performed.
The supervisor refreshes these facts after acquiring the shared dial lock on
every retry. A failed selected endpoint waits for the next gentle retry; it
does not immediately try another endpoint.

The Rust supervisor starts one real SSH connection per attempt, initially with
only the local hub forward (`-L`). It overrides ProxyCommand/ProxyJump and uses
a private master socket for that connection. Once OpenSSH creates the socket after authentication,
the supervisor releases the dial lock; established tunnels can coexist. It then
adds the reverse forward with `ssh -O forward -R ...` through that private
socket. This control operation neither authenticates a second SSH connection
nor executes a remote shell, and fails if the master is missing. See the
[OpenSSH control commands](https://man.openbsd.org/ssh#O).
No second SSH connection is used to detect readiness, and the
temporary master is never reused by a later attempt. Both supervisor and guard
hold the same locked file description before authentication. If the supervisor
crashes, the guard stops its SSH; if the guard crashes, the supervisor stops
that owned process group before releasing its lock. Kernel cleanup releases
the file lock without a PID file or stale-lock deletion.

`INBOX_OUTER_CONNECT_TIMEOUT=45` allows the real handshake, and
`INBOX_DIAL_TIMEOUT=45` bounds the whole authentication attempt. No SSH process
is started while waiting for another session's dial lock. One remote site's public path
has shown 6-25 second handshakes and mid-session resets, so the former probe plus
connection pattern created needless unauthenticated connections.

Retries use exponential backoff with a 15-second base, growing to 300 seconds,
and independent +/-20% jitter. The actual delay is capped at 300 seconds:
the first wait is 12-18 seconds; at the cap it is 240-300 seconds. Backoff resets
only after five authenticated minutes; time spent dialing does not count.
Unsafe older settings (initial below 15, maximum above 300, stable reset below
300 seconds) fail before any dial. On upgrade, update the timing fields and add
`INBOX_BIN`, `INBOX_DIAL_LOCK_FILE` and the local prefix/gateway rule. Remove
the obsolete probe settings `INBOX_CONNECT_TIMEOUT` and `INBOX_PROBE_TIMEOUT`.

`ExitOnForwardFailure=yes` rejects a failed local forward at initial setup.
If the reverse port is still held by an old sshd session, only the `-O forward`
request fails: the live SSH master and usable `-L` stay up. The supervisor
retries that request every 3 seconds with +/-20% jitter (2.4-3.6 seconds), with
at most one outstanding control request. These in-band retries consume no
pre-auth slots and do not take the shared dial lock. It logs
`both loopback forwards ready on existing SSH` once `-R` attaches. An already
attached forward is left alone while its SSH process remains alive; HTTP hub
failure alone never tears down the tunnel. Directory reports can travel via
`-L` during reverse-port recovery, so an online directory row can precede inbound
delivery readiness. Check the readiness log and both delivery directions after
startup/reconnect. The short forward retry avoids adding minutes after the old
port becomes free.

Only a real SSH exit triggers a new connection and the slower dial backoff.
The guard terminates and waits for its owned SSH/control children before the
next dial; normal cleanup first allows OpenSSH a graceful disconnect before
falling back to a kill. It cannot force the remote sshd to notice a broken
network path immediately. `ServerAliveInterval=15`
and `ServerAliveCountMax=3` detect a dead connection. These are
[OpenSSH client controls](https://man.openbsd.org/ssh_config.5).

The HTTP sender waits up to 25 seconds; the hub's peer deadline is 10 seconds.
This allows slow tunnel replies while keeping offline delivery bounded. A sender
timeout can occur after delivery, so inspect the journals before resending.

When a laptop disappears, sshd can temporarily retain its old reverse port.
After reconnecting, the hub's "peer offline" HTTP 503 can refer to that stale
`-R`, even while the laptop inbox is running. It clears once the old sshd
session ends and the supervisor successfully binds the replacement forward.
The supervisor retains the replacement connection and retries only `-R` over
it; it never forcibly deletes another session's
forward. The relay owner can review these server-side recommendations:

```text
ClientAliveInterval 15
ClientAliveCountMax 3
GatewayPorts no
```

The ClientAlive settings help release stale sessions. `GatewayPorts no` keeps
remote forwards on loopback; `yes` can override a client's loopback request.
The owner also confirms TCP forwarding is permitted for the chosen account and
checks the actual listeners with `ss -lnt` after startup. These are
[sshd settings](https://man.openbsd.org/sshd_config.5); the build agent does not
change them.

Forward setup only checks the listening port, not whether the ultimate local
inbox is alive. The hub's bounded POST and 503 handle that case.

## Protecting the relay's sshd

The relay's SSH service is also the administrator's access path. The owner
reviews effective `MaxStartups` and `LoginGraceTime` with `sshd -T` before
changing policy. `MaxStartups` limits unauthenticated connections; the default
`10:30:100` starts probabilistic rejection at 10. `LoginGraceTime` bounds how
long authentication may remain unfinished. Choose it using observed handshake
times so slow valid clients still have time to authenticate. See the
[sshd manual](https://man.openbsd.org/sshd_config.5#MaxStartups).

Increasing server limits alone does not fix a retry storm. This supervisor
performs zero route probes, holds the shared host/relay lock for a hard cap of
one concurrent dial, requests one SSH connection per attempt, bounds dialing,
and applies the slow jittered backoff above. A stale reverse port retries on
the same authenticated master, without another dial. The lock applies to
supervised sessions sharing the file on this host; other hosts and administrative SSH
clients also consume server capacity. Keep an administrative session open and
verify a separate login before applying an owner-selected sshd reload. The
build agent changes no relay sshd settings.

## Dedicated tunnel account, configured by the relay owner

Create a non-login system account on the hub host after review. These commands are
for the owner, not actions performed by the build agent:

```sh
sudo useradd --system --no-create-home --home-dir /var/lib/agent-tunnel \
  --shell /usr/sbin/nologin agent-tunnel
sudo install -d -o root -g root -m 0755 /var/lib/agent-tunnel
```

Use Debian 12's normal `UsePAM yes` configuration, with PAM allowing this service
account. Its password remains locked; authentication is public-key-only. Other
PAM/account rules can still refuse access and must be checked on the real host.

Generate a distinct key on each session's client machine. Keep its private key
there with private permissions and load it in the SSH agent when using a
passphrase. Put only that session's public key into its line in
`agent-tunnel.authorized_keys.example`, substituting its assigned reverse port
and the hub router port. Do not reuse one key for multiple sessions.

```text
restrict,port-forwarding,permitlisten="127.0.0.1:<session-reverse-port>",permitopen="127.0.0.1:<hub-router-port>",command="/usr/sbin/nologin" ssh-ed25519 <session-public-key> <logical-session-name>
```

`restrict` disables PTY, agent forwarding, X11 and user rc processing.
`port-forwarding` re-enables only forwarding; the two `permit` options constrain
its endpoints. `permitlisten` limits the requested `-R` bind, and `permitopen`
limits `-L`/direct TCP destinations. They match the literal address/port, so keep
the supervisor's explicit `127.0.0.1` arguments. The forced command denies
command execution. These are [OpenSSH authorized_keys options](https://man.openbsd.org/sshd.8).

Install the completed public-key file root-owned and readable by sshd's target
user, then install the Match policy:

```sh
sudo install -o root -g root -m 0644 <completed-authorized-keys> \
  /etc/ssh/agent-tunnel.authorized_keys
sudo install -o root -g root -m 0644 deploy/sshd-agent-tunnel.conf.example \
  /etc/ssh/sshd_config.d/agent-tunnel.conf
sudo sshd -t
sudo sshd -T -C user=agent-tunnel,host=<client-host>,addr=<client-ip>
```

This file contains public keys, not private credentials; root ownership prevents
the tunnel account from changing its restrictions. Hub/inbox token files still
require mode 0600 and remain owned by the hub service account.

The Match block also requires public keys, forbids shell/subsystem sessions with
`MaxSessions 0`, disables PTY/agent/X11/Unix-socket forwarding, and leaves TCP
forwarding enabled so the per-key port constraints can apply. `ssh -N` opens no
session, so the nologin shell and forced command do not run. Normal commands and
SFTP cannot open a session. The relay owner checks Include ordering and the
effective Match settings, keeps the administrative connection available, and
reloads sshd only after validation.

Set `INBOX_SSH_USER='agent-tunnel'` and that session's own private-key path in
`tunnel.env`. Removing a session's authorized_keys line revokes new logins;
ending an already-established tunnel is an owner action. No script kills another
session or tries to reclaim its reverse port.

## Verify and clean up

```sh
sh -n deploy/tunnel-common.sh deploy/hub-ssh-proxy.sh deploy/agent-inbox-tunnels.sh
cargo test --test deploy
cargo test
cargo clippy --all-targets -- -D warnings
```

Fake tests cover local prefix/gateway selection without probes, shared flock
serialization, coexistence after authentication, failed/stalled dials, crashes
of either lock holder, gentle jittered retries and stopping while waiting.
They use owned fixtures and never connect to the hub host or another project's inbox.
The Linux binary and unit can be checked in a local Debian 12 x86-64 container;
real SSH/account/sshd deployment is the relay owner's final validation.

A real Debian 12 sshd check can be run locally in a disposable container:

```sh
docker run --rm --platform linux/amd64 --name agent-inbox-restricted-sshd \
  --volume "$PWD/deploy:/artifacts/deploy:ro" \
  --volume "$PWD/tests/sshd:/artifacts/tests:ro" \
  --volume "$PWD/target/x86_64-unknown-linux-gnu/release/agent-inbox:/artifacts/agent-inbox:ro" \
  --env DEBIAN_FRONTEND=noninteractive --env INBOX_SSHD_TEST_CONTAINER=1 \
  debian:12 /bin/sh -ec 'apt-get update >/dev/null; apt-get install -y --no-install-recommends openssh-server netcat-openbsd curl >/dev/null; sh /artifacts/tests/verify.sh'
```

The fixture refuses to run outside a root-owned Docker test environment. It
creates disposable keys and the nologin account only inside that container,
checks `sshd -t/-T`, real `-N` reverse/local forwarding, exact per-key port limits,
rejected shell/SFTP/unregistered keys, and local-facts route selection. It also
runs the current supervisor while another master holds `-R`: repeated in-band
forward failures keep `-L` working, and releasing the port attaches `-R` without
a redial. It publishes no host ports. The fixture and `--rm` remove its keys, sockets, logs,
account and processes after verification.

Remove owned control directories, token/log fixtures and isolated compiler caches
after a step completes. Retain the source and requested release artifacts.

## Codex upgrade schedule

See [the upgrade guide](../docs/CODEX-UPGRADES.md) and
[upgrade-codex.toml.example](upgrade-codex.toml.example). Run as the Codex daemon
owner after disabling the independent updater and restarting all inboxes with
the common registry. The [launchd example](io.agent-inbox.upgrade-codex.plist.example)
and [systemd user timer](agent-inbox-upgrade-codex.timer) run at minute 17.
They are templates; the build agent does not install a schedule or restart the
real daemon. The manager reports an older npm CLI's exact manual upgrade command.
