//! Durable peer-to-peer relay mailbox. A peer ACK means this host saved the message.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Router, flytomesh, http};
use crate::{Result, hub, journal, state};

const MAX_PENDING: usize = 256;
const MAX_RECEIPTS: usize = 1024;
const MAX_SEQUENCE_PAIRS: usize = 4096;
const MAX_STATE_BYTES: usize = 24 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Queued,
    Attempting,
    Unconfirmed,
}

#[derive(Clone, Serialize, Deserialize)]
struct Message {
    id: String,
    sequence: u64,
    sender: String,
    target: String,
    text: String,
    phase: Phase,
    #[serde(default)]
    notified: bool,
    #[serde(default)]
    skip: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    id: String,
    sequence: u64,
    sender: String,
    target: String,
    fingerprint: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    next_sequences: BTreeMap<String, u64>,
    accepted_sequences: BTreeMap<String, u64>,
    outgoing: VecDeque<Message>,
    incoming: VecDeque<Message>,
    receipts: VecDeque<Receipt>,
}

pub(super) struct Mailbox {
    path: PathBuf,
    data: Mutex<Snapshot>,
}

fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn sequence_key(sender: &str, target: &str) -> String {
    format!("{sender}>{target}")
}

fn next_sequence(high: u64) -> Result<u64> {
    high.checked_add(1)
        .ok_or_else(|| "peer sequence exhausted".into())
}

fn defer_peer(retries: &mut HashMap<String, (Instant, u64)>, host: &str) {
    let delay = retries
        .get(host)
        .map(|(_, previous)| (previous * 2).min(30))
        .unwrap_or(1);
    retries.insert(
        host.to_owned(),
        (Instant::now() + Duration::from_secs(delay), delay),
    );
}

impl Mailbox {
    pub(super) fn open(path: PathBuf) -> Result<Arc<Self>> {
        state::parent(&path)?;
        let mut data = if path.exists() {
            state::read_json::<Snapshot>(&path)?
        } else {
            Snapshot {
                version: 2,
                next_sequences: BTreeMap::new(),
                accepted_sequences: BTreeMap::new(),
                outgoing: VecDeque::new(),
                incoming: VecDeque::new(),
                receipts: VecDeque::new(),
            }
        };
        if data.version != 2
            || data.outgoing.len() + data.incoming.len() > MAX_PENDING
            || data.receipts.len() > MAX_RECEIPTS
            || data.next_sequences.len() > MAX_SEQUENCE_PAIRS
            || data.accepted_sequences.len() > MAX_SEQUENCE_PAIRS
        {
            return Err("relay mailbox version or size mismatch".into());
        }
        for (key, sequence) in data
            .next_sequences
            .iter()
            .chain(data.accepted_sequences.iter())
        {
            let (sender, target) = key.split_once('>').ok_or("invalid peer sequence key")?;
            hub::qualified(sender)?;
            hub::qualified(target)?;
            if *sequence == 0 {
                return Err("invalid peer sequence value".into());
            }
        }
        let mut ids = std::collections::HashSet::new();
        for item in data.outgoing.iter().chain(data.incoming.iter()) {
            if !state::is_id(&item.id) || !ids.insert(item.id.clone()) {
                return Err("relay mailbox has invalid or duplicate IDs".into());
            }
            crate::validate_message(&item.target, &item.sender, &item.text)?;
            hub::qualified(&item.sender)?;
            hub::qualified(&item.target)?;
            if item.sequence == 0
                && hub::qualified(&item.sender)?.0 != hub::qualified(&item.target)?.0
            {
                return Err("peer message lacks a sequence".into());
            }
            if item.sequence > 0 {
                let key = sequence_key(&item.sender, &item.target);
                let known = if data.outgoing.iter().any(|out| out.id == item.id) {
                    &data.next_sequences
                } else {
                    &data.accepted_sequences
                };
                if known.get(&key).is_none_or(|value| *value < item.sequence) {
                    return Err("peer message sequence exceeds saved high-water mark".into());
                }
            }
        }
        for item in &mut data.outgoing {
            if item.phase == Phase::Attempting {
                item.phase = Phase::Queued;
            }
        }
        for item in &mut data.incoming {
            if item.phase == Phase::Attempting {
                item.phase = Phase::Unconfirmed;
                item.notified = false;
            }
        }
        if serde_json::to_vec(&data)?.len() > MAX_STATE_BYTES {
            return Err("relay mailbox state exceeds size limit".into());
        }
        state::write_json(&path, &data)?;
        Ok(Arc::new(Self {
            path,
            data: Mutex::new(data),
        }))
    }

    fn edit<T>(&self, change: impl FnOnce(&mut Snapshot) -> Result<T>) -> Result<T> {
        let mut current = self.data.lock().map_err(|_| "relay mailbox lock failed")?;
        let mut next = current.clone();
        let result = change(&mut next)?;
        if serde_json::to_vec(&next)?.len() > MAX_STATE_BYTES {
            return Err("relay mailbox state full; new message remains in journal".into());
        }
        state::write_json(&self.path, &next)?;
        *current = next;
        Ok(result)
    }

    pub(super) fn enqueue(&self, sender: &str, target: &str, text: &str) -> Result<String> {
        let id = state::id()?;
        self.edit(|data| {
            if data.outgoing.len() + data.incoming.len() >= MAX_PENDING {
                return Err("relay mailbox full; message remains in journal".into());
            }
            let key = sequence_key(sender, target);
            if !data.next_sequences.contains_key(&key)
                && data.next_sequences.len() >= MAX_SEQUENCE_PAIRS
            {
                return Err("relay mailbox sender-target table full".into());
            }
            let sequence = next_sequence(data.next_sequences.get(&key).copied().unwrap_or(0))?;
            data.next_sequences.insert(key, sequence);
            data.outgoing.push_back(Message {
                id: id.clone(),
                sequence,
                sender: sender.to_owned(),
                target: target.to_owned(),
                text: text.to_owned(),
                phase: Phase::Queued,
                notified: false,
                skip: false,
            });
            Ok(())
        })?;
        Ok(id)
    }

    pub(super) fn receive(
        &self,
        id: &str,
        sequence: u64,
        sender: &str,
        target: &str,
        text: &str,
        log: &std::path::Path,
    ) -> Result<u16> {
        if !state::is_id(id) {
            return Err("invalid peer message ID".into());
        }
        crate::validate_message(target, sender, text)?;
        let local = hub::qualified(sender)?.0 == hub::qualified(target)?.0;
        if local != (sequence == 0) {
            return Err("invalid peer message sequence".into());
        }
        let existing = self.data.lock().map_err(|_| "relay mailbox lock failed")?;
        if existing.outgoing.iter().any(|item| item.id == id) {
            return Err("peer message ID conflicts with local outgoing message".into());
        }
        if let Some(item) = existing.incoming.iter().find(|item| item.id == id) {
            return if item.sequence == sequence
                && item.sender == sender
                && item.target == target
                && item.text == text
            {
                Ok(202)
            } else {
                Err("peer message ID conflicts with saved message".into())
            };
        }
        if let Some(item) = existing.receipts.iter().find(|item| item.id == id) {
            return if item.sequence == sequence
                && item.sender == sender
                && item.target == target
                && item.fingerprint == fingerprint(text)
            {
                Ok(200)
            } else {
                Err("peer message ID conflicts with receipt".into())
            };
        }
        if !local {
            let high = existing
                .accepted_sequences
                .get(&sequence_key(sender, target))
                .copied()
                .unwrap_or(0);
            if sequence <= high {
                return Ok(200);
            }
            if sequence != next_sequence(high)? {
                return Err("peer message sequence gap".into());
            }
        }
        drop(existing);
        journal::append(log, sender, target, text)?;
        self.edit(|data| {
            if data.outgoing.len() + data.incoming.len() >= MAX_PENDING {
                return Err("relay mailbox full; message remains in journal".into());
            }
            if data.outgoing.iter().any(|item| item.id == id)
                || data.incoming.iter().any(|item| item.id == id)
                || data.receipts.iter().any(|item| item.id == id)
            {
                return Err("concurrent peer message ID; retry same ID".into());
            }
            if !local {
                let key = sequence_key(sender, target);
                if !data.accepted_sequences.contains_key(&key)
                    && data.accepted_sequences.len() >= MAX_SEQUENCE_PAIRS
                {
                    return Err("relay mailbox peer sequence table full".into());
                }
                let high = data.accepted_sequences.get(&key).copied().unwrap_or(0);
                if sequence != next_sequence(high)? {
                    return Err("peer message sequence changed; retry same ID".into());
                }
                data.accepted_sequences.insert(key, sequence);
            }
            data.incoming.push_back(Message {
                id: id.to_owned(),
                sequence,
                sender: sender.to_owned(),
                target: target.to_owned(),
                text: text.to_owned(),
                phase: Phase::Queued,
                notified: false,
                skip: false,
            });
            Ok(())
        })?;
        Ok(202)
    }

    pub(super) fn skip(
        &self,
        id: &str,
        sequence: u64,
        sender: &str,
        target: &str,
        log: &std::path::Path,
    ) -> Result<u16> {
        if !state::is_id(id)
            || sequence == 0
            || hub::qualified(sender)?.0 == hub::qualified(target)?.0
        {
            return Err("invalid peer skip".into());
        }
        let key = sequence_key(sender, target);
        let high = self
            .data
            .lock()
            .map_err(|_| "relay mailbox lock failed")?
            .accepted_sequences
            .get(&key)
            .copied()
            .unwrap_or(0);
        if sequence <= high {
            return Ok(200);
        }
        if sequence != next_sequence(high)? {
            return Err("peer skip sequence gap".into());
        }
        journal::append(
            log,
            sender,
            target,
            &format!("Peer sender skipped message {id} at sequence {sequence}"),
        )?;
        self.edit(|data| {
            if !data.accepted_sequences.contains_key(&key)
                && data.accepted_sequences.len() >= MAX_SEQUENCE_PAIRS
            {
                return Err("relay mailbox peer sequence table full".into());
            }
            let high = data.accepted_sequences.get(&key).copied().unwrap_or(0);
            if sequence != next_sequence(high)? {
                return Err("peer skip sequence changed; retry same ID".into());
            }
            data.accepted_sequences.insert(key, sequence);
            Ok(())
        })?;
        Ok(200)
    }

    fn candidates(&self, incoming: bool) -> Vec<Message> {
        let Ok(data) = self.data.lock() else {
            return Vec::new();
        };
        let queue = if incoming {
            &data.incoming
        } else {
            &data.outgoing
        };
        let mut blocked = std::collections::HashSet::new();
        let mut candidates = Vec::new();
        for item in queue {
            if blocked.insert(&item.target) && item.phase == Phase::Queued {
                candidates.push(item.clone());
            }
        }
        candidates
    }

    fn phase(&self, id: &str, incoming: bool, from: Phase, to: Phase) -> Result<()> {
        self.edit(|data| {
            let queue = if incoming {
                &mut data.incoming
            } else {
                &mut data.outgoing
            };
            let item = queue
                .iter_mut()
                .find(|item| item.id == id && item.phase == from)
                .ok_or("relay mailbox message phase changed")?;
            item.phase = to;
            if to == Phase::Unconfirmed {
                item.notified = false;
            }
            Ok(())
        })
    }

    fn completed(&self, id: &str, incoming: bool) -> Result<()> {
        self.edit(|data| {
            let queue = if incoming {
                &mut data.incoming
            } else {
                &mut data.outgoing
            };
            let position = queue
                .iter()
                .position(|item| item.id == id && item.phase == Phase::Attempting)
                .ok_or("relay mailbox receipt does not match attempt")?;
            let item = queue.remove(position).unwrap();
            if incoming {
                data.receipts.push_back(Receipt {
                    id: item.id,
                    sequence: item.sequence,
                    sender: item.sender,
                    target: item.target,
                    fingerprint: fingerprint(&item.text),
                });
                while data.receipts.len() > MAX_RECEIPTS {
                    data.receipts.pop_front();
                }
            }
            Ok(())
        })
    }

    pub(super) fn status(&self) -> Result<Value> {
        let data = self.data.lock().map_err(|_| "relay mailbox lock failed")?;
        let rows = |queue: &VecDeque<Message>| {
            queue
                .iter()
                .map(|item| {
                    json!({"id":item.id,"sequence":item.sequence,"sender":item.sender,"target":item.target,"phase":item.phase,"notified":item.notified,"skip":item.skip})
                })
                .collect::<Vec<_>>()
        };
        Ok(json!({"outgoing":rows(&data.outgoing),"incoming":rows(&data.incoming)}))
    }

    pub(super) fn resolve(&self, id: &str, actor: &str, resend: bool) -> Result<()> {
        self.edit(|data| {
            let incoming = data.incoming.iter().any(|item| item.id == id);
            let queue = if incoming {
                &mut data.incoming
            } else {
                &mut data.outgoing
            };
            let position = queue
                .iter()
                .position(|item| item.id == id)
                .ok_or("unknown relay mailbox message")?;
            let item = &queue[position];
            if item.phase != Phase::Unconfirmed
                || (if incoming { &item.target } else { &item.sender }) != actor
            {
                return Err("only the owning agent may resolve an unconfirmed message".into());
            }
            if resend {
                queue[position].phase = Phase::Queued;
                queue[position].notified = false;
                queue[position].skip = false;
            } else {
                if incoming {
                    let item = queue.remove(position).unwrap();
                    data.receipts.push_back(Receipt {
                        id: item.id,
                        sequence: item.sequence,
                        sender: item.sender,
                        target: item.target,
                        fingerprint: fingerprint(&item.text),
                    });
                    while data.receipts.len() > MAX_RECEIPTS {
                        data.receipts.pop_front();
                    }
                } else {
                    queue[position].phase = Phase::Queued;
                    queue[position].notified = false;
                    queue[position].skip = true;
                }
            }
            Ok(())
        })
    }

    pub(super) fn start(
        self: &Arc<Self>,
        router: Arc<RwLock<Arc<Router>>>,
        running: Arc<AtomicBool>,
    ) -> Vec<std::thread::JoinHandle<()>> {
        let outbound = Arc::clone(self);
        let outbound_router = Arc::clone(&router);
        let outbound_running = Arc::clone(&running);
        let send = std::thread::spawn(move || {
            let mut retries: HashMap<String, (Instant, u64)> = HashMap::new();
            while outbound_running.load(Ordering::Acquire) {
                for item in outbound.candidates(false) {
                    let Ok((host, _)) = hub::qualified(&item.target) else {
                        continue;
                    };
                    if retries
                        .get(host)
                        .is_some_and(|(until, _)| Instant::now() < *until)
                    {
                        continue;
                    }
                    let path = format!(
                        "/mesh/{}/{}/{}/{}",
                        if item.skip { "skip" } else { "receive" },
                        item.id,
                        item.sequence,
                        item.target
                    );
                    let body = if item.skip { "{}" } else { &item.text };
                    let peer = outbound_router.read().ok().and_then(|router| {
                        router
                            .peer_route(host, &path, &item.sender, body)
                            .ok()
                            .flatten()
                    });
                    let Some(route) = peer else {
                        defer_peer(&mut retries, host);
                        continue;
                    };
                    if !flytomesh::reachable(route.address()) {
                        defer_peer(&mut retries, host);
                        continue;
                    }
                    if outbound
                        .phase(&item.id, false, Phase::Queued, Phase::Attempting)
                        .is_ok()
                    {
                        match route.send(&path, &item.sender, body, Duration::from_secs(20)) {
                            Ok((200 | 202, _)) => {
                                if outbound.completed(&item.id, false).is_err() {
                                    eprintln!("agent-inbox: peer receipt could not be saved");
                                }
                                retries.remove(host);
                            }
                            Ok((400 | 401 | 403 | 404 | 405 | 409 | 413, _)) => {
                                let _ = outbound.phase(
                                    &item.id,
                                    false,
                                    Phase::Attempting,
                                    Phase::Unconfirmed,
                                );
                                eprintln!(
                                    "agent-inbox: peer rejected message {}; inspect mailbox",
                                    item.id
                                );
                                retries.remove(host);
                            }
                            _ => {
                                let _ = outbound.phase(
                                    &item.id,
                                    false,
                                    Phase::Attempting,
                                    Phase::Queued,
                                );
                                defer_peer(&mut retries, host);
                            }
                        }
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        let inbound = Arc::clone(self);
        let inbound_running = Arc::clone(&running);
        let notice_router = Arc::clone(&router);
        let receive = std::thread::spawn(move || {
            while inbound_running.load(Ordering::Acquire) {
                for item in inbound.candidates(true) {
                    let agent = hub::qualified(&item.target).ok().and_then(|(host, name)| {
                        let router = router.read().ok()?;
                        if host != router.host {
                            return None;
                        }
                        router
                            .agents
                            .get(name)
                            .filter(|agent| agent.live())
                            .cloned()
                    });
                    if let Some(agent) = agent
                        && let (Some(address), Some(token)) = (&agent.inbox, &agent.token)
                        && inbound
                            .phase(&item.id, true, Phase::Queued, Phase::Attempting)
                            .is_ok()
                    {
                        match http::post_diagnostic(
                            address,
                            "/",
                            token,
                            Some(&item.sender),
                            &item.text,
                            Duration::from_secs(5),
                        ) {
                            Ok((200 | 202, _)) => {
                                if inbound.completed(&item.id, true).is_err() {
                                    eprintln!("agent-inbox: local receipt could not be saved");
                                }
                            }
                            Ok((401, _)) => {
                                // A rotating session may have replaced its inbox token before
                                // this POST. Authentication rejects it before delivery.
                                let _ =
                                    inbound.phase(&item.id, true, Phase::Attempting, Phase::Queued);
                            }
                            Err(error)
                                if error.to_string()
                                    == "cannot connect to inbox; message was not confirmed" =>
                            {
                                let _ =
                                    inbound.phase(&item.id, true, Phase::Attempting, Phase::Queued);
                            }
                            _ => {
                                let _ = inbound.phase(
                                    &item.id,
                                    true,
                                    Phase::Attempting,
                                    Phase::Unconfirmed,
                                );
                                eprintln!(
                                    "agent-inbox: local delivery {} unconfirmed; inspect mailbox",
                                    item.id
                                );
                            }
                        }
                        break;
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let notices = Arc::clone(self);
        let notice = std::thread::spawn(move || {
            while running.load(Ordering::Acquire) {
                for (item, incoming) in notices.notice_candidates() {
                    let actor = if incoming { &item.target } else { &item.sender };
                    let agent = hub::qualified(actor).ok().and_then(|(host, name)| {
                        let router = notice_router.read().ok()?;
                        if host != router.host {
                            return None;
                        }
                        router
                            .agents
                            .get(name)
                            .filter(|agent| agent.live())
                            .cloned()
                    });
                    if let Some(agent) = agent
                        && let (Some(address), Some(token)) = (&agent.inbox, &agent.token)
                    {
                        let text = format!(
                            "Peer message {} for {} is unconfirmed. Inspect the destination session before explicit resend or drop. Use your current sender token with: agent-inbox peer-queue <local-relay> --token-file <current-sender-token> --resend {} or --drop {}. Later messages to this target remain queued.",
                            item.id, item.target, item.id, item.id
                        );
                        if http::post_diagnostic(
                            address,
                            "/",
                            token,
                            Some("agent-inbox"),
                            &text,
                            Duration::from_secs(5),
                        )
                        .is_ok_and(|(status, _)| matches!(status, 200 | 202))
                        {
                            let _ = notices.mark_notified(&item.id, incoming);
                            break;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        });
        vec![send, receive, notice]
    }

    fn notice_candidates(&self) -> Vec<(Message, bool)> {
        let Ok(data) = self.data.lock() else {
            return Vec::new();
        };
        data.incoming
            .iter()
            .filter(|item| item.phase == Phase::Unconfirmed && !item.notified)
            .cloned()
            .map(|item| (item, true))
            .chain(
                data.outgoing
                    .iter()
                    .filter(|item| item.phase == Phase::Unconfirmed && !item.notified)
                    .cloned()
                    .map(|item| (item, false)),
            )
            .collect()
    }

    fn mark_notified(&self, id: &str, incoming: bool) -> Result<()> {
        self.edit(|data| {
            let queue = if incoming {
                &mut data.incoming
            } else {
                &mut data.outgoing
            };
            let item = queue
                .iter_mut()
                .find(|item| item.id == id && item.phase == Phase::Unconfirmed)
                .ok_or("relay mailbox notice no longer current")?;
            item.notified = true;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("agent-inbox-peer-mailbox-{}", state::id().unwrap())),
            )
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn uncertain_messages_require_the_owning_agent_and_dropped_ids_stay_deduplicated() {
        let temp = Temp::new();
        let mailbox = Mailbox::open(temp.0.join("queue.json")).unwrap();
        let id = mailbox
            .enqueue("alpha.alice", "beta.bob", "outgoing")
            .unwrap();
        mailbox
            .phase(&id, false, Phase::Queued, Phase::Attempting)
            .unwrap();
        mailbox
            .phase(&id, false, Phase::Attempting, Phase::Unconfirmed)
            .unwrap();
        assert!(mailbox.resolve(&id, "alpha.other", true).is_err());
        mailbox.resolve(&id, "alpha.alice", true).unwrap();
        assert_eq!(mailbox.status().unwrap()["outgoing"][0]["phase"], "queued");
        mailbox
            .phase(&id, false, Phase::Queued, Phase::Attempting)
            .unwrap();
        mailbox
            .phase(&id, false, Phase::Attempting, Phase::Unconfirmed)
            .unwrap();
        mailbox.resolve(&id, "alpha.alice", false).unwrap();
        assert_eq!(mailbox.status().unwrap()["outgoing"][0]["skip"], true);
        mailbox
            .phase(&id, false, Phase::Queued, Phase::Attempting)
            .unwrap();
        mailbox.completed(&id, false).unwrap();
        assert!(
            mailbox.status().unwrap()["outgoing"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let inbound_id = state::id().unwrap();
        let log = temp.0.join("messages.md");
        assert_eq!(
            mailbox
                .receive(&inbound_id, 1, "beta.bob", "alpha.alice", "incoming", &log)
                .unwrap(),
            202
        );
        mailbox
            .phase(&inbound_id, true, Phase::Queued, Phase::Attempting)
            .unwrap();
        mailbox
            .phase(&inbound_id, true, Phase::Attempting, Phase::Unconfirmed)
            .unwrap();
        assert!(mailbox.resolve(&inbound_id, "beta.bob", false).is_err());
        mailbox.resolve(&inbound_id, "alpha.alice", false).unwrap();
        assert_eq!(
            mailbox
                .receive(&inbound_id, 1, "beta.bob", "alpha.alice", "incoming", &log)
                .unwrap(),
            200
        );
        assert!(
            mailbox
                .receive(&inbound_id, 1, "beta.bob", "alpha.alice", "changed", &log)
                .is_err()
        );
        assert!(
            mailbox
                .receive(
                    &state::id().unwrap(),
                    3,
                    "beta.bob",
                    "alpha.alice",
                    "gap",
                    &log
                )
                .is_err()
        );
        let skipped = state::id().unwrap();
        assert_eq!(
            mailbox
                .skip(&skipped, 2, "beta.bob", "alpha.alice", &log)
                .unwrap(),
            200
        );
        let next = state::id().unwrap();
        assert_eq!(
            mailbox
                .receive(&next, 3, "beta.bob", "alpha.alice", "after skip", &log)
                .unwrap(),
            202
        );
        mailbox
            .edit(|data| {
                data.receipts.clear();
                Ok(())
            })
            .unwrap();
        drop(mailbox);
        let mailbox = Mailbox::open(temp.0.join("queue.json")).unwrap();
        assert_eq!(
            mailbox
                .receive(&inbound_id, 1, "beta.bob", "alpha.alice", "incoming", &log)
                .unwrap(),
            200
        );
        assert_eq!(
            mailbox.status().unwrap()["incoming"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
