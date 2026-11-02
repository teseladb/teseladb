//! The replica set port (018 FR-016): a second clients' port that always serves the replica set
//! presentation, with the main port's TLS, authentication and limits, while the main port keeps
//! its own. Group 0 says which port (`replicaSetPort` of `topologyPresentation`); every node opens
//! it when it applies the entry and closes it when the parameter drops it, without a restart.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::client_listener::{ClientConfig, ClientListener, Hooks, NodeClockState, NodePresence};

/// How often the keeper looks at the port Group 0 wants: well within the second a presentation
/// change has to reach every node (018 SC-005).
const LOOK_EVERY: Duration = Duration::from_millis(50);

/// The thread that keeps the replica set port as Group 0 wants it.
pub struct PortKeeper {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
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
        workers: u16,
        state: Arc<NodeClockState>,
        presence: Arc<NodePresence>,
        hooks: Hooks,
    ) -> PortKeeper {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name("replica-set-port".into())
            .spawn(move || keep(&main, workers, &state, &presence, hooks, &flag))
            .unwrap_or_else(|_| thread::spawn(|| {}));
        PortKeeper { stop, thread }
    }

    /// Stops the keeper and closes the port.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.thread.join();
    }
}

fn keep(
    main: &ClientConfig,
    workers: u16,
    state: &Arc<NodeClockState>,
    presence: &NodePresence,
    hooks: Hooks,
    stop: &AtomicBool,
) {
    // The port open now, and the last one asked for (a port that cannot be bound is not tried
    // again until Group 0 asks for another).
    let mut open: Option<(u16, ClientListener)> = None;
    let mut tried: Option<u16> = None;
    while !stop.load(Ordering::Acquire) {
        let wanted = presence.replica_port().map(|(p, _)| p);
        if wanted != tried {
            if let Some((_, l)) = open.take() {
                l.stop();
            }
            tried = wanted;
            if let Some(port) = wanted {
                let cfg = ClientConfig { addrs: addrs_on(&main.addrs, port), ..main.clone() };
                let st: Arc<dyn crate::client_listener::NodeState> = state.clone();
                match ClientListener::spawn_with(&cfg, workers, st, hooks) {
                    Ok(l) => open = Some((port, l)),
                    Err(e) => crate::log::defer(crate::log::Pending {
                        id: 0,
                        msg: format!("cannot open the replica set port {port}: {e}"),
                        attr: String::new(),
                    }),
                }
            }
        }
        thread::sleep(LOOK_EVERY);
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
