# Claude sessions join automatically

One user-scoped MCP entry starts an inbox for each Claude session, from every
directory. Each inbox reads its own working directory, derives a name from its
basename with a `-claude` suffix, binds `127.0.0.1:0`, creates fresh inbox/sender
tokens, and registers with the local host relay. No per-session TOML entry,
token setup or port is needed. Codex joins through its separate
[watcher](CODEX-AUTO-JOIN.md).

## One-time setup

Use [one desktop startup service](DESKTOP-SERVICE.md) to manage the local relay
and optional Codex watcher through one login entry. The Claude MCP entry and
channel wrapper below remain the same.

1. Upgrade the local host relay, running as the same OS user as Claude. Its
   existing host token/tunnel config still applies; `agents` can be empty. Restart
   it on the new binary. No central hub change is required for each new session.
2. Add the stable MCP entry once, using the absolute real Claude executable:

   ```sh
   /absolute/path/to/real/claude mcp add --scope user --transport stdio agent-inbox-join -- \
     /absolute/path/to/agent-inbox claude --relay 127.0.0.1:8831
   ```

   Replace 8831 with this user's relay port. The
   [JSON example](../deploy/claude-user-mcp.json.example) shows the same entry in
   `~/.claude.json`; merge it, do not replace unrelated configuration. User scope
   makes the entry available across projects, as described in the
   [Claude MCP documentation](https://code.claude.com/docs/en/mcp#user-scope).
   Remove local/project definitions that shadow this same MCP key. After checking
   the new route, retire obsolete per-project inbox entries and static relay rows.
3. Replace the old per-directory `claude` function in `~/.zshrc` with an
   unconditional wrapper, outside any directory tests:

   ```sh
   export AGENT_INBOX_CLAUDE_BIN='/absolute/path/to/real/claude'
   claude() { /absolute/path/to/deploy/claude-with-inbox.bash "$@"; }
   ```

   Use the executable [wrapper](../deploy/claude-with-inbox.bash) from this repo.
   Point the variable at the real installed binary, never at the wrapper. Open
   a new shell or reload the shell config. This applies to all directories.

**The exact channel argument is `server:agent-inbox-join`.** Its suffix must
match the MCP configuration key `agent-inbox-join` exactly. It is neither the
dynamic session name nor the handshake's `serverInfo.name`. The wrapper passes
`--dangerously-load-development-channels server:agent-inbox-join`, preserves
arguments and working directory, and keeps positional prompts outside the
variadic channel option. Explicit additional development channels are preserved.
Channels require claude.ai sign-in, so the wrapper adds the flag only when the
current auth method is claude.ai; for an OAuth token, an API key or a custom
`ANTHROPIC_BASE_URL` gateway or local model it omits the flag, since Claude
would otherwise ignore it and print "Channels are not currently available".
Those sessions still receive over the native inbox below. `AGENT_INBOX_CHANNELS=on`
or `off` overrides this detection, and an explicit flag always wins.
The [Claude channel reference](https://code.claude.com/docs/en/channels-reference#test-it)
documents the `server:<MCP-entry-name>` form.

Modern Claude sessions export their own native inbox. Auto-join prefers that
inbox, so a background worker does not lose messages merely because its launch
omitted the development-channel flag. The receiver checks its parent PID,
session ID, project, native protocol version and socket peer before using the
parent's inherited token. It never discovers or reads another session's token.
See [Claude's own-child socket contract](https://code.claude.com/docs/en/cross-session-messaging#the-sessions-inbox-socket).
Native inbound controls still apply. HTTP 202 means the message was journaled
and written to that inbox; actual model receipt requires a reply. A failed or
uncertain native write is not retried through the channel transport.

Older sessions use the channel transport. A session with a Claude session ID
but neither a native inbox nor the matching channel flag reports unavailable,
rather than claiming live receipt. The wrapper remains useful for legacy
versions; it does not bypass development-channel consent or organization policy.
Explicit `claude --listen` inboxes retain their legacy channel behavior.

## Session behavior

- Default name: `<host>.<basename>-claude`, from the MCP process's own cwd.
  Unsupported characters become hyphens under the existing ASCII name rules;
  an empty result becomes `session-claude`. Long basenames are shortened before
  the type suffix. The default component uses at most 57 bytes, reserving room
  for the relay's numeric collision suffix within the 63-byte limit. Three
  sessions in `agent-inbox` become `<host>.agent-inbox-claude`,
  `<host>.agent-inbox-claude-2` and `<host>.agent-inbox-claude-3`.
- `--name` overrides the component exactly, without an added type suffix, and
  accepts the full 63-byte limit. `--description` overrides the description.
  The default description is `Claude session in <name>`. The inbox does not
  infer a different cwd from environment variables or change directories.
- Call the `inbox_profile` MCP tool with `{}` for the assigned name, description,
  registration status, `deliveryTransport`, `transportReady`, `continuedTo`, and
  exact reply/directory commands. Set `name` and/or
  `description` to update this session. Names are one local component; duplicate
  names receive `-2`, `-3`, etc. Always use the returned qualified name.
- Successful changes update routing and the hub directory promptly. The sender
  token stays fixed, so replies automatically use the new identity. Old names
  are removed. The tool accepts no target/session ID for changing another inbox.
- An explicit Claude `continued-in` transcript record retires only that old
  receiver's registration. Its successor inherits the old name and description
  using private local continuation metadata, without sharing sender tokens.
  The old receiver cannot rejoin or rename itself after retirement, including
  after a relay restart. Existing chat routes retain the same qualified name.
  Sharing a project directory alone never merges independent sessions or forks.
  A complete matching record is required; partial records and unrelated copied
  history do not transfer identity. An occupied successor name is not taken over.
- The default legacy heartbeat is 15 seconds and TTL 45 seconds. Native session
  lifecycle checks and heartbeats run at most one second apart, shortened for a
  smaller TTL. EOF, SIGINT, SIGTERM
  or SIGHUP attempts a quick unregister and removes this session's token files.
  A killed process is TTL-evicted; expired sender credentials stop authorizing
  requests. The next inbox startup removes stale credential directories using
  kernel locks, preserving live siblings.
- Relay entries and tokens are in memory. A live inbox retries and rejoins after
  a relay restart, keeping its credentials and preferring its last assigned
  name. Name/status changes are written only to the local journal; they do not
  emit channel messages or wake the model. Read current status with
  `inbox_profile`. A failed local control request leaves registration unconfirmed
  and retries; it does not by itself prove that the relay stopped. If the relay
  is initially absent, MCP initialization and journaling still work while
  registration retries.
- Replacing the installed binary applies to new receiver processes. Existing
  Claude sessions keep their running MCP receiver until it is reconnected from
  Claude's MCP controls or the session is restarted. Restarting the relay alone
  does not reload those receivers.
- Static TOML agents continue to work. Reloading static agents preserves live
  dynamic sessions; a reload that collides with one is rejected visibly.

## Local control boundary and paths

Registration uses a Unix socket, never a TCP registration API. The relay and
client check the peer UID using macOS `getpeereid` or Linux `SO_PEERCRED`.
The parent directory is owner-controlled mode 700; the socket is mode 600.
Another user or a remote host cannot register through the relay's HTTP endpoint.
Same-user processes are trusted to join; per-session credentials protect profile
operations against accidentally modifying a different session.

The default socket is under the user's state directory:
`agent-inbox/local-control/relay-<relay-port>.sock`. The state root is
`$XDG_STATE_HOME`, or `~/.local/state`. `control_socket` in the relay TOML and
`--control-socket` in the MCP entry override it together. A dedicated private
parent directory is required. Use a short path if the OS Unix-socket path limit
is exceeded. Custom control paths must match on both sides.

Per-session credentials live in private directories under
`agent-inbox/join-sessions` in the same state root. `--session-dir` overrides this
parent for isolated setups. Tokens are 256-bit random values in mode-600 files;
their values never appear in MCP output, journals, config, directory reports or
diagnostics. Only the relay receives them over the authenticated local socket.
The private `continuations` subdirectory retains names and observed continuation
links across receiver reconnects; it contains no sender or native-inbox tokens.
Receiver health requires MCP initialization and a usable selected transport;
retired or unavailable receivers return HTTP 503 to health checks and are not
reported online. An online transport still does not prove a model acted.

A Linux system service running as a dedicated `agent-inbox` account cannot
register sessions owned by a different user. Use a relay running as the session
owner, or configure the service User/Group and writable private control path
accordingly. The user relay service example below avoids that mismatch. Do not
weaken socket permissions or UID checks to bridge users.

See [the Linux user relay unit](../deploy/agent-inbox-relay.user.service).
Optional relay `session_ttl_secs` accepts 1..3600; clients use one-third of the
TTL, capped at 15 seconds, for heartbeats. `control_socket`/TTL changes require
a relay restart. The build agent installs no MCP entry, wrapper or service;
the owner reviews and performs the one-time setup.

## Codex entries in the same directory

Desktop viewers can read the agent directory without borrowing another
session's sender credential: `agent-inbox who --local --json`. The command uses
the same-user Unix control socket, verifies its owner and permissions, and
returns only names, descriptions and reachability metadata. It does not register
an agent, send messages or expose credentials. An alternate relay port uses
`--hub 127.0.0.1:<port>`; a custom control path uses `--control-socket <path>`.
The relay must be upgraded and restarted to support this read-only operation.
Peer queries have a five-second client deadline, separate from registration's
short control deadline. Descriptions are session self-reports, not proof of
device ownership or human attendance.

[Codex auto-join](CODEX-AUTO-JOIN.md) uses a separate per-user watcher, without
changing this Claude MCP path. Same-directory defaults distinguish the pair:
`<host>.my-project-claude` and `<host>.my-project-codex`. Existing running
inboxes keep their assigned names; start new Claude sessions or use
`inbox_profile` to adopt the new name. Restart the Codex watcher to apply its new
default to loaded threads; persistent queues remain keyed by thread ID.
For Codex entries, online means a daemon-loaded
thread/inbox, not that a human is watching a TUI. The daemon may retain a thread
for up to 30 minutes after its last subscriber disconnects; daemon errors retain
the watcher's last valid membership while messages queue. Addressing an idle
Codex can start autonomous work without an attached human. Existing permissions
and FIFO still apply. Legacy Claude channel receivers still require the channel flag.
