//! Experimental host discovery and per-request authentication over FlytoMesh.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, Ordering},
};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde::Deserialize;

use crate::mesh_membership::{AuthoritySet, Envelope, Manifest, Member, Store, parse_envelope};
use crate::mesh_pair::{HostSecret, MeshRequest, ReplayGuard};
use crate::mesh_roster::{self, Roster, Status};
use crate::{Result, http, hub};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub network: String,
    pub host_key_file: PathBuf,
    pub membership_state_file: PathBuf,
    pub replay_state_file: PathBuf,
    pub bootstrap_manifest_file: Option<PathBuf>,
    pub authorities: AuthoritySet,
    pub flytomesh_admin_key: String,
    pub status_file: Option<PathBuf>,
    pub roster_file: Option<PathBuf>,
    pub admin_file: Option<PathBuf>,
    pub listen_port: Option<u16>,
    pub service_directory: Option<PathBuf>,
    pub service_discovery: Option<bool>,
}

pub struct DynamicMesh {
    host: String,
    network: String,
    secret: HostSecret,
    membership: Mutex<Store>,
    replay: ReplayGuard,
    pinned_admin_key: [u8; 32],
    status_file: PathBuf,
    roster_file: PathBuf,
    admin_file: PathBuf,
    update_file: Option<PathBuf>,
    port: u16,
    service_directory: PathBuf,
    service_discovery: bool,
    listener_port: AtomicU16,
}

fn relative(config_path: &Path, value: PathBuf) -> PathBuf {
    hub::relative_to(config_path, value)
}

impl DynamicMesh {
    pub fn load(config: &Config, config_path: &Path, host: &str) -> Result<Arc<Self>> {
        hub::validate_component(host)?;
        let secret = HostSecret::read(&relative(config_path, config.host_key_file.clone()))?;
        let mut membership = Store::open(
            relative(config_path, config.membership_state_file.clone()),
            config.network.clone(),
            config.authorities.clone(),
        )?;
        if membership.highest_revision() == 0
            && let Some(path) = &config.bootstrap_manifest_file
        {
            let bytes = std::fs::read(relative(config_path, path.clone()))?;
            let envelope = parse_envelope(&bytes)?;
            membership.accept(&envelope, chrono::Utc::now().timestamp())?;
        }
        let public: [u8; 32] = STANDARD
            .decode(&config.flytomesh_admin_key)
            .map_err(|_| "invalid pinned FlytoMesh administrator key")?
            .try_into()
            .map_err(|_| "pinned FlytoMesh administrator key must be 32 bytes")?;
        let port = config.listen_port.unwrap_or(8832);
        if port == 0 {
            return Err("dynamic mesh listener needs a nonzero port".into());
        }
        Ok(Arc::new(Self {
            host: host.to_owned(),
            network: config.network.clone(),
            secret,
            membership: Mutex::new(membership),
            replay: ReplayGuard::open(relative(config_path, config.replay_state_file.clone()))?,
            pinned_admin_key: public,
            status_file: config
                .status_file
                .clone()
                .map(|path| relative(config_path, path))
                .unwrap_or_else(|| "/var/run/flytomesh.status".into()),
            roster_file: config
                .roster_file
                .clone()
                .map(|path| relative(config_path, path))
                .unwrap_or_else(|| "/var/run/flytomesh.roster".into()),
            admin_file: config
                .admin_file
                .clone()
                .map(|path| relative(config_path, path))
                .unwrap_or_else(|| "/var/run/flytomesh.admin.pub".into()),
            update_file: config
                .bootstrap_manifest_file
                .clone()
                .map(|path| relative(config_path, path)),
            port,
            service_directory: config
                .service_directory
                .clone()
                .map(|path| relative(config_path, path))
                .unwrap_or_else(|| "/var/run/flytomesh.services".into()),
            service_discovery: config.service_discovery.unwrap_or(false),
            listener_port: AtomicU16::new(0),
        }))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn runtime_key(&self) -> (&Path, &Path, &Path, u16, &Path, bool) {
        (
            &self.status_file,
            &self.roster_file,
            &self.admin_file,
            self.port,
            &self.service_directory,
            self.service_discovery,
        )
    }

    pub fn service_directory(&self) -> &Path {
        &self.service_directory
    }

    pub fn service_discovery(&self) -> bool {
        self.service_discovery
    }

    pub fn set_listener_port(&self, port: u16) {
        self.listener_port.store(port, Ordering::Release);
    }

    pub fn current_manifest(&self) -> Option<Manifest> {
        self.membership
            .lock()
            .ok()?
            .manifest(chrono::Utc::now().timestamp())
            .cloned()
    }

    pub fn cached_manifest(&self) -> Option<Manifest> {
        self.membership.lock().ok()?.cached_manifest().cloned()
    }

    pub fn membership_expired(&self) -> bool {
        self.cached_manifest().is_some() && self.current_manifest().is_none()
    }

    pub fn membership_timing(&self) -> Option<(u64, i64)> {
        self.cached_manifest()
            .map(|manifest| (manifest.revision, manifest.expires))
    }

    pub fn status(&self) -> serde_json::Value {
        let cached = self.cached_manifest();
        serde_json::json!({
            "host":self.host,
            "network":self.network,
            "membership_revision":cached.as_ref().map(|manifest| manifest.revision),
            "token_epoch":cached.as_ref().map(|manifest| manifest.token_epoch),
            "expires_at":cached.as_ref().map(|manifest| manifest.expires),
            "membership_valid":self.current_manifest().is_some(),
            "roster_revision":self.roster_status().ok().map(|(roster,_)| roster.version),
            "known_hosts":self.configured_hosts(),
            "service_discovery":self.service_discovery,
            "listener_port":match self.listener_port.load(Ordering::Acquire) { 0 => None, port => Some(port) },
        })
    }

    pub fn current_envelope(&self) -> Option<Envelope> {
        self.membership.lock().ok()?.current_envelope().cloned()
    }

    pub fn accept_envelope(&self, envelope: &Envelope) -> Result<bool> {
        if envelope.payload.len() > 64 * 1024 {
            return Err("membership payload exceeds 64 KiB".into());
        }
        let payload = STANDARD_NO_PAD
            .decode(&envelope.payload)
            .map_err(|_| "invalid membership payload encoding")?;
        let offered: Manifest =
            serde_json::from_slice(&payload).map_err(|_| "invalid membership payload")?;
        let mut store = self
            .membership
            .lock()
            .map_err(|_| "membership lock failed")?;
        if offered.revision < store.highest_revision() {
            return Ok(false);
        }
        store.accept(envelope, chrono::Utc::now().timestamp())
    }

    pub fn poll_update_file(&self) -> Result<bool> {
        let Some(path) = &self.update_file else {
            return Ok(false);
        };
        if std::fs::metadata(path)?.len() > 64 * 1024 {
            return Err("membership update file exceeds 64 KiB".into());
        }
        let bytes = std::fs::read(path)?;
        self.accept_envelope(&parse_envelope(&bytes)?)
    }

    pub fn gossip_targets(&self) -> Vec<String> {
        let Some(manifest) = self.cached_manifest() else {
            return Vec::new();
        };
        let Ok((roster, status)) = self.roster_status() else {
            return Vec::new();
        };
        manifest
            .members
            .iter()
            .filter(|member| member.host != self.host)
            .filter_map(|member| {
                self.advertised_address(&roster, &status, member.mesh_node_id?)
                    .ok()
                    .flatten()
                    .map(|address| address.to_string())
            })
            .collect()
    }

    fn advertised_address(
        &self,
        roster: &Roster,
        status: &Status,
        id: u16,
    ) -> Result<Option<SocketAddr>> {
        if self.service_discovery {
            Ok(
                mesh_roster::peer_service_address(roster, status, id, "agent-inbox")?
                    .map(SocketAddr::V4),
            )
        } else {
            Ok(mesh_roster::peer_address(roster, status, id)?
                .map(|ip| SocketAddr::from((ip, self.port))))
        }
    }

    pub fn configured_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<_> = self
            .cached_manifest()
            .map(|manifest| {
                manifest
                    .members
                    .into_iter()
                    .filter(|member| member.host != self.host)
                    .map(|member| member.host)
                    .collect()
            })
            .unwrap_or_default();
        hosts.sort();
        hosts
    }

    pub fn has_peer(&self, host: &str) -> bool {
        self.cached_manifest().is_some_and(|manifest| {
            manifest
                .members
                .iter()
                .any(|member| member.host == host && host != self.host)
        })
    }

    fn roster_status(&self) -> Result<(Roster, Status)> {
        let roster = mesh_roster::load_public_roster(
            &self.roster_file,
            &self.admin_file,
            &self.pinned_admin_key,
        )?;
        let status = mesh_roster::load_status(&self.status_file)?;
        if roster.version != status.roster_version {
            return Err("FlytoMesh roster/status revision mismatch".into());
        }
        if roster.nodes.get(&status.local.id) != Some(&status.local) {
            return Err("FlytoMesh local identity differs from signed roster".into());
        }
        Ok((roster, status))
    }

    fn members(&self, peer_host: &str) -> Result<(Manifest, Member, Member)> {
        let manifest = self
            .current_manifest()
            .ok_or("membership missing or expired")?;
        let own = manifest
            .members
            .iter()
            .find(|member| member.host == self.host)
            .ok_or("this host is not in signed membership")?
            .clone();
        let peer = manifest
            .members
            .iter()
            .find(|member| member.host == peer_host && peer_host != self.host)
            .ok_or("peer is not in signed membership")?
            .clone();
        if own.transport_key != STANDARD_NO_PAD.encode(self.secret.public_key()) {
            return Err("local transport key differs from signed membership".into());
        }
        Ok((manifest, own, peer))
    }

    fn peer_identity(&self, own: &Member, peer: &Member) -> Result<Option<Ipv4Addr>> {
        let (roster, status) = self.roster_status()?;
        if own.mesh_node_id != Some(status.local.id) {
            return Err("local mesh node ID differs from signed membership".into());
        }
        let peer_id = peer.mesh_node_id.ok_or("peer has no mesh node ID")?;
        mesh_roster::peer_address(&roster, &status, peer_id)
    }

    fn pair_keys(&self, manifest: &Manifest, peer: &Member) -> Result<crate::mesh_pair::PairKeys> {
        let key: [u8; 32] = STANDARD_NO_PAD
            .decode(&peer.transport_key)
            .map_err(|_| "invalid peer transport key")?
            .try_into()
            .map_err(|_| "peer transport key must be 32 bytes")?;
        self.secret.derive_keys(
            &self.network,
            &self.host,
            &peer.host,
            key,
            manifest.token_epoch,
        )
    }

    pub fn local_binding(&self) -> Option<(Ipv4Addr, u64)> {
        let manifest = self.cached_manifest()?;
        let own = manifest
            .members
            .iter()
            .find(|member| member.host == self.host)?;
        let (roster, status) = self.roster_status().ok()?;
        (own.mesh_node_id == Some(status.local.id)
            && own.transport_key == STANDARD_NO_PAD.encode(self.secret.public_key())
            && roster.nodes.get(&status.local.id) == Some(&status.local))
        .then_some((status.local.ip, status.roster_version))
    }

    pub fn target(
        &self,
        peer_host: &str,
        request: MeshRequest<'_>,
    ) -> Result<Option<(String, String)>> {
        let (manifest, own, peer) = self.members(peer_host)?;
        let Some(ip) = self.peer_identity(&own, &peer)? else {
            return Ok(None);
        };
        let (roster, status) = self.roster_status()?;
        let Some(address) = self.advertised_address(
            &roster,
            &status,
            peer.mesh_node_id.ok_or("peer has no mesh node ID")?,
        )?
        else {
            return Ok(None);
        };
        if address.ip() != ip {
            return Err("advertised service IP differs from signed FlytoMesh peer".into());
        }
        let auth = self
            .pair_keys(&manifest, &peer)?
            .sign_request(request, chrono::Utc::now().timestamp())?;
        Ok(Some((address.to_string(), auth)))
    }

    pub fn authenticate(
        &self,
        header: &[u8],
        request: MeshRequest<'_>,
        source: SocketAddr,
    ) -> Option<http::SenderIdentity> {
        let text = std::str::from_utf8(header).ok()?;
        let peer_host = text.split_whitespace().nth(1)?;
        let (manifest, own, peer) = self.members(peer_host).ok()?;
        let expected_ip = self.peer_identity(&own, &peer).ok()??;
        if source.ip() != expected_ip {
            return None;
        }
        self.pair_keys(&manifest, &peer)
            .ok()?
            .verify_request(
                header,
                request,
                chrono::Utc::now().timestamp(),
                &self.replay,
            )
            .ok()?;
        Some(http::SenderIdentity::HostPrefix(peer.host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn signer() -> Ed25519KeyPair {
        let bytes = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(bytes.as_ref()).unwrap()
    }

    #[test]
    fn signed_members_and_roster_route_without_peer_list_or_pair_tokens() {
        let root =
            std::env::temp_dir().join(format!("dynamic-mesh-{}", crate::state::id().unwrap()));
        let alpha_key = HostSecret::generate(&root.join("alpha.key")).unwrap();
        let beta_key = HostSecret::generate(&root.join("beta.key")).unwrap();
        let roots = [signer(), signer()];
        let mut authority_keys: Vec<String> = roots
            .iter()
            .map(|root| STANDARD_NO_PAD.encode(root.public_key().as_ref()))
            .collect();
        authority_keys.sort();
        let authorities = AuthoritySet {
            threshold: 2,
            keys: authority_keys,
        };
        let now = chrono::Utc::now().timestamp();
        let manifest = Manifest {
            version: 1,
            network: "team".into(),
            revision: 1,
            token_epoch: 1,
            not_before: now - 60,
            expires: now + 3600,
            authorities: authorities.clone(),
            members: vec![
                Member {
                    host: "alpha".into(),
                    mesh_node_id: Some(1),
                    transport_key: STANDARD_NO_PAD.encode(alpha_key.public_key()),
                },
                Member {
                    host: "beta".into(),
                    mesh_node_id: Some(2),
                    transport_key: STANDARD_NO_PAD.encode(beta_key.public_key()),
                },
            ],
        };
        let payload = serde_json::to_vec(&manifest).unwrap();
        let mut signed = b"agent-inbox-membership-v1\0".to_vec();
        signed.extend_from_slice(&payload);
        let envelope = Envelope {
            payload: STANDARD_NO_PAD.encode(&payload),
            signatures: roots
                .iter()
                .map(|root| crate::mesh_membership::ManifestSignature {
                    public_key: STANDARD_NO_PAD.encode(root.public_key().as_ref()),
                    signature: STANDARD_NO_PAD.encode(root.sign(&signed).as_ref()),
                })
                .collect(),
        };
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&envelope).unwrap(),
        )
        .unwrap();
        let flyto_admin = signer();
        let flyto_public: [u8; 32] = flyto_admin.public_key().as_ref().try_into().unwrap();
        std::fs::write(
            root.join("admin.pub"),
            format!("{}\n", STANDARD.encode(flyto_public)),
        )
        .unwrap();
        let mesh_alpha = STANDARD.encode([1u8; 32]);
        let mesh_beta = STANDARD.encode([2u8; 32]);
        let roster_body = format!(
            "flytomesh-roster 1\nversion 14\nnode 1 alpha 10.249.0.1 {mesh_alpha}\nnode 2 beta 10.249.0.2 {mesh_beta}\n"
        );
        let roster = format!(
            "{roster_body}sig {}\n",
            STANDARD.encode(flyto_admin.sign(roster_body.as_bytes()).as_ref())
        );
        std::fs::write(root.join("roster"), roster).unwrap();
        let config = |host: &str| Config {
            network: "team".into(),
            host_key_file: root.join(format!("{host}.key")),
            membership_state_file: root.join(format!("{host}.membership.json")),
            replay_state_file: root.join(format!("{host}.replay.json")),
            bootstrap_manifest_file: Some(root.join("manifest.json")),
            authorities: authorities.clone(),
            flytomesh_admin_key: STANDARD.encode(flyto_public),
            status_file: Some(root.join(format!("{host}.status"))),
            roster_file: Some(root.join("roster")),
            admin_file: Some(root.join("admin.pub")),
            listen_port: Some(8832),
            service_directory: None,
            service_discovery: None,
        };
        std::fs::write(
            root.join("alpha.status"),
            format!("node alpha 10.249.0.1 id 1 roster v14 key {mesh_alpha}\npeer beta 10.249.0.2 relay sh handshake 7s id 2 key {mesh_beta}\n"),
        ).unwrap();
        std::fs::write(
            root.join("beta.status"),
            format!("node beta 10.249.0.2 id 2 roster v14 key {mesh_beta}\npeer alpha 10.249.0.1 relay sh handshake 7s id 1 key {mesh_alpha}\n"),
        ).unwrap();
        let alpha = DynamicMesh::load(&config("alpha"), &root.join("relay.toml"), "alpha").unwrap();
        let beta = DynamicMesh::load(&config("beta"), &root.join("relay.toml"), "beta").unwrap();
        let request = MeshRequest {
            method: "POST",
            path: "/mesh/directory",
            body: b"{}",
            from: "alpha.agent",
        };
        let (address, header) = alpha.target("beta", request).unwrap().unwrap();
        assert_eq!(address, "10.249.0.2:8832");
        let source = SocketAddr::from(([10, 249, 0, 1], 40000));
        assert!(
            beta.authenticate(header.as_bytes(), request, source)
                .is_some()
        );
        assert!(
            beta.authenticate(header.as_bytes(), request, source)
                .is_none()
        );
        assert!(
            beta.authenticate(
                header.as_bytes(),
                request,
                SocketAddr::from(([10, 249, 0, 9], 40000)),
            )
            .is_none()
        );
        drop(alpha);
        let mut discovered_config = config("alpha");
        discovered_config.service_discovery = Some(true);
        let discovered =
            DynamicMesh::load(&discovered_config, &root.join("relay.toml"), "alpha").unwrap();
        assert!(discovered.target("beta", request).unwrap().is_none());
        std::fs::write(
            root.join("alpha.status"),
            format!(
                "node alpha 10.249.0.1 id 1 roster v14 key {mesh_alpha}\npeer beta 10.249.0.2 relay sh handshake 7s id 2 key {mesh_beta}\nservice beta 10.249.0.2 id 2 name agent-inbox port 19001 ident fingerprint expires 120s\n"
            ),
        )
        .unwrap();
        let (address, _) = discovered.target("beta", request).unwrap().unwrap();
        assert_eq!(address, "10.249.0.2:19001");
        assert_eq!(discovered.gossip_targets(), vec![address]);
        drop(beta);
        drop(discovered);
        std::fs::remove_dir_all(root).unwrap();
    }
}
