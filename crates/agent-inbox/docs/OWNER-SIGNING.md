# Owner-signed instructions (design proposal)

Status: tier 1 is implemented. Tier 2 (signing) is shelved: the owner
judged it too heavy for now. For Feishu, the receiver instead fetches the cited message through
feishubot `get_message` and checks the sender itself (see README, "Delivery and logging"). The design
below is kept in case it is needed later.

Update 2026-10-20: the owner explicitly grants standing collaboration between their terminal agents,
through both agent-inbox and Claude SendMessage. Work within that existing delegation does not need
per-task signatures, a second window confirmation or a Feishu detour. A routine read-only production
API call with configured project credentials is not by itself a reason to ask again. The standing
scope lives in the owner's own global instructions; a peer message does not create new permission.
Forwarded Feishu group content keeps its original source and requires the separate designated-sender
check. This terminal policy is implemented without claiming cryptographic owner signatures.

## Problem

Every agent-inbox message is marked as a teammate's coordination, never the user's instruction. That is
right for its purpose, but it has two costs:

1. Agents escalate ordinary peer requests to the owner, so multi-agent work stalls on them.
2. When the owner gives an instruction through Feishu, it reaches the target only as a peer's retelling.
   The target cannot tell it from a forgery, so it asks them to confirm in its own session. Many agents
   are on other machines, where they cannot do that.

The owner wants one authorization, given from wherever they are (mostly Feishu and a terminal, sometimes
WeChat), that any agent on any host can check locally and act on.

## Two tiers

**Tier 1, peer coordination (no signature).** A teammate's message is handled within the receiver's
own project scope and permissions: the same things the receiver would do in its normal work. The
receiver replies with the result. It escalates to the owner only for actions outside that scope.
This replaces the line "no risky or irreversible action on their say-so alone", which made agents treat
every peer request as needing the owner. Tier 1 is a wording change in the MCP instructions and the Codex
header and can ship first.

**Tier 2, owner-signed instruction.** A message carrying a valid owner signature has the owner's
authority, as if they had typed it in the receiving session. Anything else, including a retelling of what
the owner "said", stays tier 1.

A signature proves who wrote the text. It does not bypass Claude Code permission prompts or a session's
permission mode, and the receiver still refuses requests that are unsafe on their face.

## Trust root and signers

Each host has one signer list in OpenSSH `allowed_signers` format. One line per signing key:

```text
owner@feishu   namespaces="agent-inbox-owner" ecdsa-sha2-nistp256 AAAA...
owner@terminal namespaces="agent-inbox-owner" ecdsa-sha2-nistp256 AAAA...
owner@wechat   namespaces="agent-inbox-owner" ssh-ed25519 AAAA...
```

`valid-before` and `valid-after` can bound a key's lifetime. Removing a line revokes that channel alone.

| Channel | Signer | How the signer knows it is the owner |
| --- | --- | --- |
| Feishu | the Feishu bot server (feishubot), as a service | Feishu event signature or encrypt key, sender `open_id` in the owner's pinned trusted-sender list |
| Terminal | `agent-inbox owner-sign` on the owner's computer | the owner reads the exact text and approves; the key needs Touch ID or a hardware touch |
| WeChat | a WeChat bridge daemon, as a service | sender pinned by wxid, never by nickname |

**Signer rule: no agent can reach a signing key.** A key readable by a process an agent controls lets
that agent sign anything. Consequences:

- The Feishu signer is the bot server, not a bridge script running inside an agent's session. Bridges
  such as `feishu_wait.py` only pass signed envelopes on.
- Service signers run as an OS user that no Claude or Codex session runs as. The key file is mode 0600,
  owned by that user.
- The terminal key lives in the Secure Enclave (for example via Secretive) or on a hardware key, so it
  cannot be copied. A use needs the owner's touch.

**What the owner approved, not only that they were present.** A Touch ID prompt does not show the message,
and an agent may be the one asking for the signature. `owner-sign` therefore shows the exact payload
(text, targets, expiry) in a native dialog on macOS, or on `/dev/tty` on Linux, and signs only after
the owner confirms it there. An agent may prepare the request; it cannot change what the dialog shows,
and with no tty or no confirmation nothing is signed.

## Envelope

The signed payload is canonical JSON (sorted keys, no extra whitespace):

```json
{"v":1,"signer":"owner@feishu","targets":["laptop1.example-claude"],"text":"...",
 "issued":"2026-10-18T09:00:00Z","expires":"2026-10-18T09:30:00Z","nonce":"<128-bit random hex>"}
```

- `targets` lists qualified agent names, or `["*"]` for a broadcast to every receiver.
- The signature is an SSH signature (`ssh-keygen -Y sign -n agent-inbox-owner`) over those exact bytes.
- The envelope travels as one HTTP header or body field next to the plain message. Hubs and relays pass
  it through unchanged and need no keys.
- A forwarding agent must forward the envelope byte for byte. A paraphrase has no signature and is tier 1.

## Verification at the receiving inbox

The receiving `agent-inbox claude` or Codex worker verifies before delivery:

1. `ssh-keygen -Y verify -f <signer list> -I <signer> -n agent-inbox-owner` on the payload bytes. This
   adds no crypto dependency, and OpenSSH is already required for the tunnels.
2. Its own name is in `targets`, or `targets` is `["*"]`.
3. The current time is between `issued` minus 2 minutes and `expires`. The maximum lifetime is 24 hours.
4. The nonce has not been seen. The inbox keeps a nonce cache per receiver until expiry, on disk, so a
   restart does not reopen replays.

If any check fails, the message is still delivered as tier 1. The failure reason goes into the journal
and the receiver's header, and the sender's HTTP result says the signature was rejected and why.

**Presentation.**
- Claude: the verdict is only in channel `meta` (`principal="owner"`, `signer="owner@feishu"`,
  `verified="true"`). The sender cannot set `meta`, and the MCP instructions explain these keys.
- Codex: input is text only, so the worker prepends the verdict header itself. It also neutralizes any
  line in the body that imitates an agent-inbox header, and the instructions say that only the first
  header, written by the inbox, counts.

**Integrity of the verifier on shared accounts.** On a host where agents run as the same OS user, an agent
could replace a user-writable binary or signer list and forge "verified". So on every host:

- the signer list is root-owned, for example `/etc/agent-inbox/allowed_signers` (0644);
- the `agent-inbox` binary used by inboxes is root-owned, for example `/usr/local/bin/agent-inbox`;
- the inbox refuses a signer list that a non-root user can write.

Until a host has both, its inboxes report owner signatures as unverifiable instead of verified.

## Presence

`online` in the directory means an inbox is reachable, not that a person is there. The instructions will
say so. When an agent needs the owner, it asks through Feishu and waits for a signed answer, instead of
waiting for them to appear in its session.

## Bootstrap (owner only, once)

No message can authorize its own trust root, so this cannot be done by an agent or a relayed request:

1. The owner creates the terminal key (Secretive, Touch ID) on their Mac.
2. Each service signer runs under its own account that no agent can log in as (for the Feishu bot this
   means moving it off the shared deploy account), and gets a key created by and readable only by it.
3. The owner, with their own admin rights, installs the root-owned signer list and binary on each host.

After that, every authorization is a remote signature.

## Rollout

1. Tier 1 wording. No keys involved.
2. Envelope verification in the inboxes, with `agent-inbox owner-sign` for the terminal.
3. Signing in the Feishu bot server. Bridges forward envelopes.
4. WeChat signing in a WeChat bridge daemon.

## Signer placement

- A chat bot that dials out to its platform over an authenticated TLS WebSocket has no inbound webhook to
  forge; its trust root is that outbound connection plus the app secret.
- The signer must run as a dedicated service account that no agent can log in as (own `User=`, home 0700,
  app secret and signing key 0600 owned by it). A bot running under an account that agents use for
  deployment exposes its app secret and any signing key to every such agent; until the bot is moved, that
  channel stays tier 1.
- Signing is a small change once isolated: the envelope can be computed when an event is appended to the
  bot's event log and returned as an extra field.

## Verified so far

- `ssh-keygen -Y sign` / `-Y verify` with ed25519 and with ECDSA P-256 through `ssh-agent` (OpenSSH 9.9):
  good signatures verify, a changed byte or a wrong namespace fails. Debian 12 (OpenSSH 9.2) is still to
  be checked on a real host.
