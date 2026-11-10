//! Client for the local crossdrop daemon.
//!
//! Used by the CLI, the desktop app and every extension (interdesk included).
//! An extension typically:
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use crossdrop_sdk::Client;
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! // Serve a service…
//! let mut reg = Client::connect_default().await?.register("example/echo").await?;
//! while let Some(incoming) = reg.next().await? {
//!     let mut stream = incoming.accept().await?;
//!     tokio::spawn(async move {
//!         let mut buf = Vec::new();
//!         stream.read_to_end(&mut buf).await.ok();
//!         stream.write_all(&buf).await.ok();
//!     });
//! }
//!
//! // …or open a channel to a paired device.
//! let mut client = Client::connect_default().await?;
//! let mut stream = client.open_channel("laptop", "example/echo").await?;
//! stream.write_all(b"ping").await?;
//! # Ok(()) }
//! ```

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use crossdrop_proto::ipc::{DeviceInfo, Event, OfferInfo, PairRequestInfo, Request, Response};
use crossdrop_proto::{read_frame, write_frame};

pub mod files;
pub mod paths;
pub mod signal;

pub use paths::{write_atomic, Paths};
pub use signal::shutdown_signal;

#[cfg(unix)]
pub type IpcStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type IpcStream = tokio::net::windows::named_pipe::NamedPipeClient;

/// Opens a raw connection to the daemon's IPC endpoint.
pub async fn connect_ipc(paths: &Paths) -> Result<IpcStream> {
    connect_endpoint(&paths.ipc_endpoint())
        .await
        .with_context(|| "cannot reach the crossdrop daemon. Is it running? Start it with `crossdrop daemon`.")
}

#[cfg(unix)]
async fn connect_endpoint(endpoint: &Path) -> std::io::Result<IpcStream> {
    tokio::net::UnixStream::connect(endpoint).await
}

#[cfg(windows)]
async fn connect_endpoint(endpoint: &Path) -> std::io::Result<IpcStream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return Ok(client),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// A request/response connection to the daemon.
pub struct Client {
    paths: Paths,
    stream: IpcStream,
}

impl Client {
    /// Connects using the default data directory (or `CROSSDROP_HOME`).
    pub async fn connect_default() -> Result<Self> {
        Self::connect(Paths::resolve(None)?).await
    }

    pub async fn connect(paths: Paths) -> Result<Self> {
        let stream = connect_ipc(&paths).await?;
        Ok(Self { paths, stream })
    }

    /// Sends one request and waits for its response.
    pub async fn request(&mut self, req: &Request) -> Result<Response> {
        write_frame(&mut self.stream, req).await?;
        let resp: Response = read_frame(&mut self.stream).await.map_err(version_hint)?;
        if let Response::Error { message } = resp {
            bail!(message);
        }
        Ok(resp)
    }

    pub async fn status(&mut self) -> Result<(String, String, u16, String)> {
        match self.request(&Request::Status).await? {
            Response::Status { device_id, name, port, version } => Ok((device_id, name, port, version)),
            other => Err(unexpected(other)),
        }
    }

    pub async fn devices(&mut self) -> Result<Vec<DeviceInfo>> {
        match self.request(&Request::ListDevices).await? {
            Response::Devices { devices } => Ok(devices),
            other => Err(unexpected(other)),
        }
    }

    /// Starts pairing; returns `(pair_id, code, peer_id, peer_name)`.
    pub async fn pair_start(&mut self, target: &str) -> Result<(String, String, String, String)> {
        match self.request(&Request::PairStart { target: target.into() }).await? {
            Response::PairChallenge { pair_id, sas, peer_id, peer_name } => Ok((pair_id, sas, peer_id, peer_name)),
            other => Err(unexpected(other)),
        }
    }

    /// Confirms (or cancels) a pairing started here. Waits for the other side.
    pub async fn pair_confirm(&mut self, pair_id: &str, accept: bool) -> Result<(bool, String, String)> {
        match self.request(&Request::PairConfirm { pair_id: pair_id.into(), accept }).await? {
            Response::PairResult { paired, peer_id, peer_name } => Ok((paired, peer_id, peer_name)),
            other => Err(unexpected(other)),
        }
    }

    pub async fn pair_requests(&mut self) -> Result<Vec<PairRequestInfo>> {
        match self.request(&Request::ListPairRequests).await? {
            Response::PairRequests { requests } => Ok(requests),
            other => Err(unexpected(other)),
        }
    }

    pub async fn pair_answer(&mut self, pair_id: &str, accept: bool) -> Result<()> {
        self.request(&Request::PairAnswer { pair_id: pair_id.into(), accept }).await?;
        Ok(())
    }

    pub async fn unpair(&mut self, device: &str) -> Result<()> {
        self.request(&Request::Unpair { device: device.into() }).await?;
        Ok(())
    }

    pub async fn set_permission(&mut self, device: &str, service: &str, allow: bool) -> Result<()> {
        let req = Request::SetPermission { device: device.into(), service: service.into(), allow };
        self.request(&req).await?;
        Ok(())
    }

    /// Incoming file transfers waiting for a decision.
    pub async fn offers(&mut self) -> Result<Vec<OfferInfo>> {
        match self.request(&Request::ListOffers).await? {
            Response::Offers { offers } => Ok(offers),
            other => Err(unexpected(other)),
        }
    }

    pub async fn offer_answer(&mut self, offer_id: &str, accept: bool) -> Result<()> {
        self.request(&Request::OfferAnswer { offer_id: offer_id.into(), accept }).await?;
        Ok(())
    }

    /// Store files from `device` without asking.
    pub async fn set_auto_accept(&mut self, device: &str, enabled: bool) -> Result<()> {
        self.request(&Request::SetAutoAccept { device: device.into(), enabled }).await?;
        Ok(())
    }

    /// Sends (or stops sending) this device's clipboard copies to `device`.
    pub async fn set_clipboard_send(&mut self, device: &str, enabled: bool) -> Result<()> {
        self.request(&Request::SetClipboardSend { device: device.into(), enabled }).await?;
        Ok(())
    }

    /// Sends text to one device, or to all clipboard peers. Returns how many received it.
    pub async fn clipboard_send(&mut self, text: &str, device: Option<&str>) -> Result<u32> {
        let req = Request::ClipboardSend { text: text.into(), device: device.map(Into::into) };
        match self.request(&req).await? {
            Response::Sent { count } => Ok(count),
            other => Err(unexpected(other)),
        }
    }

    /// Last text received from another device, with its sender's name.
    pub async fn clipboard_get(&mut self) -> Result<Option<(String, String)>> {
        match self.request(&Request::ClipboardGet).await? {
            Response::Clipboard { text: Some(text), from } => Ok(Some((text, from.unwrap_or_default()))),
            Response::Clipboard { text: None, .. } => Ok(None),
            other => Err(unexpected(other)),
        }
    }

    /// Opens a channel to `device` for `service` and returns a raw byte stream.
    pub async fn open_channel(&mut self, device: &str, service: &str) -> Result<IpcStream> {
        let req = Request::OpenChannel { device: device.into(), service: service.into() };
        match self.request(&req).await? {
            Response::ChannelReady { channel_id } => attach(&self.paths, &channel_id).await,
            other => Err(unexpected(other)),
        }
    }

    /// Becomes the handler of `service` on this device.
    pub async fn register(mut self, service: &str) -> Result<Registration> {
        self.request(&Request::Register { service: service.into() }).await?;
        Ok(Registration { paths: self.paths, stream: self.stream })
    }
}

async fn attach(paths: &Paths, channel_id: &str) -> Result<IpcStream> {
    let mut stream = connect_ipc(paths).await?;
    write_frame(&mut stream, &Request::Attach { channel_id: channel_id.into() }).await?;
    match read_frame::<_, Response>(&mut stream).await? {
        Response::Ok => Ok(stream),
        Response::Error { message } => bail!(message),
        other => Err(unexpected(other)),
    }
}

fn unexpected(resp: Response) -> anyhow::Error {
    anyhow!("unexpected response from daemon: {resp:?}. {VERSION_HINT}")
}

const VERSION_HINT: &str = concat!(
    "The daemon may run another version of crossdrop than this client (",
    env!("CARGO_PKG_VERSION"),
    "); compare with `crossdrop status` and restart the daemon."
);

/// An answer the client cannot read, or no answer at all to a request (the
/// daemon closes the connection on a request it cannot parse), usually means
/// the daemon and this client are different versions.
fn version_hint(e: crossdrop_proto::FrameError) -> anyhow::Error {
    use crossdrop_proto::FrameError;
    match e {
        FrameError::Json(_) | FrameError::Closed => anyhow::Error::new(e).context(VERSION_HINT),
        other => other.into(),
    }
}

/// A registered service. Drop it to unregister.
pub struct Registration {
    paths: Paths,
    stream: IpcStream,
}

impl Registration {
    /// Waits for the next channel a peer opens to this service.
    /// `None` when the daemon goes away.
    pub async fn next(&mut self) -> Result<Option<Incoming>> {
        match read_frame::<_, Event>(&mut self.stream).await {
            Ok(Event::IncomingChannel { channel_id, device, service }) => {
                Ok(Some(Incoming { paths: self.paths.clone(), channel_id, device, service }))
            }
            Err(crossdrop_proto::FrameError::Closed) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// A channel opened by a peer, not yet attached.
pub struct Incoming {
    paths: Paths,
    pub channel_id: String,
    /// Device id of the peer.
    pub device: String,
    pub service: String,
}

impl Incoming {
    pub async fn accept(self) -> Result<IpcStream> {
        attach(&self.paths, &self.channel_id).await
    }
}

/// Re-exported so callers can build paths without extra dependencies.
pub fn default_home() -> Result<PathBuf> {
    Paths::resolve(None).map(|p| p.home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_answers_point_at_a_version_mismatch() {
        let bad = serde_json::from_str::<Response>("{\"type\":\"from_the_future\"}").unwrap_err();
        let msg = format!("{:#}", version_hint(crossdrop_proto::FrameError::Json(bad)));
        assert!(msg.contains("another version"), "{msg}");
        let io = std::io::Error::other("broken pipe");
        assert!(!format!("{:#}", version_hint(io.into())).contains("another version"));
    }
}
