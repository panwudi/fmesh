# agent-inbox

One Rust binary for coordination messages between running agent sessions.
`claude` is a Claude Code MCP channel with an authenticated HTTP inbox. `send`
POSTs raw text to an inbox. `steer` delivers text into an existing Codex thread
during its current turn, or starts a turn when the thread is idle.
`codex` runs an authenticated HTTP inbox for one Codex thread.
`relay` routes `<host>.<agent>` names on each computer. In the experimental [peer-only
mode](docs/FLYTOMESH-TRANSPORT.md), relays use FlytoMesh to query each other
directly and durably queue messages without a central hub. The existing `hub`
and SSH tunnel mode remains available during migration and for older hosts;
see [dynamic peers](docs/DYNAMIC-PEERS.md) for moving off the hub and [hub setup](docs/HUB-USAGE.md).
The [on-demand local GUI](docs/GUI.md) groups agents by computer and sends
through the same relay identity and routes.
Auto-join defaults distinguish same-directory pairs as `<host>.<basename>-claude`
and `<host>.<basename>-codex`; same-type duplicates receive `-2`, `-3`, etc.
Explicit inbox `--name` overrides keep their chosen component.
`who` discovers authenticated agent descriptions and availability. Start with
[host relay setup and migration](docs/HOST-RELAY.md).

For Claude sessions, [automatic joining](docs/CLAUDE-AUTO-JOIN.md) uses one
user-scoped `agent-inbox-join` MCP entry and an all-directory channel wrapper.
Each session gets an ephemeral inbox, private credentials and a directory name;
`inbox_profile` reads or updates its own name/description. No manual session row.
Modern Claude sessions receive through their own native inbox, including in
the background without a channel launch flag. Explicit continuations retire
the old registration and retain its name; independent sessions stay separate.
Unavailable receivers are not advertised online. Legacy channel inboxes still
need the matching channel flag. See the auto-join guide for transport checks.

For Codex, [the per-user watcher](docs/CODEX-AUTO-JOIN.md) registers one inbox per
exact loaded thread ID, with names from thread cwd. Online does not imply a
human is watching; a message to an idle Codex may start autonomous work.

For Grok Build, [the optional watcher](docs/GROK-AUTO-JOIN.md) registers exact
resident sessions from its shared leader and uses ACP interjections for active
or idle delivery. Standalone TUI sessions need to resume through the leader
before they can join.

`upgrade-codex` holds Codex inbox deliveries while waiting for an idle upgrade
window, verifies the new daemon, then releases FIFO queues. See
[scheduled Codex upgrades](docs/CODEX-UPGRADES.md) for disabling the independent
updater, registering every inbox, reports, recovery and schedule examples.
Relay service and supervised tunnel setup are in [deploy/README.md](deploy/README.md).

For desktop startup, use [one unified service](docs/DESKTOP-SERVICE.md):
`agent-inbox daemon --config <toml>` owns the relay and optional Codex watcher.
One login entry starts, stops and restarts them together, while preserving
existing session queues and configuration. `--check` validates the configuration
without starting services. Standalone commands remain available.

## Build and use

Requires Rust 1.95 or later and a running Codex app-server daemon.
The experimental protocol was verified with Codex 0.162.1. Other versions are
accepted with a warning; incompatible response shapes fail loudly.

```sh
cargo build --release
./target/release/agent-inbox steer --thread <thread-id> --from claude 'Please check the failing test.'
./target/release/agent-inbox send <inbox-address> --token-file <private-token-file> --from codex 'Hello from Codex.'
./target/release/agent-inbox send --hub <local-hub-address> --to <logical-name> --token-file <my-hub-token> 'Hello through the hub.'
```

To install, replace the file rather than overwrite it (`rm ~/.local/bin/agent-inbox && cp target/release/agent-inbox ~/.local/bin/`).
On macOS, a `cp` over an existing binary keeps the old code-signature cache and the new binary is killed at start (exit 137).

Find the receiver's thread ID in its `CODEX_THREAD_ID` environment variable.
Use the same Codex home and daemon as the receiving session.
The receiver can keep working while the command runs.

Optional `steer` flags, placed before the message:

```text
--from <name>           Sender label, defaults to teammate
--log <path>            Durable Markdown message journal
--codex-bin <path>      Codex executable, defaults to codex
--sock <path>           Custom app-server control socket
--timeout-secs <n>      Timeout per response, 1 to 60 seconds, defaults to 10
--                     Treat remaining arguments as message text
```

The sender sees the accepted RPC method, thread ID, turn ID, and journal path.
Acceptance means the server accepted the input; it does not mean the receiver
has already read or acted on it.

Messages are labeled as teammate coordination, not the user's instructions or
approvals. Empty messages and messages larger than 64 KiB are rejected.
Sender labels and thread IDs cannot contain newlines.

## Claude Code setup

Create a token file without printing its contents. Both the HTTP inbox and
`send` require a regular file with permissions 0600, containing a nonempty
ASCII token without whitespace. A trailing newline is allowed.

```sh
mkdir -p ~/.config/agent-inbox
(umask 077; openssl rand -hex 32 > ~/.config/agent-inbox/token)
```

Each receiving session chooses its own address. `--listen` is required; there
is no default port. A bare port binds 127.0.0.1. Add an MCP entry using an
address reserved for that session:

```json
{
  "mcpServers": {
    "agent-inbox": {
      "command": "/absolute/path/to/agent-inbox",
      "args": [
        "claude",
        "--listen", "127.0.0.1:8797",
        "--token-file", "/absolute/path/to/private-token",
        "--log", "/absolute/path/to/messages.md",
        "--reply-thread", "<codex-thread-id>"
      ]
    }
  }
}
```

Then start the receiving interactive Claude Code session with:

```sh
claude --dangerously-load-development-channels server:agent-inbox
```

The MCP process owns the HTTP listener. Do not start a second process on the
same port. The channel flag and configured MCP entry name must match.
Without `--dangerously-load-development-channels`, Claude Code silently drops
the notifications, even though HTTP POSTs succeed. The journal is then the only
copy available to the receiver; notifications are not replayed automatically.
`--reply-thread` is optional; when present, the channel instructions include
the exact `steer` command Claude can use to reply.

The [Claude Code channel contract](https://code.claude.com/docs/en/channels-reference)
requires `capabilities.experimental["claude/channel"]`, a stdio transport, and
`notifications/claude/channel` events. This server implements those wire formats
directly in Rust. Startup diagnostics go to stderr; stdout contains only MCP
JSON lines.

The old raw POST form continues to work:

```sh
curl -sS -H "Authorization: Bearer $(cat /absolute/path/to/private-token)" \
  --data-binary 'Hello from a teammate.' http://127.0.0.1:8797/
```

`claude` flags:

```text
--listen <addr>         Required; a bare port uses 127.0.0.1; port 0 chooses a free port
--token-file <path>     Required, or use AGENT_INBOX_TOKEN_FILE
--log <path>            Durable message journal
--name <receiver>       Journal receiver label, defaults to claude
--from <sender>         Default sender for curl, defaults to codex
--reply-thread <id>     Codex thread used by the configured reply command
--hub <addr>            Hub address through this session's local SSH forward
--hub-token-file <path> This sender's hub token, for configured hub replies
```

An explicit non-loopback `--listen` address enables LAN access. Each inbox uses
its own address and token file. `send` accepts `host:port` or
`http://host:port/`, with `--token-file`, `--from`, and `--log` before the text.
It never follows redirects or prints credentials or arbitrary HTTP responses.
The optional `X-Agent-From` header carries the sender label; it is coordination
metadata, not proof of a distinct sender identity.
Hub replies use `--hub` and `--hub-token-file` together, instead of
`--reply-thread`. The MCP instructions include the exact hub address and token
file and use the incoming `from` attribute as the reply's logical destination.

## HTTP results

| Status | Meaning |
| --- | --- |
| 200 | Message logged and Claude notification written, or input accepted by Codex. |
| 202 | Message logged and queued for initialization, or posted to Claude's native inbox. |
| 400 | Empty, oversized, truncated, or incorrectly framed request. |
| 401 | Missing or incorrect bearer token. |
| 431 | Request headers exceed 16 KiB. |
| 500 | Journal write failed; no notification was emitted. |
| 503 | Busy or unavailable channel; inspect the response for whether the message was saved. |

HTTP requests use `Content-Length` and have a 64 KiB body limit. Duplicate
authorization or length headers and transfer encodings are rejected. Reading
each request has a five-second deadline, and at most 16 requests run at once.
Before channel initialization, up to 64 messages wait in memory; additional
authenticated messages are saved to the journal and return 503.

HTTP 200/202 confirms transport acceptance, not that Claude has processed
the message. Ask for a reply to verify actual receipt. `send` shows accepted
or pending feedback and returns a nonzero status on rejection.

## Codex HTTP inbox setup

Choose a port and private token file for the receiving Codex thread, then run:

```sh
./target/release/agent-inbox codex \
  --thread <codex-thread-id> --listen 127.0.0.1:8799 \
  --token-file <private-token-file> --log <receiver-journal>
```

Senders use `agent-inbox send <that-address> --token-file <that-token-file>
--from claude <text>` or the same raw curl POST form. This is a standalone
process and stays running even when stdin is closed. Stop it with Ctrl-C or
your process supervisor.

`codex` requires `--thread`, `--listen`, and a token file. It also accepts
`--log`, `--from` (default: claude), `--codex-bin`, `--sock`, and
`--timeout-secs`, using the same protocol settings as `steer`.

Each authenticated message is journaled and durably queued before HTTP 202.
The FIFO worker reconnects and retries draining/refused connections with
backoff. Active turns receive `turn/steer`; idle threads receive `turn/start`.
Persistent client message IDs and complete-history reconciliation protect
lost acknowledgements and process restarts. Unreadable/incomplete history
blocks later messages to that thread, writes a journal notice and invokes the
configured notification hook. See [resilience and explicit resend/drop](docs/CODEX-RESILIENCE.md)
for queue paths, notify flags and `agent-inbox codex-queue` status/control.

## Set up host relays

The central hub registers hosts, while each host owns its agent table and local
sender tokens. Add a one-line description for every agent. The local relay
reports its prefix-scoped directory and handles same-host messages locally.
Use `agent-inbox who --hub <local-relay> --token-file <local-sender-token>` when
unsure whom to ask. Channel instructions include that command; copy the
[Codex AGENTS.md recipe](docs/HOST-RELAY.md#set-up-a-host) into each project.

The [host relay guide](docs/HOST-RELAY.md) includes the per-host key/token setup,
launchd/systemd services, status meanings, live agent reloads and migration.

## Forward Feishu messages automatically

`agent-inbox feishu-watch --config <toml>` follows the event journal of an
existing Feishu bot and sends each configured chat to one qualified agent name.
It uses the bot's existing connection. It never opens a second Feishu listener.
The receiver checks the cited original message before acting on an owner's
instruction. No owner identity is inferred from a group name or message text.

After creating a reporting chat, each agent registers it with
`agent-inbox chat-route register --chat <chat-id> --hub <local-relay> --token-file <own-sender-token>`.
The hub binds the chat to the authenticated agent, persists the mapping, and
rejects another agent's attempt to overwrite it. `chat-route list` confirms the
result; `chat-route remove` removes the caller's own mapping. Claude's profile
and Codex's current session status include a registration command.
With `use_hub_routes = true`, the watcher loads these registrations dynamically.
`fallback_agent` receives unregistered group messages only from the locally
pinned `fallback_sender_id` (one open_id or a list). Other senders remain in the bot's original log
without being forwarded. Registered groups keep their existing routing;
an offline registered agent keeps its messages queued. Private chats and bot
messages are ignored.

The watcher saves its cursor and pending deliveries before forwarding. Offline
agents keep their messages queued. Restarts and recent duplicate events do not
repeat accepted messages. A lost acknowledgement stays unconfirmed for manual
inspection before `--retry <delivery-id>`. The first start skips old history;
later starts resume the saved cursor. `--once` processes one batch and exits.

Use a fixed sender-only relay entry for the service: omit both `inbox` and
`token_file`, keeping `name`, `description` and `sender_token_file`. Such an entry
can send and query the directory, but cannot receive replies and is omitted
from the list of available inboxes. See the
[Feishu watcher setup](docs/FEISHU-WATCH.md) for routing, state and recovery.

## Legacy per-session hub setup

The relay owner registers two stable logical names, such as `my-pair-claude`
and `my-pair-codex`, in both the hub's `peers` and `senders` tables. Each role has
an inbox token and a separate sender hub token, with mode-600 copies where
needed. The names and credentials stay stable when a session restarts.

Choose unused local and relay ports owned by this pair. This example wiring is
configuration, not a set of default ports:

| Role | Local inbox | Local hub forward | Relay reverse endpoint |
| --- | --- | --- | --- |
| my-pair-claude | 127.0.0.1:8797 | 127.0.0.1:8821 | 127.0.0.1:8817 |
| my-pair-codex | 127.0.0.1:8799 | 127.0.0.1:8823 | 127.0.0.1:8819 |

1. The relay owner configures the hub's peer routes and sender tokens using
   [the hub guide](docs/HUB-USAGE.md). Keep four independent token files locally:
   `claude.inbox.token`, `claude.hub.token`, `codex.inbox.token`, and
   `codex.hub.token`. Start this pair's outbound SSH tunnels using the wiring
   above and the relay's chosen router port, using [the tunnel supervisor](deploy/README.md).
   Each role's `-R` reaches its inbox;
   its `-L` reaches the hub. The relay owner deploys the hub after review.

2. Start the receiving Codex session and obtain its `CODEX_THREAD_ID`, then run
   its inbox locally:

   ```sh
   /absolute/path/to/agent-inbox codex --thread <codex-thread-id> \
     --listen 127.0.0.1:8799 --token-file <codex.inbox.token> --log <codex-journal>
   ```

   Put this reply instruction in that project's `AGENTS.md`, using absolute
   binary and token paths. The sender name comes from the incoming message:

   ```text
   Reply to agent-inbox coordination messages with:
   /absolute/path/to/agent-inbox send --hub 127.0.0.1:8823 --to <incoming-logical-from-name> --token-file <my-codex.hub.token> '<msg>'
   ```

3. In the Claude project's directory, configure its inbox and hub reply path,
   then start the interactive session with the matching channel flag:

   ```sh
   claude mcp add --transport stdio --scope local my-pair-inbox -- \
     /absolute/path/to/agent-inbox claude --listen 127.0.0.1:8797 \
     --token-file <claude.inbox.token> --log <claude-journal> \
     --name my-pair-claude --hub 127.0.0.1:8821 --hub-token-file <claude.hub.token>
   claude --dangerously-load-development-channels server:my-pair-inbox
   ```

4. Check both directions using each sender's own hub token, then verify the
   message in each receiving session:

   ```sh
   agent-inbox send --hub 127.0.0.1:8823 --to my-pair-claude \
     --token-file <codex.hub.token> 'Codex -> Claude through the hub'
   agent-inbox send --hub 127.0.0.1:8821 --to my-pair-codex \
     --token-file <claude.hub.token> 'Claude -> Codex through the hub'
   ```

The caller does not choose its identity: the hub derives `from` from its sender
token. `--from` and `X-Agent-From` claims cannot override that identity. You need
only your hub address, your token, and the destination's logical name. An
offline destination returns 503; inspect sender, hub, and receiver journals
before resending. After a reconnect, "peer offline" 503 can also mean sshd is
still holding a stale `-R` forward. It clears once the old SSH session ends and
the supervisor attaches the replacement reverse forward over its existing
authenticated connection. Its local hub forward remains available during
reverse-port recovery. See the
[public-link timing and stale-port guidance](deploy/README.md#route-selection-retries-and-stale-reverse-ports).
Claude still needs the channel flag or it drops notifications.

For both direct and hub delivery, `send` allows 25 seconds for the HTTP exchange.
The hub's peer forwarding deadline is 10 seconds, allowing slow tunnel replies
while the sender has extra time for the hub exchange and response latency.
An unconfirmed send may already have been delivered; inspect the journals
before retrying to avoid a duplicate.

## Protecting the relay's sshd

Route selection uses only the host's configured intranet IPv4 prefix or default
gateway. Each supervised attempt makes one real SSH connection, with a shared
kernel file lock limiting concurrent dials to one per host/relay. Established
tunnels can coexist. Retry backoff starts at 15 seconds, grows to 300 seconds,
uses +/-20% jitter with a 300-second hard ceiling, and resets after five
authenticated minutes. Update older tunnel environment files before restarting.
If an old sshd still holds the reverse port, only that forward is retried every
2.4-3.6 seconds over the existing master; it keeps the local hub forward alive
and makes no extra SSH login. The log confirms when both forwards are ready.

The relay owner reviews `MaxStartups` and `LoginGraceTime` for administrative
access and measured handshake times. See
[the deployment limits and upgrade instructions](deploy/README.md#protecting-the-relays-sshd).

## Set up a direct pair

Use two unused ports owned by this pair. The example uses 8797 for Claude and
8799 for Codex. Replace `/absolute/path/to/agent-inbox` with the built binary's
absolute path, and use a new directory for each pair.

1. Build, then create two independent private tokens in a fresh pair directory:

   ```sh
   cargo build --release
   mkdir -p "$HOME/.config/agent-inbox/my-pair"
   (umask 077
    openssl rand -hex 32 > "$HOME/.config/agent-inbox/my-pair/claude.token"
    openssl rand -hex 32 > "$HOME/.config/agent-inbox/my-pair/codex.token")
   ```

2. Start the receiving Codex session in its project and ask it to report
   `CODEX_THREAD_ID` (`printenv CODEX_THREAD_ID` inside that session). In a separate
   terminal, keep this inbox process running using that ID:

   ```sh
   /absolute/path/to/agent-inbox codex --thread <codex-thread-id> \
     --listen 127.0.0.1:8799 \
     --token-file "$HOME/.config/agent-inbox/my-pair/codex.token" \
     --log "$HOME/.config/agent-inbox/my-pair/codex.md"
   ```

   Also put this reply instruction into that Codex project's `AGENTS.md`, using
   the actual binary path and this pair's Claude port and token file. Codex has
   no MCP channel instructions, so the project file tells it how to answer:

   ```text
   Reply to Claude's agent-inbox coordination messages with:
   /absolute/path/to/agent-inbox send 127.0.0.1:8797 --token-file "$HOME/.config/agent-inbox/my-pair/claude.token" --from codex --log "$HOME/.config/agent-inbox/my-pair/sent.md" '<msg>'
   ```

3. In the receiving Claude project's directory, register the channel in local
   scope, then start the interactive Claude session with the channel flag:

   ```sh
   claude mcp add --transport stdio --scope local my-pair-inbox -- \
     /absolute/path/to/agent-inbox claude --listen 127.0.0.1:8797 \
     --token-file "$HOME/.config/agent-inbox/my-pair/claude.token" \
     --log "$HOME/.config/agent-inbox/my-pair/claude.md" \
     --name my-pair-claude --reply-thread <codex-thread-id>
   claude --dangerously-load-development-channels server:my-pair-inbox
   ```

4. From another terminal, send a check to each receiver and confirm each session
   shows its message. Use the target receiver's token file:

   ```sh
   /absolute/path/to/agent-inbox send 127.0.0.1:8797 \
     --token-file "$HOME/.config/agent-inbox/my-pair/claude.token" \
     --from codex --log "$HOME/.config/agent-inbox/my-pair/sent.md" \
     'Pair setup check: Codex -> Claude'
   /absolute/path/to/agent-inbox send 127.0.0.1:8799 \
     --token-file "$HOME/.config/agent-inbox/my-pair/codex.token" \
     --from claude --log "$HOME/.config/agent-inbox/my-pair/sent.md" \
     'Pair setup check: Claude -> Codex'
   ```

HTTP 200 confirms transport acceptance; verify the messages in the receivers.
If Claude's message is missing, check the exact channel flag and `/mcp` status.
If a send fails, inspect this pair's journals before resending. Stop the Codex
inbox with Ctrl-C; Claude owns the lifetime of its MCP inbox process.

## Several inboxes and LAN

Each inbox is a separate process with its own explicit address and token file.
No command chooses a default port. For example:

| Receiver | Address | Token file | Journal |
| --- | --- | --- | --- |
| Pair A, Claude | 127.0.0.1:8797 | pair-a/claude.token | pair-a/claude.md |
| Pair A, Codex | 127.0.0.1:8799 | pair-a/codex.token | pair-a/codex.md |
| Pair B, Claude | 127.0.0.1:8807 | pair-b/claude.token | pair-b/claude.md |
| Pair B, Codex | 127.0.0.1:8809 | pair-b/codex.token | pair-b/codex.md |

Use separate MCP entry names or project-local configurations for distinct
Claude receivers, and distinct thread IDs for Codex receivers. A token for one
inbox does not authenticate to an inbox configured with another token. Journals
can be separate or shared; file locks serialize writes across processes, and
each heading records the sender and receiver.

A bare port (`--listen 8809`) binds only 127.0.0.1. IPv6 loopback uses an
explicit address such as `--listen '[::1]:8809'`. LAN binding happens only when
an explicit non-loopback address is supplied. On the receiving host, substitute
its actual LAN IP:

```sh
agent-inbox codex --thread <thread-id> --listen 192.168.1.20:8809 \
  --token-file <receiver-token-file> --log <receiver-journal>
```

Give authorized senders a mode-600 copy of that receiver's token, then send to
the configured host and port:

```sh
agent-inbox send http://192.168.1.20:8809/ --token-file <matching-token-file> \
  --from <sender-name> 'Hello over the LAN.'
```

The same `--listen` forms work for the Claude MCP process. An explicit
`0.0.0.0:8809` listens on all IPv4 interfaces; a specific LAN IP restricts the
listener to that address. Existing sessions in other projects keep their own
ports, tokens, and configuration.

## Delivery and logging

Every valid message is appended and synced to the journal before contacting the
daemon or sending an HTTP request. Both inboxes also journal authenticated
messages before delivery. The default journal is
`$XDG_STATE_HOME/agent-inbox/messages.md`, or
`~/.local/state/agent-inbox/messages.md` when `XDG_STATE_HOME` is unset.
Each entry records sender, receiver, local readable time with its UTC offset,
and the original message.
New journal files have permissions 0600. An exclusive file lock serializes
concurrent appends.

Receivers are told to reply to every message without asking their user first:
an answer, a result, a one-line acknowledgement, or why they will not act.
Claude gets this in the MCP instructions and as `reply="expected"` on each
`<channel>` event; each Codex message carries it in its header.

Receivers execute delegated work under the authority already granted in their
own instructions, including a standing policy for the user's terminal agents.
They do not ask the user to repeat authorization in another window or in Feishu
merely because an agent dispatched the task. A routine read-only production API
request using configured project credentials does not by itself need another
confirmation. Existing tool permissions and explicit action limits still apply;
a peer's claim of human approval does not expand them.

Terminal delegation and forwarded Feishu content have separate trust rules.
An external group message does not become a terminal instruction when forwarded.

For a forwarded Feishu request, the receiver fetches and checks the original. A
forwarder cites it with a line `feishu-message: om_<id>`; the receiver reads it
with the feishubot `get_message` tool and treats it as the user's instruction only
if the sender is a user whose open_id matches the user's own, or another trusted
sender the user listed, in the receiver's own instructions, it is not a merged forward, it is at most 24 hours old, and its
text on its own asks for this action. The receiver then acts on the fetched text
alone. Claude gets this rule in the MCP instructions; Codex gets it in the
message header whenever the message cites a Feishu message. The user's open_id,
and any trusted senders the user lists, belong in the receiver's own instruction file (for example a global
`AGENTS.md`), never in a message.

If logging fails, delivery is not attempted. If delivery fails, the command
returns a nonzero exit status and identifies the saved journal entry's file.
Safe turn-state races are retried up to three times after re-reading the thread.
Transport failures are not retried, and journals are not replayed automatically.
A connection failure after
a request was sent may mean the receiver accepted it before the connection
closed; inspect the receiver before resending to avoid duplicate messages.

For the default socket, `agent-inbox` reads both CLI and daemon versions using
`codex --version` and `codex app-server daemon version`. With `--sock`, it checks
the CLI version and validates protocol responses. Versions different from the
tested version produce one warning line instead of preventing delivery.

The control socket belongs to Codex and is local to the current user. `steer`
does not start an HTTP listener. A bare `--listen` port binds to 127.0.0.1.

## Protocol

See [the tested protocol contract](docs/protocol-0.162.1.md).
The transport uses WebSocket frames over `codex app-server proxy` stdin/stdout.
Sending JSONL directly to the proxy fails because the daemon expects an HTTP
Upgrade handshake followed by WebSocket frames.

The client initializes once, reads the thread's runtime status, and then:

- Active: reads the newest turn with `thread/turns/list`, then uses `turn/steer`
  with that turn's ID as `expectedTurnId`.
- Idle: uses `turn/start` without changing the thread's configuration.
- Not loaded: uses `thread/resume` without configuration overrides, then
  chooses the active or idle path from the returned status.
- Unavailable or unknown status: fails and keeps the message in the journal.

If a turn changes between lookup and delivery, the command re-reads the thread
and retries up to three times. Only confirmed precondition rejections or a
lookup with no active turn qualify for retries. Ambiguous delivery failures
remain errors, and their messages stay in the journal.

## Verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Tests check the MCP handshake and notification format and POST real requests to
a Claude-side subprocess, then read the notifications from its stdout. They
also exercise authentication, mode-600 token files, message and header limits,
safe HTTP rejection, pending delivery, sender feedback, concurrent notifications
and journal writes, and credential-free output.
Regressions cover requests arriving 300 ms after connect on macOS and direct
or hub responses delayed 16 seconds after successful delivery, plus peer replies
that take six seconds inside the hub's forwarding deadline.

Codex tests compile a Rust-only fake executable and use it as a raw stdio proxy
to a fake WebSocket app-server. They check handshake sequencing, active and idle
delivery, unloaded-thread resume, notifications and pings, stale-turn rejection,
timeouts, version mismatches, message limits, and durable failure handling.
The Codex HTTP inbox is tested through real POSTs against that fake server,
including a failed delivery followed by a fresh successful connection.
Multi-process tests check token and receiver isolation, shared journal appends,
loopback binding for bare ports, and an explicit non-loopback bind.
Deployment tests cover route selection from local facts, shared file locks,
crash cleanup, gentle jittered retries and cleanup of tunnel control sockets.

An optional read-only check inspects the current live daemon and active turn:

```sh
CODEX_THREAD_ID=<thread-id> cargo test --test steer real_daemon_read_only -- --ignored --nocapture
```

This check does not send input or start a turn.
