//! Encryption at rest of a node (013 FR-015, D-030): `--enableEncryption` seals the log, the
//! snapshots, the SST files and their manifests with data keys wrapped by a master key. The master
//! key is the one in `--encryptionKeyFile` (32 bytes in Base64, readable only by its owner, as
//! RefDb's keyfile) or, without it, one derived with HKDF-SHA-256 from the cluster's master key
//! of the PKI, the same on every node, so that files copied between nodes open everywhere.
//! `--encryptionCipherMode` chooses AES256-CBC (the default) or AES256-GCM for new files; a file
//! keeps the mode it was written with.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use teseladb_crypto::at_rest::{CipherMode, Encryption, KeyRing};
use teseladb_crypto::{Sha256, hkdf};
use teseladb_x509::pem::base64_decode;

/// Bytes of the master key.
pub const KEY_LEN: usize = 32;
/// What the at-rest master key is derived for from the cluster's master key.
const DERIVE_INFO: &[u8] = b"teseladb encryption at rest v1";
/// Permission bits a key file may have besides its owner's.
const GROUP_OTHER: u32 = 0o077;

/// The options of encryption at rest.
#[derive(Clone, PartialEq, Eq)]
pub struct AtRestOpts {
    /// `--enableEncryption`.
    pub enable: bool,
    /// `--encryptionKeyFile`.
    pub key_file: Option<PathBuf>,
    /// `--encryptionCipherMode`.
    pub mode: CipherMode,
    /// The KMIP server of the master key (013 FR-028).
    pub kmip: Option<crate::kmip::KmipOpts>,
}

impl Default for AtRestOpts {
    fn default() -> AtRestOpts {
        AtRestOpts { enable: false, key_file: None, mode: CipherMode::Cbc, kmip: None }
    }
}

impl std::fmt::Debug for AtRestOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtRestOpts")
            .field("enable", &self.enable)
            .field("key_file", &self.key_file)
            .field("mode", &self.mode)
            .field("kmip", &self.kmip)
            .finish()
    }
}

/// The ring of master key `key` making keys of `mode`, seeded from the operating system.
pub fn ring(key: &[u8; KEY_LEN], mode: CipherMode) -> Encryption {
    Encryption(Arc::new(KeyRing::new(key, mode, teseladb_sys::fill_random)))
}

/// The at-rest master key derived from the cluster's master key.
fn derived(cluster: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
    let prk = hkdf::extract::<Sha256>(b"", cluster);
    let mut out = [0u8; KEY_LEN];
    // 32 bytes of SHA-256 output are always available.
    let _ = hkdf::expand::<Sha256>(prk.as_ref(), DERIVE_INFO, &mut out);
    out
}

fn read_key_file(path: &std::path::Path) -> Result<[u8; KEY_LEN], String> {
    let name = path.display();
    let meta = std::fs::metadata(path).map_err(|e| format!("--encryptionKeyFile {name}: {e}"))?;
    if meta.permissions().mode() & GROUP_OTHER != 0 {
        return Err(format!("--encryptionKeyFile {name}: permissions are too open (use 0600)"));
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("--encryptionKeyFile {name}: {e}"))?;
    let mut bytes = base64_decode(text.trim().as_bytes())
        .map_err(|_| format!("--encryptionKeyFile {name}: not Base64"))?;
    let key: Result<[u8; KEY_LEN], _> = bytes.as_slice().try_into();
    bytes.fill(0);
    key.map_err(|_| format!("--encryptionKeyFile {name}: the key must have 32 bytes"))
}

/// The encryption of a node with options `o`, the key `kmip` got from the KMIP server and, on a
/// node with a PKI, the cluster's master key `cluster`; `None` when encryption is off.
pub fn from_opts(
    o: &AtRestOpts,
    kmip: Option<&[u8; KEY_LEN]>,
    cluster: Option<&[u8; KEY_LEN]>,
) -> Result<Option<Encryption>, String> {
    if !o.enable {
        return Ok(None);
    }
    let mut key = match (&o.key_file, kmip, cluster) {
        (Some(path), _, _) => read_key_file(path)?,
        (None, Some(k), _) => *k,
        (None, None, Some(c)) => derived(c),
        (None, None, None) => {
            return Err("--enableEncryption without a PKI (--insecure) needs --encryptionKeyFile"
                .to_owned());
        }
    };
    let r = ring(&key, o.mode);
    key.fill(0);
    Ok(Some(r))
}
