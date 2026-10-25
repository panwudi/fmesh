# Codex upgrades between turns

`agent-inbox upgrade-codex --config <toml>` checks the official stable Codex
release, holds local Codex inboxes, waits for an idle gap, updates the managed
daemon, verifies its response, then releases FIFO delivery. No new version means
a quiet exit without a hold, upgrade or notification. The npm CLI remains an
owner-managed package. After a daemon upgrade, a report gives the exact
`npm install -g @openai/codex@<daemon-version>` command when that CLI is older.
The manager never runs npm.

## Disable the independent updater first

The supported setting is `updater.autoUpdateEnabled=false` in
`~/.codex/app-server-daemon/settings.json` (or the corresponding existing Codex
home). The [upstream daemon documentation](https://github.com/openai/codex/blob/main/codex-rs/app-server-daemon/README.md)
and [settings parser](https://github.com/openai/codex/blob/main/codex-rs/app-server-daemon/src/settings.rs)
define it. It preserves explicit `daemon update` support. The enabled state is
applied at a daemon lifecycle boundary; editing the file alone does not prove
an already running updater has stopped.

1. Fill [upgrade-codex.toml.example](../deploy/upgrade-codex.toml.example) with
   absolute paths for the daemon owner. Choose the registry path below.
2. Run the settings helper:

   ```sh
   agent-inbox upgrade-codex --config /absolute/path/upgrade-codex.toml --disable-auto-update
   ```

   It atomically changes only that preference, preserving other settings. It
   does not restart or upgrade anything. Coordinate an application of the setting
   with the owner: upstream documents `codex app-server daemon restart`; on the
   tested 0.162.1 installation, idempotent `codex app-server daemon start` also
   stopped the updater loop without rotating the running daemon. Verify this on
   the target installation before enabling a schedule.
3. Confirm the preference is false, the updater process has stopped, and
   `codex app-server daemon version` still reports the intended running daemon.
   The manager refuses a missing/enabled preference or a live updater PID record.

The build session read the real local 0.162.1 settings as false and found no
updater PID record after the owner applied the change. No real restart or update
was performed for this implementation. Upgrade tests use fake processes and
app-servers exclusively.

To restore hourly automatic updates, first disable this manager's schedule,
set `updater.autoUpdateEnabled` to `true` (preserve other settings), and apply it
with an owner-coordinated daemon restart. An optional positive
`updater.updateIntervalMinutes` changes the independent updater's interval.

## Register every inbox

Upgrade and restart every `agent-inbox codex` process before scheduling the
manager. Older inbox binaries cannot participate in holds. Every inbox and the
manager run as the same user and use one registry directory for one daemon:

```sh
agent-inbox codex --thread THREAD_ID --listen 127.0.0.1:PORT \
  --token-file /absolute/path/codex.token \
  --registry-dir /absolute/path/to/state/codex-inboxes
```

The default registry is `$XDG_STATE_HOME/agent-inbox/codex-inboxes`, or
`~/.local/state/agent-inbox/codex-inboxes`. It is independent of `--log` and
`--queue-file`, so inboxes from different projects are discovered together.
Explicit `--registry-dir` must match the manager config. A custom `--sock` is
recorded and must match the daemon's reported socket; mixed daemons require
separate registries/configs. Remote or other-user daemons are outside this
manager's scope. All clients that can inject work must cooperate with scheduling.

Each mode-600 record contains an instance ID, local address, thread, socket and
token-file path, never the token value or message text. A kernel lock identifies
live registrations; crashed inbox records are pruned without trusting PID reuse.
Authenticated controls verify the instance before holding/releasing, and status
also verifies the registered thread. Registry maintenance contacts loopback only.

## Upgrade flow and limits

- The default release check uses `curl` with bounded connection/total timeouts
  against `https://api.github.com/repos/openai/codex/releases/latest`. A configured
  `latest_command` can supply stable JSON metadata from an owner-managed source.
  Commands are argument arrays, without shell interpretation. Drafts, prereleases
  and unrecognized versions fail before a hold.
- A kernel lock prevents overlapping managers. A persistent startup barrier
  covers inboxes started during the wait/update. Existing inboxes durably save
  the hold and finish any already submitted delivery before acknowledging a
  quiescent state. HTTP messages remain journaled and queued in FIFO order.
- Two idle snapshots and a fresh final check page through all loaded daemon
  threads. Unknown statuses, failed reads and incomplete pagination prevent the
  update. The default maximum gap wait is 7200 seconds. When it runs out, the
  manager releases holds and reports `skippedBusy`. A bounded in-flight control
  or RPC can extend that deadline by a few seconds.
- `codex app-server daemon update --yes` performs the supported installation and
  restart. The manager waits for the expected newer running version and a real
  app-server initialize response before releasing the hold. It verifies the
  automatic-update preference stayed disabled. There is no extra blind restart.
- There is no atomic global turn gate in the tested protocol. A human, TUI or
  external client can start work between the idle check and update. The daemon
  drains such work, but its shutdown grace is bounded and a long turn may be
  interrupted. Inbox FIFO/reconciliation preserves messages through the restart.
  The report lists loaded CLI-origin threads and terminal Codex processes as
  `may need codex resume`. Loaded threads are candidates, not proof of a live
  TUI attachment; the protocol has no supported attachment census.
- Error paths attempt every hold release, including requests whose ack was lost.
  A failed update or command timeout is loud; inspect the daemon before retrying,
  because a lifecycle operation accepted by Codex can continue after its CLI
  exits. Queues retain their ordinary draining/refused retry behavior.

Holds are persisted in each queue with a wall-clock expiry. The manager lease
expires after the maximum gap wait, command/ready budgets and 300 seconds of
grace, always within 24 hours. This recovers from a killed manager. A later upgrade attempt
reuses the maintenance lease under the kernel lock and releases prior crash
holds while preserving unrelated manual holds. Normal releases wake workers
immediately. A restarted inbox keeps its manual holds and queued client IDs. If
it was down when the manager released its peers, startup clears the known
maintenance lease against the shared barrier before starting the worker.

## Reports and manual recovery

`last-upgrade.json` in the registry is saved mode 600 before the notify hook.
It contains the old/new/stable versions, hold duration, optional npm upgrade
command, TUI candidates, outcome, lease expiry and any failed hold releases.
It contains no tokens or message bodies. Configure `notify_command` and
`notify_args` to run a generic executable with one JSON report on stdin.
Without a hook, the JSON goes to stderr. Hooks have a 10-second deadline; failure
returns nonzero while the durable report remains available.

Inspect an inbox using its local token:

```sh
agent-inbox codex-queue 127.0.0.1:PORT --token-file /absolute/path/codex.token
```

If a release failed, first stop/inspect the manager and any accepted lifecycle
operation. Wait for the lease to expire or explicitly release the reported lease:

```sh
agent-inbox codex-queue 127.0.0.1:PORT --token-file /absolute/path/codex.token \
  --release-hold LEASE_FROM_REPORT
```

This only releases that hold. Existing unconfirmed-message controls remain
`--resend MESSAGE_ID` and `--drop MESSAGE_ID`; see [resilience](CODEX-RESILIENCE.md).

## Schedule as the daemon owner

Ship definitions are examples, not installed services. Fill absolute paths and
create state/log directories first. Run one manager per daemon/user registry.

- macOS: [launchd example](../deploy/io.agent-inbox.upgrade-codex.plist.example)
  runs hourly at minute 17, with a PATH that includes Homebrew. Load it only after
  the updater and inbox checks above. It has no KeepAlive or RunAtLoad loop.
- Linux: copy the [user service](../deploy/agent-inbox-upgrade-codex.service) and
  [timer](../deploy/agent-inbox-upgrade-codex.timer) into `~/.config/systemd/user/`,
  then enable the timer with `systemctl --user enable --now agent-inbox-upgrade-codex.timer`.
  It runs at minute 17. If increasing manager time budgets, also increase the
  service timeout beyond their sum. The user manager must have the same daemon
  home, PATH and state paths as the inboxes.

No jobs run at minute 0 or 30. The manager never modifies another project's inbox
configuration or installs anything on the central relay host.
