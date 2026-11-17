//! The virtio-net queue pair over vhost-net and a tap device (the original's virtio.cc),
//! end to end with the kernel on the other side of the tap: the native stack over the
//! virtio device answers the kernel's ARP and exchanges UDP datagrams (with checksum
//! offload, and larger than the MTU) and a TCP stream (with segmentation offload one way and
//! the kernel's large receive segments, spread over several merged receive buffers, the
//! other). The tap is configured with interface ioctls, since the image has no `ip` tool.
//! Needs /dev/vhost-net and /dev/net/tun (a privileged container); without them the test
//! says so and passes.
//!
//! x86_64 only: the vhost memory table, as in the original (virtio.cc), maps the user
//! addresses below 2^47, the x86_64 user space. On aarch64 the shard memory lies above that
//! (heap blocks at 0xe01000300000 in the test container), so vhost cannot read the rings and
//! no frame crosses the tap in either direction.
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
// This file calls unsafe engine interfaces and libc directly; every `unsafe` block states
// why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

// For its backend selection and its waiting helpers.
#[path = "common/cov_net_wire.rs"]
mod wire;

use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use teseladb::engine::net::ip::Ipv4;
use teseladb::engine::net::ipv4_address::Ipv4Address;
use teseladb::engine::net::native_stack::NativeStackOptions;
use teseladb::engine::net::net::{Device, Interface};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::Ipv4Addr;
use teseladb::engine::net::tcp::Tcp;
use teseladb::engine::net::virtio::{VirtioOptions, create_virtio_net_device};
use teseladb::engine::util::program_options::Value;
use teseladb::engine_thread_test_case;

use wire::{packet_bytes, sleep_ms, sockaddr, wait, wait_for};

const TAP: &str = "cov3tap0";
/// The kernel's end of the tap.
const HOST_IP: [u8; 4] = [10, 77, 0, 1];
/// The native stack's address.
const STACK_IP: [u8; 4] = [10, 77, 0, 2];

const AF_INET: i32 = 2;
const SOCK_DGRAM: i32 = 2;
const SIOCGIFFLAGS: u64 = 0x8913;
const SIOCSIFFLAGS: u64 = 0x8914;
const SIOCSIFADDR: u64 = 0x8916;
const SIOCSIFNETMASK: u64 = 0x891c;
const IFF_UP: i16 = 0x1;

unsafe extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn close(fd: i32) -> i32;
}

/// `struct ifreq` with the address or the flags of its union.
#[repr(C)]
struct Ifreq {
    name: [u8; 16],
    data: [u8; 24],
}

impl Ifreq {
    fn new(name: &str) -> Ifreq {
        let mut r = Ifreq { name: [0; 16], data: [0; 24] };
        r.name[..name.len()].copy_from_slice(name.as_bytes());
        r
    }

    /// A `sockaddr_in` with `ip` in the union.
    fn with_addr(name: &str, ip: [u8; 4]) -> Ifreq {
        let mut r = Ifreq::new(name);
        r.data[..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
        r.data[4..8].copy_from_slice(&ip);
        r
    }
}

/// Runs the interface ioctl `request` on `req`.
fn if_ioctl(request: u64, req: &mut Ifreq) -> std::io::Result<()> {
    // SAFETY: a plain socket for the interface ioctls, closed below.
    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the interface ioctls read and write a `struct ifreq`, which `Ifreq` lays out.
    let r = unsafe { ioctl(fd, request, req as *mut Ifreq as *mut c_void) };
    let e = std::io::Error::last_os_error();
    // SAFETY: `fd` is ours.
    unsafe { close(fd) };
    if r < 0 { Err(e) } else { Ok(()) }
}

/// Gives the kernel's end of the tap its address and brings it up.
fn configure_tap() -> std::io::Result<()> {
    if_ioctl(SIOCSIFADDR, &mut Ifreq::with_addr(TAP, HOST_IP))?;
    if_ioctl(SIOCSIFNETMASK, &mut Ifreq::with_addr(TAP, [255, 255, 255, 0]))?;
    let mut flags = Ifreq::new(TAP);
    if_ioctl(SIOCGIFFLAGS, &mut flags)?;
    let f = i16::from_ne_bytes([flags.data[0], flags.data[1]]) | IFF_UP;
    flags.data[..2].copy_from_slice(&f.to_ne_bytes());
    if_ioctl(SIOCSIFFLAGS, &mut flags)
}

/// The kernel's view of the tap (state and counters), for failure messages.
fn tap_state(tap: &str) -> String {
    let base = format!("/sys/class/net/{tap}");
    let read = |f: &str| std::fs::read_to_string(format!("{base}/{f}")).unwrap_or_default();
    format!(
        "routes {:?} arp {:?} operstate {} carrier {} flags {} tx_packets {} rx_packets {} \
         tx_dropped {} rx_dropped {}",
        std::fs::read_to_string("/proc/net/route").unwrap_or_default(),
        std::fs::read_to_string("/proc/net/arp").unwrap_or_default(),
        read("operstate").trim(),
        read("carrier").trim(),
        read("flags").trim(),
        read("statistics/tx_packets").trim(),
        read("statistics/rx_packets").trim(),
        read("statistics/tx_dropped").trim(),
        read("statistics/rx_dropped").trim()
    )
}

fn ip_u32(ip: [u8; 4]) -> u32 {
    u32::from_be_bytes(ip)
}

/// `n` bytes of a repeating pattern.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// Waits (polling the reactor) for a value from a kernel-side thread.
fn recv_from_thread<T>(rx: &mpsc::Receiver<T>) -> T {
    for _ in 0..10_000 {
        if let Ok(v) = rx.try_recv() {
            return v;
        }
        sleep_ms(1);
    }
    panic!("the kernel side did not answer");
}

/// Joins a kernel-side thread without blocking the reactor, which the thread may need.
fn join(t: std::thread::JoinHandle<()>) {
    for _ in 0..20_000 {
        if t.is_finished() {
            t.join().expect("the kernel side");
            return;
        }
        sleep_ms(1);
    }
    panic!("the kernel side did not finish");
}

engine_thread_test_case!(the_native_stack_talks_to_the_kernel_through_vhost, {
    if !std::path::Path::new("/dev/vhost-net").exists() {
        eprintln!("no /dev/vhost-net: nothing to test");
        return;
    }
    let opts = VirtioOptions::default();
    let lro = Value::new("lro", Some("on".to_string()), "LRO");
    let dev: Arc<dyn Device> = Arc::from(create_virtio_net_device(&opts, &lro).expect("a device"));
    let mut net_opts = NativeStackOptions::new();
    net_opts.tap_device.set_value(TAP.to_string());
    let qp = dev.init_local_queue(&net_opts, 0).expect("the vhost queue");
    dev.set_local_queue(qp);
    // The interface starts the receive ring and registers the transmit provider.
    let netif = Interface::new(dev.clone()).into_raw();
    let slot: *mut Ipv4 = Box::into_raw(Box::<Ipv4>::new_uninit()).cast();
    // SAFETY: a fresh block for an Ipv4 that is never moved nor freed; the interface is
    // leaked, so it outlives the layer.
    unsafe { Ipv4::init_at(slot, netif) };
    // SAFETY: as above; the test touches the layer only between reactor tasks.
    let ip = unsafe { &mut *slot };
    ip.set_host_address(Ipv4Address::from_u32(ip_u32(STACK_IP)));
    ip.set_netmask_address(Ipv4Address::from_u32(0xffff_ff00));
    ip.set_gw_address(Ipv4Address::from_u32(ip_u32(HOST_IP)));
    configure_tap().expect("the tap is configured");

    // UDP from the kernel: it resolves our address first (our ARP reply goes out through
    // the transmit ring), then the datagrams come in through the receive ring.
    let mut ch = ip.get_udp().make_channel(Ipv4Addr { ip: 0, port: 7000 }).expect("a channel");
    let host = UdpSocket::bind(("10.77.0.1", 0)).expect("a kernel socket");
    host.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let host_port = host.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    let kernel = std::thread::spawn(move || {
        for size in [5usize, 3000] {
            host.send_to(&pattern(size), ("10.77.0.2", 7000)).expect("sent");
        }
        // What the stack sends back.
        let mut got = Vec::new();
        let mut buf = vec![0u8; 65536];
        for _ in 0..2 {
            let (n, _) = host.recv_from(&mut buf).expect("received");
            got.push(buf[..n].to_vec());
        }
        let _ = tx.send(got);
    });
    for size in [5usize, 3000] {
        let f = ch.receive();
        for _ in 0..3000 {
            if f.available() {
                break;
            }
            sleep_ms(1);
        }
        if !f.available() {
            panic!("no datagram from the kernel; tap: {}", tap_state(TAP));
        }
        let mut d = f.get().expect("a datagram");
        assert_eq!(packet_bytes(d.get_data()), pattern(size));
    }
    let to = sockaddr(ip_u32(HOST_IP), host_port);
    wait(ch.send(&to, Packet::from_data(b"small"))).expect("sent");
    // Larger than the MTU: one datagram for the device to segment (UFO) or fragments.
    wait(ch.send(&to, Packet::from_data(&pattern(4000)))).expect("sent");
    let got = recv_from_thread(&rx);
    assert_eq!(got, vec![b"small".to_vec(), pattern(4000)]);
    join(kernel);

    // TCP: the stack sends with segmentation offload, the kernel answers with large
    // segments that fill several receive buffers each.
    let listener = TcpListener::bind(("10.77.0.1", 0)).expect("a kernel listener");
    let port = listener.local_addr().expect("addr").port();
    const UP: usize = 200_000;
    const DOWN: usize = 300_000;
    let (tx, rx) = mpsc::channel();
    let kernel = std::thread::spawn(move || {
        let (mut c, _) = listener.accept().expect("accepted");
        let mut got = vec![0u8; UP];
        c.read_exact(&mut got).expect("the upload");
        c.write_all(&pattern(DOWN)).expect("the download");
        let _ = tx.send(got == pattern(UP));
        let mut rest = Vec::new();
        let _ = c.read_to_end(&mut rest);
    });
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(ip.get_tcp(), sockaddr(ip_u32(HOST_IP), port)) };
    wait(conn.connected()).expect("connected");
    let data = pattern(UP);
    for chunk in data.chunks(50_000) {
        wait_for(conn.send(Packet::from_data(chunk)), Duration::from_secs(20)).expect("sent");
    }
    let mut down = Vec::with_capacity(DOWN);
    while down.len() < DOWN {
        wait_for(conn.wait_for_data(), Duration::from_secs(20)).expect("data");
        let p = conn.read();
        assert!(p.len() > 0, "the stream ended after {} bytes", down.len());
        down.extend(packet_bytes(&p));
    }
    assert_eq!(down, pattern(DOWN));
    assert!(recv_from_thread(&rx), "the kernel received the upload intact");
    conn.close_write();
    join(kernel);
});
