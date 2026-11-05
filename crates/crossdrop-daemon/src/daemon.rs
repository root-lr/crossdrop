//! Daemon state: connections, trust, pairing, channel routing.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use crossdrop_core::{
    open_channel, peer_id, sas, Channel, Connection, DeviceId, Discovered, Node, PendingChannel, Rejected,
};
use crossdrop_proto::ipc::{DeviceInfo, Event, OfferInfo, PairRequestInfo, Request, Response};
use crossdrop_proto::wire::{ChannelReply, Clipboard, Pair};
use crossdrop_proto::{read_frame, services, write_frame};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::clipboard::SystemClipboard;
use crate::config::{Config, TrustStore, TrustedDevice};

/// How long a user has to compare pairing codes.
const PAIR_DECISION_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an opened channel waits for its client to attach.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an incoming transfer waits for the user to accept it.
const OFFER_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a cached connection may take to open a channel before it is
/// considered dead.
const STALE_CHECK: Duration = Duration::from_secs(3);
/// How long dialing a device may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the system clipboard is checked for changes.
const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

struct OutgoingPair {
    conn: Connection,
    peer: DeviceId,
}

struct PendingOffer {
    info: OfferInfo,
    decision: oneshot::Sender<bool>,
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
    offers: Mutex<HashMap<String, PendingOffer>>,
    clipboard: Option<SystemClipboard>,
    /// Last clipboard text seen locally or applied from a peer (loop guard).
    clip_known: Mutex<Option<String>>,
    /// Last text received from a peer, with the sender's name.
    clip_received: Mutex<Option<(String, String)>>,
    counter: AtomicU64,
}

impl Daemon {
    pub fn new(config: Config, node: Node, trust: TrustStore, clipboard: Option<SystemClipboard>) -> Arc<Self> {
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
            offers: Mutex::default(),
            clipboard,
            clip_known: Mutex::default(),
            clip_received: Mutex::default(),
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
            let offer = crate::files::read_offer(&mut io).await?;
            // A device allowed to drive this computer may drop files on its desktop.
            let to_desktop = offer.desktop && device.allows(services::INTERDESK_INPUT);
            let accepted = to_desktop || device.auto_accept || self.ask_offer(peer, &device.name, &offer).await;
            if !accepted {
                info!(from = %device.name, "transfer declined");
                return crate::files::decline(&mut io).await;
            }
            let dir = if to_desktop { &self.config.desktop_dir } else { &self.config.download_dir };
            let n = crate::files::receive(&mut io, offer, dir, &peer.to_string(), &device.name).await?;
            info!(from = %device.name, files = n, dir = %dir.display(), "transfer complete");
            return Ok(());
        }
        if service == services::CLIPBOARD {
            let mut io = pending.accept().await?.into_io();
            let Clipboard { text } = read_frame(&mut io).await?;
            self.apply_remote_clipboard(text, device.name.clone());
            write_frame(&mut io, &ChannelReply::Accepted).await?;
            let _ = io.shutdown().await;
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
        self.dial_peer(peer).await
    }

    /// Opens a channel to `peer`. A cached connection that does not answer
    /// quickly is dropped and the peer dialed again: it may have restarted,
    /// and a dead QUIC connection would otherwise hang until its idle timeout.
    async fn channel_to(self: &Arc<Self>, peer: DeviceId, service: &str) -> Result<Channel> {
        let cached = self.conns.lock().unwrap().get(&peer).filter(|c| c.close_reason().is_none()).cloned();
        if let Some(conn) = cached {
            match tokio::time::timeout(STALE_CHECK, open_channel(&conn, service)).await {
                Ok(Ok(channel)) => return Ok(channel),
                Ok(Err(e)) if e.downcast_ref::<Rejected>().is_some() => return Err(e),
                _ => {
                    debug!(peer = %peer.short(), "connection looks stale; reconnecting");
                    conn.close(0u32.into(), b"stale");
                    let mut conns = self.conns.lock().unwrap();
                    if conns.get(&peer).is_some_and(|c| c.stable_id() == conn.stable_id()) {
                        conns.remove(&peer);
                    }
                }
            }
        }
        let conn = self.dial_peer(peer).await?;
        open_channel(&conn, service).await
    }

    /// Dials `peer` at its known addresses.
    async fn dial_peer(self: &Arc<Self>, peer: DeviceId) -> Result<Connection> {
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
        let (conn, peer) = tokio::time::timeout(DIAL_TIMEOUT, self.node.connect(addr))
            .await
            .map_err(|_| anyhow!("{addr} did not answer"))??;
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

    /// Puts an incoming transfer in the inbox and waits for the user.
    async fn ask_offer(&self, peer: DeviceId, name: &str, offer: &crate::files::Offer) -> bool {
        let offer_id = self.new_token();
        let info = OfferInfo {
            offer_id: offer_id.clone(),
            peer_id: peer.to_string(),
            peer_name: name.to_string(),
            files: offer.entries.len() as u32,
            total_bytes: offer.total_bytes(),
            preview: offer.entries.iter().take(5).map(|e| e.path.clone()).collect(),
        };
        let (tx, rx) = oneshot::channel();
        self.offers.lock().unwrap().insert(offer_id.clone(), PendingOffer { info, decision: tx });
        info!(from = %name, files = offer.entries.len(), "incoming files: run `crossdrop requests`");
        let accepted = matches!(tokio::time::timeout(OFFER_TIMEOUT, rx).await, Ok(Ok(true)));
        self.offers.lock().unwrap().remove(&offer_id);
        accepted
    }

    // ---------------------------------------------------------------- clipboard

    fn apply_remote_clipboard(&self, text: String, from: String) {
        info!(%from, chars = text.chars().count(), "clipboard received");
        *self.clip_known.lock().unwrap() = Some(text.clone());
        if self.config.clipboard_sync {
            if let Some(cb) = &self.clipboard {
                cb.set(text.clone());
            }
        }
        *self.clip_received.lock().unwrap() = Some((text, from));
    }

    /// Watches the system clipboard and shares every new text.
    pub async fn clipboard_loop(self: Arc<Self>) {
        let Some(cb) = self.clipboard.clone() else { return };
        // Whatever is there at startup is not a "copy" by the user.
        *self.clip_known.lock().unwrap() = cb.get().await;
        loop {
            tokio::time::sleep(CLIPBOARD_POLL).await;
            let Some(text) = cb.get().await else { continue };
            if text.is_empty() || self.clip_known.lock().unwrap().as_deref() == Some(text.as_str()) {
                continue;
            }
            *self.clip_known.lock().unwrap() = Some(text.clone());
            let n = self.broadcast_clipboard(text).await;
            debug!(devices = n, "clipboard shared");
        }
    }

    /// Paired devices this device shares its clipboard with.
    fn clipboard_peers(&self) -> Vec<DeviceId> {
        self.trust
            .lock()
            .unwrap()
            .devices
            .iter()
            .filter(|(_, d)| d.allows(services::CLIPBOARD))
            .filter_map(|(id, _)| id.parse().ok())
            .collect()
    }

    async fn send_clipboard(self: &Arc<Self>, peer: DeviceId, text: String) -> Result<()> {
        let mut io = self.channel_to(peer, services::CLIPBOARD).await?.into_io();
        write_frame(&mut io, &Clipboard { text }).await?;
        // Delivered only once the peer says it applied the text.
        let ack = tokio::time::timeout(Duration::from_secs(5), read_frame::<_, ChannelReply>(&mut io)).await??;
        if ack != ChannelReply::Accepted {
            bail!("clipboard refused");
        }
        let _ = io.shutdown().await;
        Ok(())
    }

    /// Sends `text` to every clipboard peer; returns how many got it.
    async fn broadcast_clipboard(self: &Arc<Self>, text: String) -> u32 {
        let tasks: Vec<_> = self
            .clipboard_peers()
            .into_iter()
            .map(|peer| {
                let this = self.clone();
                let text = text.clone();
                tokio::spawn(async move {
                    let r = this.send_clipboard(peer, text).await;
                    if let Err(e) = &r {
                        debug!(peer = %peer.short(), "clipboard not delivered: {e:#}");
                    }
                    r.is_ok()
                })
            })
            .collect();
        let mut n = 0;
        for t in tasks {
            if matches!(t.await, Ok(true)) {
                n += 1;
            }
        }
        n
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
                let channel = self.channel_to(id, &service).await?;
                Ok(Response::ChannelReady { channel_id: self.park_channel(channel) })
            }
            Request::ListOffers => {
                let offers = self.offers.lock().unwrap().values().map(|o| o.info.clone()).collect();
                Ok(Response::Offers { offers })
            }
            Request::OfferAnswer { offer_id, accept } => {
                let offer = self.offers.lock().unwrap().remove(&offer_id);
                let offer = offer.ok_or_else(|| anyhow!("no incoming transfer with id {offer_id}"))?;
                let _ = offer.decision.send(accept);
                Ok(Response::Ok)
            }
            Request::SetAutoAccept { device, enabled } => {
                let id = self.resolve_device(&device)?;
                let mut trust = self.trust.lock().unwrap();
                let entry = trust.devices.get_mut(&id.to_string()).ok_or_else(|| anyhow!("{device} is not paired"))?;
                entry.auto_accept = enabled;
                trust.save()?;
                Ok(Response::Ok)
            }
            Request::ClipboardSend { text, device } => match device {
                Some(device) => {
                    let id = self.resolve_device(&device)?;
                    if self.trusted(id).is_none() {
                        bail!("{device} is not paired");
                    }
                    self.send_clipboard(id, text).await?;
                    Ok(Response::Sent { count: 1 })
                }
                None => {
                    if self.clipboard_peers().is_empty() {
                        bail!("no device shares the clipboard with this one; run `crossdrop allow <device> clipboard`");
                    }
                    Ok(Response::Sent { count: self.broadcast_clipboard(text).await })
                }
            },
            Request::ClipboardGet => {
                let (text, from) = match self.clip_received.lock().unwrap().clone() {
                    Some((t, f)) => (Some(t), Some(f)),
                    None => (None, None),
                };
                Ok(Response::Clipboard { text, from })
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
                    auto_accept: d.auto_accept,
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
                auto_accept: false,
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
