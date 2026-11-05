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
use crossdrop_proto::ipc::{DeviceInfo, Event, PairRequestInfo, Request, Response};
use crossdrop_proto::{read_frame, write_frame};

pub mod files;
pub mod paths;

pub use paths::Paths;

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
        let resp: Response = read_frame(&mut self.stream).await?;
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
    anyhow!("unexpected response from daemon: {resp:?}")
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
