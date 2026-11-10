//! Local API served to the CLI, the app and extensions.
//!
//! Unix socket (mode 0600 inside a 0700 directory) on Unix, named pipe on
//! Windows. Each connection is one of three kinds, decided by its first
//! request: request/response, `Register` (event stream), or `Attach` (raw
//! bytes bridged to a QUIC channel).

use std::sync::Arc;

use anyhow::Result;
use crossdrop_proto::ipc::{Request, Response};
use crossdrop_proto::{read_frame, write_frame, FrameError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::debug;

use crate::daemon::Daemon;

async fn handle_client<S>(mut stream: S, daemon: Arc<Daemon>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        let req: Request = match read_frame(&mut stream).await {
            Ok(r) => r,
            Err(FrameError::Closed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match req {
            Request::Attach { channel_id } => {
                let Some(channel) = daemon.take_channel(&channel_id) else {
                    let msg = "unknown or expired channel".to_string();
                    write_frame(&mut stream, &Response::Error { message: msg }).await?;
                    return Ok(());
                };
                write_frame(&mut stream, &Response::Ok).await?;
                let mut remote = channel.into_io();
                let copied = tokio::io::copy_bidirectional(&mut stream, &mut remote).await;
                debug!(channel = %channel_id, ?copied, "channel closed");
                return Ok(());
            }
            Request::Register { service } => return serve_registration(stream, daemon, service).await,
            other => {
                let resp = daemon
                    .handle_request(other)
                    .await
                    .unwrap_or_else(|e| Response::Error { message: format!("{e:#}") });
                write_frame(&mut stream, &resp).await?;
            }
        }
    }
}

/// Forwards incoming-channel events to the extension until it disconnects.
async fn serve_registration<S>(stream: S, daemon: Arc<Daemon>, service: String) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut events = match daemon.register_service(&service) {
        Ok(rx) => rx,
        Err(e) => {
            write_frame(&mut wr, &Response::Error { message: format!("{e:#}") }).await?;
            return Ok(());
        }
    };
    write_frame(&mut wr, &Response::Ok).await?;
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => if write_frame(&mut wr, &event).await.is_err() { break },
                None => break,
            },
            // The extension never writes after registering: any read result
            // (end of stream or stray data) means it is done.
            _ = rd.read(&mut probe) => break,
        }
    }
    drop(events);
    daemon.unregister_service(&service);
    Ok(())
}

#[cfg(unix)]
pub(crate) use unix::bind;
#[cfg(windows)]
pub(crate) use windows::bind;

#[cfg(unix)]
mod unix {
    use super::*;
    use anyhow::{bail, Context};
    use crossdrop_sdk::Paths;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tokio::net::UnixListener;

    pub(crate) struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    /// Longest Unix socket path, in bytes (`sun_path` minus its final NUL).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const MAX_SOCKET_PATH: usize = 107;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const MAX_SOCKET_PATH: usize = 103;

    pub(crate) fn bind(paths: &Paths) -> Result<Listener> {
        let path = paths.ipc_endpoint();
        let len = path.as_os_str().len();
        if len > MAX_SOCKET_PATH {
            bail!(
                "the data directory {} is too deep for the daemon's socket ({len} bytes, at most {MAX_SOCKET_PATH} \
                 with \"daemon.sock\"); use a shorter --home or CROSSDROP_HOME",
                paths.home.display()
            );
        }
        if path.exists() {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                bail!("a crossdrop daemon is already running for {}", paths.home.display());
            }
            std::fs::remove_file(&path)?;
        }
        let inner = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener { inner, path })
    }

    impl Listener {
        pub(crate) async fn serve(self, daemon: Arc<Daemon>) {
            loop {
                match self.inner.accept().await {
                    Ok((stream, _)) => {
                        let d = daemon.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, d).await {
                                debug!("ipc client: {e:#}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("ipc accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }

        pub(crate) fn path(&self) -> PathBuf {
            self.path.clone()
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use anyhow::Context;
    use crossdrop_sdk::Paths;
    use std::path::PathBuf;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    pub(crate) struct Listener {
        first: NamedPipeServer,
        path: PathBuf,
    }

    pub(crate) fn bind(paths: &Paths) -> Result<Listener> {
        let path = paths.ipc_endpoint();
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&path)
            .with_context(|| format!("a crossdrop daemon may already be running ({})", path.display()))?;
        Ok(Listener { first, path })
    }

    impl Listener {
        pub(crate) async fn serve(self, daemon: Arc<Daemon>) {
            let mut server = self.first;
            loop {
                if let Err(e) = server.connect().await {
                    tracing::warn!("ipc accept failed: {e}");
                    continue;
                }
                let client = server;
                server = match ServerOptions::new().reject_remote_clients(true).create(&self.path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("cannot create named pipe: {e}");
                        return;
                    }
                };
                let d = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(client, d).await {
                        debug!("ipc client: {e:#}");
                    }
                });
            }
        }

        pub(crate) fn path(&self) -> PathBuf {
            self.path.clone()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use crossdrop_sdk::Paths;

    #[test]
    fn a_too_deep_data_directory_is_explained() {
        let home = std::env::temp_dir().join("x".repeat(120));
        let err = super::bind(&Paths { home }).err().expect("must fail");
        assert!(err.to_string().contains("too deep"), "{err}");
    }
}
