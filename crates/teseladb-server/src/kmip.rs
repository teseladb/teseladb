//! The master key of encryption at rest from a KMIP server (013 FR-028), as the reference server's
//! enterprise edition 9.0.4 uses one: on the first start the node asks the server for a new AES-256
//! key (or takes `--kmipKeyIdentifier`), activates it, gets its bytes and seals with them a random
//! key of its own, the one that wraps the data keys of its files. The data directory keeps that
//! sealed key and the KMIP key's identifier in [`KEYSTORE`]; each start gets the KMIP key again and
//! opens it. `--kmipRotateMasterKey` seals it under a new KMIP key and stops, as the reference
//! server does, and every `--kmipKeyStatePollingSeconds` the node checks that its key is still
//! active: a key that is not stops the node.
//!
//! The conversation with the server runs on the node's engine (023 FR-024): an engine connection
//! under TLS, each step within `--kmipConnectTimeoutMS` on the engine's timers, the name resolved
//! off the shard. The keystore and the certificate files are read and written by the thread that
//! starts the node, off the shards; the check of the key's state is a background task of a shard.
//!
//! ```text
//! kmip.keystore := "key=" id "\n" "sealed=" base64(nonce(12) · AES-256-GCM(kmip key, own key) · tag(16)) "\n"
//! ```

use std::fmt::Write as _;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use teseladb_auth::kmip::KmipError;
#[cfg(not(target_os = "linux"))]
use teseladb_auth::kmip::{Answer, Request};
#[cfg(target_os = "linux")]
use teseladb_auth::kmip::{V1_0, V1_2, Version};
use teseladb_crypto::aes::Aes;
use teseladb_crypto::modes::AesGcm;
use teseladb_tls::{CertifiedKey, ClientConfig, Trust};
use teseladb_x509::pem::{base64_decode, base64_encode};
use teseladb_x509::{Certificate, PrivateKey};

use crate::cli::Password;

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
#[cfg(target_os = "linux")]
const POLL_STEP: Duration = Duration::from_millis(100);
/// The error of a server that closed the connection before it answered.
#[cfg(target_os = "linux")]
const CLOSED: &str = "the KMIP server closed the connection";
/// The error without servers.
#[cfg(target_os = "linux")]
const NO_SERVER: &str = "no --kmipServerName";

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
    #[cfg(target_os = "linux")]
    fn version(&self) -> Version {
        if self.legacy { V1_0 } else { V1_2 }
    }

    /// The client side of TLS: the server's authorities and the node's certificate, from their
    /// files (read by the calling thread, never by a shard).
    pub fn tls(&self) -> Result<Arc<ClientConfig>, String> {
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

/// What a start asks the KMIP server for.
#[derive(Clone, Debug)]
enum Ask {
    /// A new key, activated with `--kmipActivateKeys`, and its bytes.
    Create,
    /// The bytes of an existing key, which has to be active with `--kmipActivateKeys`.
    Key(String),
}

/// A KMIP key the server gave: its identifier, its bytes and the log lines of its creation.
struct Fetched {
    id: String,
    key: [u8; KEY_LEN],
    log: Vec<String>,
}

impl Drop for Fetched {
    fn drop(&mut self) {
        self.key = [0; KEY_LEN];
        std::hint::black_box(&self.key);
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

/// A random array.
fn drawn<const N: usize>(
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<[u8; N], String> {
    let mut out = [0u8; N];
    random(&mut out).map_err(|e| format!("{KEYSTORE}: {e}"))?;
    Ok(out)
}

/// The node's key from the KMIP server of `o` for data directory `dir`: created on the first
/// start, opened on the next ones, sealed under a new KMIP key with `--kmipRotateMasterKey`. Called
/// by the thread that starts the node, which reads and writes the keystore; the server is asked on
/// the node's engine.
pub fn master_key(
    o: &KmipOpts,
    dir: &Path,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<MasterKey, String> {
    let tls = o.tls()?;
    let path = dir.join(KEYSTORE);
    if !path.exists() {
        let ask = o.key_identifier.clone().map_or(Ask::Create, Ask::Key);
        let mut got = fetch_off_shard(o, &tls, ask)?;
        let nonce = drawn::<NONCE_LEN>(random)?;
        let own = drawn::<KEY_LEN>(random)?;
        write_keystore(dir, &keystore_text(&got.id, &got.key, &own, &nonce))?;
        let (id, log) = (std::mem::take(&mut got.id), std::mem::take(&mut got.log));
        return Ok(MasterKey { key: own, id, log, rotated: false });
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{KEYSTORE}: {e}"))?;
    let (id, sealed) = parse_keystore(&text).ok_or_else(|| format!("{KEYSTORE}: damaged"))?;
    if o.key_identifier.as_ref().is_some_and(|k| *k != id) {
        return Err(format!(
            "--kmipKeyIdentifier only applies the first time encryption is enabled: this data directory is encrypted with KMIP key '{id}'"
        ));
    }
    let got = fetch_off_shard(o, &tls, Ask::Key(id.clone()))?;
    let own = open_sealed(&id, &got.key, &sealed)
        .ok_or_else(|| format!("{KEYSTORE}: KMIP key '{id}' does not open it"))?;
    if !o.rotate {
        return Ok(MasterKey { key: own, id, log: Vec::new(), rotated: false });
    }
    let mut new = fetch_off_shard(o, &tls, Ask::Create)?;
    let nonce = drawn::<NONCE_LEN>(random)?;
    write_keystore(dir, &keystore_text(&new.id, &new.key, &own, &nonce))?;
    let (new_id, mut log) = (std::mem::take(&mut new.id), std::mem::take(&mut new.log));
    log.push(format!("Rotated master encryption key oldMasterKeyId={id} newMasterKeyId={new_id}"));
    Ok(MasterKey { key: own, id: new_id, log, rotated: true })
}

/// The log line of a node whose KMIP key stopped being active.
pub fn lost_message(id: &str) -> String {
    format!("KMIP Key used for ESE is not in active state. Shutting down server. key_uid={id}")
}

/// The background check of a KMIP key's state started by [`poll`].
#[derive(Debug)]
pub struct Poller {
    #[cfg(target_os = "linux")]
    work: crate::outbound::Spawned,
}

impl Poller {
    /// Waits until the check ended: the key was lost or the node stopped it.
    pub fn join(self) {
        #[cfg(target_os = "linux")]
        self.work.join();
    }
}

#[cfg(target_os = "linux")]
mod on_engine {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use teseladb::engine::core::sleep::sleep_steady;
    use teseladb_auth::kmip::{ACTIVE, Answer, Request, Version, message_len};
    use teseladb_io::engine::link::Link;
    use teseladb_io::engine::{blocking, connect, tls_stream};
    use teseladb_tls::ClientConfig;

    use super::{
        Ask, CLOSED, Fetched, KEY_LEN, KmipFailure, KmipOpts, NO_SERVER, POLL_STEP, Poller,
    };
    use crate::outbound::{self, text};

    /// A connection to the KMIP server, on the shard that opened it.
    pub(super) struct Session {
        link: Link,
        version: Version,
    }

    impl Session {
        /// Connects to the first server of `o` that answers, trying each `--kmipConnectRetries`
        /// more times; the reference server's message when none does.
        pub(super) async fn connect(
            o: &KmipOpts,
            tls: &Arc<ClientConfig>,
        ) -> Result<Session, String> {
            let mut last = NO_SERVER.to_owned();
            for name in &o.servers {
                let Ok(mut addrs) = blocking::resolve(name, o.port, o.connect_timeout).await else {
                    last = format!("Could not resolve KMIP server {name}:{}", o.port);
                    continue;
                };
                // IPv4 first, as the reference server names the address it could not reach.
                addrs.sort_by_key(SocketAddr::is_ipv6);
                let mut first = None;
                for _ in 0..=o.connect_retries {
                    for addr in &addrs {
                        match dial(*addr, name, o.connect_timeout, tls).await {
                            Ok(link) => return Ok(Session { link, version: o.version() }),
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
        pub(super) async fn ask(&mut self, req: &Request) -> Result<Answer, KmipFailure> {
            let io = |e| KmipFailure::Io(text(&e));
            self.link.send(&req.encode(self.version)).await.map_err(io)?;
            let mut got = Vec::new();
            loop {
                if let Some(len) = message_len(&got)
                    && got.len() >= len
                {
                    return req.answer(&got[..len]).map_err(KmipFailure::Kmip);
                }
                let data = self.link.recv().await.map_err(io)?;
                if data.empty() {
                    return Err(KmipFailure::Io(CLOSED.into()));
                }
                got.extend_from_slice(data.as_slice());
            }
        }

        pub(super) async fn close(self) {
            self.link.close().await;
        }
    }

    /// A TLS connection to `addr`, the certificate checked against `name`.
    async fn dial(
        addr: SocketAddr,
        name: &str,
        timeout: Duration,
        tls: &Arc<ClientConfig>,
    ) -> Result<Link, String> {
        let refused = || format!("Could not connect to KMIP server {addr}");
        let sock = connect::connect(addr, timeout).await.map_err(|_| refused())?;
        let t = tls_stream::client(sock, tls.clone(), Some(name), None, Some(timeout))
            .await
            .map_err(|e| format!("{}: {}", refused(), text(&e)))?;
        Ok(Link::new(t.socket, Some(t.session), timeout))
    }

    async fn created(
        s: &mut Session,
        o: &KmipOpts,
        log: &mut Vec<String>,
    ) -> Result<String, String> {
        let id = match s.ask(&Request::CreateKey).await {
            Ok(Answer::Created(id)) => id,
            Ok(_) => return Err("KMIP create key: an answer of another kind".into()),
            Err(e) => return Err(format!("KMIP create key failed, {e}")),
        };
        log.push(format!("Created KMIP key keyId={id}"));
        if o.activate_keys {
            s.ask(&Request::Activate(id.clone()))
                .await
                .map_err(|e| format!("KMIP activate key '{id}' failed, {e}"))?;
            log.push(format!("Activated KMIP key uid={id}"));
        }
        Ok(id)
    }

    /// The bytes of KMIP key `id`, which has to be active with `--kmipActivateKeys`.
    async fn kmip_key(s: &mut Session, o: &KmipOpts, id: &str) -> Result<[u8; KEY_LEN], String> {
        if o.activate_keys {
            check_active(s, id).await?;
        }
        match s.ask(&Request::Get(id.to_owned())).await {
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
    async fn check_active(s: &mut Session, id: &str) -> Result<(), String> {
        match s.ask(&Request::State(id.to_owned())).await {
            Ok(Answer::State(ACTIVE)) => Ok(()),
            Ok(_) => Err(format!("KMIP Key used for ESE is not in active state key_uid={id}")),
            Err(e) => Err(format!("KMIP get attributes of key '{id}' failed, {e}")),
        }
    }

    /// What `ask` gets from the server, on one connection.
    async fn fetch(o: &KmipOpts, tls: &Arc<ClientConfig>, ask: Ask) -> Result<Fetched, String> {
        let mut s = Session::connect(o, tls).await?;
        let mut log = Vec::new();
        let got = async {
            let id = match ask {
                Ask::Create => created(&mut s, o, &mut log).await?,
                Ask::Key(id) => id,
            };
            let key = kmip_key(&mut s, o, &id).await?;
            Ok::<_, String>((id, key))
        }
        .await;
        s.close().await;
        let (id, key) = got?;
        Ok(Fetched { id, key, log })
    }

    /// [`fetch`] on the node's engine, from the thread that starts the node.
    pub(super) fn fetch_off_shard(
        o: &KmipOpts,
        tls: &Arc<ClientConfig>,
        ask: Ask,
    ) -> Result<Fetched, String> {
        let (o, tls) = (o.clone(), tls.clone());
        outbound::call(move || outbound::future(async move { fetch(&o, &tls, ask).await }))
    }

    /// Sends `req` to the first server of `o` that answers and reads its answer, on the node's
    /// engine, from a thread that is not a shard (the start-up of the audit log); the outer error
    /// is the connection's.
    pub fn ask_once(o: &KmipOpts, req: Request) -> Result<Result<Answer, KmipFailure>, String> {
        let tls = o.tls()?;
        let o = o.clone();
        outbound::call(move || {
            outbound::future(async move {
                let mut s = Session::connect(&o, &tls).await?;
                let answer = s.ask(&req).await;
                s.close().await;
                Ok(answer)
            })
        })
    }

    /// One check of the key's state; `true` when the server says it is not active. A server out
    /// of reach, or one that does not answer the question, is asked again at the next check.
    async fn lost(o: &KmipOpts, tls: &Arc<ClientConfig>, id: &str) -> bool {
        let Ok(mut s) = Session::connect(o, tls).await else { return false };
        let answer = s.ask(&Request::State(id.to_owned())).await;
        s.close().await;
        matches!(answer, Ok(Answer::State(state)) if state != ACTIVE)
    }

    /// The checks every `every` until `stop` or a lost key, which sets `lost`.
    async fn checks(
        o: KmipOpts,
        tls: Arc<ClientConfig>,
        id: String,
        every: Duration,
        stop: Arc<AtomicBool>,
        lost_flag: Arc<AtomicBool>,
    ) {
        let step = POLL_STEP.min(every);
        loop {
            let mut waited = Duration::ZERO;
            while waited < every {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let _ = sleep_steady(step).await;
                waited += step;
            }
            if lost(&o, &tls, &id).await {
                lost_flag.store(true, Ordering::Relaxed);
                return;
            }
        }
    }

    /// Starts the background check of KMIP key `id` every `every` on the node's engine, until
    /// `stop`; sets `lost` when the key is not active.
    pub fn poll(
        o: KmipOpts,
        id: String,
        every: Duration,
        stop: Arc<AtomicBool>,
        lost: Arc<AtomicBool>,
    ) -> Result<Poller, String> {
        let tls = o.tls()?;
        let work = outbound::spawn(move || {
            outbound::future(async move {
                checks(o, tls, id, every, stop, lost).await;
                Ok(())
            })
        })?;
        Ok(Poller { work })
    }
}

#[cfg(target_os = "linux")]
use on_engine::fetch_off_shard;
#[cfg(target_os = "linux")]
pub use on_engine::{ask_once, poll};

#[cfg(not(target_os = "linux"))]
fn fetch_off_shard(_o: &KmipOpts, _tls: &Arc<ClientConfig>, _ask: Ask) -> Result<Fetched, String> {
    Err(crate::LINUX_ONLY.into())
}

/// The KMIP client runs on the engine, which only Linux has (023 D3).
#[cfg(not(target_os = "linux"))]
pub fn ask_once(o: &KmipOpts, _req: Request) -> Result<Result<Answer, KmipFailure>, String> {
    o.tls()?;
    Err(crate::LINUX_ONLY.into())
}

/// The KMIP client runs on the engine, which only Linux has (023 D3).
#[cfg(not(target_os = "linux"))]
pub fn poll(
    o: KmipOpts,
    _id: String,
    _every: Duration,
    _stop: Arc<AtomicBool>,
    _lost: Arc<AtomicBool>,
) -> Result<Poller, String> {
    o.tls()?;
    Err(crate::LINUX_ONLY.into())
}
