//! crossdropd: the per-device daemon.
//!
//! It owns the identity, the QUIC endpoint and the trust store, runs the
//! built-in services and exposes the local API used by the CLI, the app and
//! extensions such as interdesk.

mod clipboard;
mod config;
mod daemon;
mod files;
mod ipc;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::Result;
use crossdrop_core::{DeviceId, Discovery, DiscoveryEvent, Identity, Node};
use crossdrop_sdk::Paths;
use tokio::task::JoinHandle;
use tracing::info;

pub use config::{Config, TrustStore, TrustedDevice};

/// Overrides applied on top of `config.toml`.
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub home: Option<PathBuf>,
    pub name: Option<String>,
    pub port: Option<u16>,
    pub mdns: Option<bool>,
    pub download_dir: Option<PathBuf>,
    /// Override `clipboard_sync` (tests turn it off).
    pub clipboard_sync: Option<bool>,
    pub desktop_dir: Option<PathBuf>,
}

/// A running daemon. Dropping it does not stop it; call [`Handle::shutdown`].
pub struct Handle {
    pub id: DeviceId,
    pub port: u16,
    pub paths: Paths,
    node: Node,
    tasks: Vec<JoinHandle<()>>,
    ipc_path: PathBuf,
    _discovery: Option<Discovery>,
}

impl Handle {
    pub fn shutdown(self) {
        self.node.close();
        for task in &self.tasks {
            task.abort();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.ipc_path);
        #[cfg(not(unix))]
        let _ = &self.ipc_path;
    }
}

/// Starts the daemon in the current tokio runtime.
pub async fn start(opts: Options) -> Result<Handle> {
    let paths = Paths::resolve(opts.home)?;
    create_private_dir(&paths.home)?;

    let mut config = Config::load_or_init(&paths.config_file())?;
    if let Some(name) = opts.name {
        config.name = name;
    }
    if let Some(port) = opts.port {
        config.port = port;
    }
    if let Some(mdns) = opts.mdns {
        config.mdns = mdns;
    }
    if let Some(dir) = opts.download_dir {
        config.download_dir = dir;
    }
    if let Some(dir) = opts.desktop_dir {
        config.desktop_dir = dir;
    }
    if let Some(sync) = opts.clipboard_sync {
        config.clipboard_sync = sync;
    }

    let identity = Identity::load_or_generate(&paths.home)?;
    let listener = ipc::bind(&paths)?;
    let node = Node::bind(&identity, SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port)))?;
    let port = node.local_addr()?.port();
    let trust = TrustStore::load(&paths.trust_file())?;
    let system_clipboard = if config.clipboard_sync { clipboard::SystemClipboard::start() } else { None };
    if config.clipboard_sync && system_clipboard.is_none() {
        tracing::warn!("no system clipboard here; `crossdrop clip send/get` still work");
    }
    let daemon = daemon::Daemon::new(config.clone(), node.clone(), trust, system_clipboard);

    let mut tasks = vec![tokio::spawn(daemon.clone().accept_loop()), tokio::spawn(daemon.clone().clipboard_loop())];
    let ipc_path = listener.path();
    tasks.push(tokio::spawn(listener.serve(daemon.clone())));

    let discovery = if config.mdns {
        match Discovery::start(identity.id(), &config.name, port) {
            Ok((discovery, mut found)) => {
                let d = daemon.clone();
                tasks.push(tokio::spawn(async move {
                    while let Some(event) = found.recv().await {
                        match event {
                            DiscoveryEvent::Found(device) => d.on_discovered(device),
                            DiscoveryEvent::Lost(id) => d.on_lost(id),
                        }
                    }
                }));
                Some(discovery)
            }
            Err(e) => {
                tracing::warn!("mDNS unavailable, add devices by address: {e}");
                None
            }
        }
    } else {
        None
    };

    info!(id = %identity.id(), name = %config.name, port, home = %paths.home.display(), "crossdropd started");
    Ok(Handle { id: identity.id(), port, paths, node, tasks, ipc_path, _discovery: discovery })
}

/// Runs the daemon until Ctrl+C or SIGTERM.
pub async fn run(opts: Options) -> Result<()> {
    let stop = crossdrop_sdk::shutdown_signal()?;
    let handle = start(opts).await?;
    stop.await;
    info!("shutting down");
    handle.shutdown();
    Ok(())
}

fn create_private_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
