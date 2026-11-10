//! crossdrop-core: identity, encrypted QUIC transport, channels and LAN
//! discovery. It knows nothing about trust or permissions; that is the
//! daemon's job.

mod discovery;
mod identity;
mod node;
mod tls;

pub use discovery::{Discovered, Discovery, DiscoveryEvent};
pub use identity::{DeviceId, Identity};
pub use node::{open_channel, peer_id, sas, Channel, Node, PendingChannel, Rejected, CHANNEL_VERSION};
pub use quinn::Connection;
