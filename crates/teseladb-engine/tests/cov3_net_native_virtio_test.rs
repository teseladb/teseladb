//! The native network stack selected on the command line (`--network-stack native`, the
//! original's native-stack.cc) over a virtio device on vhost-net and a tap, with a static
//! address on two shards: shard 0 owns the device queue and shard 1 sends and receives
//! through a proxy queue. A kernel TCP server and client on the tap's other end talk to
//! connections and listeners of both shards, and a UDP channel exchanges datagrams. The
//! tap is configured with interface ioctls (the image has no `ip` tool). Needs
//! /dev/vhost-net and /dev/net/tun (a privileged container). x86_64 only, for the reason
//! cov3_net_virtio_test gives (the vhost memory table covers the user addresses below 2^47).
#![cfg(all(target_os = "linux", target_arch = "x86_64"))]
// This file calls libc and unsafe engine interfaces directly; every `unsafe` block states
// why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{connect, engine, listen_with};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{ConnectedSocketInputStreamConfig, ListenOptions};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine_test_case;

const TAP: &str = "cov3tap1";

/// Starts the test reactor on the native stack, with two shards, before `main`; also points
/// `HOME` at a configuration that selects the linux-aio backend (the device's pollers never
/// let the reactor sleep, which the io_uring backend needs to re-arm its timer).
#[used]
#[unsafe(link_section = ".init_array")]
static START_NATIVE: extern "C" fn() = {
    extern "C" fn start() {
        let home = std::env::temp_dir().join(format!("cov3_native_home_{}", std::process::id()));
        let dir = home.join(".config").join("engine");
        std::fs::create_dir_all(&dir).expect("the configuration directory");
        std::fs::write(dir.join("engine.conf"), "reactor-backend = linux-aio\n")
            .expect("the engine configuration");
        // SAFETY: runs from `.init_array`, before `main`, while the process has one thread.
        unsafe { std::env::set_var("HOME", &home) };
        let args: Vec<String> = [
            "cov3_net_native_virtio_test",
            "--smp=2",
            "--network-stack=native",
            "--tap-device=cov3tap1",
            "--dhcp=0",
            "--host-ipv4-addr=10.78.0.2",
            "--gw-ipv4-addr=10.78.0.1",
            "--netmask-ipv4-addr=255.255.255.0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        global_test_runner().start(&args);
    }
    start
};

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

    fn with_addr(name: &str, ip: [u8; 4]) -> Ifreq {
        let mut r = Ifreq::new(name);
        r.data[..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
        r.data[4..8].copy_from_slice(&ip);
        r
    }
}

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

/// Gives the kernel's end of the tap 10.78.0.1/24 and brings it up.
fn configure_tap() -> std::io::Result<()> {
    if_ioctl(SIOCSIFADDR, &mut Ifreq::with_addr(TAP, [10, 78, 0, 1]))?;
    if_ioctl(SIOCSIFNETMASK, &mut Ifreq::with_addr(TAP, [255, 255, 255, 0]))?;
    let mut flags = Ifreq::new(TAP);
    if_ioctl(SIOCGIFFLAGS, &mut flags)?;
    let f = i16::from_ne_bytes([flags.data[0], flags.data[1]]) | IFF_UP;
    flags.data[..2].copy_from_slice(&f.to_ne_bytes());
    if_ioctl(SIOCSIFFLAGS, &mut flags)
}

fn addr(host: &str, port: u16) -> Result<SocketAddress, ExceptionPtr> {
    Ok(SocketAddress::from_ipv4_addr(Ipv4Addr::from_string_port(host, port)?))
}

fn sleep_ms(ms: u64) {
    sleep_steady(Duration::from_millis(ms)).get().expect("sleep");
}

/// Waits (polling the reactor) for a value from a kernel-side thread.
fn recv_from_thread<T>(rx: &mpsc::Receiver<T>) -> T {
    for _ in 0..20_000 {
        if let Ok(v) = rx.try_recv() {
            return v;
        }
        sleep_ms(1);
    }
    panic!("the kernel side did not answer");
}

/// Connects to the kernel's echo server from the current shard, writes `msg` and reads it
/// back.
fn echo_round_trip(port: u16, msg: &'static str) -> Result<(), ExceptionPtr> {
    let mut c = connect(addr("10.78.0.1", port)?).get()?;
    let mut out = c.output(8192);
    out.write_str(msg).get()?;
    out.flush().get()?;
    let mut input = c.input(ConnectedSocketInputStreamConfig::default());
    let mut got = Vec::new();
    while got.len() < msg.len() {
        let b = input.read().get()?;
        assert!(!b.empty(), "the echo ended early");
        got.extend_from_slice(b.as_slice());
    }
    assert_eq!(got, msg.as_bytes());
    out.close().get()?;
    Ok(())
}

engine_test_case!(the_native_stack_on_two_shards_talks_to_the_kernel, {
    async_(|| -> Result<(), ExceptionPtr> {
        assert_eq!(Smp::count(), 2);
        configure_tap().map_err(ExceptionPtr::from)?;

        // A kernel echo server.
        let server = TcpListener::bind(("10.78.0.1", 0)).map_err(ExceptionPtr::from)?;
        let port = server.local_addr().map_err(ExceptionPtr::from)?.port();
        std::thread::spawn(move || {
            for c in server.incoming() {
                let Ok(mut c) = c else { break };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = c.read(&mut buf) {
                        if n == 0 || c.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });

        // Connections from shard 0 (the device's queue) and from shard 1 (its proxy).
        echo_round_trip(port, "from shard 0")?;
        Smp::submit_to(1, move || {
            async_(move || -> Result<(), ExceptionPtr> {
                assert_eq!(this_shard_id(), 1);
                echo_round_trip(port, "from shard 1")
            })
        })
        .get()?;

        // A listener of the native stack, which a kernel client connects to.
        let mut ss = listen_with(addr("10.78.0.2", 5555)?, ListenOptions::default())?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = (|| -> std::io::Result<Vec<u8>> {
                let mut c = TcpStream::connect(("10.78.0.2", 5555))?;
                c.set_read_timeout(Some(Duration::from_secs(10)))?;
                c.write_all(b"kernel says hi")?;
                let mut back = [0u8; 2];
                c.read_exact(&mut back)?;
                Ok(back.to_vec())
            })();
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });
        let accepted = ss.accept().get()?;
        let mut s = accepted.connection;
        let mut input = s.input(ConnectedSocketInputStreamConfig::default());
        let mut got = Vec::new();
        while got.len() < 14 {
            let b = input.read().get()?;
            got.extend_from_slice(b.as_slice());
        }
        assert_eq!(got, b"kernel says hi");
        let mut out = s.output(1024);
        out.write_str("ok").get()?;
        out.flush().get()?;
        assert_eq!(recv_from_thread(&rx).expect("the kernel client"), b"ok");
        out.close().get()?;
        ss.abort_accept();

        // UDP both ways.
        let host = UdpSocket::bind(("10.78.0.1", 0)).map_err(ExceptionPtr::from)?;
        host.set_read_timeout(Some(Duration::from_secs(10))).map_err(ExceptionPtr::from)?;
        let host_port = host.local_addr().map_err(ExceptionPtr::from)?.port();
        // SAFETY: this shard's running reactor.
        let mut ch =
            unsafe { (*engine()).net() }.make_bound_datagram_channel(&addr("10.78.0.2", 6000)?)?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = host.send_to(b"ping", ("10.78.0.2", 6000));
            let mut buf = [0u8; 64];
            let r = host.recv_from(&mut buf).map(|(n, _)| buf[..n].to_vec());
            let _ = tx.send(r.map_err(|e| e.to_string()));
        });
        let mut d = ch.receive().get()?;
        assert_eq!(d.get_data().len(), 4);
        let from = d.get_src();
        assert_eq!(from.port(), host_port);
        ch.send(&from, Packet::from_data(b"pong")).get()?;
        assert_eq!(recv_from_thread(&rx).expect("the kernel socket"), b"pong");
        ch.close();
        Ok(())
    })
});
