# Windows Agent Mesh for a Codex desktop user

Goal: deliver coordination messages into a running Codex session on Windows, for
a user who installed only the Codex desktop app and no CLI. Today Agent Mesh is
Unix only (`package.sh` omits `agent-inbox` from the Windows package, and
`main.rs` gates `codex-watch` behind `#[cfg(unix)]`). This is the plan to lift
that, scoped to the smallest path that serves that user.

## What already works on Windows

- `agent-inbox` cross-compiles for `x86_64-pc-windows-gnu` today (lib and bin,
  zero errors), because the Unix-only code is behind `#[cfg(unix)]`. The Windows
  binary is a hollow shell: the gated code paths are compiled out.
- The delivery core is cross-platform. agent-inbox injects into Codex by
  spawning `codex app-server proxy --sock <path>` and relaying WebSocket bytes
  over the child's stdio (`proxy.rs`, `codex.rs`). The `codex` executable that
  does the actual socket work ships inside the Codex desktop app (on macOS at
  `~/.codex/packages/app-server-daemon/releases/<ver>/bin/codex`), so no CLI
  install is needed. agent-inbox never touches the Codex control socket itself.
- The relay transport is TCP on loopback (`relay.rs`, `TcpListener`/`TcpStream`),
  so receiving a message from a relay is already cross-platform.

## What must be ported

1. **`local_control` (the biggest piece).** It is a Unix-domain-socket server
   and client (`UnixListener`/`UnixStream`, peer-credential `same_user`,
   `private_dir` mode 0700). This is how `fmesh` and the watcher talk locally.
   Port to a Windows named pipe (`\\.\pipe\...`): a synchronous named-pipe
   server and client with an owner-only ACL and a peer-identity check. Rust std
   has no named-pipe server, so this needs a thin `windows-sys` dependency
   (stated reason: named pipes have no std equivalent; no async runtime is
   introduced). Keep the Unix path unchanged behind `#[cfg(unix)]`.

2. **`codex_watch` Unix internals.**
   - Signals `SIGINT`/`SIGTERM`/`SIGHUP` -> a Windows console control handler
     (`SetConsoleCtrlHandler`) for the same clean-stop behavior.
   - Process-group kill (negative pid, `CommandExt` `process_group`/`setsid`,
     `libc::kill`) -> a Windows Job Object so the Codex proxy child tree is
     terminated with the watcher.
   - `local_control::private_dir` mode 0700 -> a Windows ACL that grants only
     the current user, or accept the per-user profile location's default
     protection.

3. **`proxy.rs::check_socket_owner`.** Already `#[cfg(unix)]`-guarded and a no-op
   elsewhere. Add a real Windows check: the Codex control socket on Windows is an
   `AF_UNIX` path (Windows 10 1803+ supports `AF_UNIX`); verify the path exists
   and is under the user's own profile.

4. **Token-file permission checks** (`hub.rs` `PermissionsExt`) -> a Windows
   branch, or relax to an ACL check.

5. **`main.rs`** -> un-gate `codex-watch` (and `codex-self`) for Windows once the
   above compile and run. Leave `grok-watch` and `feishu-watch` Unix only for
   now; they are out of scope for this user.

6. **`package.sh`** -> add `agent-inbox` to the `windows-x86_64` binary list, and
   an `install.ps1` path that installs the Codex watcher for the current user
   (no service account, no admin).

Not required for this user: the full desktop daemon (`daemon.rs`) and its
relay-control Unix socket. The minimal shape is a standalone
`agent-inbox codex-watch` on Windows pointed at a relay over TCP.

## Hard constraint: no local Windows test environment

The build agent develops on macOS and has no Windows host. Every Windows path
(named pipes, Job Objects, console control handler, `AF_UNIX` on Windows, the
Codex desktop app's bundled `codex` and its control-socket location) can only be
validated on a real Windows machine. The named-pipe and process-tree code is
exactly the kind that compiles yet misbehaves until run. Plan for an iterative
loop with a Windows tester, not a single overnight drop. Do not ship an untested
daemon to a colleague's machine unattended.

## Open questions to answer on a Windows box first

- Where does the Codex desktop app place its `codex` executable and its
  app-server control socket on Windows, and does `codex app-server proxy --sock`
  behave the same there?
- Does the Codex desktop app run the app-server daemon the same way, so a
  watcher can find and attach to the live session?

Answering these on the tester's machine comes before writing the port, so the
Windows-specific code targets the real layout rather than a guess.
