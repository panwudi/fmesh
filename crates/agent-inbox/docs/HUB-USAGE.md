# Hub router

For new installations use [host relays and the directory](HOST-RELAY.md).
The host registry is `[[hosts]] name, relay, token_file`; a host token can claim
senders only inside its prefix. The legacy `peers`/`senders` form below is kept
for per-session migration. Both forms can coexist without namespace overlap.

The hub listens only on loopback. SSH forwards provide transport between hosts;
the binary does not configure SSH or install anything on a relay host.

```sh
agent-inbox hub --listen 127.0.0.1:<router-port> --config /absolute/path/hub.toml
```

```toml
journal = "hub.md"

[[senders]]
name = "laptop-codex"
token_file = "laptop-codex.hub.token"

[[senders]]
name = "relay-main"
token_file = "relay-main.hub.token"

[[peers]]
name = "relay-main"
inbox = "127.0.0.1:8817"
token_file = "relay-main.inbox.token"

[[peers]]
name = "laptop-codex"
inbox = "127.0.0.1:8819"
token_file = "laptop-codex.inbox.token"
```

All names and ports are configuration values. Names are stable logical labels,
case-sensitive, with 1..128 ASCII letters, digits, dots, underscores or hyphens,
starting with a letter or digit. They are independent of transient session names.
Token files and the optional journal path are relative to the TOML file unless
absolute. `--log` overrides the journal using the caller's path. Token files
must be regular mode-600 files. Sender hub tokens must be independent; duplicate
names and tokens fail startup. The configuration and credentials are loaded at
startup; restart the hub after changing them.

POST raw text to `/to/<logical-name>` with the sender's hub token. The hub ignores
caller-supplied sender headers, identifies the sender from its token, then sends
the peer's inbox token and the authenticated name as `X-Agent-From`. The same
router serves Claude and Codex inboxes.

The CLI sends this form without knowing the peer's address or inbox token:

```sh
agent-inbox send --hub 127.0.0.1:<local-forward-port> --to relay-main \
  --token-file /absolute/path/my.hub.token 'Hello from my logical sender.'
```

`--hub` and `--to` are required together and cannot be mixed with a direct inbox
address. `--from` is ignored in hub mode. The sender journal uses
`hub-token-holder` until the hub resolves the identity; the hub journal records
the authenticated name. `--log` and `AGENT_INBOX_TOKEN_FILE` work in either
mode, using the appropriate direct inbox token or sender hub token.

For Claude, configure `--hub <local-forward-address> --hub-token-file <my-token>`
on the `claude` role. These flags are paired and replace `--reply-thread`.
MCP instructions then include the reply command and use the incoming logical
`from` name as the destination. For Codex, put the equivalent `send --hub/--to`
reply command in that project's `AGENTS.md`. Names expecting replies must appear
in both the `senders` and `peers` tables.

| Status | Meaning |
| --- | --- |
| 200 | Peer accepted the message. |
| 202 | Peer queued it during channel initialization. |
| 400 | Invalid body, framing, or logical name. |
| 401 | Hub sender token was not accepted. |
| 404 | Unknown peer or route. |
| 500 | Hub journal failed; no forwarding was attempted. |
| 502 | Peer returned a rejection; the hub saved the message. |
| 503 | Peer offline or response unconfirmed; the hub saved the message. |

Every authenticated message with a valid destination name is synced to the hub
journal before lookup or forwarding, including unknown and offline peers. A
peer connection and response share a ten-second deadline; `send` allows 25
seconds for its full exchange. The hub never
replays or blindly retries a message; a peer may have received it before a
connection failure. Inspect the journal and receiver before resending.

`GET /` is a generic health check and does not disclose the routing table.
Bodies remain limited to 64 KiB. The existing request framing, header limits,
constant-time token comparison, and credential-free diagnostics are reused.
