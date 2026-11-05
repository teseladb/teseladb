//! The PKI of the data directory (013 FR-004, D-030): on the first start the master key, the
//! cluster CA (its key encrypted with the master key), the node certificate and the initial
//! `root` credentials; on later starts the same material, checked. Secrets are written with
//! mode 0600 and never shown: only the path of the credentials file is.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use teseladb_auth::roles::ADMIN_DB;
use teseladb_auth::scram::{Credentials, Mechanism};
use teseladb_auth::users::{RoleName, User, UserName};
use teseladb_crypto::ec::NamedCurve;
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams, SERIAL_LEN};
use teseladb_x509::key::Pbes2Params;
use teseladb_x509::pem::{self, base64_decode, base64_encode};
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

/// Directory of the certificates and keys.
pub const PKI_DIR: &str = "pki";
/// The cluster CA certificate.
pub const CA_CERT: &str = "pki/ca.pem";
/// The CA key, encrypted with the master key.
pub const CA_KEY: &str = "pki/ca.key";
/// The node certificate chain.
pub const NODE_CERT: &str = "pki/node.pem";
/// The node key.
pub const NODE_KEY: &str = "pki/node.key";
/// The node key of a renewal, before it takes the place of [`NODE_KEY`].
pub const NODE_KEY_NEW: &str = "pki/node.key.new";
/// The node certificate of a renewal while it is written.
pub const NODE_CERT_TMP: &str = "pki/node.pem.tmp";
/// The node certificate of a renewal once written: from then on the renewal goes ahead, also
/// after a crash.
pub const NODE_CERT_NEW: &str = "pki/node.pem.new";
/// The master key of the cluster (D-030).
pub const MASTER_KEY: &str = "master.key";
/// The initial `root` user and password, written once.
pub const ROOT_CREDENTIALS: &str = "root-credentials.txt";
/// The initial administrator.
pub const ROOT_USER: &str = "root";

const SECRET_MODE: u32 = 0o600;
const PUBLIC_MODE: u32 = 0o644;
const DIR_MODE: u32 = 0o700;
/// Bytes of the master key.
pub const MASTER_KEY_LEN: usize = 32;
const PASSWORD_BYTES: usize = 24;
const DAY: i64 = 86_400;
/// The CA lasts ten years.
pub const CA_VALIDITY_DAYS: i64 = 3650;
/// A node certificate lasts one year; `teseladb admin cert rotate` renews it (FR-006).
pub const NODE_VALIDITY_DAYS: i64 = 365;
/// Clock skew allowed between nodes for a certificate that has just been issued.
const BACKDATE_SECS: i64 = 300;
/// The master key is high-entropy: one PBKDF2 iteration is enough to turn it into the AES key.
const MASTER_KEY_ITERATIONS: u32 = 1;
/// Random bytes an ECDSA P-256 key is derived from.
const KEY_RANDOM_LEN: usize = 40;

/// What the start needs to know to create the PKI.
#[derive(Clone, Debug)]
pub struct PkiParams {
    /// Current time, seconds since the epoch.
    pub now: i64,
    /// The names the node certificate holds (host names and addresses the node listens on).
    pub node_names: Vec<GeneralName>,
}

/// Why the PKI could not be loaded or created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PkiError {
    /// A file of an existing PKI is missing.
    Missing(&'static str),
    /// A file does not parse or does not fit the rest.
    Invalid(&'static str),
    /// The master key does not open the CA key.
    WrongMasterKey,
    /// Reading, writing or the random source failed.
    Io(String),
    /// A file of the bootstrap directory is missing, does not parse or may be read by others.
    Bootstrap(String),
}

impl std::fmt::Display for PkiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PkiError::Missing(file) => {
                write!(f, "{file} is missing: copy it from another node or join again with a token")
            }
            PkiError::Invalid(file) => write!(f, "{file} is not valid for this node's PKI"),
            PkiError::WrongMasterKey => write!(f, "{MASTER_KEY} does not decrypt {CA_KEY}"),
            PkiError::Io(e) => write!(f, "PKI: {e}"),
            PkiError::Bootstrap(e) => write!(f, "--bootstrap-dir: {e}"),
        }
    }
}

impl std::error::Error for PkiError {}

fn io(e: std::io::Error) -> PkiError {
    PkiError::Io(e.to_string())
}

/// The loaded PKI.
pub struct Pki {
    /// The cluster CA.
    pub ca: Certificate,
    /// The CA key.
    pub ca_key: PrivateKey,
    /// The node chain, leaf first.
    pub node_chain: Vec<Certificate>,
    /// The node key.
    pub node_key: PrivateKey,
    /// The master key.
    pub master_key: MasterKey,
    /// Whether this start created it.
    pub created: bool,
    /// Where the `root` credentials are.
    pub credentials_path: PathBuf,
    dir: PathBuf,
}

/// The master key, wiped when dropped.
pub struct MasterKey(pub [u8; MASTER_KEY_LEN]);

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0 = [0; MASTER_KEY_LEN];
        std::hint::black_box(&self.0);
    }
}

impl std::fmt::Debug for Pki {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pki")
            .field("ca", &self.ca.subject.to_rfc4514())
            .field("node", &self.node_chain.first().map(|c| c.subject.to_rfc4514()))
            .field("created", &self.created)
            .field("dir", &self.dir)
            .finish()
    }
}

impl Pki {
    /// The line the start logs: where the PKI is and, on the first start, where the `root`
    /// credentials are.
    pub fn summary(&self) -> String {
        if self.created {
            format!(
                "PKI created in {}; the {ROOT_USER} credentials are in {}",
                self.dir.join(PKI_DIR).display(),
                self.credentials_path.display()
            )
        } else {
            format!("PKI loaded from {}", self.dir.join(PKI_DIR).display())
        }
    }
}

/// The `root` user of the credentials file of `dir`, as the BSON document `admin.system.users`
/// keeps (roles `root` of `admin`, SCRAM-SHA-1 and SCRAM-SHA-256 keys derived from the password
/// with fresh salts; the password itself is not in it), or `None` without that file. The node
/// creates it in Group 0 while the cluster has no user (013 FR-004).
pub fn root_user(
    dir: &Path,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<Option<Vec<u8>>, PkiError> {
    let text = match std::fs::read_to_string(dir.join(ROOT_CREDENTIALS)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e)),
    };
    let field = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .map(str::to_owned)
    };
    let (Some(user), Some(password)) = (field("user"), field("password")) else {
        return Err(PkiError::Invalid(ROOT_CREDENTIALS));
    };
    let mut u = User::new(UserName::new(user.as_str(), ADMIN_DB));
    u.roles = vec![RoleName::new(ROOT_USER, ADMIN_DB)];
    let mut id = [0u8; 16];
    random(&mut id).map_err(io)?;
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    u.user_id = Some(id);
    for m in [Mechanism::ScramSha1, Mechanism::ScramSha256] {
        let prepared = m
            .prepare_password(&user, &password, true)
            .map_err(|_| PkiError::Invalid(ROOT_CREDENTIALS))?;
        let mut salt = vec![0u8; m.salt_len()];
        random(&mut salt).map_err(io)?;
        let creds = Credentials::derive(m, &prepared, &salt, m.default_iterations());
        match m {
            Mechanism::ScramSha1 => u.scram_sha1 = Some(creds),
            Mechanism::ScramSha256 => u.scram_sha256 = Some(creds),
        }
    }
    u.to_document().to_bytes().map(Some).map_err(|_| PkiError::Invalid(ROOT_CREDENTIALS))
}

/// The CA's certificate in a bootstrap directory (013 FR-035).
pub const BOOTSTRAP_CA: &str = "ca.pem";
/// The CA's key in a bootstrap directory, unencrypted PKCS #8.
pub const BOOTSTRAP_CA_KEY: &str = "ca-key.pem";
/// The initial password of `root` in a bootstrap directory, on its first line.
pub const BOOTSTRAP_ROOT_PASSWORD: &str = "root-password";

/// What a first start takes from `--bootstrap-dir` instead of generating it (013 FR-035): the
/// cluster's CA and the initial password of `root`, which the deployer knows without reading the
/// node's disk.
pub struct Bootstrap {
    /// The CA.
    pub ca: Certificate,
    /// The CA's key.
    pub ca_key: PrivateKey,
    /// The initial password of `root`.
    pub root_password: String,
}

/// Reads the bootstrap directory `dir`: its three files, each 0600, a CA certificate and its key.
pub fn read_bootstrap(dir: &Path) -> Result<Bootstrap, PkiError> {
    use std::os::unix::fs::MetadataExt;
    let text = |file: &str| -> Result<String, PkiError> {
        let path = dir.join(file);
        let meta = std::fs::metadata(&path)
            .map_err(|e| PkiError::Bootstrap(format!("{}: {e}", path.display())))?;
        if meta.mode() & 0o777 != SECRET_MODE {
            return Err(PkiError::Bootstrap(format!(
                "{} has mode {:o}: it must be 0600",
                path.display(),
                meta.mode() & 0o777
            )));
        }
        std::fs::read_to_string(&path)
            .map_err(|e| PkiError::Bootstrap(format!("{}: {e}", path.display())))
    };
    let bad = |file: &str, why: &str| PkiError::Bootstrap(format!("{file}: {why}"));
    let ca = Certificate::from_pem_all(&text(BOOTSTRAP_CA)?)
        .ok()
        .and_then(|mut v| (!v.is_empty()).then(|| v.remove(0)))
        .ok_or_else(|| bad(BOOTSTRAP_CA, "not a PEM certificate"))?;
    if !ca.is_ca() {
        return Err(bad(BOOTSTRAP_CA, "not a CA certificate"));
    }
    let ca_key = PrivateKey::from_pem(&text(BOOTSTRAP_CA_KEY)?, None)
        .map_err(|_| bad(BOOTSTRAP_CA_KEY, "not an unencrypted PKCS #8 key"))?;
    if ca_key.public_key() != ca.public_key {
        return Err(bad(BOOTSTRAP_CA_KEY, "not the key of the CA in ca.pem"));
    }
    let root_password =
        text(BOOTSTRAP_ROOT_PASSWORD)?.lines().next().unwrap_or_default().trim().to_owned();
    if root_password.is_empty() {
        return Err(bad(BOOTSTRAP_ROOT_PASSWORD, "empty"));
    }
    Ok(Bootstrap { ca, ca_key, root_password })
}

/// Loads the PKI of `dir`, or creates it if none of its files exist yet.
pub fn load_or_create(
    dir: &Path,
    params: &PkiParams,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<Pki, PkiError> {
    load_or_create_with(dir, params, None, random)
}

/// [`load_or_create`], creating it with the CA and the `root` password of `bootstrap` if given
/// (013 FR-035); a PKI that exists is loaded as it is.
pub fn load_or_create_with(
    dir: &Path,
    params: &PkiParams,
    bootstrap: Option<&Bootstrap>,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<Pki, PkiError> {
    if absent(dir) {
        create(dir, params, bootstrap, random)?;
        let mut pki = load(dir)?;
        pki.created = true;
        return Ok(pki);
    }
    load(dir)
}

fn read(dir: &Path, file: &'static str) -> Result<String, PkiError> {
    match std::fs::read_to_string(dir.join(file)) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(PkiError::Missing(file)),
        Err(_) => Err(PkiError::Invalid(file)),
    }
}

/// Loads the PKI of `dir`, after finishing or dropping a renewal of the node certificate that a
/// crash left halfway.
pub fn load(dir: &Path) -> Result<Pki, PkiError> {
    finish_renewal(dir)?;
    let master_file = read(dir, MASTER_KEY)?;
    let master_text = master_file.trim();
    let master_bytes =
        base64_decode(master_text.as_bytes()).map_err(|_| PkiError::Invalid(MASTER_KEY))?;
    let master =
        MasterKey(master_bytes.as_slice().try_into().map_err(|_| PkiError::Invalid(MASTER_KEY))?);
    let ca = Certificate::from_pem_all(&read(dir, CA_CERT)?)
        .ok()
        .and_then(|mut v| (!v.is_empty()).then(|| v.remove(0)))
        .ok_or(PkiError::Invalid(CA_CERT))?;
    // The CA key's password is the master key's Base64 line, so that
    // `openssl pkey -in pki/ca.key -passin file:master.key` opens it too.
    let ca_key = match PrivateKey::from_pem(&read(dir, CA_KEY)?, Some(master_text.as_bytes())) {
        Ok(k) => k,
        Err(teseladb_x509::Error::Decrypt) => return Err(PkiError::WrongMasterKey),
        Err(_) => return Err(PkiError::Invalid(CA_KEY)),
    };
    if ca_key.public_key() != ca.public_key {
        return Err(PkiError::Invalid(CA_KEY));
    }
    let node_chain = Certificate::from_pem_all(&read(dir, NODE_CERT)?)
        .map_err(|_| PkiError::Invalid(NODE_CERT))?;
    let leaf = node_chain.first().ok_or(PkiError::Invalid(NODE_CERT))?;
    if leaf.is_ca() || leaf.issuer != ca.subject || leaf.verify_signed_by(&ca.public_key).is_err() {
        return Err(PkiError::Invalid(NODE_CERT));
    }
    let node_key = PrivateKey::from_pem(&read(dir, NODE_KEY)?, None)
        .map_err(|_| PkiError::Invalid(NODE_KEY))?;
    if node_key.public_key() != leaf.public_key {
        return Err(PkiError::Invalid(NODE_KEY));
    }
    Ok(Pki {
        ca,
        ca_key,
        node_chain,
        node_key,
        master_key: master,
        created: false,
        credentials_path: dir.join(ROOT_CREDENTIALS),
        dir: dir.to_path_buf(),
    })
}

fn write_new(dir: &Path, file: &str, contents: &[u8], mode: u32) -> Result<(), PkiError> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(dir.join(file))
        .map_err(io)?;
    f.write_all(contents).map_err(io)?;
    f.sync_all().map_err(io)
}

fn bytes<const N: usize>(
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<[u8; N], PkiError> {
    let mut b = [0u8; N];
    random(&mut b).map_err(io)?;
    Ok(b)
}

fn create(
    dir: &Path,
    params: &PkiParams,
    bootstrap: Option<&Bootstrap>,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<(), PkiError> {
    let master = MasterKey(bytes::<MASTER_KEY_LEN>(random)?);
    let node_key = PrivateKey::generate_ec(NamedCurve::P256, &bytes::<KEY_RANDOM_LEN>(random)?)
        .ok()
        .ok_or(PkiError::Invalid(CA_KEY))?;
    let (ca, ca_key, password) = match bootstrap {
        Some(b) => (b.ca.clone(), b.ca_key.clone(), b.root_password.clone()),
        None => {
            let (ca, ca_key) = new_ca(params, random)?;
            let password = base64_encode(&bytes::<PASSWORD_BYTES>(random)?)
                .replace('+', "-")
                .replace('/', "_");
            (ca, ca_key, password)
        }
    };
    let names = params.node_names.clone();
    let node = issue_node(&ca, &ca_key, &node_key.public_key(), names, params.now, random)?;
    write_files(dir, &master, &ca, &ca_key, &node_key, &node, random)?;
    write_new(
        dir,
        ROOT_CREDENTIALS,
        format!("user={ROOT_USER}\npassword={password}\n").as_bytes(),
        SECRET_MODE,
    )
}

/// A new cluster CA: its self-signed certificate and its key.
fn new_ca(
    params: &PkiParams,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<(Certificate, PrivateKey), PkiError> {
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &bytes::<KEY_RANDOM_LEN>(random)?)
        .ok()
        .ok_or(PkiError::Invalid(CA_KEY))?;
    let ca_id: [u8; 4] = bytes(random)?;
    let ca_name =
        format!("TeselaDB CA {}", ca_id.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let start = params.now - BACKDATE_SECS;
    let mut cap = CertificateParams::node(
        bytes::<SERIAL_LEN>(random)?,
        Name::build(&[(oid::ORGANIZATION, "TeselaDB"), (oid::COMMON_NAME, &ca_name)]),
        start,
        params.now + CA_VALIDITY_DAYS * DAY,
    );
    cap.ca = Some(Some(0));
    cap.ext_key_usage.clear();
    let ca = generate::self_signed(&cap, &ca_key).ok().ok_or(PkiError::Invalid(CA_KEY))?;
    Ok((ca, ca_key))
}

/// A node certificate signed by `ca` for `public` with host `names`, valid from a few minutes
/// before `now` (seconds) for [`NODE_VALIDITY_DAYS`].
pub fn issue_node(
    ca: &Certificate,
    ca_key: &PrivateKey,
    public: &teseladb_x509::PublicKey,
    names: Vec<teseladb_x509::ext::GeneralName>,
    now: i64,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<Certificate, PkiError> {
    let mut np = CertificateParams::node(
        bytes::<SERIAL_LEN>(random)?,
        Name::build(&[
            (oid::ORGANIZATION, "TeselaDB"),
            (oid::ORGANIZATIONAL_UNIT, "node"),
            (oid::COMMON_NAME, "teseladb node"),
        ]),
        now - BACKDATE_SECS,
        now + NODE_VALIDITY_DAYS * DAY,
    );
    np.alt_names = names;
    generate::issue(&np, public, ca, ca_key).ok().ok_or(PkiError::Invalid(CA_KEY))
}

/// Writes the PKI of a node that enrolled: what [`load`] reads, without `root` credentials (the
/// cluster already has its users).
pub fn write_enrolled(
    dir: &Path,
    master: &[u8; MASTER_KEY_LEN],
    ca: &Certificate,
    ca_key: &PrivateKey,
    node_key: &PrivateKey,
    node: &Certificate,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<(), PkiError> {
    write_files(dir, &MasterKey(*master), ca, ca_key, node_key, node, random)
}

/// Gives the node of `dir` a new key and a new certificate signed by the CA for host `names`,
/// valid from `now` (seconds): what `teseladb admin cert rotate` does before the rotation of the
/// TLS settings (013 FR-006). The CA, the master key and the old pair stay until the new pair is
/// whole on disk.
pub fn renew_node(
    dir: &Path,
    names: Vec<GeneralName>,
    now: i64,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<(), PkiError> {
    // Loading drops what an unfinished renewal left behind.
    let pki = load(dir)?;
    let key = PrivateKey::generate_ec(NamedCurve::P256, &bytes::<KEY_RANDOM_LEN>(random)?)
        .ok()
        .ok_or(PkiError::Invalid(NODE_KEY))?;
    let cert = issue_node(&pki.ca, &pki.ca_key, &key.public_key(), names, now, random)?;
    let pkcs8 = pem::encode(pem::label::PRIVATE_KEY, &key.to_pkcs8());
    write_new(dir, NODE_KEY_NEW, pkcs8.as_bytes(), SECRET_MODE)?;
    write_new(dir, NODE_CERT_TMP, cert.to_pem().as_bytes(), PUBLIC_MODE)?;
    std::fs::rename(dir.join(NODE_CERT_TMP), dir.join(NODE_CERT_NEW)).map_err(io)?;
    sync_dir(dir)?;
    finish_renewal(dir)
}

fn remove_if_present(dir: &Path, file: &str) -> Result<(), PkiError> {
    match std::fs::remove_file(dir.join(file)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
        _ => Ok(()),
    }
}

fn sync_dir(dir: &Path) -> Result<(), PkiError> {
    std::fs::File::open(dir.join(PKI_DIR)).and_then(|d| d.sync_all()).map_err(io)
}

/// A renewal whose certificate reached [`NODE_CERT_NEW`] puts the new pair in place; one that
/// did not leaves the old pair and drops its files.
fn finish_renewal(dir: &Path) -> Result<(), PkiError> {
    if dir.join(NODE_CERT_NEW).exists() {
        if dir.join(NODE_KEY_NEW).exists() {
            std::fs::rename(dir.join(NODE_KEY_NEW), dir.join(NODE_KEY)).map_err(io)?;
        }
        std::fs::rename(dir.join(NODE_CERT_NEW), dir.join(NODE_CERT)).map_err(io)?;
        return sync_dir(dir);
    }
    remove_if_present(dir, NODE_KEY_NEW)?;
    remove_if_present(dir, NODE_CERT_TMP)
}

/// Whether `dir` has no PKI file yet.
pub fn absent(dir: &Path) -> bool {
    [MASTER_KEY, CA_CERT, CA_KEY, NODE_CERT, NODE_KEY].iter().all(|f| !dir.join(f).exists())
}

fn write_files(
    dir: &Path,
    master: &MasterKey,
    ca: &Certificate,
    ca_key: &PrivateKey,
    node_key: &PrivateKey,
    node: &Certificate,
    random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
) -> Result<(), PkiError> {
    let salt: [u8; 16] = bytes(random)?;
    let iv: [u8; 16] = bytes(random)?;
    let master_line = base64_encode(&master.0);
    let enc = ca_key
        .to_encrypted_pkcs8(
            master_line.as_bytes(),
            Pbes2Params { salt: &salt, iterations: MASTER_KEY_ITERATIONS, iv },
        )
        .ok()
        .ok_or(PkiError::Invalid(CA_KEY))?;
    DirBuilder::new().mode(DIR_MODE).recursive(true).create(dir.join(PKI_DIR)).map_err(io)?;
    write_new(dir, MASTER_KEY, format!("{master_line}\n").as_bytes(), SECRET_MODE)?;
    write_new(
        dir,
        CA_KEY,
        pem::encode(pem::label::ENCRYPTED_PRIVATE_KEY, &enc).as_bytes(),
        SECRET_MODE,
    )?;
    write_new(
        dir,
        NODE_KEY,
        pem::encode(pem::label::PRIVATE_KEY, &node_key.to_pkcs8()).as_bytes(),
        SECRET_MODE,
    )?;
    write_new(dir, CA_CERT, ca.to_pem().as_bytes(), PUBLIC_MODE)?;
    write_new(dir, NODE_CERT, node.to_pem().as_bytes(), PUBLIC_MODE)
}

/// TLS 1.3 between nodes with the node's certificate (013 FR-006): as a server it sends its
/// chain with the CA (a node that enrolls trusts the CA by its fingerprint) and asks for a
/// client certificate, which every peer but an enrolling one must show; as a client it shows its
/// certificate and trusts the CA. `fips` keeps only FIPS-approved algorithms (`--tlsFIPSMode`).
pub fn peer_tls(pki: &Pki, fips: bool) -> teseladb_io::secure::PeerTls {
    use teseladb_tls::{CertifiedKey, ClientAuth, ClientConfig, ServerConfig, Trust};
    let mut chain = pki.node_chain.clone();
    if !chain.iter().any(|c| c.der() == pki.ca.der()) {
        chain.push(pki.ca.clone());
    }
    let identity = CertifiedKey { chain, key: pki.node_key.clone() };
    let mut server = ServerConfig::new(identity.clone());
    server.client_auth = ClientAuth::Optional;
    server.trust = Trust::anchors(vec![pki.ca.clone()]);
    let mut client = ClientConfig::new(Trust::anchors(vec![pki.ca.clone()]));
    client.identity = Some(identity);
    if fips {
        return teseladb_io::secure::PeerTls::new(server.fips(), client.fips());
    }
    teseladb_io::secure::PeerTls::new(server, client)
}

/// The names of the node certificate: `localhost`, the loopback addresses and each host of
/// `--bind_ip` (an address as an IP name, anything else as a DNS name), the same for the first node
/// and for one that enrolls (013 FR-004, FR-005).
pub fn node_names(bind_ip: &str) -> Vec<teseladb_x509::ext::GeneralName> {
    use teseladb_x509::ext::GeneralName;
    let mut out = vec![
        GeneralName::Dns("localhost".to_owned()),
        GeneralName::Ip(vec![127, 0, 0, 1]),
        GeneralName::Ip(std::net::Ipv6Addr::LOCALHOST.octets().to_vec()),
    ];
    for h in bind_ip.split(',').map(str::trim).filter(|h| !h.is_empty()) {
        let name = match h.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(a)) => GeneralName::Ip(a.octets().to_vec()),
            Ok(std::net::IpAddr::V6(a)) => GeneralName::Ip(a.octets().to_vec()),
            Err(_) => GeneralName::Dns(h.to_owned()),
        };
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}
