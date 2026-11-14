//! The clients' port on AF_XDP (012 FR-033, `--net-xdp <interface>`): the IPv4 TCP segments to
//! the port that arrive on the interface go to an AF_XDP socket per queue with a TCP stack of its
//! own (`teseladb_io::xdp_port`), and their connections are served as on the io_uring rings
//! (`client_ring`), with a command pool. The kernel's listener of the port stays: it serves the
//! clients of the other interfaces, of loopback and of IPv6. An interface that cannot take AF_XDP
//! (no such interface, a kernel without AF_XDP or `bpf_link`, no `CAP_NET_ADMIN` and `CAP_BPF`,
//! or macOS) leaves the clients to the kernel's listener, and the node says why.

#[cfg(target_os = "linux")]
use std::net::{IpAddr, SocketAddr};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize};

#[cfg(target_os = "linux")]
use teseladb_wire::commands::Shared;

use crate::client_listener::ClientConfig;
#[cfg(target_os = "linux")]
use crate::client_listener::{ClientKit, NodeState};
#[cfg(target_os = "linux")]
use crate::diag::Diag;

/// The port on AF_XDP of a clients' port.
pub(crate) struct XdpState {
    status: Option<Result<String, String>>,
    #[cfg(target_os = "linux")]
    running: Option<(crate::client_ring::RingPort, teseladb_sys::xdp::XdpProgram)>,
}

impl XdpState {
    /// `None` without `--net-xdp`; what runs, or why nothing does.
    pub(crate) fn status(&self) -> Option<Result<String, String>> {
        self.status.clone()
    }
}

#[cfg(target_os = "linux")]
impl Drop for XdpState {
    /// Stops the queues' threads (the port's stop flag is set), then takes the program off the
    /// interface.
    fn drop(&mut self) {
        if let Some((port, program)) = self.running.take() {
            // The threads first: the program sends frames to their sockets until it goes.
            port.stop();
            drop(program);
        }
    }
}

#[cfg(target_os = "linux")]
/// The address and port the XDP program takes: the first IPv4 address of `addrs` (any address
/// of the interface when it is `0.0.0.0`).
pub(crate) fn target(addrs: &[SocketAddr]) -> Option<(Option<std::net::Ipv4Addr>, u16)> {
    addrs.iter().find_map(|a| match a.ip() {
        IpAddr::V4(ip) => Some(((!ip.is_unspecified()).then_some(ip), a.port())),
        IpAddr::V6(_) => None,
    })
}

#[cfg(target_os = "linux")]
/// A secret for the initial sequence numbers (RFC 6528), from the keys the standard library
/// seeds from the system.
fn secret() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::hash::RandomState::new().build_hasher();
    h.write_u64(0x7465_7365_6c61);
    h.finish()
}

#[cfg(target_os = "linux")]
/// What the port's connections on AF_XDP share with the rest of the port.
pub(crate) struct XdpParts {
    pub(crate) state: Arc<dyn NodeState>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) kit: ClientKit,
    pub(crate) diag: Arc<Diag>,
    pub(crate) open: Arc<AtomicUsize>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) ids: Arc<AtomicI32>,
}

#[cfg(target_os = "linux")]
/// Starts the port on AF_XDP of `cfg`, if it asks for one, on the listener's addresses `addrs`
/// (with the ports the system chose).
pub(crate) fn start(cfg: &ClientConfig, addrs: &[SocketAddr], parts: XdpParts) -> XdpState {
    let Some(interface) = &cfg.xdp else { return XdpState { status: None, running: None } };
    {
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
        let parts = crate::client_ring::RingParts {
            state: parts.state,
            shared: parts.shared,
            kit: parts.kit,
            diag: parts.diag,
            open: parts.open,
            stop: parts.stop,
            max: cfg.max_connections,
            next_id: parts.ids,
        };
        let teseladb_io::xdp_port::XdpListener { program, ports } = listener;
        match crate::client_ring::spawn_xdp(parts, ports) {
            Ok(port) => XdpState { status: Some(Ok(text)), running: Some((port, program)) },
            Err(e) => XdpState {
                status: Some(Err(format!("{interface}: the queues' threads: {e}"))),
                running: None,
            },
        }
    }
}

/// The port of `cfg` on a system without AF_XDP: the reason, if it asked for one.
#[cfg(not(target_os = "linux"))]
pub(crate) fn unsupported(cfg: &ClientConfig) -> XdpState {
    XdpState { status: cfg.xdp.as_ref().map(|i| Err(format!("{i}: AF_XDP is Linux only"))) }
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
