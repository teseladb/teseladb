//! The encrypted audit log of the reference enterprise server 9.0.4 (`auditLog.localAuditKeyFile`,
//! 013 FR-031), worked out from files it wrote with a known key (measured on 31-10-2026): a header
//! with the log's own key wrapped under the key file's (AES-256-CBC) and a GMAC, and each message
//! as its BSON document, compressed with zstd when asked, sealed with AES-256-GCM under the log's
//! key with a 12-byte tag, the time as additional data and a counter in the nonce.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::{Document, Value};
use teseladb_crypto::aes::Aes;
use teseladb_crypto::modes::{AesGcm, cbc_encrypt};

/// `version` of the header.
pub const VERSION: &str = "0.0";
/// `auditRecordType` of the header.
const HEADER: &str = "header";
/// `keyStoreIdentifier.provider` of a key file.
const PROVIDER_LOCAL: &str = "local";
/// `keyStoreIdentifier.provider` of a KMIP key.
const PROVIDER_KMIP: &str = "kmip";
/// `keyStoreIdentifier.keyWrapMethod` of a key wrapped with KMIP's Encrypt.
const KEY_WRAP_ENCRYPT: &str = "encrypt";
/// Length of the GCM tag the reference server keeps.
pub const TAG_LEN: usize = 12;
/// Length of the GCM nonce: a fixed field and the message counter.
pub const NONCE_LEN: usize = 12;
/// The fixed field at the start of every nonce (measured: always 1).
const NONCE_FIELD: [u8; 4] = [1, 0, 0, 0];

/// How each message is compressed before it is sealed (`auditLog.compressionMode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// `none`.
    #[default]
    None,
    /// `zstd`.
    Zstd,
}

impl Compression {
    /// Every mode.
    pub const ALL: [Compression; 2] = [Compression::None, Compression::Zstd];

    /// Its name in `auditLog.compressionMode`.
    pub fn name(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Zstd => "zstd",
        }
    }
}

/// The state of an encrypted audit log: the log's key, wrapped and in clear, and the counter of
/// the messages sealed.
pub struct AuditEncryption {
    log_key: [u8; 32],
    encrypted_key: Vec<u8>,
    /// `keyStoreIdentifier` of the header.
    key_store: Document,
    compression: Compression,
    counter: AtomicU64,
}

impl std::fmt::Debug for AuditEncryption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditEncryption")
            .field("key_store", &self.key_store)
            .field("compression", &self.compression)
            .finish_non_exhaustive()
    }
}

impl Drop for AuditEncryption {
    fn drop(&mut self) {
        self.log_key = [0; 32];
        std::hint::black_box(&self.log_key);
    }
}

fn nonce(counter: u64) -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    n[..4].copy_from_slice(&NONCE_FIELD);
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
}

/// `tag ‖ nonce ‖ ciphertext` of AES-256-GCM with a 12-byte tag.
fn seal(key: &[u8; 32], counter: u64, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let n = nonce(counter);
    let mut data = plaintext.to_vec();
    let tag = match Aes::new(key) {
        Ok(aes) => AesGcm::new(aes).seal_in_place(&n, aad, &mut data),
        // A 32-byte key is always an AES-256 key.
        Err(_) => [0; 16],
    };
    let mut out = Vec::with_capacity(TAG_LEN + NONCE_LEN + data.len());
    out.extend_from_slice(&tag[..TAG_LEN]);
    out.extend_from_slice(&n);
    out.extend_from_slice(&data);
    out
}

impl AuditEncryption {
    /// An encrypted log with `log_key`, wrapped under `master` (the key file's) with `wrap_iv`.
    pub fn new(
        master: &[u8; 32],
        log_key: [u8; 32],
        wrap_iv: [u8; 16],
        key_file: &str,
        compression: Compression,
    ) -> AuditEncryption {
        let mut encrypted_key = wrap_iv.to_vec();
        if let Ok(aes) = Aes::new(master) {
            encrypted_key.extend_from_slice(&cbc_encrypt(&aes, &wrap_iv, &log_key));
        }
        let key_store = Document::new().with("provider", PROVIDER_LOCAL).with("filename", key_file);
        AuditEncryption {
            log_key,
            encrypted_key,
            key_store,
            compression,
            counter: AtomicU64::new(0),
        }
    }

    /// An encrypted log whose key a KMIP server wrapped with Encrypt: the key `uid`, the servers
    /// and port of the options, and what Encrypt gave (measured on 9.0.4).
    pub fn with_kmip(
        log_key: [u8; 32],
        uid: &str,
        servers: &[String],
        port: u16,
        wrapped: (&[u8], &[u8]),
        compression: Compression,
    ) -> AuditEncryption {
        let (data, iv) = wrapped;
        let key_store = Document::new()
            .with("provider", PROVIDER_KMIP)
            .with("uid", uid)
            .with(
                "kmipServerName",
                Value::Array(servers.iter().map(|s| Value::from(s.as_str())).collect()),
            )
            .with("kmipPort", i32::from(port))
            .with("keyWrapMethod", KEY_WRAP_ENCRYPT);
        let encrypted_key = Document::new()
            .with("iv", Value::Binary { subtype: 0, bytes: iv.to_vec() })
            .with("key", Value::Binary { subtype: 0, bytes: data.to_vec() })
            .to_bytes()
            .unwrap_or_default();
        AuditEncryption {
            log_key,
            encrypted_key,
            key_store,
            compression,
            counter: AtomicU64::new(0),
        }
    }

    /// The header written when the log opens: what wraps the log's key, and a GMAC of the time
    /// and the version.
    pub fn header(&self, ts_ms: i64) -> Document {
        let signed = Document::new().with("ts", Value::DateTime(ts_ms)).with("version", VERSION);
        let aad = signed.to_bytes().unwrap_or_default();
        let mac = seal(&self.log_key, 0, &aad, &[]);
        Document::new()
            .with("ts", Value::DateTime(ts_ms))
            .with("version", VERSION)
            .with("compressionMode", self.compression.name())
            .with("keyStoreIdentifier", self.key_store.clone())
            .with("encryptedKey", Value::Binary { subtype: 0, bytes: self.encrypted_key.clone() })
            .with("MAC", teseladb_x509::pem::base64_encode(&mac))
            .with("auditRecordType", HEADER)
    }

    /// The line of one message: its time and the sealed BSON of the message.
    pub fn record(&self, ts_ms: i64, message: &[u8]) -> Document {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let plain = match self.compression {
            Compression::None => message.to_vec(),
            Compression::Zstd => teseladb_compress::zstd::compress(message),
        };
        let sealed = seal(&self.log_key, n, &ts_ms.to_le_bytes(), &plain);
        Document::new()
            .with("ts", Value::DateTime(ts_ms))
            .with("log", teseladb_x509::pem::base64_encode(&sealed))
    }
}

/// The JSON line of a header or a message, as the reference server writes it: canonical Extended
/// JSON, with the binary subtype as one digit.
pub fn to_json(doc: &Document) -> String {
    let bytes = doc.to_bytes().unwrap_or_default();
    let text = teseladb_bson::RawDoc::from_bytes(&bytes)
        .map(teseladb_bson::extjson::to_canonical)
        .unwrap_or_default();
    text.replace("\"subType\":\"00\"", "\"subType\":\"0\"")
}
