//! The clients' port on AF_XDP (012 FR-033, `--net-xdp <interface>`): the IPv4 TCP segments to
//! the port that arrive on the interface go to an AF_XDP socket per queue with a TCP stack of its
//! own (`teseladb_io::xdp_port`). On the engine (023 FR-021) the socket of queue `q` is waited on
//! by the reactor of shard `q mod n` and its connections are served by the same sessions as the
//! kernel's (`teseladb_io::engine::xdp_clients`), with no thread per queue. The kernel's
//! listener of the port stays: it serves the clients of the other interfaces, of loopback and of
//! IPv6. An interface that cannot take AF_XDP (no such interface, a kernel without AF_XDP or
//! `bpf_link`, no `CAP_NET_ADMIN` and `CAP_BPF`, or macOS) leaves the clients to the kernel's
//! listener, and the node says why.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::client_listener::ClientConfig;

/// The port on AF_XDP of a clients' port.
pub(crate) struct XdpState {
    status: Option<Result<String, String>>,
    running: Option<(teseladb_io::engine::xdp_clients::XdpRun, teseladb_sys::xdp::XdpProgram)>,
}

impl XdpState {
    /// `None` without `--net-xdp`; what runs, or why nothing does.
    pub(crate) fn status(&self) -> Option<Result<String, String>> {
        self.status.clone()
    }
}

impl XdpState {
    /// What wakes every connection on AF_XDP (the node's state changed), if the port runs.
    pub(crate) fn waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (run, _) = self.running.as_ref()?;
        let run = run.clone();
        Some(Arc::new(move || run.wake()))
    }

    /// Stops the queues and takes the program off the interface.
    pub(crate) fn stop(self) {
        drop(self);
    }
}

impl Drop for XdpState {
    /// Stops the queues on their shards, then takes the program off the interface.
    fn drop(&mut self) {
        if let Some((run, program)) = self.running.take() {
            // The queues first: the program sends frames to their sockets until it goes.
            run.stop();
            drop(program);
        }
    }
}

/// The address and port the XDP program takes: the first IPv4 address of `addrs` (any address
/// of the interface when it is `0.0.0.0`).
pub(crate) fn target(addrs: &[SocketAddr]) -> Option<(Option<std::net::Ipv4Addr>, u16)> {
    addrs.iter().find_map(|a| match a.ip() {
        IpAddr::V4(ip) => Some(((!ip.is_unspecified()).then_some(ip), a.port())),
        IpAddr::V6(_) => None,
    })
}

/// A secret for the initial sequence numbers (RFC 6528), from the keys the standard library
/// seeds from the system.
fn secret() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::hash::RandomState::new().build_hasher();
    h.write_u64(0x7465_7365_6c61);
    h.finish()
}

/// Starts the port on AF_XDP of `cfg`, if it asks for one, on the listener's addresses `addrs`
/// (with the ports the system chose), with the sessions of `core`.
pub(crate) fn start(
    cfg: &ClientConfig,
    addrs: &[SocketAddr],
    core: Arc<crate::client_session::PortCore>,
) -> XdpState {
    let Some(interface) = &cfg.xdp else { return XdpState { status: None, running: None } };
    let Some((address, port)) = target(addrs) else {
        return XdpState {
            status: Some(Err(format!("{interface}: the port has no IPv4 address"))),
            running: None,
        };
    };
    let listener = teseladb_io::xdp_port::XdpListener::open(
        interface,
        port,
        address,
        cfg.max_connections,
        secret(),
    );
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            return XdpState { status: Some(Err(format!("{interface}: {e}"))), running: None };
        }
    };
    let describe = |l: &teseladb_io::xdp_port::XdpListener| {
        let mode = match l.mode() {
            Some(teseladb_sys::xdp::AttachMode::Driver) => "driver",
            _ => "generic",
        };
        let copy = if l.ports.iter().all(|p| p.zerocopy()) { "zero-copy" } else { "copy" };
        let huge = l.ports.iter().all(|p| p.on_huge_pages());
        format!(
            "{interface}: {} queues, XDP in {mode} mode, {copy}, UMEM on {} pages",
            l.ports.len(),
            if huge { "2 MiB" } else { "normal" }
        )
    };
    let text = describe(&listener);
    let teseladb_io::xdp_port::XdpListener { program, ports } = listener;
    let service: Arc<dyn teseladb_io::engine::client_port::Service> = core;
    match teseladb_io::engine::xdp_clients::start_xdp(ports, service) {
        Ok(run) => XdpState { status: Some(Ok(text)), running: Some((run, program)) },
        Err(e) => XdpState {
            status: Some(Err(format!("{interface}: the queues on the shards: {e}"))),
            running: None,
        },
    }
}

#[cfg(all(test, target_os = "linux"))]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_program_takes_the_first_ipv4_address_of_the_port() {
        let any: SocketAddr = "0.0.0.0:27017".parse().unwrap();
        let one: SocketAddr = "10.0.0.5:27018".parse().unwrap();
        let six: SocketAddr = "[::1]:27019".parse().unwrap();
        assert_eq!(target(&[six, one, any]), Some((Some("10.0.0.5".parse().unwrap()), 27018)));
        assert_eq!(target(&[any]), Some((None, 27017)));
        assert_eq!(target(&[six]), None);
        assert_ne!(secret(), secret());
    }
}
