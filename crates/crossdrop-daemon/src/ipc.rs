//! Local API served to the CLI, the app and extensions.
//!
//! Unix socket (mode 0600 inside a 0700 directory) on Unix, named pipe
//! open to this user and SYSTEM only on Windows. Each connection is one of three kinds, decided by its first
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
        security: Option<security::Descriptor>,
    }

    /// Creates one instance of the pipe, reachable only by this user (and
    /// SYSTEM) when `security` is set. The default descriptor of a named
    /// pipe also lets Everyone and anonymous users open it for reading.
    fn create(
        options: &mut ServerOptions,
        path: &std::path::Path,
        security: Option<&security::Descriptor>,
    ) -> std::io::Result<NamedPipeServer> {
        options.reject_remote_clients(true);
        match security {
            // SAFETY: the attributes point to a valid descriptor that outlives the call.
            Some(sd) => unsafe { options.create_with_security_attributes_raw(path, sd.attributes()) },
            None => options.create(path),
        }
    }

    pub(crate) fn bind(paths: &Paths) -> Result<Listener> {
        let path = paths.ipc_endpoint();
        let security = match security::Descriptor::current_user_only() {
            Ok(sd) => Some(sd),
            Err(e) => {
                tracing::warn!("cannot restrict the daemon's pipe to this user: {e}");
                None
            }
        };
        let first = create(ServerOptions::new().first_pipe_instance(true), &path, security.as_ref())
            .with_context(|| format!("a crossdrop daemon may already be running ({})", path.display()))?;
        Ok(Listener { first, path, security })
    }

    impl Listener {
        pub(crate) async fn serve(self, daemon: Arc<Daemon>) {
            let new_instance = || create(&mut ServerOptions::new(), &self.path, self.security.as_ref());
            let mut server = self.first;
            loop {
                if let Err(e) = server.connect().await {
                    // This instance may be unusable: replace it rather than
                    // retrying it at once, which could spin.
                    tracing::warn!("ipc accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    server = match new_instance() {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::error!("cannot create named pipe: {e}");
                            return;
                        }
                    };
                    continue;
                }
                let client = server;
                server = match new_instance() {
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

    /// A security descriptor granting the pipe to the current user and SYSTEM.
    mod security {
        use std::ffi::c_void;
        use std::io;
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
        use windows_sys::Win32::Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        pub(crate) struct Descriptor {
            sd: PSECURITY_DESCRIPTOR,
            attributes: SECURITY_ATTRIBUTES,
        }

        // SAFETY: the descriptor is immutable once built and freed only on drop.
        unsafe impl Send for Descriptor {}
        unsafe impl Sync for Descriptor {}

        impl Descriptor {
            /// `D:P(A;;GA;;;<user>)(A;;GA;;;SY)`: full access for this user and
            /// SYSTEM, nobody else, not inherited.
            pub(crate) fn current_user_only() -> io::Result<Self> {
                let sid = current_user_sid()?;
                let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)").encode_utf16().chain(Some(0)).collect();
                let mut sd: PSECURITY_DESCRIPTOR = null_mut();
                // SAFETY: `sddl` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor.
                let ok = unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl.as_ptr(),
                        SDDL_REVISION_1,
                        &mut sd,
                        null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                let attributes = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: sd,
                    bInheritHandle: 0,
                };
                Ok(Self { sd, attributes })
            }

            pub(crate) fn attributes(&self) -> *mut c_void {
                &self.attributes as *const SECURITY_ATTRIBUTES as *mut c_void
            }
        }

        impl Drop for Descriptor {
            fn drop(&mut self) {
                // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
                unsafe { LocalFree(self.sd) };
            }
        }

        /// The SID of the user this process runs as, as a string (`S-1-5-21-…`).
        fn current_user_sid() -> io::Result<String> {
            // SAFETY: plain Win32 calls on handles and buffers owned here.
            unsafe {
                let mut token: HANDLE = null_mut();
                if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut len = 0u32;
                GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
                // u64s keep TOKEN_USER (which holds a pointer) aligned.
                let mut buf = vec![0u64; (len as usize).div_ceil(8)];
                let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
                let err = io::Error::last_os_error();
                CloseHandle(token);
                if ok == 0 {
                    return Err(err);
                }
                let user = &*(buf.as_ptr() as *const TOKEN_USER);
                let mut text = null_mut();
                if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let n = (0..).take_while(|&i| *text.add(i) != 0).count();
                let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
                LocalFree(text.cast());
                Ok(sid)
            }
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
