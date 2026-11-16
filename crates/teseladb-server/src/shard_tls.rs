//! The TLS settings of a port as each shard holds them (023 FR-023): a connection takes its
//! shard's copy without any lock, and a rotation (`rotateCertificates`, a new OCSP staple) is put
//! in place by shard 0 on every shard before it counts as done ([`ShardTls::publish`]).
//!
//! A shard that has not seen the settings yet takes the last ones published once and keeps them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use teseladb::engine::core::future::Future;
use teseladb::engine::core::smp::Smp;
use teseladb_io::engine::tls_stream::Sniff;
use teseladb_tls::ServerConfig;

use crate::tls_port::{TlsMode, TlsPort};

/// How a port chooses between TLS and plain bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// As `--tlsMode` says (the clients' and the HTTP ports).
    ByMode,
    /// TLS when the first byte opens a handshake and the port has a server side, plain
    /// otherwise, whatever the mode (the administration port, 013 FR-017).
    FirstByte,
}

/// The ids of the settings of the process.
static NEXT: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// This shard's copy of each port's settings, by id.
    static LOCAL: RefCell<HashMap<u64, Option<Arc<ServerConfig>>>> =
        RefCell::new(HashMap::new());
}

/// A port's TLS settings on the shards. Clones share them.
#[derive(Clone, Debug)]
pub struct ShardTls {
    id: u64,
    mode: TlsMode,
    policy: Policy,
    /// The protocols the port offers by ALPN, added to every server side published.
    alpn: Option<Arc<Vec<Vec<u8>>>>,
    /// The last settings published, for a shard that has none yet.
    latest: Arc<Mutex<Option<Arc<ServerConfig>>>>,
}

impl ShardTls {
    /// The settings of `port` now, offering `alpn` when given, chosen by `policy`.
    #[must_use]
    pub fn new(port: &TlsPort, alpn: Option<Vec<Vec<u8>>>, policy: Policy) -> ShardTls {
        let alpn = alpn.map(Arc::new);
        let latest = port.config().map(|c| with_alpn(c, alpn.as_deref()));
        ShardTls {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            mode: port.mode,
            policy,
            alpn,
            latest: Arc::new(Mutex::new(latest)),
        }
    }

    /// A port without TLS.
    #[must_use]
    pub fn plain() -> ShardTls {
        ShardTls::new(&TlsPort::default(), None, Policy::ByMode)
    }

    /// This shard's server side; `None` without TLS.
    #[must_use]
    pub fn current(&self) -> Option<Arc<ServerConfig>> {
        LOCAL.with(|l| {
            let mut l = l.borrow_mut();
            l.entry(self.id)
                .or_insert_with(|| {
                    self.latest.lock().unwrap_or_else(PoisonError::into_inner).clone()
                })
                .clone()
        })
    }

    /// How this shard takes a new connection.
    #[must_use]
    pub fn sniff(&self) -> Sniff {
        match (self.current(), self.policy, self.mode) {
            (None, ..) | (Some(_), Policy::ByMode, TlsMode::Disabled) => Sniff::Plain,
            (Some(c), Policy::ByMode, TlsMode::RequireTls) => Sniff::Tls(c),
            (Some(c), Policy::ByMode, TlsMode::AllowTls | TlsMode::PreferTls)
            | (Some(c), Policy::FirstByte, _) => Sniff::Either(c),
        }
    }

    /// Puts `config` (with the port's protocols) in place on every shard; resolves once every
    /// shard has it. The connections already open keep theirs. A port without TLS keeps none.
    pub fn publish(&self, config: Arc<ServerConfig>) -> Future<()> {
        let config = with_alpn(config, self.alpn.as_deref());
        *self.latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(config.clone());
        let id = self.id;
        Smp::invoke_on_all(move || {
            LOCAL.with(|l| l.borrow_mut().insert(id, Some(config)));
        })
    }

    /// [`ShardTls::publish`] of `port`'s server side now; nothing when it has none.
    pub fn publish_port(&self, port: &TlsPort) -> Future<()> {
        match port.config() {
            Some(c) => self.publish(c),
            None => teseladb::engine::core::future::make_ready_future(()),
        }
    }

    /// Forgets this shard's copy, as a stopped port does.
    pub fn forget(&self) {
        LOCAL.with(|l| l.borrow_mut().remove(&self.id));
    }
}

fn with_alpn(config: Arc<ServerConfig>, alpn: Option<&Vec<Vec<u8>>>) -> Arc<ServerConfig> {
    match alpn {
        Some(p) if config.alpn != *p => {
            let mut c = (*config).clone();
            c.alpn.clone_from(p);
            Arc::new(c)
        }
        _ => config,
    }
}
