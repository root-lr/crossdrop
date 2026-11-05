//! LAN discovery over mDNS / DNS-SD.

use std::net::{IpAddr, SocketAddr};

use anyhow::Result;
use crossdrop_proto::MDNS_SERVICE_TYPE;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;

use crate::DeviceId;

/// A device announced on the local network.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub id: DeviceId,
    pub name: String,
    pub addrs: Vec<SocketAddr>,
}

/// Announces this device and reports the others. Stops when dropped.
pub struct Discovery {
    daemon: ServiceDaemon,
}

impl Discovery {
    pub fn start(id: DeviceId, name: &str, port: u16) -> Result<(Self, mpsc::UnboundedReceiver<Discovered>)> {
        let daemon = ServiceDaemon::new()?;
        let instance = id.compact();
        let host = format!("crossdrop-{}.local.", instance[..8].to_lowercase());
        let id_text = id.to_string();
        let props = [("id", id_text.as_str()), ("name", name), ("v", "1")];
        let info = ServiceInfo::new(MDNS_SERVICE_TYPE, &instance, &host, "", port, &props[..])?.enable_addr_auto();
        daemon.register(info)?;

        let events = daemon.browse(MDNS_SERVICE_TYPE)?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(event) = events.recv_async().await {
                let ServiceEvent::ServiceResolved(service) = event else { continue };
                let Some(peer) = service.get_property_val_str("id").and_then(|s| s.parse::<DeviceId>().ok()) else {
                    continue;
                };
                if peer == id {
                    continue;
                }
                let name = service.get_property_val_str("name").unwrap_or("unknown").to_string();
                let addrs = service
                    .get_addresses_v4()
                    .into_iter()
                    .map(|ip| SocketAddr::new(IpAddr::V4(ip), service.port))
                    .collect();
                if tx.send(Discovered { id: peer, name, addrs }).is_err() {
                    break;
                }
            }
        });
        Ok((Self { daemon }, rx))
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}
