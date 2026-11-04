//! `--keyFile` and `--clusterAuthMode` (013 FR-034), checked at start as `refnode` 9.0.4 checks
//! them (measured; each fault stops the start with code 1), and the options that relax the
//! secure default, which the node names in its log when it starts (constitution IX).
//!
//! TeselaDB answers as a `refrouter` (D-006): through one, `__system` of `local` cannot log in
//! with the shared key ("Can't use 'local' database through refrouter", measured), so the key only
//! sets `clusterAuthMode`; a member's certificate logs in as `__system` with `sendX509` and
//! `x509`. The links between TeselaDB nodes keep TLS 1.3 with the certificates of the cluster's
//! PKI (D-078) whatever these options say.

use std::path::Path;
use std::sync::Arc;

use teseladb_auth::keyfile;
use teseladb_wire::commands::server_params::ClusterAuthMode;

use crate::cli::StartOpts;
use crate::tls_port::{TlsMode, TlsPort};

/// Group and other permission bits a key file must not have.
const OPEN_BITS: u32 = 0o077;

/// The keys of key file `path`, refused with `refnode`'s messages: readable by others, missing,
/// or with contents that are not keys. No message shows a key.
pub fn read_key_file(path: &Path) -> Result<Vec<String>, String> {
    let shown = path.display().to_string();
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("Error reading file {shown}: {}", io_text(&e)))?;
    if permission_bits(&meta) & OPEN_BITS != 0 {
        return Err(format!("permissions on {shown} are too open"));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Error reading file {shown}: {}", io_text(&e)))?;
    keyfile::parse(&text).map_err(|e| e.message(&shown))
}

#[cfg(unix)]
fn permission_bits(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn permission_bits(_meta: &std::fs::Metadata) -> u32 {
    0
}

/// The error's text without Rust's ` (os error N)`.
fn io_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.find(" (os error") {
        Some(i) => text[..i].to_owned(),
        None => text,
    }
}

/// Whether the node has TLS on the clients' port.
pub fn tls_enabled(o: &StartOpts) -> bool {
    !o.insecure && o.tls.mode != Some(TlsMode::Disabled)
}

/// Whether the connections the node opens use TLS: with `preferTLS` and `requireTLS` (the
/// default), not with `allowTLS` (measured on a `refrouter` 9.0.4 and a `refnode` 9.0.4, 04-11-2026).
/// A `clusterAuthMode` past `keyFile` needs them.
pub fn outgoing_tls(o: &StartOpts) -> bool {
    tls_enabled(o) && o.tls.mode != Some(TlsMode::AllowTls)
}

/// The node's `clusterAuthMode`: the one given, `keyFile` with a key file, `undefined`
/// otherwise; checked as `refnode` checks it, with the key file read.
pub fn mode(o: &StartOpts) -> Result<ClusterAuthMode, String> {
    let mode = match o.cluster_auth_mode.as_deref() {
        None if o.key_file.is_some() => ClusterAuthMode::KeyFile,
        None => ClusterAuthMode::Undefined,
        Some(text) => ClusterAuthMode::parse(text).ok_or_else(|| {
            "security.clusterAuthMode expects one of 'keyFile', 'sendKeyFile', 'sendX509', or 'X509'"
                .to_owned()
        })?,
    };
    if mode != ClusterAuthMode::KeyFile && mode != ClusterAuthMode::Undefined && !tls_enabled(o) {
        return Err("need to enable TLS via the tlsMode flag".into());
    }
    match &o.key_file {
        Some(path) => {
            read_key_file(path)?;
        }
        None if mode.uses_shared_key() => {
            return Err("Error reading file : No such file or directory".into());
        }
        None => {}
    }
    Ok(mode)
}

/// The node's `clusterAuthMode` once [`mode`] accepted the options.
pub fn mode_or_undefined(o: &StartOpts) -> ClusterAuthMode {
    match o.cluster_auth_mode.as_deref() {
        Some(text) => ClusterAuthMode::parse(text).unwrap_or_default(),
        None if o.key_file.is_some() => ClusterAuthMode::KeyFile,
        None => ClusterAuthMode::Undefined,
    }
}

/// The options given that relax the secure default, each with what it lets in.
pub fn relaxations(o: &StartOpts) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    match o.tls.mode {
        Some(m @ (TlsMode::AllowTls | TlsMode::PreferTls)) => {
            out.push((format!("--tlsMode {}", m.as_str()), "clients may connect without TLS"))
        }
        Some(TlsMode::Disabled) => {
            out.push(("--tlsMode disabled".to_owned(), "the clients' port has no TLS"));
        }
        _ => {}
    }
    if o.key_file.is_some() {
        out.push(("--keyFile".to_owned(), "a shared key authenticates the members"));
    }
    let mode = mode_or_undefined(o);
    if o.cluster_auth_mode.is_some() && mode.uses_shared_key() {
        out.push((
            format!("--clusterAuthMode {}", mode.name()),
            "a shared key authenticates the members",
        ));
    }
    out
}

/// The attributes a member's subject shares with the node's, as `refnode` compares them.
const MEMBER_ATTRIBUTES: [&[u8]; 3] = [
    teseladb_x509::oid::ORGANIZATION,
    teseladb_x509::oid::ORGANIZATIONAL_UNIT,
    teseladb_x509::oid::DOMAIN_COMPONENT,
];

/// The subject of `port`'s certificate, when it has one of [`MEMBER_ATTRIBUTES`].
pub fn member_subject(port: &TlsPort) -> Option<Arc<teseladb_x509::Name>> {
    let config = port.config()?;
    let subject = &config.identity.chain.first()?.subject;
    MEMBER_ATTRIBUTES
        .iter()
        .any(|a| !subject.values(a).is_empty())
        .then(|| Arc::new(subject.clone()))
}

/// Whether a client certificate's `subject`, which the TLS handshake already checked against
/// the trusted authorities, is a member's: the organization, units and domain components of
/// the node's certificate, measured on `refnode` 9.0.4.
pub fn is_member(subject: &teseladb_x509::Name, node: &teseladb_x509::Name) -> bool {
    MEMBER_ATTRIBUTES.iter().all(|a| subject.values(a) == node.values(a))
}
