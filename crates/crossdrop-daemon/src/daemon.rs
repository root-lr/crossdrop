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
use crossdrop_proto::{read_frame, read_frame_max, services, write_frame, MAX_SMALL_FRAME_LEN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::clipboard::SystemClipboard;
use crate::config::{Config, TrustStore, TrustedDevice};

/// How long a user has to compare pairing codes.
const PAIR_DECISION_TIMEOUT: Duration = Duration::from_secs(120);
/// How long unpairing waits to tell the other device.
const UNPAIR_NOTICE_TIMEOUT: Duration = Duration::from_secs(3);
/// Pairing requests waiting for the local user at once (one per device).
const MAX_PAIR_REQUESTS: usize = 8;
/// How long the responder waits for the initiator to acknowledge the answer.
const PAIR_ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an opened channel waits for its client to attach.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an incoming transfer waits for the user to accept it.
const OFFER_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a cached connection may take to open a channel before it is
/// considered dead.
const STALE_CHECK: Duration = Duration::from_secs(3);
/// How long a peer has to send the header of a stream it opened.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
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

    /// The device stopped announcing itself: it is no longer nearby. A live
    /// connection still counts as online in [`Self::list_devices`].
    pub fn on_lost(&self, id: DeviceId) {
        if self.discovered.lock().unwrap().remove(&id).is_some() {
            debug!(peer = %id.short(), "gone from the network");
        }
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
        // Incoming or outgoing, the peer's endpoint is the one it listens on.
        self.remember_addr(peer, conn.remote_address());
        self.conns.lock().unwrap().insert(peer, conn.clone());
        while let Ok((send, recv)) = conn.accept_bi().await {
            let this = self.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                let pending = match tokio::time::timeout(HEADER_TIMEOUT, PendingChannel::read_header(send, recv)).await
                {
                    Ok(Ok(pending)) => pending,
                    Ok(Err(e)) => return debug!(peer = %peer.short(), "bad channel header: {e:#}"),
                    Err(_) => return debug!(peer = %peer.short(), "no channel header in time"),
                };
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
        if service == services::UNPAIR {
            let mut io = pending.accept().await?.into_io();
            self.forget(peer)?;
            info!(peer = %device.name, id = %peer.short(), "unpaired by the other device");
            write_frame(&mut io, &ChannelReply::Accepted).await?;
            let _ = io.shutdown().await;
            return Ok(());
        }
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
        let addrs = self.known_addrs(peer);
        if addrs.is_empty() {
            bail!("no known address for {}; is it on this network?", peer.short());
        }
        // All addresses at once: a stale one must not delay a good one by
        // the whole dial timeout.
        let mut attempts = tokio::task::JoinSet::new();
        for addr in addrs {
            let node = self.node.clone();
            attempts.spawn(async move { (addr, connect(&node, addr).await) });
        }
        let mut last_err = None;
        let mut winner = None;
        while let Some(joined) = attempts.join_next().await {
            let Ok((addr, result)) = joined else { continue };
            match result {
                Ok((conn, id)) if id == peer && winner.is_none() => {
                    winner = Some(conn);
                    attempts.abort_all();
                }
                // Another address reached the same device first.
                Ok((conn, id)) if id == peer => conn.close(0u32.into(), b"duplicate"),
                Ok((conn, _)) => {
                    conn.close(0u32.into(), b"wrong device");
                    last_err = Some(anyhow!("{addr} is a different device"));
                }
                Err(e) => last_err = Some(e),
            }
        }
        let conn = winner.ok_or_else(|| last_err.unwrap_or_else(|| anyhow!("unreachable")))?;
        tokio::spawn(self.clone().serve_connection(peer, conn.clone()));
        Ok(conn)
    }

    /// Addresses to try for `peer`, each once: those announced on the network
    /// first, then the ones remembered at pairing time.
    fn known_addrs(&self, peer: DeviceId) -> Vec<SocketAddr> {
        let mut addrs: Vec<SocketAddr> = Vec::new();
        if let Some(d) = self.discovered.lock().unwrap().get(&peer) {
            addrs.extend(&d.addrs);
        }
        if let Some(t) = self.trusted(peer) {
            addrs.extend(t.addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()));
        }
        let mut seen = std::collections::HashSet::new();
        addrs.retain(|a| seen.insert(*a));
        addrs
    }

    async fn dial(self: &Arc<Self>, addr: SocketAddr) -> Result<(Connection, DeviceId)> {
        let (conn, peer) = connect(&self.node, addr).await?;
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
        // A pairing whose user never confirms nor cancels (Ctrl+C, closed
        // window) is dropped with its connection handle.
        let this = self.clone();
        let key = pair_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PAIR_DECISION_TIMEOUT).await;
            if this.outgoing_pairs.lock().unwrap().remove(&key).is_some() {
                debug!(pair = %key, "pairing abandoned");
            }
        });
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
        let answer = tokio::time::timeout(
            PAIR_DECISION_TIMEOUT + Duration::from_secs(10),
            read_frame_max::<_, Pair>(&mut io, MAX_SMALL_FRAME_LEN),
        )
        .await
        .context("the other device did not answer in time")??;
        let _ = io.shutdown().await;
        let Pair::Response { accepted, name } = answer else { bail!("unexpected pairing message") };
        let name = peer_name(&name);
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
        let Pair::Request { name } =
            tokio::time::timeout(Duration::from_secs(30), read_frame_max(&mut io, MAX_SMALL_FRAME_LEN)).await??
        else {
            bail!("unexpected pairing message");
        };
        let name = peer_name(&name);
        let code = sas(&conn, self.id(), peer)?;
        let pair_id = self.new_token();
        let (tx, rx) = oneshot::channel();
        let refused = {
            let mut pairs = self.incoming_pairs.lock().unwrap();
            // A device asking again (its user retried) replaces its previous
            // request, which is then answered "no".
            pairs.retain(|_, p| p.peer != peer);
            // Anyone on the network may ask: keep the list short and readable.
            let refused = pairs.len() >= MAX_PAIR_REQUESTS;
            if !refused {
                pairs.insert(
                    pair_id.clone(),
                    IncomingPair { peer, name: name.clone(), sas: code.clone(), decision: tx },
                );
            }
            refused
        };
        if refused {
            debug!(from = %name, id = %peer.short(), "pairing request refused: too many waiting");
            write_frame(&mut io, &Pair::Response { accepted: false, name: self.config.name.clone() }).await?;
            let _ = io.shutdown().await;
            return Ok(());
        }
        info!(from = %name, id = %peer.short(), code = %code, "pairing request: run `crossdrop requests`");
        let accepted = matches!(tokio::time::timeout(PAIR_DECISION_TIMEOUT, rx).await, Ok(Ok(true)));
        self.incoming_pairs.lock().unwrap().remove(&pair_id);
        write_frame(&mut io, &Pair::Response { accepted, name: self.config.name.clone() }).await?;
        let _ = io.shutdown().await;
        if !accepted {
            return Ok(());
        }
        // The initiator finishes its side once it has read the answer. Trust
        // it only then: if it gave up or went away, neither side is paired.
        let mut rest = [0u8; 1];
        match tokio::time::timeout(PAIR_ACK_TIMEOUT, io.read(&mut rest)).await {
            Ok(Ok(0)) => {
                self.add_trusted(peer, name.clone(), conn.remote_address())?;
                info!(peer = %name, id = %peer.short(), "paired");
                Ok(())
            }
            _ => bail!("{name} went away before the pairing completed; not paired"),
        }
    }

    /// Removes `peer` from the trust store and drops its connection.
    fn forget(&self, peer: DeviceId) -> Result<()> {
        {
            let mut trust = self.trust.lock().unwrap();
            if trust.devices.remove(&peer.to_string()).is_some() {
                trust.save()?;
            }
        }
        if let Some(conn) = self.conns.lock().unwrap().remove(&peer) {
            conn.close(0u32.into(), b"unpaired");
        }
        Ok(())
    }

    /// Tells `peer` that this device forgets it.
    async fn notify_unpair(self: &Arc<Self>, peer: DeviceId) -> Result<()> {
        let mut io = self.channel_to(peer, services::UNPAIR).await?.into_io();
        let reply: ChannelReply = read_frame(&mut io).await?;
        if reply != ChannelReply::Accepted {
            bail!("unpair notice refused");
        }
        Ok(())
    }

    fn add_trusted(&self, peer: DeviceId, name: String, addr: SocketAddr) -> Result<()> {
        let mut trust = self.trust.lock().unwrap();
        let entry = trust.devices.entry(peer.to_string()).or_insert_with(|| TrustedDevice::new(name.clone(), None));
        entry.name = name;
        entry.remember_addr(addr.to_string());
        trust.save()
    }

    /// Records where a paired device was just reached, so that it can be
    /// dialed again without mDNS after its address changed (DHCP).
    fn remember_addr(&self, peer: DeviceId, addr: SocketAddr) {
        let mut trust = self.trust.lock().unwrap();
        let Some(entry) = trust.devices.get_mut(&peer.to_string()) else { return };
        if entry.remember_addr(addr.to_string()) {
            if let Err(e) = trust.save() {
                warn!("cannot save {}: {e:#}", peer.short());
            }
        }
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
                if self.trusted(id).is_none() {
                    bail!("{device} is not paired");
                }
                // Best effort: the other device may be offline, or too old to
                // know this service; it then keeps a stale entry, as before.
                if let Err(e) = tokio::time::timeout(UNPAIR_NOTICE_TIMEOUT, self.notify_unpair(id)).await {
                    debug!(peer = %id.short(), "unpair notice not delivered: {e}");
                }
                self.forget(id)?;
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

/// Connects to `addr`, giving up after [`DIAL_TIMEOUT`].
async fn connect(node: &Node, addr: SocketAddr) -> Result<(Connection, DeviceId)> {
    tokio::time::timeout(DIAL_TIMEOUT, node.connect(addr)).await.map_err(|_| anyhow!("{addr} did not answer"))?
}

/// A name another device gave itself, safe to store and show.
fn peer_name(raw: &str) -> String {
    crossdrop_proto::clean_device_name(raw).unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossdrop_core::Identity;

    #[tokio::test]
    async fn a_device_that_leaves_is_no_longer_listed_online() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::bind(&Identity::generate().unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let trust = TrustStore::load(&dir.path().join("trusted.toml")).unwrap();
        let daemon = Daemon::new(Config::default(), node, trust, None);

        let peer = Identity::generate().unwrap().id();
        let addrs = vec!["10.0.0.2:47470".parse().unwrap()];
        daemon.on_discovered(Discovered { id: peer, name: "bravo".into(), addrs });
        let listed = daemon.list_devices();
        assert_eq!((listed.len(), listed[0].online), (1, true));

        daemon.on_lost(peer);
        assert!(daemon.list_devices().is_empty());
    }

    #[tokio::test]
    async fn each_address_is_tried_once() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::bind(&Identity::generate().unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut trust = TrustStore::load(&dir.path().join("trusted.toml")).unwrap();
        let peer = Identity::generate().unwrap().id();
        let mut device = TrustedDevice::new("bravo".into(), Some("10.0.0.2:47470".into()));
        device.addrs.push("10.0.0.3:47470".into());
        trust.devices.insert(peer.to_string(), device);
        let daemon = Daemon::new(Config::default(), node, trust, None);

        // Announced: .3 then .2. Remembered: .2 then .3. Not adjacent duplicates.
        let addrs = vec!["10.0.0.3:47470".parse().unwrap(), "10.0.0.2:47470".parse().unwrap()];
        daemon.on_discovered(Discovered { id: peer, name: "bravo".into(), addrs });
        let got: Vec<String> = daemon.known_addrs(peer).iter().map(|a| a.to_string()).collect();
        assert_eq!(got, ["10.0.0.3:47470", "10.0.0.2:47470"]);
    }

    #[tokio::test]
    async fn a_dead_address_does_not_delay_a_live_one() {
        let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let alive = Node::bind(&Identity::generate().unwrap(), local).unwrap();
        let alive_addr = alive.local_addr().unwrap();
        let peer = alive.id();
        tokio::spawn(async move {
            while let Some(incoming) = alive.accept().await {
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        conn.closed().await;
                    }
                });
            }
        });
        // Bound but silent: QUIC packets to it are never answered.
        let silent = std::net::UdpSocket::bind(local).unwrap();
        let dead_addr = silent.local_addr().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut trust = TrustStore::load(&dir.path().join("trusted.toml")).unwrap();
        let mut device = TrustedDevice::new("bravo".into(), Some(alive_addr.to_string()));
        device.addrs.insert(0, dead_addr.to_string());
        trust.devices.insert(peer.to_string(), device);
        let node = Node::bind(&Identity::generate().unwrap(), local).unwrap();
        let daemon = Daemon::new(Config::default(), node, trust, None);

        let started = std::time::Instant::now();
        let conn = daemon.dial_peer(peer).await.unwrap();
        assert_eq!(conn.remote_address(), alive_addr);
        assert!(started.elapsed() < DIAL_TIMEOUT / 2, "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn an_unconfirmed_pairing_expires() {
        let dir = tempfile::tempdir().unwrap();
        let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let node = Node::bind(&Identity::generate().unwrap(), local).unwrap();
        let trust = TrustStore::load(&dir.path().join("trusted.toml")).unwrap();
        let daemon = Daemon::new(Config::default(), node, trust, None);

        let other = Node::bind(&Identity::generate().unwrap(), local).unwrap();
        let addr = other.local_addr().unwrap();
        tokio::spawn(async move {
            let conn = other.accept().await.unwrap().await.unwrap();
            conn.closed().await;
        });

        let Response::PairChallenge { pair_id, .. } = daemon.pair_start(&addr.to_string()).await.unwrap() else {
            panic!("expected a challenge");
        };
        assert!(daemon.outgoing_pairs.lock().unwrap().contains_key(&pair_id));

        // Let the expiry task start its timer, then jump past it.
        tokio::task::yield_now().await;
        tokio::time::pause();
        tokio::time::advance(PAIR_DECISION_TIMEOUT + Duration::from_secs(1)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(daemon.outgoing_pairs.lock().unwrap().is_empty());
        assert!(daemon.pair_confirm(&pair_id, true).await.is_err());
    }

    /// A responder daemon serving connections, and a bare node to pair from.
    fn responder() -> (Arc<Daemon>, SocketAddr, Node, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let local: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let node = Node::bind(&Identity::generate().unwrap(), local).unwrap();
        let addr = node.local_addr().unwrap();
        let trust = TrustStore::load(&dir.path().join("trusted.toml")).unwrap();
        let daemon = Daemon::new(Config::default(), node, trust, None);
        tokio::spawn(daemon.clone().accept_loop());
        let initiator = Node::bind(&Identity::generate().unwrap(), local).unwrap();
        (daemon, addr, initiator, dir)
    }

    /// Sends a pairing request and waits until the responder lists it.
    async fn request_pairing(daemon: &Daemon, initiator: &Node, addr: SocketAddr) -> (Connection, Channel, String) {
        let (conn, _) = initiator.connect(addr).await.unwrap();
        let mut channel = open_channel(&conn, services::PAIR).await.unwrap();
        write_frame(&mut channel.send, &Pair::Request { name: "alpha".into() }).await.unwrap();
        for _ in 0..100 {
            if let Some(id) = daemon.incoming_pairs.lock().unwrap().keys().next().cloned() {
                return (conn, channel, id);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the pairing request never showed up");
    }

    async fn trusts(daemon: &Daemon, peer: DeviceId) -> bool {
        for _ in 0..50 {
            if daemon.trusted(peer).is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn accepted_pairing_is_trusted_once_the_initiator_read_the_answer() {
        let (daemon, addr, initiator, _dir) = responder();
        let (_conn, mut channel, pair_id) = request_pairing(&daemon, &initiator, addr).await;
        daemon.handle_request(Request::PairAnswer { pair_id, accept: true }).await.unwrap();
        let answer: Pair = read_frame(&mut channel.recv).await.unwrap();
        assert!(matches!(answer, Pair::Response { accepted: true, .. }));
        channel.send.finish().unwrap();
        assert!(trusts(&daemon, initiator.id()).await);
    }

    #[tokio::test]
    async fn initiator_gone_before_the_answer_is_not_trusted() {
        let (daemon, addr, initiator, _dir) = responder();
        let (conn, _channel, pair_id) = request_pairing(&daemon, &initiator, addr).await;
        conn.close(0u32.into(), b"gone");
        tokio::time::sleep(Duration::from_millis(50)).await;
        daemon.handle_request(Request::PairAnswer { pair_id, accept: true }).await.unwrap();
        assert!(!trusts(&daemon, initiator.id()).await);
    }

    #[tokio::test]
    async fn a_new_request_from_the_same_device_replaces_the_old_one() {
        let (daemon, addr, initiator, _dir) = responder();
        let (conn, mut first, first_id) = request_pairing(&daemon, &initiator, addr).await;
        let mut second = open_channel(&conn, services::PAIR).await.unwrap();
        write_frame(&mut second.send, &Pair::Request { name: "alpha".into() }).await.unwrap();
        let answer: Pair = read_frame(&mut first.recv).await.unwrap();
        assert!(matches!(answer, Pair::Response { accepted: false, .. }));
        let pending: Vec<String> = daemon.incoming_pairs.lock().unwrap().keys().cloned().collect();
        assert_eq!(pending.len(), 1);
        assert_ne!(pending[0], first_id);
    }

    #[tokio::test]
    async fn waiting_pairing_requests_are_capped() {
        let (daemon, addr, _, _dir) = responder();
        let mut kept = Vec::new();
        for _ in 0..MAX_PAIR_REQUESTS {
            let initiator = Node::bind(&Identity::generate().unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
            let (conn, _) = initiator.connect(addr).await.unwrap();
            let mut channel = open_channel(&conn, services::PAIR).await.unwrap();
            write_frame(&mut channel.send, &Pair::Request { name: "x".into() }).await.unwrap();
            kept.push((initiator, conn, channel));
        }
        let extra = Node::bind(&Identity::generate().unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let (conn, _) = extra.connect(addr).await.unwrap();
        let mut channel = open_channel(&conn, services::PAIR).await.unwrap();
        write_frame(&mut channel.send, &Pair::Request { name: "x".into() }).await.unwrap();
        for _ in 0..100 {
            if daemon.incoming_pairs.lock().unwrap().len() == MAX_PAIR_REQUESTS {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let answer: Pair = read_frame(&mut channel.recv).await.unwrap();
        assert!(matches!(answer, Pair::Response { accepted: false, .. }));
        assert_eq!(daemon.incoming_pairs.lock().unwrap().len(), MAX_PAIR_REQUESTS);
    }

    #[tokio::test]
    async fn a_stream_without_header_does_not_block_the_next_channels() {
        let (_daemon, addr, peer, _dir) = responder();
        let (conn, _) = peer.connect(addr).await.unwrap();
        // One byte of a length prefix, then nothing.
        let (mut stuck, _recv) = conn.open_bi().await.unwrap();
        stuck.write_all(&[0]).await.unwrap();

        let next = tokio::time::timeout(Duration::from_secs(2), open_channel(&conn, services::PAIR)).await;
        assert!(matches!(next, Ok(Ok(_))), "the second channel was held up");
    }
}
