//! The engine of a test process whose clients' ports run without a node behind them (023
//! FR-018, D4: one engine per process): started the first time a port needs it and shared by
//! every port of the process (of two shards, over which the connections spread). A file that
//! also starts a node in the process runs that test first, before the engine starts. A file
//! declares it with `#[path = "common/port_engine.rs"] mod port_engine;`.

use std::sync::{Arc, OnceLock};

use teseladb_io::engine::{EngineConfig, NodeEngine};
use teseladb_server::client_listener::{ClientConfig, ClientListener, NodeState};

/// A clients' port of `cfg` over `state` on the process's engine.
///
/// # Errors
///
/// When an address of `cfg` cannot be listened on.
pub fn spawn_port(
    cfg: &ClientConfig,
    state: Arc<dyn NodeState>,
) -> std::io::Result<ClientListener> {
    static ENGINE: OnceLock<NodeEngine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let cfg = EngineConfig { shards: 2, ..EngineConfig::default() };
        NodeEngine::start(&cfg).expect("the engine of the test process")
    });
    ClientListener::spawn(cfg, state)
}
