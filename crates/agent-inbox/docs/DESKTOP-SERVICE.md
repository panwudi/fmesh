# One desktop startup service

Use `agent-inbox daemon` as the desktop's single startup entry. It owns the host
relay and optional Codex and Grok watchers, using the same binary for all
components.
Claude MCP inboxes still start with their sessions. Their setup is unchanged.
The central hub and a server-side Feishu watcher remain services on their
respective server; they are not extra desktop login entries.

## Setup

1. Keep the existing [host relay](HOST-RELAY.md) and optional
   [Codex watcher](CODEX-AUTO-JOIN.md) and
   [Grok watcher](GROK-AUTO-JOIN.md) configuration files. For a new machine,
   fill those files first, including paths, loopback ports and credentials.
2. Copy [daemon.toml.example](../deploy/daemon.toml.example) to the user's
   `~/.config/agent-inbox/daemon.toml`. Point it at the existing component files.
   Relative paths are resolved from this file. Omit watcher sections for a
   Claude-only machine. Each watcher must use this relay's listen address and
   the same control socket. `no_tunnel = true` explicitly selects a local-only
   relay or a separately managed tunnel.
3. Check the configuration without starting services or opening connections:

   ```sh
   agent-inbox daemon --config /absolute/path/daemon.toml --check
   ```

4. For a new install, run it once in the foreground:

   ```sh
   agent-inbox daemon --config /absolute/path/daemon.toml
   ```

   Stop that foreground process before installing the service. On an existing
   install, follow the migration below before starting the unified daemon.
5. Fill [the macOS launch agent](../deploy/io.agent-inbox.plist.example), or
   install [the Linux user service](../deploy/agent-inbox-desktop.service).
   Create the private state/log directories first. Keep one user startup entry
   named `io.agent-inbox` on macOS or `agent-inbox-desktop.service` on Linux.

All components run as the same OS user as Claude and Codex. No host token or
sender credential is moved into the daemon config. The existing relay remains
responsible for authentication and private local registrations.

## Migrate two existing startup entries

Keep the current listen port, relay config, watcher config, watcher state and
registry directory. Queue and journal files stay where they are.

1. Prepare the new binary, config, service file and log directory; run `--check`.
2. Disable and unload the standalone relay and Codex watcher services. Move
   their startup files outside the auto-start directory and retain them for
   rollback. Wait for their owned processes to stop and release their locks.
   Stop only these services, not Claude/Codex sessions or the Codex daemon.
3. Enable the unified startup entry. Do not run both old entries and the new
   daemon: component locks reject overlapping instances rather than replacing
   existing queues or stealing an inbox.
4. Verify one enabled startup entry, one host relay, one watcher and the
   expected agent directory. Claude inboxes rejoin after the relay restart.
   Codex and Grok workers reopen their existing per-session queues and rotate
   their per-session credentials; read the current reply recipe after cutover.
5. For rollback, stop the unified service and wait for its components to exit,
   then restore and enable the two retained service files. Never delete queue
   or journal state to resolve a startup error.

On macOS, `launchctl bootout` unloads a service, while moving its plist out of
`~/Library/LaunchAgents` prevents it loading at the next login. The unified
plist uses `KeepAlive`; use the service manager to stop it instead of killing
its process and expecting it to remain off.

## Ownership and recovery

The daemon holds a private single-instance state lock and starts only fixed
`relay` and `codex-watch` subcommands of its own executable. It does not use a
shell to build child commands. Existing standalone commands remain available
for advanced deployments and compatibility.

If one component exits, only that component restarts with backoff from one to
30 seconds. The other remains running. After a minute of healthy process
lifetime, its retry delay resets. Logs distinguish component start, exit and
retry. A running process does not by itself confirm remote connectivity; use
`who` and an actual message check for delivery.

SIGINT, SIGTERM and SIGHUP stop both components, with a shared ten-second
shutdown budget before killing only owned child process groups. Each component
monitors its actual daemon parent; a killed parent therefore cannot leave its
relay or watcher running indefinitely. Existing tunnel and Codex worker parent
guards handle their own descendants. The daemon never scans for or kills an
unrelated process to take a port or registry lock.
