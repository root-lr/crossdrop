//! Where crossdrop keeps its files, and where the daemon listens.

use std::path::PathBuf;

use anyhow::{anyhow, Result};

/// Environment variable overriding the data directory. Lets several daemons
/// run on one machine (tests, demos).
pub const HOME_ENV: &str = "CROSSDROP_HOME";

#[derive(Debug, Clone)]
pub struct Paths {
    /// Data directory: identity, trust store, config, IPC socket.
    pub home: PathBuf,
}

impl Paths {
    /// `explicit` > `$CROSSDROP_HOME` > the OS config directory.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let home = match explicit {
            Some(p) => p,
            None => match std::env::var_os(HOME_ENV) {
                Some(p) => PathBuf::from(p),
                None => {
                    dirs::config_dir().ok_or_else(|| anyhow!("no config directory on this system"))?.join("crossdrop")
                }
            },
        };
        Ok(Self { home })
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    pub fn trust_file(&self) -> PathBuf {
        self.home.join("trusted.toml")
    }

    /// Unix socket path, or named pipe name on Windows.
    #[cfg(unix)]
    pub fn ipc_endpoint(&self) -> PathBuf {
        self.home.join("daemon.sock")
    }

    #[cfg(windows)]
    pub fn ipc_endpoint(&self) -> PathBuf {
        let hash = blake3::hash(self.home.to_string_lossy().as_bytes()).to_hex();
        PathBuf::from(format!(r"\\.\pipe\crossdrop-{}", &hash[..12]))
    }
}

/// Default folder for received files.
pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("crossdrop")
}
