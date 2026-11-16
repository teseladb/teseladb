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
    /// The ports on the engine's shards and the settings each serves (023 FR-023).
    shards: std::sync::Mutex<Vec<ShardPort>>,
}

/// A port of the engine and the settings it serves.
type ShardPort = (crate::shard_tls::ShardTls, TlsPort);

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
        Certificates {
            dir,
            opts,
            port,
            peer,
            names,
            admin: None,
            shards: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Rotates the administration port's TLS `admin` too.
    #[must_use]
    pub fn with_admin(self, admin: TlsPort) -> Certificates {
        Certificates { admin: Some(admin), ..self }
    }

    /// Puts every later rotation in place on the shards of a port of the engine too: its shard
    /// copies take the settings of `port` (023 FR-023).
    pub fn add_shard_port(&self, slot: crate::shard_tls::ShardTls, port: TlsPort) {
        self.shard_ports().push((slot, port));
    }

    /// The settings of the clients' port.
    #[must_use]
    pub fn client_port(&self) -> &TlsPort {
        &self.port
    }

    /// The ports on the engine's shards now, each with the settings it serves; a copy, so that
    /// nothing holds the list while it publishes.
    #[must_use]
    pub fn shard_ports_now(&self) -> Vec<(crate::shard_tls::ShardTls, TlsPort)> {
        self.shard_ports().clone()
    }

    fn shard_ports(&self) -> std::sync::MutexGuard<'_, Vec<ShardPort>> {
        self.shards.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// [`Certificates::reload`] from a shard: the new settings are on every shard of every
    /// port of the engine and between nodes before the future resolves; on error nothing
    /// changes.
    pub fn rotate(&self) -> teseladb::engine::core::future::Future<Result<(), String>> {
        use teseladb::engine::core::coroutine::make_coroutine;
        let reloaded = self.load_and_replace();
        let shards = self.shard_ports().clone();
        let mine = self.peer.clone();
        make_coroutine(async move {
            let peer = match reloaded {
                Ok(peer) => peer,
                Err(e) => return Ok(Err(e)),
            };
            publish_peer_from_shard0(mine, peer).await?;
            for (slot, port) in &shards {
                slot.publish_port(port).await?;
            }
            Ok(Ok(()))
        })
    }

    /// `rotateCertificates`: loads the PKI and the `--tls*` files again and serves the new
    /// connections with them; on error nothing changes and the error says which file failed.
    /// From a thread that is not a shard: the ports of the engine and the shards' settings
    /// between nodes are the new ones on every shard when it returns (023 FR-023).
    pub fn reload(&self) -> Result<(), String> {
        let peer = self.load_and_replace()?;
        self.publish_peer(peer)?;
        self.publish()
    }

    /// Puts the settings between nodes `peer` in place: on every shard, by a task of shard 0,
    /// while an engine runs; on this thread in a process without one (a node that has not
    /// started its engine, the unit tests).
    fn publish_peer(&self, peer: PeerTls) -> Result<(), String> {
        use teseladb::engine::core::{alien, reactor::engine_is_ready};
        if !engine_is_ready() && alien::internal::default_instance().is_null() {
            self.peer.replace_here(&peer);
            return Ok(());
        }
        let mine = self.peer.clone();
        crate::shard_client::block_on(move || publish_peer_from_shard0(mine, peer))
            .map_err(crate::error_text)
    }

    /// Puts the settings the shared [`TlsPort`]s hold now on every shard of the ports of the
    /// engine (a rotation, a new OCSP staple). From a thread that is not a shard.
    ///
    /// # Errors
    ///
    /// When the engine stopped.
    pub fn publish(&self) -> Result<(), String> {
        for (slot, port) in self.shard_ports().clone() {
            crate::shard_client::block_on(move || slot.publish_port(&port))
                .map_err(crate::error_text)?;
        }
        Ok(())
    }

    /// Loads the files and puts their settings in the shared [`TlsPort`]s; the settings between
    /// nodes it gives back for the shards (they hold a copy each).
    fn load_and_replace(&self) -> Result<PeerTls, String> {
        let pki = pki::load(&self.dir).map_err(crate::error_text)?;
        let port = tls_port::build(&self.opts, &pki)?;
        let peer = pki::peer_tls(&pki, self.opts.fips_mode);
        if let Some(admin) = &self.admin {
            admin.replace(&tls_port::admin(&self.opts, &pki)?);
        }
        self.port.replace(&port);
        Ok(peer)
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

/// The rotation's task on shard 0: it puts a copy of `peer` in place of `mine` on every shard
/// (023 FR-023) and resolves once each has it.
fn publish_peer_from_shard0(
    mine: PeerTls,
    peer: PeerTls,
) -> teseladb::engine::core::future::Future<()> {
    teseladb::engine::core::smp::Smp::submit_to(0, move || mine.publish(&peer))
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
        // In its turn: no engine runs in the process meanwhile.
        let _turn = crate::outbound::test_engine::exclusive();
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
