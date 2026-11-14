//! The port on AF_XDP (012 FR-033) on a real Linux kernel: a `veth` pair, one end in a namespace
//! of its own where kernel TCP clients connect, the other with the port's XDP program and its
//! sockets. Linux only; `scripts/test-linux.sh` runs it with `CAP_NET_ADMIN` and `CAP_BPF`.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use teseladb_io::conn_port::{ConnPort, NetHandler, PortDriver};
use teseladb_io::xdp_port::{Flows, XdpListener};
use teseladb_sys::netns::{self, NetNs};
use teseladb_sys::uring::Part;

const SERVER: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
const CLIENT: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);

/// Echoes what arrives; `bye` closes the connection.
#[derive(Default)]
struct Echo {
    peers: HashMap<u64, SocketAddr>,
    closed: usize,
}

impl NetHandler for Echo {
    fn accepted(
        &mut self,
        _net: &mut dyn ConnPort,
        token: u64,
        peer: Option<SocketAddr>,
        local: Option<SocketAddr>,
    ) -> bool {
        assert_eq!(local.map(|l| l.ip()), Some(SERVER.into()));
        self.peers.insert(token, peer.unwrap());
        true
    }

    fn received(&mut self, net: &mut dyn ConnPort, token: u64, bytes: &[u8]) {
        if bytes.ends_with(b"bye") {
            net.close(token);
            return;
        }
        assert!(net.send(token, Part::new(bytes.to_vec())));
    }

    fn sent(&mut self, _net: &mut dyn ConnPort, _token: u64) {}

    fn closed(&mut self, _net: &mut dyn ConnPort, token: u64) {
        self.peers.remove(&token);
        self.closed += 1;
    }

    fn woken(&mut self, _net: &mut dyn ConnPort) {}
}

/// A pair `<prefix>0` (here, `SERVER`) and `<prefix>1` (in `ns`, `CLIENT`) with `queues` queues.
fn pair(prefix: &str, queues: u32, ns: &NetNs) -> String {
    let (here, there) = (format!("{prefix}0"), format!("{prefix}1"));
    netns::create_veth(&here, &there, queues).unwrap();
    netns::move_to(&there, ns).unwrap();
    netns::add_address(&here, SERVER, 24).unwrap();
    netns::set_up(&here).unwrap();
    let t = there.clone();
    ns.run(move || {
        netns::add_address(&t, CLIENT, 24).unwrap();
        netns::set_up(&t).unwrap();
        // A veth leaves the checksum of what it sends to its peer; the socket gets the frame
        // as it is.
        netns::disable_tx_checksum(&t).unwrap();
    })
    .unwrap();
    here
}

/// Runs every port of `listener` on a thread of its own until `stop`; the closes each saw.
fn serve(listener: XdpListener, stop: Arc<AtomicBool>) -> Vec<thread::JoinHandle<usize>> {
    let XdpListener { program, ports } = listener;
    let program = Arc::new(program);
    ports
        .into_iter()
        .map(|mut port| {
            let (stop, keep) = (stop.clone(), program.clone());
            thread::spawn(move || {
                let _program = keep;
                let mut echo = Echo::default();
                while !stop.load(Ordering::Acquire) {
                    port.run_once(&mut echo, Duration::from_millis(20)).unwrap();
                }
                echo.closed
            })
        })
        .collect()
}

fn talk(port: u16, bytes: usize) -> Vec<u8> {
    let mut s = TcpStream::connect((SERVER, port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let data: Vec<u8> = (0..bytes).map(|i| (i % 251) as u8).collect();
    let writer = {
        let mut w = s.try_clone().unwrap();
        let data = data.clone();
        thread::spawn(move || w.write_all(&data).unwrap())
    };
    let mut back = vec![0u8; bytes];
    s.read_exact(&mut back).unwrap();
    writer.join().unwrap();
    assert_eq!(back, data, "the echo of {bytes} bytes");
    s.write_all(b"bye").unwrap();
    // The port closes: the client reads the end of the stream.
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).unwrap();
    rest
}

#[test]
fn kernel_clients_talk_to_the_port_over_af_xdp_on_every_queue() {
    let ns = NetNs::new().unwrap();
    let iface = pair("xq", 2, &ns);
    let port = 27_101;
    let listener = XdpListener::open(&iface, port, Some(SERVER), 64, 11).unwrap();
    assert_eq!(listener.ports.len(), 2);
    assert!(listener.mode().is_some());
    let stop = Arc::new(AtomicBool::new(false));
    let threads = serve(listener, stop.clone());
    let clients = 8;
    let sizes: Vec<usize> = (0..clients).map(|i| 1 + i * 200_000).collect();
    let rests = ns
        .run(move || {
            let hs: Vec<_> =
                sizes.into_iter().map(|n| thread::spawn(move || talk(port, n))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        })
        .unwrap();
    assert!(rests.iter().all(Vec::is_empty));
    thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Release);
    let closed: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
    assert_eq!(closed, clients, "each connection closed once, on its queue");
}

#[test]
fn other_ports_stay_with_the_kernel() {
    let ns = NetNs::new().unwrap();
    let iface = pair("xk", 1, &ns);
    let listener = XdpListener::open(&iface, 27_102, Some(SERVER), 8, 3).unwrap();
    // A kernel listener on another port of the same address still answers through the XDP
    // program, which passes its segments on.
    let kernel = std::net::TcpListener::bind((SERVER, 27_103)).unwrap();
    let t = thread::spawn(move || {
        let (mut c, _) = kernel.accept().unwrap();
        c.write_all(b"kernel").unwrap();
    });
    let got = ns
        .run(|| {
            let mut s = TcpStream::connect((SERVER, 27_103)).unwrap();
            let mut b = Vec::new();
            s.read_to_end(&mut b).unwrap();
            b
        })
        .unwrap();
    t.join().unwrap();
    assert_eq!(got, b"kernel");
    drop(listener);
}

#[test]
fn an_interface_that_does_not_exist_says_so() {
    let e = XdpListener::open("no-such-if", 27_104, None, 8, 1).unwrap_err();
    assert!(e.to_string().starts_with("the interface"), "{e}");
}

#[test]
fn the_table_of_flows_names_the_port_of_each_flow_until_it_forgets_it() {
    let flows = Flows::default();
    assert_eq!(flows.owner(42), None);
    assert!(flows.insert(42 << 8, 1));
    assert!(flows.insert(43 << 8, 0));
    assert_eq!((flows.owner(42 << 8), flows.owner(43 << 8)), (Some(1), Some(0)));
    // The same flow moved to another port.
    assert!(flows.insert(42 << 8, 3));
    assert_eq!(flows.owner(42 << 8), Some(3));
    // Only the port that has it forgets it.
    flows.remove(42 << 8, 1);
    assert_eq!(flows.owner(42 << 8), Some(3));
    flows.remove(42 << 8, 3);
    assert_eq!(flows.owner(42 << 8), None);
    // A forgotten slot does not hide the flows after it, and takes a new one.
    let start = 7u64 << 8;
    let next = start + (1 << 25);
    assert!(flows.insert(start, 0));
    flows.remove(start, 0);
    assert_eq!(flows.owner(next), None);
    // Flows whose slots collide find a place among the probes of a lookup (one of them, 43, is
    // taken).
    let colliding: Vec<u64> = (0..63u64).map(|i| (5 << 8) | (i << 40)).collect();
    assert!(colliding.iter().all(|&f| flows.insert(f, 2)));
    assert!(colliding.iter().all(|&f| flows.owner(f) == Some(2)));
    assert!(!flows.insert((5 << 8) | (99 << 40), 2), "no room near the slot");
    // The extremes of a flow are still flows.
    assert!(flows.insert(u64::MAX, 254) && flows.insert(1, 5));
    assert_eq!((flows.owner(u64::MAX), flows.owner(1)), (Some(254), Some(5)));
}
