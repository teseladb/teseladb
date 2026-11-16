//! The engine of a node outside Linux (D3): the same names as on Linux, so that the crates that
//! start nodes still build, and an engine that never starts. No value of these types exists, so
//! every method but [`NodeEngine::start`] is unreachable.

use std::convert::Infallible;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use teseladb_raftlog::{Disk, IoDone, IoOp};

use super::core_loop::{CoreLogic, CoreSender};
use super::options::EngineConfig;
use crate::IoClass;
use crate::device_log::LogPort;
use crate::handshake::AcceptorOptions;
use crate::peer::Hello;
use crate::secure::PeerTls;

/// What a core's task needs besides its logic and disk.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// The core's `Hello` (with its core).
    pub hello: Hello,
    /// Tick period.
    pub tick: Duration,
    /// Seed of the backoff jitter.
    pub seed: u64,
    /// TLS for the connections the core dials; `None` in clear.
    pub tls: Option<PeerTls>,
    /// Where the monotonic time of the node's cores starts.
    pub origin: Instant,
}

/// The executor of one core's disk operations.
pub trait CoreDisk: 'static {
    /// Takes `op` of `class`.
    fn submit(&mut self, op: IoOp, class: IoClass) -> bool;
    /// Moves the results ready to `out`.
    fn reap(&mut self, out: &mut Vec<(IoDone, IoClass)>);
}

/// Runs a [`Disk`] on the core's disk thread (Linux only).
pub struct ThreadDisk<D> {
    never: Infallible,
    _disk: PhantomData<D>,
}

impl<D: Disk + Send + 'static> ThreadDisk<D> {
    /// Never made outside Linux: it needs an engine.
    pub fn new(_name: String, _disk: D) -> ThreadDisk<D> {
        unreachable!("the engine runs on Linux only")
    }

    /// Unreachable.
    pub fn into_disk(self) -> Option<D> {
        match self.never {}
    }
}

impl<D: Disk + Send + 'static> CoreDisk for ThreadDisk<D> {
    fn submit(&mut self, _op: IoOp, _class: IoClass) -> bool {
        match self.never {}
    }

    fn reap(&mut self, _out: &mut Vec<(IoDone, IoClass)>) {
        match self.never {}
    }
}

/// A core running on its shard.
pub struct CoreHandle<M, R> {
    never: Infallible,
    _types: PhantomData<(M, R)>,
}

impl<M: Send + 'static, R> CoreHandle<M, R> {
    /// Unreachable.
    pub fn sender(&self) -> CoreSender<M> {
        match self.never {}
    }

    /// Unreachable.
    pub fn join(self) -> Option<R> {
        match self.never {}
    }
}

/// The engine of a node: it does not start outside Linux.
#[derive(Debug)]
pub struct NodeEngine {
    never: Infallible,
}

impl NodeEngine {
    /// Fails: the engine runs on Linux only (D3).
    ///
    /// # Errors
    ///
    /// Always.
    pub fn start(_cfg: &EngineConfig) -> io::Result<NodeEngine> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the server runs on Linux only"))
    }

    /// Unreachable.
    pub fn shards(&self) -> u16 {
        match self.never {}
    }

    /// Unreachable.
    ///
    /// # Errors
    ///
    /// Unreachable.
    pub fn run_on<T: Send + 'static>(
        &self,
        _shard: u16,
        _f: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<T> {
        match self.never {}
    }

    /// Unreachable.
    ///
    /// # Errors
    ///
    /// Unreachable.
    pub fn start_core<L, X, R, E, B, F>(
        &self,
        _shard: u16,
        _cfg: CoreConfig,
        _build: B,
        _finish: F,
    ) -> Result<CoreHandle<L::Msg, R>, E>
    where
        L: CoreLogic + 'static,
        X: CoreDisk,
        R: Send + 'static,
        E: Send + 'static + From<io::Error>,
        B: FnOnce() -> Result<(L, X, Option<LogPort>), E> + Send + 'static,
        F: FnOnce(L, X, bool) -> R + Send + 'static,
    {
        match self.never {}
    }

    /// Unreachable.
    ///
    /// # Errors
    ///
    /// Unreachable.
    pub fn listen_peers(
        &self,
        _at: SocketAddr,
        _local: &Hello,
        _opts: &AcceptorOptions,
        _base: u16,
        _cores: u16,
    ) -> io::Result<SocketAddr> {
        match self.never {}
    }

    /// Unreachable.
    pub fn stop_peers(&self, _at: Option<SocketAddr>) {
        match self.never {}
    }

    /// Unreachable.
    pub fn stop(self) {
        match self.never {}
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_engine_does_not_start_outside_linux() {
        let cfg = EngineConfig { shards: 2, ..EngineConfig::default() };
        let e = NodeEngine::start(&cfg).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(e.to_string().contains("Linux"), "{e}");
    }
}
