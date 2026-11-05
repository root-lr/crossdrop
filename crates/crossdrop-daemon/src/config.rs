//! `config.toml` and `trusted.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossdrop_proto::services;
use serde::{Deserialize, Serialize};

/// User settings. Missing fields take their default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name shown to other devices.
    pub name: String,
    /// UDP port of the QUIC endpoint.
    pub port: u16,
    /// Folder where received files land.
    pub download_dir: PathBuf,
    /// Where files dragged from a neighbouring screen land.
    pub desktop_dir: PathBuf,
    /// Announce and discover devices with mDNS.
    pub mdns: bool,
    /// Watch the system clipboard and share it with devices allowed
    /// `crossdrop/clipboard`, and apply what they share.
    pub clipboard_sync: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: hostname(),
            port: crossdrop_proto::DEFAULT_PORT,
            download_dir: crossdrop_sdk::paths::default_download_dir(),
            desktop_dir: dirs::desktop_dir().unwrap_or_else(crossdrop_sdk::paths::default_download_dir),
            mdns: true,
            clipboard_sync: true,
        }
    }
}

impl Config {
    /// Loads the config, writing the defaults on first run.
    pub fn load_or_init(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            return toml::from_str(&text).with_context(|| format!("invalid {}", path.display()));
        }
        let config = Self::default();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(&config)?)?;
        Ok(config)
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "crossdrop".to_string())
}

/// A paired device and what it may do here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedDevice {
    pub name: String,
    /// Services this device may open a channel to.
    pub permissions: Vec<String>,
    /// Last known addresses, used when mDNS is unavailable.
    #[serde(default)]
    pub addrs: Vec<String>,
    /// Store files from this device without asking.
    #[serde(default)]
    pub auto_accept: bool,
}

impl TrustedDevice {
    pub fn new(name: String, addr: Option<String>) -> Self {
        Self {
            name,
            permissions: vec![services::FILES.to_string()],
            addrs: addr.into_iter().collect(),
            auto_accept: false,
        }
    }

    pub fn allows(&self, service: &str) -> bool {
        self.permissions.iter().any(|p| p == service)
    }
}

/// Persistent list of paired devices, keyed by device id.
#[derive(Debug, Default)]
pub struct TrustStore {
    path: PathBuf,
    pub devices: BTreeMap<String, TrustedDevice>,
}

#[derive(Serialize, Deserialize, Default)]
struct TrustFile {
    #[serde(default)]
    devices: BTreeMap<String, TrustedDevice>,
}

impl TrustStore {
    pub fn load(path: &Path) -> Result<Self> {
        let devices = if path.exists() {
            let text = std::fs::read_to_string(path)?;
            toml::from_str::<TrustFile>(&text).with_context(|| format!("invalid {}", path.display()))?.devices
        } else {
            BTreeMap::new()
        };
        Ok(Self { path: path.to_path_buf(), devices })
    }

    pub fn save(&self) -> Result<()> {
        let file = TrustFile { devices: self.devices.clone() };
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(&file)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted.toml");
        let mut store = TrustStore::load(&path).unwrap();
        store.devices.insert("ABCD".into(), TrustedDevice::new("laptop".into(), Some("10.0.0.2:47470".into())));
        store.save().unwrap();
        let again = TrustStore::load(&path).unwrap();
        assert_eq!(again.devices, store.devices);
        assert!(again.devices["ABCD"].allows(services::FILES));
        assert!(!again.devices["ABCD"].allows("interdesk/input"));
    }

    #[test]
    fn partial_config_uses_defaults() {
        let config: Config = toml::from_str("name = \"desk\"\nmdns = false").unwrap();
        assert_eq!(config.name, "desk");
        assert_eq!(config.port, crossdrop_proto::DEFAULT_PORT);
        assert!(!config.mdns);
    }
}
