# Forward an existing Feishu bot's events

Run one `agent-inbox feishu-watch` beside the existing Feishu bot. It reads that
bot's append-only `events.jsonl`, with records shaped as
`{kind, received_ms, event: {header, event: {sender, message}}}`. It does not use
Feishu credentials, subscribe to Feishu, or change the bot's event handling.

## Configuration

1. Add a fixed `[[agents]]` entry to the local relay with `name`, `description`
   and `sender_token_file`. Omit both `inbox` and `token_file`. The sender token
   must be an independent mode-600 file, just like other relay credentials.
   The fixed authenticated name is `<host>.feishubot` when the name is
   `feishubot`. Sender-only entries do not appear as available inboxes; attempts
   to address one return HTTP 405 after journaling.
2. Copy [feishu-watch.toml.example](../deploy/feishu-watch.toml.example), filling
   the event path, local relay, sender token, private state file and each chat's
   qualified `to` name. Paths are relative to the config file unless absolute.
   Enable `use_hub_routes = true` and set `fallback_agent` to the qualified
   agent that handles unregistered groups. Also set `fallback_sender_id` from
   trusted local owner configuration to the permitted sender's Feishu open_id, or
   a list of open_ids when the owner trusts more than one sender.
   Both fields are required together; leaving out the sender filter is an error,
   rather than enabling unrestricted forwarding. Each agent registers its own chat
   using the command below. Optional static `[[routes]]` entries support an
   existing deployment during migration; hub registrations take priority.
3. Give the watcher read access to the bot's event file and its own sender
   token, and write access to its private state directory. It needs neither
   the bot's app secret nor the relay's host token. Do not weaken the relay's
   private control socket to bridge OS users.
4. Adapt [the systemd unit](../deploy/agent-inbox-feishu-watch.service) to the
   service account, binary/config/state paths and local relay unit name. Keep
   only one watcher for a state file; a kernel lock rejects a second instance.

```sh
agent-inbox feishu-watch --config /absolute/path/watch.toml --once
agent-inbox feishu-watch --config /absolute/path/watch.toml
```

On first use, `start_at_end = true` skips existing history. Subsequent runs use
the saved cursor. Set it to false only when deliberately importing a fresh
journal with a fresh state file. Hub registrations are loaded live, without a
service restart. Restart the watcher only to change its local configuration.

## Agents register their own reporting chats

After creating a reporting group, use the current session's own sender token:

```sh
agent-inbox chat-route register --chat oc_example --hub 127.0.0.1:8831 --token-file /absolute/path/own.sender.token
agent-inbox chat-route list --hub 127.0.0.1:8831 --token-file /absolute/path/own.sender.token
agent-inbox chat-route remove --chat oc_example --hub 127.0.0.1:8831 --token-file /absolute/path/own.sender.token
```

Claude's `inbox_profile` result and Codex's current session status expose a
`chatRouteCommand` with current local paths. Existing reply recipes use the same
local relay and sender token. Always refresh the recipe instead of caching a
rotating credential path. Never read the token value into a prompt.

The relay replaces caller-supplied identity with its authenticated logical name.
The hub accepts only a `chat` field for changes: there is no caller-selected
target, forwarding hop, signature or approval flag. Repeating a registration for
the same agent is safe. A chat registered to another agent returns HTTP 409;
the existing agent must remove it before another agent registers it. Successful
operations return the chat, bound agent and visible registered/removed status.

The central hub persists registrations in mode-600 state. `chat_routes_file`
can select a path; the default is beside its directory state. Up to 4096 chats
are paged in batches of 64. A revision change during pagination aborts that
read, so the watcher keeps its source cursor and retries a coherent table.
Changing a directory name does not rename a chat registration: remove the old
mapping before renaming, then register using the new name. Registrations are
routing metadata, never user instructions or approvals.

Only unregistered group messages from a sender in `fallback_sender_id` go
to `fallback_agent`. The check uses the bot event's
`event.event.sender.sender_id.open_id`, after requiring sender type `user`.
Text claiming to be the owner and missing sender IDs cannot pass this filter.
Other participants' unregistered-group messages remain in the bot's existing
event log, advance the watcher cursor, and are not forwarded to an agent.
Registered groups continue to route other participants' messages as before.
This is a routing filter, not user authorization; receivers still fetch and
verify the original Feishu message before accepting an owner's instruction.

A registered but offline
agent's messages stay queued for it, rather than falling back to somebody else.
If an unsent fallback message gains a registration before delivery, it is
rerouted to that registered agent. Unconfirmed attempts are never rerouted or
automatically repeated. If hub routing cannot be read, ingestion stops before
advancing the source cursor; an outage must not make every chat look unregistered.

Only `im.message.receive_v1` events whose sender type is `user` and whose chat
has a route or fallback are forwarded. Private chats, bot messages and other
events are ignored. Text
is preserved; other content is forwarded as JSON. Each delivery cites the
original `feishu-message: om_<id>`, its source chat and type. The receiver must
fetch that original and apply its own owner verification rules. Each message
includes the complete verification rule, so older sessions can read their own
global instructions if the pinned owner ID is not yet in their context. No
owner ID is supplied by the bridge. The bridge does not turn other participants'
messages or card clicks into owner approval.
Replies belong in the source Feishu chat when authorized; the sender-only
service has no reply inbox.

## State and recovery

The mode-600 JSON state contains the source file identity, byte cursor, the
last 4096 message IDs and up to 256 pending deliveries. It contains message
content; protect it like the inbox journals. Partial final lines wait for the
writer to finish. File replacement/truncation restarts reading at the beginning
with recent message-ID deduplication. Malformed JSON, invalid routed content
or messages exceeding the inbox's 64 KiB limit stop visibly without advancing
past the bad record. Inspect the source before correcting it and restarting.

Directory availability is checked before sending. Messages addressed to an
offline or absent agent stay `queued`; one unavailable agent does not stop
other routes. Pending capacity stops ingestion without advancing the cursor.
Watch the service journal and state file for queued or unconfirmed work.

Pending records retain source sender IDs. Before sending or explicitly retrying
a fallback delivery, the watcher rechecks the current fallback recipient and
sender filter. Mismatched records, including legacy fallback records without
stored sender IDs, become `blocked` and remain in private queue state for
operator reconciliation; they are not silently discarded or sent based on text.
Registered-group queued work is unchanged. Inspect the original bot event to
reconcile blocked legacy work, and do not guess identity from the forwarded body.

Before each HTTP delivery, its phase is durably marked `unconfirmed`. HTTP
200/202 records acceptance; an error, disconnect or process crash preserves
the uncertain state and never triggers an automatic resend. Inspect the
receiver's inbox journal or queue first. If it was not accepted, stop the
watcher and explicitly retry the delivery ID:

```sh
agent-inbox feishu-watch --config /absolute/path/watch.toml --retry <delivery-id> --once
```

Then restart the service. HTTP 202 confirms durable acceptance by the inbox,
not that an agent has completed the work. Delivery is not exactly-once across
all failure modes: uncertainty requires reconciliation, and replaying more
than the recent 4096 IDs after source rotation can repeat older messages.
