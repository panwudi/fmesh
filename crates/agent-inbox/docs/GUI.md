# Optional local GUI

`agent-inbox gui` starts a small local HTTP interface on demand, opens the
default browser and stops when the command stops. It adds no resident service,
Node process or external web assets. The server binds loopback only and uses the
existing relay to list agents and send messages.

```sh
agent-inbox gui --hub 127.0.0.1:8831 --token-file /private/path/gui.sender.token
```

`AGENT_INBOX_RELAY` and `AGENT_INBOX_TOKEN_FILE` can provide the same values.
The default GUI address uses a free port on `127.0.0.1`; `--listen` can select a
fixed loopback address. The command prints the clean local URL and opens a
browser tab. Stop the command to close the local server.

Use a dedicated sender-only relay entry for the GUI if messages should have a
distinct sender name:

```toml
[[agents]]
name = 'gui'
description = 'Local GUI operator'
sender_token_file = 'tokens/gui.sender.token'
```

The interface shows the authenticated sender name when the relay supports
`/whoami`; an older relay shows that the name is unavailable. Agents are grouped
by computer, and the interface searches names and descriptions and shows the
result of each selection, refresh and send. An accepted transport response means
the destination accepted or queued the message; it does not prove the agent
acted on it. An uncertain send
keeps the draft so the sender can inspect the receiver before deciding whether
to resend.

The relay sender token remains in the Rust GUI process. The browser receives
only a one-time bootstrap secret in a URL fragment, which is removed from the
visible URL before exchange. The server then uses an HttpOnly, SameSite cookie
and a per-process action token. Neither the relay token nor the bootstrap secret
is printed by the command. The interface serves no third-party resources and
rejects nonlocal Host and cross-origin POST requests.
