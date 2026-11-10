//! Where crossdrop keeps its files, and where the daemon listens.

use std::path::{Path, PathBuf};

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

/// Writes `data` to `path` through a synced temporary file next to it and a
/// rename, so that a crash or a full disk leaves either the previous file or
/// the complete new one, never a truncated file.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
