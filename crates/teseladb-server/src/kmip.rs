//! The master key of encryption at rest from a KMIP server (013 FR-028), as the reference server's
//! enterprise edition 9.0.4 uses one: on the first start the node asks the server for a new AES-256
//! key (or takes `--kmipKeyIdentifier`), activates it, gets its bytes and seals with them a random
//! key of its own, the one that wraps the data keys of its files. The data directory keeps that
//! sealed key and the KMIP key's identifier in [`KEYSTORE`]; each start gets the KMIP key again and
//! opens it. `--kmipRotateMasterKey` seals it under a new KMIP key and stops, as the reference
//! server does, and every `--kmipKeyStatePollingSeconds` the node checks that its key is still
//! active: a key that is not stops the node.
//!
//! ```text
//! kmip.keystore := "key=" id "\n" "sealed=" base64(nonce(12) · AES-256-GCM(kmip key, own key) · tag(16)) "\n"
//! ```

use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use teseladb_auth::kmip::{ACTIVE, Answer, KmipError, Request, V1_0, V1_2, Version, message_len};
use teseladb_crypto::aes::Aes;
use teseladb_crypto::modes::AesGcm;
use teseladb_tls::{CertifiedKey, ClientConfig, Trust};
use teseladb_x509::pem::{base64_decode, base64_encode};
use teseladb_x509::{Certificate, PrivateKey};

use crate::cli::Password;
use crate::tls_stream::TlsStream;

/// The file of the data directory with the sealed key and the KMIP key's identifier.
pub const KEYSTORE: &str = "kmip.keystore";
/// `--kmipPort` by default.
pub const DEFAULT_PORT: u16 = 5696;
/// `--kmipConnectTimeoutMS` by default.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(5000);
/// `--kmipKeyStatePollingSeconds` by default.
pub const DEFAULT_POLLING: Duration = Duration::from_secs(900);
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const SEAL_AAD: &[u8] = b"teseladb kmip keystore ";
const KEY_LINE: &str = "key=";
const SEALED_LINE: &str = "sealed=";
const KEYSTORE_MODE: u32 = 0o600;
/// How often the poller looks at the node's stop while it waits.
const POLL_STEP: Duration = Duration::from_millis(100);

/// The KMIP options (`security.kmip.*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KmipOpts {
    /// `--kmipServerName`: the servers, tried in order at the start.
    pub servers: Vec<String>,
    /// `--kmipPort`.
    pub port: u16,
    /// `--kmipServerCAFile`; the system's authorities without it.
    pub ca_file: Option<PathBuf>,
    /// `--kmipClientCertificateFile`: the node's certificate and key, PEM.
    pub client_certificate_file: Option<PathBuf>,
    /// `--kmipClientCertificatePassword`.
    pub client_certificate_password: Option<Password>,
    /// `--kmipKeyIdentifier`: an existing key, on the first start.
    pub key_identifier: Option<String>,
    /// `--kmipRotateMasterKey`.
    pub rotate: bool,
    /// `--kmipConnectRetries`.
    pub connect_retries: u32,
    /// `--kmipConnectTimeoutMS`.
    pub connect_timeout: Duration,
    /// `--kmipKeyStatePollingSeconds`; `None` (−1) checks only at the start.
    pub polling: Option<Duration>,
    /// `--kmipActivateKeys`.
    pub activate_keys: bool,
    /// `--kmipUseLegacyProtocol`: KMIP 1.0 instead of 1.2.
    pub legacy: bool,
    /// `--tlsFIPSMode`: the TLS to the server keeps only FIPS-approved algorithms.
    pub fips: bool,
}

impl Default for KmipOpts {
    fn default() -> KmipOpts {
        KmipOpts {
            servers: Vec::new(),
            port: DEFAULT_PORT,
            ca_file: None,
            client_certificate_file: None,
            client_certificate_password: None,
            key_identifier: None,
            rotate: false,
            connect_retries: 0,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            polling: Some(DEFAULT_POLLING),
            activate_keys: true,
            legacy: false,
            fips: false,
        }
    }
}

impl KmipOpts {
    fn version(&self) -> Version {
        if self.legacy { V1_0 } else { V1_2 }
    }

    /// The client side of TLS: the server's authorities and the node's certificate.
    fn tls(&self) -> Result<Arc<ClientConfig>, String> {
        let trust = match &self.ca_file {
            Some(path) => {
                let text = read(path, "kmipServerCAFile")?;
                Trust::anchors(
                    Certificate::from_pem_all(&text)
                        .map_err(|e| format!("--kmipServerCAFile {}: {e}", path.display()))?,
                )
            }
            None => crate::https::system_trust()?,
        };
        // TLS 1.2 or 1.3, as the reference server reaches a KMIP server through OpenSSL.
        let mut config =
            ClientConfig { min_version: teseladb_tls::Version::Tls12, ..ClientConfig::new(trust) };
        if let Some(path) = &self.client_certificate_file {
            let text = read(path, "kmipClientCertificateFile")?;
            let bad = |e: String| format!("--kmipClientCertificateFile {}: {e}", path.display());
            let password = self.client_certificate_password.as_ref().map(|p| p.0.as_bytes());
            let chain = Certificate::from_pem_all(&text).map_err(|e| bad(e.to_string()))?;
            let key = PrivateKey::from_pem(&text, password).map_err(|e| bad(e.to_string()))?;
            config.identity = Some(CertifiedKey { chain, key });
        }
        Ok(Arc::new(if self.fips { config.fips() } else { config }))
    }
}

fn read(path: &Path, option: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("--{option} {}: {e}", path.display()))
}

/// A connection to the KMIP server.
pub struct Session {
    stream: TlsStream,
    version: Version,
}

impl Session {
    /// Connects to the first server of `o` that answers, trying each `--kmipConnectRetries` more
    /// times; the reference server's message when none does.
    pub fn connect(o: &KmipOpts) -> Result<Session, String> {
        let tls = o.tls()?;
        let mut last = "no --kmipServerName".to_owned();
        for name in &o.servers {
            let Ok(found) = (name.as_str(), o.port).to_socket_addrs() else {
                last = format!("Could not resolve KMIP server {name}:{}", o.port);
                continue;
            };
            // IPv4 first, as the reference server names the address it could not reach.
            let mut addrs: Vec<SocketAddr> = found.collect();
            addrs.sort_by_key(SocketAddr::is_ipv6);
            let mut first = None;
            for _ in 0..=o.connect_retries {
                for addr in &addrs {
                    match dial(*addr, name, o.connect_timeout, &tls) {
                        Ok(stream) => return Ok(Session { stream, version: o.version() }),
                        Err(e) => {
                            first.get_or_insert(e);
                        }
                    }
                }
            }
            last = first.unwrap_or(last);
        }
        Err(last)
    }

    /// Sends `req` and reads its answer.
    pub fn ask(&mut self, req: &Request) -> Result<Answer, KmipFailure> {
        self.stream.send(&req.encode(self.version)).map_err(KmipFailure::Io)?;
        let mut got = Vec::new();
        loop {
            if let Some(len) = message_len(&got)
                && got.len() >= len
            {
                return req.answer(&got[..len]).map_err(KmipFailure::Kmip);
            }
            let data = self.stream.recv().map_err(KmipFailure::Io)?;
            if data.is_empty() {
                return Err(KmipFailure::Io("the KMIP server closed the connection".into()));
            }
            got.extend_from_slice(&data);
        }
    }
}

fn dial(
    addr: SocketAddr,
    name: &str,
    timeout: Duration,
    tls: &Arc<ClientConfig>,
) -> Result<TlsStream, String> {
    let refused = || format!("Could not connect to KMIP server {addr}");
    let sock = TcpStream::connect_timeout(&addr, timeout).map_err(|_| refused())?;
    // A timeout that is not zero is always taken (the options never give zero).
    let _ = sock.set_read_timeout(Some(timeout));
    TlsStream::client(sock, tls.clone(), name).map_err(|e| format!("{}: {e}", refused()))
}

/// Why a KMIP request failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KmipFailure {
    /// The connection.
    Io(String),
    /// The server's answer.
    Kmip(KmipError),
}

impl std::fmt::Display for KmipFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KmipFailure::Io(e) => f.write_str(e),
            KmipFailure::Kmip(e) => write!(f, "{e}"),
        }
    }
}

/// What the start did with the KMIP server, and the key that wraps the data keys.
pub struct MasterKey {
    /// The node's own key, which wraps the data keys of its files.
    pub key: [u8; KEY_LEN],
    /// The KMIP key's identifier.
    pub id: String,
    /// Lines for the node's log ("Created KMIP key", "Activated KMIP key", the rotation).
    pub log: Vec<String>,
    /// Whether this start rotated the key (and the node stops now).
    pub rotated: bool,
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.key = [0; KEY_LEN];
        std::hint::black_box(&self.key);
    }
}

impl std::fmt::Debug for MasterKey {
    /// Without the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKey").field("id", &self.id).field("rotated", &self.rotated).finish()
    }
}

fn created(s: &mut Session, o: &KmipOpts, log: &mut Vec<String>) -> Result<String, String> {
    let id = match s.ask(&Request::CreateKey) {
        Ok(Answer::Created(id)) => id,
        Ok(_) => return Err("KMIP create key: an answer of another kind".into()),
        Err(e) => return Err(format!("KMIP create key failed, {e}")),
    };
    log.push(format!("Created KMIP key keyId={id}"));
    if o.activate_keys {
        s.ask(&Request::Activate(id.clone()))
            .map_err(|e| format!("KMIP activate key '{id}' failed, {e}"))?;
        log.push(format!("Activated KMIP key uid={id}"));
    }
    Ok(id)
}

/// The bytes of KMIP key `id`, which has to be active with `--kmipActivateKeys`.
fn kmip_key(s: &mut Session, o: &KmipOpts, id: &str) -> Result<[u8; KEY_LEN], String> {
    if o.activate_keys {
        check_active(s, id)?;
    }
    match s.ask(&Request::Get(id.to_owned())) {
        Ok(Answer::Key(mut bytes)) => {
            let mut key = [0u8; KEY_LEN];
            // The answer was checked to hold 256 bits.
            key.copy_from_slice(&bytes[..KEY_LEN]);
            bytes.fill(0);
            Ok(key)
        }
        Ok(_) => Err(format!("KMIP get key '{id}': an answer of another kind")),
        Err(e) => Err(format!("KMIP get key '{id}' failed, {e}")),
    }
}

/// Whether KMIP key `id` is active; the reference server's message when it is not.
fn check_active(s: &mut Session, id: &str) -> Result<(), String> {
    match s.ask(&Request::State(id.to_owned())) {
        Ok(Answer::State(ACTIVE)) => Ok(()),
        Ok(_) => Err(format!("KMIP Key used for ESE is not in active state key_uid={id}")),
        Err(e) => Err(format!("KMIP get attributes of key '{id}' failed, {e}")),
    }
}

fn gcm(key: &[u8; KEY_LEN]) -> AesGcm {
    AesGcm::new(Aes::new_256(key))
}

fn aad(id: &str) -> Vec<u8> {
    [SEAL_AAD, id.as_bytes()].concat()
}

/// The text of the keystore: `own` sealed under `kmip` key `id`.
fn keystore_text(
    id: &str,
    kmip: &[u8; KEY_LEN],
    own: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
) -> String {
    let sealed = gcm(kmip).seal(nonce, &aad(id), own);
    let mut out = String::new();
    let _ = writeln!(out, "{KEY_LINE}{id}");
    let _ = writeln!(out, "{SEALED_LINE}{}", base64_encode(&[nonce.as_slice(), &sealed].concat()));
    out
}

/// The identifier and the sealed bytes of a keystore's text.
fn parse_keystore(text: &str) -> Option<(String, Vec<u8>)> {
    let mut lines = text.lines();
    let id = lines.next()?.strip_prefix(KEY_LINE)?.to_owned();
    let sealed = base64_decode(lines.next()?.strip_prefix(SEALED_LINE)?.as_bytes()).ok()?;
    (lines.next().is_none() && !id.is_empty()).then_some((id, sealed))
}

fn open_sealed(id: &str, kmip: &[u8; KEY_LEN], sealed: &[u8]) -> Option<[u8; KEY_LEN]> {
    let (nonce, body) = sealed.split_at_checked(NONCE_LEN)?;
    let mut plain = gcm(kmip).open(nonce, &aad(id), body).ok()?;
    let key: Option<[u8; KEY_LEN]> = plain.as_slice().try_into().ok();
    plain.fill(0);
    key
}

/// Writes the keystore whole: a new file, synced, renamed over the old one.
fn write_keystore(dir: &Path, text: &str) -> Result<(), String> {
    let tmp = dir.join(format!("{KEYSTORE}.tmp"));
    let io = |e: std::io::Error| format!("{KEYSTORE}: {e}");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(KEYSTORE_MODE)
        .open(&tmp)
        .map_err(io)?;
    f.write_all(text.as_bytes()).and_then(|()| f.sync_all()).map_err(io)?;
    std::fs::rename(&tmp, dir.join(KEYSTORE)).map_err(io)?;
    std::fs::File::open(dir).and_then(|d| d.sync_all()).map_err(io)
}

/// The node's key from the KMIP server of `o` for data directory `dir`: created on the first
/// start, opened on the next ones, sealed under a new KMIP key with `--kmipRotateMasterKey`.
pub fn master_key(
    o: &KmipOpts,
    dir: &Path,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<MasterKey, String> {
    let mut s = Session::connect(o)?;
    let mut log = Vec::new();
    let path = dir.join(KEYSTORE);
    let mut nonce = [0u8; NONCE_LEN];
    random(&mut nonce).map_err(|e| format!("{KEYSTORE}: {e}"))?;
    if !path.exists() {
        let id = match &o.key_identifier {
            Some(id) => id.clone(),
            None => created(&mut s, o, &mut log)?,
        };
        let kmip = kmip_key(&mut s, o, &id)?;
        let mut own = [0u8; KEY_LEN];
        random(&mut own).map_err(|e| format!("{KEYSTORE}: {e}"))?;
        write_keystore(dir, &keystore_text(&id, &kmip, &own, &nonce))?;
        return Ok(MasterKey { key: own, id, log, rotated: false });
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{KEYSTORE}: {e}"))?;
    let (id, sealed) = parse_keystore(&text).ok_or_else(|| format!("{KEYSTORE}: damaged"))?;
    if o.key_identifier.as_ref().is_some_and(|k| *k != id) {
        return Err(format!(
            "--kmipKeyIdentifier only applies the first time encryption is enabled: this data directory is encrypted with KMIP key '{id}'"
        ));
    }
    let kmip = kmip_key(&mut s, o, &id)?;
    let own = open_sealed(&id, &kmip, &sealed)
        .ok_or_else(|| format!("{KEYSTORE}: KMIP key '{id}' does not open it"))?;
    if !o.rotate {
        return Ok(MasterKey { key: own, id, log, rotated: false });
    }
    let new_id = created(&mut s, o, &mut log)?;
    let new_kmip = kmip_key(&mut s, o, &new_id)?;
    write_keystore(dir, &keystore_text(&new_id, &new_kmip, &own, &nonce))?;
    log.push(format!("Rotated master encryption key oldMasterKeyId={id} newMasterKeyId={new_id}"));
    Ok(MasterKey { key: own, id: new_id, log, rotated: true })
}

/// Checks every `every` that KMIP key `id` is still active, until `stop`; sets `lost` when it is
/// not. A server that does not answer is asked again at the next check.
pub fn poll(
    o: KmipOpts,
    id: String,
    every: Duration,
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let step = POLL_STEP.min(every);
        loop {
            let mut waited = Duration::ZERO;
            while waited < every {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(step);
                waited += step;
            }
            let Ok(mut s) = Session::connect(&o) else { continue };
            if let Ok(Answer::State(state)) = s.ask(&Request::State(id.clone()))
                && state != ACTIVE
            {
                lost.store(true, Ordering::Relaxed);
                return;
            }
        }
    })
}

/// The log line of a node whose KMIP key stopped being active.
pub fn lost_message(id: &str) -> String {
    format!("KMIP Key used for ESE is not in active state. Shutting down server. key_uid={id}")
}
