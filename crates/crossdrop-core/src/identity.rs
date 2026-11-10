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
        Self::certify(&key)
    }

    /// A new self-signed certificate over `key`.
    fn certify(key: &rcgen::KeyPair) -> Result<Self> {
        let params = rcgen::CertificateParams::new(vec!["crossdrop".to_string()])?;
        let cert = params.self_signed(key)?;
        Ok(Self::from_parts(cert.der().to_vec(), key.serialize_der()))
    }

    fn from_parts(cert: Vec<u8>, key: Vec<u8>) -> Self {
        let id = DeviceId::from_cert(&cert);
        Self { cert: CertificateDer::from(cert), key, id }
    }

    /// Loads the identity stored in `dir`, generating it on first run.
    ///
    /// The key is written before the certificate, and the certificate
    /// defines the device id. So a key without a certificate is a first run
    /// that was cut short (the id was never shown to anyone) and gets its
    /// certificate now, while a certificate without its key is an error: the
    /// identity is lost and must not be silently replaced.
    pub fn load_or_generate(dir: &Path) -> Result<Self> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        match (cert_path.exists(), key_path.exists()) {
            (true, true) => {
                let cert = std::fs::read(&cert_path).context("reading certificate")?;
                let key = std::fs::read(&key_path).context("reading private key")?;
                let identity = Self::from_parts(cert, key);
                identity.check().with_context(|| {
                    format!("{} and {} do not belong together", cert_path.display(), key_path.display())
                })?;
                Ok(identity)
            }
            (true, false) => bail!(
                "{} is missing, so this device's identity is lost. Move {} away to start with a new \
                 identity; paired devices will have to pair with it again",
                key_path.display(),
                cert_path.display()
            ),
            (false, true) => {
                let key = std::fs::read(&key_path).context("reading private key")?;
                let key = rcgen::KeyPair::try_from(key.as_slice()).context("invalid private key")?;
                let identity = Self::certify(&key)?;
                write_atomic(&cert_path, identity.cert.as_ref(), false)?;
                Ok(identity)
            }
            (false, false) => {
                std::fs::create_dir_all(dir)?;
                let identity = Self::generate()?;
                write_atomic(&key_path, &identity.key, true)?;
                write_atomic(&cert_path, identity.cert.as_ref(), false)?;
                Ok(identity)
            }
        }
    }

    /// Fails unless the key parses and matches the certificate's public key,
    /// which TLS would otherwise only notice at the first handshake.
    fn check(&self) -> Result<()> {
        let provider = rustls::crypto::ring::default_provider();
        rustls::sign::CertifiedKey::from_der(vec![self.cert()], self.key(), &provider)?;
        Ok(())
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

/// Writes `data` to `path` through a synced temporary file and a rename, so
/// that a crash leaves either the old file or the complete new one.
/// `private`: readable by this user only (Unix).
fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    // On Windows the data directory lives in the user's profile, which is
    // already restricted to that user.
    #[cfg(not(unix))]
    let _ = private;
    let mut f = options.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
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
    fn a_key_without_certificate_is_completed() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::load_or_generate(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(CERT_FILE)).unwrap();
        let again = Identity::load_or_generate(dir.path()).unwrap();
        assert_eq!(again.key, first.key, "the key must be kept");
        assert!(dir.path().join(CERT_FILE).exists());
        assert_eq!(Identity::load_or_generate(dir.path()).unwrap().id(), again.id());
    }

    #[test]
    fn a_certificate_without_key_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_generate(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        let err = Identity::load_or_generate(dir.path()).err().unwrap();
        assert!(err.to_string().contains("identity is lost"), "{err}");
        assert!(!dir.path().join(KEY_FILE).exists(), "nothing must be regenerated");
    }

    #[test]
    fn a_key_from_another_identity_is_refused() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        Identity::load_or_generate(a.path()).unwrap();
        Identity::load_or_generate(b.path()).unwrap();
        std::fs::copy(b.path().join(KEY_FILE), a.path().join(KEY_FILE)).unwrap();
        let err = Identity::load_or_generate(a.path()).err().unwrap();
        assert!(format!("{err:#}").contains("do not belong together"), "{err:#}");

        std::fs::write(a.path().join(KEY_FILE), b"garbage").unwrap();
        assert!(Identity::load_or_generate(a.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_generate(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(KEY_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn identity_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_generate(dir.path()).unwrap();
        let b = Identity::load_or_generate(dir.path()).unwrap();
        assert_eq!(a.id(), b.id());
    }
}
