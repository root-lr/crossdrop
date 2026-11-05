//! Device identity: a self-signed certificate over an Ed25519 key.
//!
//! The [`DeviceId`] is the BLAKE3 hash of the certificate (truncated to 160
//! bits), the same model Syncthing uses with SHA-256. Peers pin it at pairing
//! time, so no certificate authority is involved.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use data_encoding::BASE32_NOPAD;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const CERT_FILE: &str = "identity.crt";
const KEY_FILE: &str = "identity.key";

/// Stable, public identifier of a device.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId([u8; 20]);

impl DeviceId {
    pub fn from_cert(cert: &[u8]) -> Self {
        let hash = blake3::hash(cert);
        let mut id = [0u8; 20];
        id.copy_from_slice(&hash.as_bytes()[..20]);
        Self(id)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// First two groups, enough to tell devices apart in a list.
    pub fn short(&self) -> String {
        self.to_string()[..9].to_string()
    }

    /// Compact form without separators, used in mDNS names and file names.
    pub fn compact(&self) -> String {
        BASE32_NOPAD.encode(&self.0)
    }
}

impl fmt::Display for DeviceId {
    /// `ABCD-EFGH-…` : 32 base32 characters in groups of four.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.compact();
        let groups: Vec<&str> = (0..s.len()).step_by(4).map(|i| &s[i..i + 4]).collect();
        f.write_str(&groups.join("-"))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({})", self.short())
    }
}

impl FromStr for DeviceId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let clean: String = s.chars().filter(|c| *c != '-').collect::<String>().to_uppercase();
        let bytes = BASE32_NOPAD.decode(clean.as_bytes()).context("invalid device id")?;
        if bytes.len() != 20 {
            bail!("invalid device id length");
        }
        let mut id = [0u8; 20];
        id.copy_from_slice(&bytes);
        Ok(Self(id))
    }
}

/// Certificate and private key of this device.
pub struct Identity {
    cert: CertificateDer<'static>,
    key: Vec<u8>,
    id: DeviceId,
}

impl Identity {
    /// Creates a fresh identity in memory.
    pub fn generate() -> Result<Self> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let params = rcgen::CertificateParams::new(vec!["crossdrop".to_string()])?;
        let cert = params.self_signed(&key)?;
        Ok(Self::from_parts(cert.der().to_vec(), key.serialize_der()))
    }

    fn from_parts(cert: Vec<u8>, key: Vec<u8>) -> Self {
        let id = DeviceId::from_cert(&cert);
        Self { cert: CertificateDer::from(cert), key, id }
    }

    /// Loads the identity stored in `dir`, generating it on first run.
    pub fn load_or_generate(dir: &Path) -> Result<Self> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        if cert_path.exists() && key_path.exists() {
            let cert = std::fs::read(&cert_path).context("reading certificate")?;
            let key = std::fs::read(&key_path).context("reading private key")?;
            return Ok(Self::from_parts(cert, key));
        }
        std::fs::create_dir_all(dir)?;
        let identity = Self::generate()?;
        std::fs::write(&cert_path, identity.cert.as_ref())?;
        write_private(&key_path, &identity.key)?;
        Ok(identity)
    }

    pub fn id(&self) -> DeviceId {
        self.id
    }

    pub fn cert(&self) -> CertificateDer<'static> {
        self.cert.clone()
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone()))
    }
}

#[cfg(unix)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    // The data directory lives in the user's profile, which Windows already
    // restricts to that user.
    std::fs::write(path, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_roundtrip() {
        let identity = Identity::generate().unwrap();
        let text = identity.id().to_string();
        assert_eq!(text.len(), 32 + 7);
        assert_eq!(text.parse::<DeviceId>().unwrap(), identity.id());
        assert_eq!(text.to_lowercase().replace('-', "").parse::<DeviceId>().unwrap(), identity.id());
    }

    #[test]
    fn identity_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_generate(dir.path()).unwrap();
        let b = Identity::load_or_generate(dir.path()).unwrap();
        assert_eq!(a.id(), b.id());
    }
}
