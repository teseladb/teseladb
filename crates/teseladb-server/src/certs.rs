//! Certificate rotation without stopping the node (013 FR-006): `rotateCertificates` loads the
//! PKI of the data directory and the `--tls*` files again and puts the new TLS settings in place
//! on the clients' port and between nodes; the connections already open keep theirs. A failure
//! leaves the old settings. `teseladb admin cert rotate` first gives the node a new key and
//! certificate signed by the CA ([`Certificates::renew`]).

use std::path::PathBuf;

use teseladb_io::secure::PeerTls;
use teseladb_x509::ext::GeneralName;

use crate::cli::TlsOpts;
use crate::pki;
use crate::tls_port::{self, TlsPort};

/// What a node rotates: where its PKI is, its `--tls*` options and the TLS settings in use.
pub struct Certificates {
    dir: PathBuf,
    opts: TlsOpts,
    port: TlsPort,
    peer: PeerTls,
    names: Vec<GeneralName>,
    admin: Option<TlsPort>,
}

impl std::fmt::Debug for Certificates {
    /// Without the keys.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Certificates").field("dir", &self.dir).finish_non_exhaustive()
    }
}

impl PartialEq for Certificates {
    /// The same rotation: one per node.
    fn eq(&self, other: &Certificates) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for Certificates {}

impl Certificates {
    /// The rotation of the node whose PKI is in `dir`, with options `opts`, serving `port` to
    /// clients and `peer` to the other nodes; a renewal names the host `names`.
    pub fn new(
        dir: PathBuf,
        opts: TlsOpts,
        port: TlsPort,
        peer: PeerTls,
        names: Vec<GeneralName>,
    ) -> Certificates {
        Certificates { dir, opts, port, peer, names, admin: None }
    }

    /// Rotates the administration port's TLS `admin` too.
    #[must_use]
    pub fn with_admin(self, admin: TlsPort) -> Certificates {
        Certificates { admin: Some(admin), ..self }
    }

    /// `rotateCertificates`: loads the PKI and the `--tls*` files again and serves the new
    /// connections with them; on error nothing changes and the error says which file failed.
    pub fn reload(&self) -> Result<(), String> {
        let pki = pki::load(&self.dir).map_err(crate::error_text)?;
        let port = tls_port::build(&self.opts, &pki)?;
        let peer = pki::peer_tls(&pki, self.opts.fips_mode);
        if let Some(admin) = &self.admin {
            admin.replace(&tls_port::admin(&self.opts, &pki)?);
        }
        self.port.replace(&port);
        self.peer.replace(&peer);
        Ok(())
    }

    /// `teseladb admin cert rotate`: a new node key and certificate valid from `now` (seconds),
    /// then [`Certificates::reload`].
    pub fn renew(
        &self,
        now: i64,
        random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
    ) -> Result<(), String> {
        pki::renew_node(&self.dir, self.names.clone(), now, random).map_err(crate::error_text)?;
        self.reload()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::tls_port::TlsMode;

    fn random(b: &mut [u8]) -> std::io::Result<()> {
        teseladb_sys::fill_random(b)
    }

    fn names() -> Vec<GeneralName> {
        vec![GeneralName::Dns("localhost".into())]
    }

    #[test]
    fn a_renewal_rotates_both_sides_and_a_failure_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("teseladb-certs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let params = pki::PkiParams { now: 1_800_000_000, node_names: names() };
        let pki = pki::load_or_create(&dir, &params, &mut random).unwrap();
        let opts = TlsOpts::default();
        let port = tls_port::build(&opts, &pki).unwrap();
        let peer = pki::peer_tls(&pki, false);
        let certs = Certificates::new(dir.clone(), opts, port.clone(), peer.clone(), names());
        assert!(format!("{certs:?}").contains("Certificates") && certs == certs);
        let keys = || {
            let p = port.config().unwrap().identity.key.public_key();
            (p.clone(), peer.server().identity.key.public_key(), peer.client().identity.clone())
        };
        let (before, _, _) = keys();
        certs.renew(1_800_000_100, &mut random).unwrap();
        let (port_key, server_key, client) = keys();
        assert_ne!(port_key, before);
        assert_eq!(server_key, port_key);
        assert_eq!(client.unwrap().key.public_key(), port_key);
        assert_eq!(port.mode, TlsMode::RequireTls);
        // A broken certificate on disk: an error naming the file, and the settings stay.
        std::fs::write(dir.join(pki::NODE_CERT), b"garbage").unwrap();
        let e = certs.reload().unwrap_err();
        assert!(e.contains(pki::NODE_CERT), "{e}");
        assert_eq!(keys().0, port_key);
        assert!(certs.renew(1_800_000_200, &mut random).is_err());
        // A `--tls*` file that no longer reads fails the same way.
        let pki = pki::load_or_create(&dir.join("other"), &params, &mut random).unwrap();
        let missing = TlsOpts { ca_file: Some(dir.join("none.pem")), ..TlsOpts::default() };
        let other_dir = dir.join("other");
        let certs = Certificates::new(
            other_dir,
            missing,
            port.clone(),
            pki::peer_tls(&pki, false),
            names(),
        );
        assert!(certs.reload().unwrap_err().contains("none.pem"));
        assert_eq!(keys().0, port_key);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
