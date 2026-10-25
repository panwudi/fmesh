# Grok Build auto-join

`agent-inbox grok-watch` observes the resident sessions in Grok Build's shared
leader. Each exact session ID gets one local inbox and sender identity. The
watcher does not resume dormant sessions, create conversations, or attach to a
standalone TUI process that is not using the shared leader.

The adapter was built against Grok Build 1.0.70 and the ACP methods
`_x.ai/sessions/list` and `_x.ai/interject`. It validates response shapes and
stops advertising a session after a complete roster no longer contains it.
Active turns receive a mid-turn interjection; an idle resident session starts
a prompt turn through Grok's own interjection fallback. Grok's
[session loop](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-shell/src/session/acp_session_impl/run_loop.rs)
handles both cases in one method.

## Enable the shared leader

Grok Build's interactive TUI uses a private process by default. Set this in
`~/.grok/config.toml` for future sessions:

```toml
[cli]
use_leader = true
```

A TUI already running without the leader must exit and resume before the watcher
can see it. Do not start a second process that resumes the same live session to
simulate delivery. `grok leader list` and the leader socket show whether the
shared process is running. The watcher never starts a leader by itself when the
socket is absent.

Grok may also import Claude Code's MCP entries. If that makes it load
`agent-inbox-join`, disable only that entry with
`grok mcp disable agent-inbox-join`. The Grok watcher owns this runtime's inbox;
the Claude channel entry would create a second, misleading Claude-named row.
Other imported MCP servers can remain enabled.

Fill [the watcher example](../deploy/grok-watch.toml.example) with the local
relay, Grok binary, leader socket and private state paths. Check the live
protocol without registering any inboxes:

```sh
agent-inbox grok-watch --config /absolute/path/grok-watch.toml --check
```

Add `[grok_watch] config = '/absolute/path/grok-watch.toml'` to the existing
desktop daemon config to supervise it with the relay. The daemon checks that
the watcher uses the same loopback relay and private control socket. A
standalone service can also run `agent-inbox grok-watch --config <file>`.

## Delivery and recovery

An authenticated HTTP 202 means the message was journaled and saved in the
per-session FIFO. The worker sends `_x.ai/interject` with a persistent message
ID and records the ACP acknowledgement. This does not prove Grok acted on the
message; ask for a reply when the result matters. Every delivered message
includes the current reply, directory and routing-refresh commands. Token
contents never enter Grok context, only paths to private token files.

A definite `session not found` rejection is safely requeued and waits for the
session to become resident again. It has not entered Grok's interjection buffer.

If an interjection acknowledgement is lost or the watcher restarts after an
attempt, the queue marks that message `unconfirmed` and blocks later messages.
It does not automatically replay a possibly delivered request. Inspect the
current status and Grok session, then resolve the queue explicitly:

```sh
agent-inbox grok-self --session <session-id>
agent-inbox grok-queue <inbox-address> --token-file <current-inbox-token-file>
agent-inbox grok-queue <inbox-address> --token-file <current-inbox-token-file> --resend <message-id>
agent-inbox grok-queue <inbox-address> --token-file <current-inbox-token-file> --drop <message-id>
```

`grok-self` rejects a stale status file after its inbox exits. A leader outage
leaves already registered inboxes available for durable queueing. A complete
roster that removes a session unregisters it while preserving its queue and
journal on disk for a later resume.

The watcher accepts only Unix leader sockets owned by its OS user. Grok and
the watcher should run under the same account. Grok Build's ACP method names
are experimental; pin the tested version and verify a new version before
upgrading a live watcher.
