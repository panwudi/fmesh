# Codex inbox resilience

`agent-inbox codex` journals and durably queues each authenticated message before
returning HTTP 202. That confirms storage, not a completed Codex delivery.
Messages to a thread are delivered FIFO on a background worker. Refused
connections, pre-send transport failures and explicit `Server is draining`
(-32600) responses retry on fresh connections with 1, 2, 4, 8, 16, then
30-second backoff. New messages do not shorten a pending retry's wait.

Each queue entry has a persistent UUID sent as `clientUserMessageId` on both
`turn/start` and `turn/steer`. This field and history's `userMessage.clientId`
were verified against the local 0.162.1 generated schemas. The current daemon's
`thread/items/list` page shape was checked read-only against this session;
no turn was started, steered or interrupted by that check.

## Lost acknowledgements and restart recovery

The inbox persists its attempt before sending. A restart during an attempt or
a missing acknowledgement triggers history reconciliation on a fresh daemon
connection, resuming an unloaded thread with its existing settings if needed.
Pagination scans the whole thread, not just its most recent turn.

- A matching `clientId` confirms delivery and records its containing turn ID.
- Successful, complete history with no match permits a resend with the same ID.
- A failed page, malformed page, repeated cursor or unavailable history keeps
  the entry unconfirmed. It blocks later entries to that thread. Other threads'
  inboxes remain independent.

The complete-history rule is the owner's accepted recovery policy: after the
daemon is back, an accepted input is expected to have been persisted. The
inbox does not assume that the server provides a separate idempotent-submit
guarantee. Protocol or non-draining RPC rejections require explicit resolution.

Unconfirmed/blocked states are appended to the readable message journal and
sent to the notify hook with the message ID, reason, FIFO impact and exact
resend/drop commands. With no external hook configured the structured notice
goes to stderr. The notice contains no message body or token value.

## Configuration and operation

Optional inbox flags:

```text
--queue-file <path>       Persistent queue snapshot, mode 600
--notify-command <path>   Executable invoked for blocked-delivery notices
--notify-arg <arg>        Repeatable static argument for the executable
```

The default queue is `codex-<hex thread ID>.queue.json` beside the message
journal. Keep it across process restarts, along with its separate persistent
`.queue-lock` file. One inbox owns a queue at a time. Snapshots are atomically
replaced and synced; state, queue locks and temporary snapshots are mode 600.
The queue holds up to 256 pending messages. A full queue or failed persistence
returns 503 with an unconfirmed-acceptance warning; the journal remains the
copy to inspect before resending. The last 128 completion/drop receipts are
available in status. Do not delete queue state to resolve an uncertain input.

The external notify executable receives one JSON notice on stdin, with a
10-second budget. It should read stdin and complete synchronously. Configure
the owner's preferred notification destination in that executable; no channel
or vendor is built into the inbox.

Inspect and resolve through the authenticated local control endpoint:

```sh
agent-inbox codex-queue <inbox-address> --token-file <inbox.token>
agent-inbox codex-queue <inbox-address> --token-file <inbox.token> --resend <message-id>
agent-inbox codex-queue <inbox-address> --token-file <inbox.token> --drop <message-id>
```

Explicit resend reuses the existing client ID. Drop stops further delivery
attempts for that entry; it cannot undo an input the daemon already accepted.
Only blocked/unconfirmed entries can be resolved. HTTP control routes require
the inbox token and do not expose queued message bodies.

## Verification

Fakes cover draining during initialize/start/steer, refused-connection recovery,
FIFO ordering, queue and in-flight restart recovery, a lost ack found in
history, a complete paginated absence followed by resend with the same ID,
an incomplete page that blocks FIFO and invokes the notify hook, and explicit
resend/drop. Authentication and private-file checks remain enforced.
