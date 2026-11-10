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
        crossdrop_sdk::write_atomic(path, toml::to_string_pretty(&config)?.as_bytes())?;
        Ok(config)
    }
}

/// Default device name: the name the user gave this computer.
///
/// macOS has neither `HOSTNAME` in the environment of GUI apps and services
/// nor `/etc/hostname`, so it asks the system for the "Computer Name" first,
/// then falls back to the network host name.
fn hostname() -> String {
    computer_name()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| gethostname::gethostname().into_string().ok())
        .and_then(|s| clean_name(&s))
        .unwrap_or_else(|| "crossdrop".to_string())
}

/// The user-visible computer name (System Settings → General → Sharing).
#[cfg(target_os = "macos")]
fn computer_name() -> Option<String> {
    let out = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn computer_name() -> Option<String> {
    None
}

/// Trims a host name and drops the mDNS `.local` suffix.
fn clean_name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let name = name.strip_suffix(".local").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

/// How many last known addresses are kept per device.
pub const MAX_ADDRS: usize = 4;

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
    /// Send this device our clipboard copies. Unset: we do when it may write
    /// to our clipboard (`crossdrop/clipboard`), as before this setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard_send: Option<bool>,
}

impl TrustedDevice {
    pub fn new(name: String, addr: Option<String>) -> Self {
        Self {
            name,
            permissions: vec![services::FILES.to_string()],
            addrs: addr.into_iter().collect(),
            auto_accept: false,
            clipboard_send: None,
        }
    }

    /// Whether this device gets our clipboard copies.
    pub fn sends_clipboard(&self) -> bool {
        self.clipboard_send.unwrap_or_else(|| self.allows(services::CLIPBOARD))
    }

    /// Puts `addr` first among the last known addresses (at most
    /// [`MAX_ADDRS`]). Returns whether the list changed.
    pub fn remember_addr(&mut self, addr: String) -> bool {
        if self.addrs.first() == Some(&addr) {
            return false;
        }
        self.addrs.retain(|a| *a != addr);
        self.addrs.insert(0, addr);
        self.addrs.truncate(MAX_ADDRS);
        true
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
        crossdrop_sdk::write_atomic(&self.path, toml::to_string_pretty(&file)?.as_bytes())?;
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
    fn host_names_are_cleaned() {
        assert_eq!(clean_name("mona-2.local\n").as_deref(), Some("mona-2"));
        assert_eq!(clean_name("  desk  ").as_deref(), Some("desk"));
        assert_eq!(clean_name(" \n"), None);
        assert_eq!(clean_name(".local"), None);
        assert!(!hostname().is_empty());
    }

    #[test]
    fn latest_address_comes_first() {
        let mut d = TrustedDevice::new("laptop".into(), Some("10.0.0.2:1".into()));
        assert!(!d.remember_addr("10.0.0.2:1".into()));
        for a in ["10.0.0.3:1", "10.0.0.4:1", "10.0.0.5:1", "10.0.0.6:1"] {
            assert!(d.remember_addr(a.into()));
        }
        assert_eq!(d.addrs, ["10.0.0.6:1", "10.0.0.5:1", "10.0.0.4:1", "10.0.0.3:1"]);
        assert!(d.remember_addr("10.0.0.4:1".into()));
        assert_eq!(d.addrs, ["10.0.0.4:1", "10.0.0.6:1", "10.0.0.5:1", "10.0.0.3:1"]);
    }

    #[test]
    fn clipboard_sending_follows_the_permission_until_set() {
        let mut d = TrustedDevice::new("laptop".into(), None);
        assert!(!d.sends_clipboard());
        d.permissions.push(services::CLIPBOARD.into());
        assert!(d.sends_clipboard(), "default: same as the permission");
        d.clipboard_send = Some(false);
        assert!(!d.sends_clipboard(), "receive only");
        d.permissions.clear();
        d.clipboard_send = Some(true);
        assert!(d.sends_clipboard(), "send only");
        // Unset stays out of trusted.toml.
        d.clipboard_send = None;
        assert!(!toml::to_string(&d).unwrap().contains("clipboard_send"));
    }

    #[test]
    fn partial_config_uses_defaults() {
        let config: Config = toml::from_str("name = \"desk\"\nmdns = false").unwrap();
        assert_eq!(config.name, "desk");
        assert_eq!(config.port, crossdrop_proto::DEFAULT_PORT);
        assert!(!config.mdns);
    }
}
