//! QUIC endpoint and channels.

use std::net::SocketAddr;

use anyhow::{anyhow, bail, Context, Result};
use crossdrop_proto::wire::{ChannelReply, OpenChannel};
use crossdrop_proto::{read_frame, write_frame};
use quinn::{Connection, RecvStream, SendStream};
use rustls::pki_types::CertificateDer;

use crate::{tls, DeviceId, Identity};

/// Protocol version of channel headers.
pub const CHANNEL_VERSION: u32 = 1;

/// A QUIC endpoint that both accepts and initiates connections.
#[derive(Clone)]
pub struct Node {
    endpoint: quinn::Endpoint,
    id: DeviceId,
}

impl Node {
    /// Binds the endpoint on `addr` (use port 0 for a random port).
    pub fn bind(identity: &Identity, addr: SocketAddr) -> Result<Self> {
        let mut endpoint = quinn::Endpoint::server(tls::server_config(identity)?, addr)
            .with_context(|| format!("binding UDP {addr}"))?;
        endpoint.set_default_client_config(tls::client_config(identity)?);
        Ok(Self { endpoint, id: identity.id() })
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Connects to a remote daemon and returns the connection and the
    /// identity the peer proved during the handshake.
    pub async fn connect(&self, addr: SocketAddr) -> Result<(Connection, DeviceId)> {
        let conn = self.endpoint.connect(addr, "crossdrop")?.await.with_context(|| format!("connecting to {addr}"))?;
        let peer = peer_id(&conn)?;
        if peer == self.id {
            conn.close(0u32.into(), b"self");
            bail!("{addr} is this device");
        }
        Ok((conn, peer))
    }

    /// Waits for the next incoming connection. `None` once the endpoint is closed.
    pub async fn accept(&self) -> Option<quinn::Incoming> {
        self.endpoint.accept().await
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"shutdown");
    }
}

/// Identity proved by the peer of an established connection.
pub fn peer_id(conn: &Connection) -> Result<DeviceId> {
    let identity = conn.peer_identity().ok_or_else(|| anyhow!("peer sent no certificate"))?;
    let certs =
        identity.downcast::<Vec<CertificateDer<'static>>>().map_err(|_| anyhow!("unexpected peer identity type"))?;
    let cert = certs.first().ok_or_else(|| anyhow!("empty certificate chain"))?;
    Ok(DeviceId::from_cert(cert.as_ref()))
}

/// A bidirectional, reliable, ordered byte stream dedicated to one service.
pub struct Channel {
    pub send: SendStream,
    pub recv: RecvStream,
}

impl Channel {
    /// Merges both halves into a single `AsyncRead + AsyncWrite` value.
    pub fn into_io(self) -> tokio::io::Join<RecvStream, SendStream> {
        tokio::io::join(self.recv, self.send)
    }
}

/// Error returned when the peer refuses to open a channel.
#[derive(Debug)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "channel rejected by peer: {}", self.0)
    }
}

impl std::error::Error for Rejected {}

/// Opens a channel for `service` on an existing connection.
pub async fn open_channel(conn: &Connection, service: &str) -> Result<Channel> {
    let (mut send, mut recv) = conn.open_bi().await?;
    let header = OpenChannel { service: service.to_string(), version: CHANNEL_VERSION };
    write_frame(&mut send, &header).await?;
    match read_frame::<_, ChannelReply>(&mut recv).await? {
        ChannelReply::Accepted => Ok(Channel { send, recv }),
        ChannelReply::Rejected { reason } => Err(Rejected(reason).into()),
    }
}

/// A channel the peer asked to open. Answer with [`PendingChannel::accept`] or
/// [`PendingChannel::reject`].
pub struct PendingChannel {
    pub header: OpenChannel,
    send: SendStream,
    recv: RecvStream,
}

impl PendingChannel {
    /// Waits for the next stream the peer opens on `conn` and reads its header.
    pub async fn next(conn: &Connection) -> Result<Self> {
        let (send, mut recv) = conn.accept_bi().await?;
        let header: OpenChannel = read_frame(&mut recv).await?;
        Ok(Self { header, send, recv })
    }

    pub async fn accept(mut self) -> Result<Channel> {
        write_frame(&mut self.send, &ChannelReply::Accepted).await?;
        Ok(Channel { send: self.send, recv: self.recv })
    }

    pub async fn reject(mut self, reason: impl Into<String>) -> Result<()> {
        write_frame(&mut self.send, &ChannelReply::Rejected { reason: reason.into() }).await?;
        self.send.finish()?;
        Ok(())
    }
}

/// Short authentication string shown to users during pairing.
///
/// Derived from the TLS session (exporter) and both device ids, so both
/// ends compute the same six digits only if no one sits in the middle: an
/// attacker would hold two different TLS sessions and the codes would differ.
pub fn sas(conn: &Connection, a: DeviceId, b: DeviceId) -> Result<String> {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    let mut context = Vec::with_capacity(40);
    context.extend_from_slice(first.as_bytes());
    context.extend_from_slice(second.as_bytes());
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, b"crossdrop pairing sas v1", &context)
        .map_err(|_| anyhow!("could not derive pairing code"))?;
    let n = u32::from_be_bytes([out[0], out[1], out[2], out[3]]) % 1_000_000;
    Ok(format!("{:03} {:03}", n / 1000, n % 1000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn local() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[tokio::test]
    async fn mutual_identity_channel_and_sas() {
        let ia = Identity::generate().unwrap();
        let ib = Identity::generate().unwrap();
        let a = Node::bind(&ia, local()).unwrap();
        let b = Node::bind(&ib, local()).unwrap();
        let b_addr = b.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let conn = b.accept().await.unwrap().await.unwrap();
            let peer = peer_id(&conn).unwrap();
            let pending = PendingChannel::next(&conn).await.unwrap();
            assert_eq!(pending.header.service, "test/echo");
            let mut io = pending.accept().await.unwrap().into_io();
            let mut buf = [0u8; 5];
            io.read_exact(&mut buf).await.unwrap();
            io.write_all(&buf).await.unwrap();
            io.shutdown().await.unwrap();
            let code = sas(&conn, b.id(), peer).unwrap();
            // keep the connection alive until the client is done
            conn.closed().await;
            (peer, code)
        });

        let (conn, peer) = a.connect(b_addr).await.unwrap();
        assert_eq!(peer, ib.id());
        let mut io = open_channel(&conn, "test/echo").await.unwrap().into_io();
        io.write_all(b"hello").await.unwrap();
        let mut back = Vec::new();
        io.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"hello");
        let code = sas(&conn, a.id(), peer).unwrap();
        conn.close(0u32.into(), b"done");

        let (seen, server_code) = server.await.unwrap();
        assert_eq!(seen, ia.id());
        assert_eq!(code, server_code);
        assert_eq!(code.len(), 7);
    }

    #[tokio::test]
    async fn rejected_channel_reports_reason() {
        let a = Node::bind(&Identity::generate().unwrap(), local()).unwrap();
        let b = Node::bind(&Identity::generate().unwrap(), local()).unwrap();
        let b_addr = b.local_addr().unwrap();
        tokio::spawn(async move {
            let conn = b.accept().await.unwrap().await.unwrap();
            let pending = PendingChannel::next(&conn).await.unwrap();
            pending.reject("not paired").await.unwrap();
            conn.closed().await;
        });
        let (conn, _) = a.connect(b_addr).await.unwrap();
        let err = open_channel(&conn, "x").await.err().unwrap();
        assert!(err.to_string().contains("not paired"));
    }
}
