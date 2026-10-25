# Tested Codex protocol contract, 0.162.1

Verified against `codex-cli 0.162.1` and the local app-server daemon 0.162.1 on
2026-10-14 by Codex. Protocol shapes were read from schemas generated with:

```sh
codex app-server generate-json-schema --experimental --out <temporary-directory>
```

The [official OpenAI App Server documentation](https://learn.chatgpt.com/docs/app-server)
describes these transports and RPC methods. The installed version's generated
schema is the tested contract for this implementation. Auto-updated versions
are accepted with a warning and checked through their actual RPC responses.

## Transport and initialization

`codex app-server proxy --sock <path>` relays bytes over stdin/stdout to the
daemon's Unix control socket. Those bytes are a standard HTTP WebSocket Upgrade
handshake, then one JSON-RPC message per WebSocket text frame. Codex omits the
`jsonrpc` field. Notifications have no `id`.

Requests use numeric `id`, `method`, and `params`. Responses must echo the
request ID and contain either `result` or `error`. Unrelated notifications,
server-to-client requests with a `method` and `id`, and WebSocket ping/pong
frames can arrive between requests and responses. Frames with a `method` are
skipped because they are not responses to this client.

The client sends `initialize` with `clientInfo` and
`capabilities.experimentalApi: true`. The response must contain string fields
`userAgent`, `platformFamily`, `platformOs`, and `codexHome`, as required by
`v1/InitializeResponse.json`. After a successful response, the client sends the
`initialized` notification.

## Thread and turn lookup

`thread/read` takes `threadId` and returns `thread`. The returned thread must
have the requested ID and a runtime `status.type`:

- `active`: a turn is in progress.
- `idle`: loaded with no active turn.
- `notLoaded`: must be resumed before starting work.
- `systemError`: delivery fails.

`thread/turns/list` takes `threadId`, `limit: 1`, `sortDirection: "desc"`, and
`itemsView: "notLoaded"`. `result.data[0]` must have a nonempty ID and
`status: "inProgress"` when steering. An empty page or a finished latest turn
causes a fresh thread read. This avoids full-history hydration and
does not read the receiving session's messages.

`thread/resume` takes `threadId` and `excludeTurns: true`. No model, instructions,
workspace, approval, or sandbox overrides are supplied. Its returned thread ID
and runtime status are checked before delivery.

## Delivery

`turn/steer` requires `threadId`, `expectedTurnId`, and `input`. The response
requires `turnId`, which must equal the expected ID. Known precondition failures
(`no active turn to steer`, `expected active turn id ...`) permit a fresh thread
read and retry. A rejected start with `thread already has an active or pending
turn` permits the same recovery. There are at most three retries after the
initial attempt, and a transport failure or unknown RPC error is never retried.

`turn/start` requires `threadId` and `input`. A successful response contains
`turn.id`. Other thread settings are inherited.

Text input has `type: "text"` and `text`. Optional `text_elements` defaults to
an empty array in the pinned schema. The message text is prefixed with a clear
teammate coordination label; it is not an approval from the user.

## Compatibility boundaries

The default path reads CLI and daemon versions before connecting and warns if
either differs from 0.162.1. A custom socket reads the CLI version and checks
response shapes. Unknown response
shapes, thread IDs, response IDs, statuses, and unexpected frame types fail
loudly. Successful acceptance is reported only after a matching RPC response.

A read-only live check verified the WebSocket handshake, initialization,
`thread/read`, and retrieval of the actual running turn ID. Fake-server tests
exercise delivery over a Rust stdio proxy. A release-binary loopback test on
2026-10-14 delivered the marker `steer-live-20261014` into the receiving session
while its turn was active, and received a matching `turn/steer` acceptance.
Session and turn IDs are intentionally omitted because they change between
runs.

The Codex HTTP inbox was also checked on 2026-10-14 using an automatically
allocated loopback port, a temporary mode-600 token, and the implementing
agent's own thread. `send` received HTTP 200, the daemon accepted `turn/steer`,
and `codex-http-live-20261014` arrived mid-turn. The listener was stopped and
its temporary token and journals were removed after verification.
