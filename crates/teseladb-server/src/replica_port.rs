//! The replica set port (018 FR-016): a second clients' port that always serves the replica set
//! presentation, with the main port's TLS, authentication and limits, while the main port keeps
//! its own. Group 0 says which port (`replicaSetPort` of `topologyPresentation`); every node opens
//! it when it applies the entry and closes it when the parameter drops it, without a restart.
//!
//! On the engine (023 FR-019) its keeper is a task of shard 0 and the port, like the main one,
//! listens on every shard.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::client_listener::{ClientConfig, ClientListener, Hooks, NodeClockState, NodePresence};
use crate::client_tasks::{Task, TaskStop};

/// How often the keeper looks at the port Group 0 wants: well within the second a presentation
/// change has to reach every node (018 SC-005).
const LOOK_EVERY: Duration = Duration::from_millis(50);

/// The task that keeps the replica set port as Group 0 wants it.
pub struct PortKeeper {
    /// `None` when the task could not start: the node goes on without the port.
    task: Option<Task>,
}

/// The main port's addresses with port `port`.
pub fn addrs_on(main: &[SocketAddr], port: u16) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for a in main {
        let moved = SocketAddr::new(a.ip(), port);
        if !out.contains(&moved) {
            out.push(moved);
        }
    }
    out
}

impl PortKeeper {
    /// Starts the keeper of a node whose main clients' port is `main`.
    pub fn spawn(
        main: ClientConfig,
        state: Arc<NodeClockState>,
        presence: Arc<NodePresence>,
        hooks: Hooks,
    ) -> PortKeeper {
        let body = Box::new(move |stop: &TaskStop| keep(&main, &state, &presence, hooks, stop));
        PortKeeper { task: crate::client_tasks::spawn(0, body) }
    }

    /// Stops the keeper and closes the port.
    pub fn stop(self) {
        if let Some(t) = self.task {
            let _ = t.stop();
        }
    }
}

fn keep(
    main: &ClientConfig,
    state: &Arc<NodeClockState>,
    presence: &NodePresence,
    hooks: Hooks,
    stop: &TaskStop,
) {
    // The port open now, and the last one asked for (a port that cannot be bound is not tried
    // again until Group 0 asks for another).
    let mut open: Option<(u16, ClientListener)> = None;
    let mut tried: Option<u16> = None;
    while !stop.stopped() {
        let wanted = presence.replica_port().map(|(p, _)| p);
        if wanted != tried {
            if let Some((_, l)) = open.take() {
                l.stop();
            }
            tried = wanted;
            if let Some(port) = wanted {
                let cfg = ClientConfig { addrs: addrs_on(&main.addrs, port), ..main.clone() };
                let st: Arc<dyn crate::client_listener::NodeState> = state.clone();
                match ClientListener::spawn_with(&cfg, st, hooks) {
                    Ok(l) => open = Some((port, l)),
                    Err(e) => crate::log::defer(crate::log::Pending {
                        id: 0,
                        msg: format!("cannot open the replica set port {port}: {e}"),
                        attr: String::new(),
                    }),
                }
            }
        }
        stop.pause(LOOK_EVERY);
    }
    if let Some((_, l)) = open {
        l.stop();
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_port_moves_every_address_once() {
        let main: Vec<SocketAddr> =
            vec!["127.0.0.1:27017".parse().unwrap(), "127.0.0.1:27018".parse().unwrap()];
        assert_eq!(addrs_on(&main, 27100), vec!["127.0.0.1:27100".parse::<SocketAddr>().unwrap()]);
    }
}
