//! Daemon state: connections, trust, pairing, channel routing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use crossdrop_core::{open_channel, peer_id, sas, Channel, Connection, DeviceId, Discovered, Node, PendingChannel};
use crossdrop_proto::ipc::{DeviceInfo, Event, PairRequestInfo, Request, Response};
use crossdrop_proto::wire::Pair;
use crossdrop_proto::{read_frame, services, write_frame};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::config::{Config, TrustStore, TrustedDevice};

/// How long a user has to compare pairing codes.
const PAIR_DECISION_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an opened channel waits for its client to attach.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

struct OutgoingPair {
    conn: Connection,
    peer: DeviceId,
}

struct IncomingPair {
    peer: DeviceId,
    name: String,
    sas: String,
    decision: oneshot::Sender<bool>,
}

pub(crate) struct Daemon {
    pub config: Config,
    pub node: Node,
    trust: Mutex<TrustStore>,
    discovered: Mutex<HashMap<DeviceId, Discovered>>,
    conns: Mutex<HashMap<DeviceId, Connection>>,
    outgoing_pairs: Mutex<HashMap<String, OutgoingPair>>,
    incoming_pairs: Mutex<HashMap<String, IncomingPair>>,
    services: Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>,
    channels: Mutex<HashMap<String, Channel>>,
    counter: AtomicU64,
}

impl Daemon {
    pub fn new(config: Config, node: Node, trust: TrustStore) -> Arc<Self> {
        Arc::new(Self {
            config,
            node,
            trust: Mutex::new(trust),
            discovered: Mutex::default(),
            conns: Mutex::default(),
            outgoing_pairs: Mutex::default(),
            incoming_pairs: Mutex::default(),
            services: Mutex::default(),
            channels: Mutex::default(),
            counter: AtomicU64::new(0),
        })
    }

    pub fn id(&self) -> DeviceId {
        self.node.id()
    }

    fn new_token(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let mut h = blake3::Hasher::new();
        h.update(self.id().as_bytes());
        h.update(&n.to_le_bytes());
        h.update(&t.to_le_bytes());
        h.finalize().to_hex()[..16].to_string()
    }

    fn trusted(&self, id: DeviceId) -> Option<TrustedDevice> {
        self.trust.lock().unwrap().devices.get(&id.to_string()).cloned()
    }

    // ---------------------------------------------------------------- network

    pub fn on_discovered(&self, d: Discovered) {
        debug!(peer = %d.id.short(), name = %d.name, "discovered");
        self.discovered.lock().unwrap().insert(d.id, d);
    }

    /// Accept loop for incoming QUIC connections.
    pub async fn accept_loop(self: Arc<Self>) {
        while let Some(incoming) = self.node.accept().await {
            let this = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => match peer_id(&conn) {
                        Ok(peer) => this.serve_connection(peer, conn).await,
                        Err(e) => warn!("rejecting connection: {e}"),
                    },
                    Err(e) => debug!("handshake failed: {e}"),
                }
            });
        }
    }

    /// Remembers `conn` and dispatches the channels the peer opens on it.
    async fn serve_connection(self: Arc<Self>, peer: DeviceId, conn: Connection) {
        debug!(peer = %peer.short(), addr = %conn.remote_address(), "connected");
        self.conns.lock().unwrap().insert(peer, conn.clone());
        while let Ok(pending) = PendingChannel::next(&conn).await {
            let this = self.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                let service = pending.header.service.clone();
                if let Err(e) = this.handle_channel(peer, conn, pending).await {
                    warn!(peer = %peer.short(), %service, "channel failed: {e:#}");
                }
            });
        }
        let mut conns = self.conns.lock().unwrap();
        if conns.get(&peer).is_some_and(|c| c.stable_id() == conn.stable_id()) {
            conns.remove(&peer);
        }
        debug!(peer = %peer.short(), "disconnected");
    }

    async fn handle_channel(self: Arc<Self>, peer: DeviceId, conn: Connection, pending: PendingChannel) -> Result<()> {
        let service = pending.header.service.clone();
        if service == services::PAIR {
            return self.handle_incoming_pair(peer, conn, pending).await;
        }
        let Some(device) = self.trusted(peer) else {
            return pending.reject("not paired").await;
        };
        if !device.allows(&service) {
            info!(peer = %device.name, %service, "channel refused: no permission");
            return pending.reject(format!("permission denied for {service}")).await;
        }
        if service == services::FILES {
            let mut io = pending.accept().await?.into_io();
            let n = crate::files::receive(&mut io, &self.config.download_dir, &device.name).await?;
            info!(from = %device.name, files = n, dir = %self.config.download_dir.display(), "transfer complete");
            return Ok(());
        }
        let handler = self.services.lock().unwrap().get(&service).filter(|tx| !tx.is_closed()).cloned();
        let Some(handler) = handler else {
            return pending.reject(format!("service {service} is not available")).await;
        };
        let channel = pending.accept().await?;
        let channel_id = self.park_channel(channel);
        let event = Event::IncomingChannel { channel_id, device: peer.to_string(), service };
        handler.send(event).map_err(|_| anyhow!("extension went away"))?;
        Ok(())
    }

    /// Keeps a channel until a client attaches to it, or drops it after a while.
    fn park_channel(self: &Arc<Self>, channel: Channel) -> String {
        let id = self.new_token();
        self.channels.lock().unwrap().insert(id.clone(), channel);
        let this = self.clone();
        let key = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(ATTACH_TIMEOUT).await;
            if this.channels.lock().unwrap().remove(&key).is_some() {
                debug!(channel = %key, "channel expired before attach");
            }
        });
        id
    }

    pub fn take_channel(&self, id: &str) -> Option<Channel> {
        self.channels.lock().unwrap().remove(id)
    }

    /// Returns a live connection to `peer`, dialing it if needed.
    async fn connection_to(self: &Arc<Self>, peer: DeviceId) -> Result<Connection> {
        if let Some(conn) = self.conns.lock().unwrap().get(&peer) {
            if conn.close_reason().is_none() {
                return Ok(conn.clone());
            }
        }
        let mut addrs: Vec<SocketAddr> = Vec::new();
        if let Some(d) = self.discovered.lock().unwrap().get(&peer) {
            addrs.extend(&d.addrs);
        }
        if let Some(t) = self.trusted(peer) {
            addrs.extend(t.addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()));
        }
        addrs.dedup();
        if addrs.is_empty() {
            bail!("no known address for {}; is it on this network?", peer.short());
        }
        let mut last_err = None;
        for addr in addrs {
            match self.dial(addr).await {
                Ok((conn, id)) if id == peer => return Ok(conn),
                Ok((conn, _)) => {
                    conn.close(0u32.into(), b"wrong device");
                    last_err = Some(anyhow!("{addr} is a different device"));
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("unreachable")))
    }

    async fn dial(self: &Arc<Self>, addr: SocketAddr) -> Result<(Connection, DeviceId)> {
        let (conn, peer) = self.node.connect(addr).await?;
        tokio::spawn(self.clone().serve_connection(peer, conn.clone()));
        Ok((conn, peer))
    }

    /// Finds a device by id, id prefix (≥ 4 characters) or name.
    fn resolve_device(&self, target: &str) -> Result<DeviceId> {
        if let Ok(id) = target.parse::<DeviceId>() {
            return Ok(id);
        }
        let mut known: HashMap<DeviceId, String> = HashMap::new();
        for (id, d) in &self.trust.lock().unwrap().devices {
            if let Ok(id) = id.parse() {
                known.insert(id, d.name.clone());
            }
        }
        for (id, d) in self.discovered.lock().unwrap().iter() {
            known.entry(*id).or_insert_with(|| d.name.clone());
        }
        let wanted: String = target.chars().filter(|c| *c != '-').collect::<String>().to_uppercase();
        let matches: Vec<DeviceId> = known
            .iter()
            .filter(|(id, name)| {
                name.eq_ignore_ascii_case(target) || (wanted.len() >= 4 && id.compact().starts_with(&wanted))
            })
            .map(|(id, _)| *id)
            .collect();
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => bail!("no device matches {target:?}"),
            _ => bail!("{target:?} matches several devices; use more of its id"),
        }
    }

    fn device_name(&self, id: DeviceId) -> String {
        if let Some(t) = self.trusted(id) {
            return t.name;
        }
        self.discovered.lock().unwrap().get(&id).map(|d| d.name.clone()).unwrap_or_else(|| "unknown".into())
    }

    // ---------------------------------------------------------------- pairing

    async fn pair_start(self: &Arc<Self>, target: &str) -> Result<Response> {
        let (conn, peer) = match target.parse::<SocketAddr>() {
            Ok(addr) => self.dial(addr).await?,
            Err(_) => {
                let peer = self.resolve_device(target)?;
                (self.connection_to(peer).await?, peer)
            }
        };
        let code = sas(&conn, self.id(), peer)?;
        let pair_id = self.new_token();
        let peer_name = self.device_name(peer);
        self.outgoing_pairs.lock().unwrap().insert(pair_id.clone(), OutgoingPair { conn, peer });
        Ok(Response::PairChallenge { pair_id, sas: code, peer_id: peer.to_string(), peer_name })
    }

    async fn pair_confirm(self: &Arc<Self>, pair_id: &str, accept: bool) -> Result<Response> {
        let pair = self
            .outgoing_pairs
            .lock()
            .unwrap()
            .remove(pair_id)
            .ok_or_else(|| anyhow!("no pairing in progress with id {pair_id}"))?;
        let peer_id = pair.peer.to_string();
        if !accept {
            return Ok(Response::PairResult { paired: false, peer_id, peer_name: self.device_name(pair.peer) });
        }
        let mut io = open_channel(&pair.conn, services::PAIR).await?.into_io();
        write_frame(&mut io, &Pair::Request { name: self.config.name.clone() }).await?;
        let answer =
            tokio::time::timeout(PAIR_DECISION_TIMEOUT + Duration::from_secs(10), read_frame::<_, Pair>(&mut io))
                .await
                .context("the other device did not answer in time")??;
        let _ = io.shutdown().await;
        let Pair::Response { accepted, name } = answer else { bail!("unexpected pairing message") };
        if accepted {
            self.add_trusted(pair.peer, name.clone(), pair.conn.remote_address())?;
            info!(peer = %name, id = %pair.peer.short(), "paired");
        }
        Ok(Response::PairResult { paired: accepted, peer_id, peer_name: name })
    }

    async fn handle_incoming_pair(
        self: Arc<Self>,
        peer: DeviceId,
        conn: Connection,
        pending: PendingChannel,
    ) -> Result<()> {
        let mut io = pending.accept().await?.into_io();
        let Pair::Request { name } = tokio::time::timeout(Duration::from_secs(30), read_frame(&mut io)).await?? else {
            bail!("unexpected pairing message");
        };
        let code = sas(&conn, self.id(), peer)?;
        let pair_id = self.new_token();
        let (tx, rx) = oneshot::channel();
        self.incoming_pairs
            .lock()
            .unwrap()
            .insert(pair_id.clone(), IncomingPair { peer, name: name.clone(), sas: code.clone(), decision: tx });
        info!(from = %name, id = %peer.short(), code = %code, "pairing request: run `crossdrop requests`");
        let accepted = matches!(tokio::time::timeout(PAIR_DECISION_TIMEOUT, rx).await, Ok(Ok(true)));
        self.incoming_pairs.lock().unwrap().remove(&pair_id);
        if accepted {
            self.add_trusted(peer, name.clone(), conn.remote_address())?;
            info!(peer = %name, id = %peer.short(), "paired");
        }
        write_frame(&mut io, &Pair::Response { accepted, name: self.config.name.clone() }).await?;
        let _ = io.shutdown().await;
        Ok(())
    }

    fn add_trusted(&self, peer: DeviceId, name: String, addr: SocketAddr) -> Result<()> {
        let mut trust = self.trust.lock().unwrap();
        let entry = trust.devices.entry(peer.to_string()).or_insert_with(|| TrustedDevice::new(name.clone(), None));
        entry.name = name;
        let addr = addr.to_string();
        if !entry.addrs.contains(&addr) {
            entry.addrs.insert(0, addr);
            entry.addrs.truncate(4);
        }
        trust.save()
    }

    // ---------------------------------------------------------------- IPC

    pub fn register_service(&self, service: &str) -> Result<mpsc::UnboundedReceiver<Event>> {
        let valid = service.contains('/')
            && !service.starts_with("crossdrop/")
            && service.len() <= 64
            && service.chars().all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c));
        if !valid {
            bail!("invalid service name {service:?} (expected e.g. \"myapp/feature\")");
        }
        let mut services = self.services.lock().unwrap();
        if services.get(service).is_some_and(|tx| !tx.is_closed()) {
            bail!("service {service} is already registered by another client");
        }
        let (tx, rx) = mpsc::unbounded_channel();
        services.insert(service.to_string(), tx);
        info!(%service, "extension registered");
        Ok(rx)
    }

    pub fn unregister_service(&self, service: &str) {
        let mut services = self.services.lock().unwrap();
        if services.get(service).is_some_and(|tx| tx.is_closed()) {
            services.remove(service);
            info!(%service, "extension unregistered");
        }
    }

    pub async fn handle_request(self: &Arc<Self>, req: Request) -> Result<Response> {
        match req {
            Request::Status => Ok(Response::Status {
                device_id: self.id().to_string(),
                name: self.config.name.clone(),
                port: self.node.local_addr()?.port(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            Request::ListDevices => Ok(Response::Devices { devices: self.list_devices() }),
            Request::PairStart { target } => self.pair_start(&target).await,
            Request::PairConfirm { pair_id, accept } => self.pair_confirm(&pair_id, accept).await,
            Request::ListPairRequests => {
                let requests = self
                    .incoming_pairs
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, p)| PairRequestInfo {
                        pair_id: id.clone(),
                        peer_id: p.peer.to_string(),
                        peer_name: p.name.clone(),
                        sas: p.sas.clone(),
                    })
                    .collect();
                Ok(Response::PairRequests { requests })
            }
            Request::PairAnswer { pair_id, accept } => {
                let pair = self.incoming_pairs.lock().unwrap().remove(&pair_id);
                let pair = pair.ok_or_else(|| anyhow!("no pairing request with id {pair_id}"))?;
                let _ = pair.decision.send(accept);
                Ok(Response::Ok)
            }
            Request::Unpair { device } => {
                let id = self.resolve_device(&device)?;
                let removed = {
                    let mut trust = self.trust.lock().unwrap();
                    let removed = trust.devices.remove(&id.to_string()).is_some();
                    trust.save()?;
                    removed
                };
                if !removed {
                    bail!("{device} is not paired");
                }
                if let Some(conn) = self.conns.lock().unwrap().remove(&id) {
                    conn.close(0u32.into(), b"unpaired");
                }
                Ok(Response::Ok)
            }
            Request::SetPermission { device, service, allow } => {
                let id = self.resolve_device(&device)?;
                let mut trust = self.trust.lock().unwrap();
                let entry = trust.devices.get_mut(&id.to_string()).ok_or_else(|| anyhow!("{device} is not paired"))?;
                entry.permissions.retain(|s| s != &service);
                if allow {
                    entry.permissions.push(service);
                    entry.permissions.sort();
                }
                trust.save()?;
                Ok(Response::Ok)
            }
            Request::OpenChannel { device, service } => {
                let id = self.resolve_device(&device)?;
                if self.trusted(id).is_none() {
                    bail!("{device} is not paired; run `crossdrop pair {device}` first");
                }
                let conn = self.connection_to(id).await?;
                let channel = open_channel(&conn, &service).await?;
                Ok(Response::ChannelReady { channel_id: self.park_channel(channel) })
            }
            Request::Register { .. } | Request::Attach { .. } => bail!("must be the first request of a connection"),
        }
    }

    fn list_devices(&self) -> Vec<DeviceInfo> {
        let conns = self.conns.lock().unwrap();
        let discovered = self.discovered.lock().unwrap();
        let mut out: HashMap<DeviceId, DeviceInfo> = HashMap::new();
        for (id, d) in &self.trust.lock().unwrap().devices {
            let Ok(did) = id.parse::<DeviceId>() else { continue };
            out.insert(
                did,
                DeviceInfo {
                    id: id.clone(),
                    name: d.name.clone(),
                    addrs: d.addrs.clone(),
                    paired: true,
                    online: false,
                    permissions: d.permissions.clone(),
                },
            );
        }
        for (id, d) in discovered.iter() {
            let entry = out.entry(*id).or_insert_with(|| DeviceInfo {
                id: id.to_string(),
                name: d.name.clone(),
                addrs: vec![],
                paired: false,
                online: false,
                permissions: vec![],
            });
            for a in &d.addrs {
                let a = a.to_string();
                if !entry.addrs.contains(&a) {
                    entry.addrs.push(a);
                }
            }
            entry.online = true;
        }
        for (id, conn) in conns.iter() {
            if let Some(entry) = out.get_mut(id) {
                entry.online |= conn.close_reason().is_none();
            }
        }
        let mut list: Vec<DeviceInfo> = out.into_values().collect();
        list.sort_by(|a, b| b.paired.cmp(&a.paired).then(a.name.cmp(&b.name)));
        list
    }
}
