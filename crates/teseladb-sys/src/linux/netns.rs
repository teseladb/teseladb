//! Network namespaces and virtual Ethernet pairs over rtnetlink, for the tests of the port on
//! AF_XDP (012 FR-033): a test makes a `veth` pair, moves one end into a namespace of its own and
//! talks across the pair as a client on another host would. It needs `CAP_NET_ADMIN`
//! (`scripts/test-linux.sh` runs the tests in a container of its own with it).

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::thread;

const AF_NETLINK: i32 = 16;
const AF_INET: i32 = 2;
const SOCK_RAW: i32 = 3;
const SOCK_DGRAM: i32 = 2;
const SOCK_CLOEXEC: i32 = 0o2_000_000;
const NETLINK_ROUTE: i32 = 0;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const NLMSG_ERROR: u16 = 2;
const RTM_NEWLINK: u16 = 16;
const RTM_NEWADDR: u16 = 20;
const IFLA_IFNAME: u16 = 3;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFF_UP: u32 = 1;
const CLONE_NEWNET: i32 = 0x4000_0000;
const SIOCETHTOOL: u64 = 0x8946;
const ETHTOOL_STXCSUM: u32 = 0x17;
const IFNAMSIZ: usize = 16;

unsafe extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize;
    fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize;
    fn unshare(flags: i32) -> i32;
    fn setns(fd: i32, nstype: i32) -> i32;
    fn ioctl(fd: i32, req: u64, ...) -> i32;
}

fn sock(domain: i32, ty: i32, proto: i32) -> io::Result<OwnedFd> {
    // SAFETY: no pointers.
    let fd = unsafe { socket(domain, ty | SOCK_CLOEXEC, proto) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor, owned here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// An rtnetlink attribute: its type and payload, padded to 4 bytes.
fn attr(out: &mut Vec<u8>, ty: u16, payload: &[u8]) {
    out.extend_from_slice(&((4 + payload.len()) as u16).to_ne_bytes());
    out.extend_from_slice(&ty.to_ne_bytes());
    out.extend_from_slice(payload);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn nested(out: &mut Vec<u8>, ty: u16, inner: &[u8]) {
    attr(out, ty, inner);
}

fn name_bytes(name: &str) -> Vec<u8> {
    let mut b = name.as_bytes().to_vec();
    b.push(0);
    b
}

/// `struct ifinfomsg` for interface `index` with `flags` changed by `change`.
fn ifinfo(index: i32, flags: u32, change: u32) -> Vec<u8> {
    let mut m = vec![0u8; 16];
    m[4..8].copy_from_slice(&index.to_ne_bytes());
    m[8..12].copy_from_slice(&flags.to_ne_bytes());
    m[12..16].copy_from_slice(&change.to_ne_bytes());
    m
}

/// Sends rtnetlink request `ty` with `body` and waits for its acknowledgement.
fn request(ty: u16, flags: u16, body: &[u8]) -> io::Result<()> {
    let fd = sock(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)?;
    let mut msg = Vec::with_capacity(16 + body.len());
    msg.extend_from_slice(&((16 + body.len()) as u32).to_ne_bytes());
    msg.extend_from_slice(&ty.to_ne_bytes());
    msg.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK | flags).to_ne_bytes());
    msg.extend_from_slice(&1u32.to_ne_bytes());
    msg.extend_from_slice(&0u32.to_ne_bytes());
    msg.extend_from_slice(body);
    // SAFETY: `msg` lives for the call.
    if unsafe { send(fd.as_raw_fd(), msg.as_ptr(), msg.len(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; 8192];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe { recv(fd.as_raw_fd(), buf.as_mut_ptr(), buf.len(), 0) };
    if n < 20 {
        return Err(io::Error::last_os_error());
    }
    let kind = u16::from_ne_bytes([buf[4], buf[5]]);
    let code = i32::from_ne_bytes([buf[16], buf[17], buf[18], buf[19]]);
    match (kind, code) {
        (NLMSG_ERROR, 0) => Ok(()),
        (NLMSG_ERROR, e) => Err(io::Error::from_raw_os_error(-e)),
        _ => Err(io::Error::from(io::ErrorKind::InvalidData)),
    }
}

/// Makes the `veth` pair `a` and `b`, each with `queues` queues each way.
pub fn create_veth(a: &str, b: &str, queues: u32) -> io::Result<()> {
    let q = queues.max(1).to_ne_bytes();
    let mut peer = ifinfo(0, 0, 0);
    attr(&mut peer, IFLA_IFNAME, &name_bytes(b));
    attr(&mut peer, IFLA_NUM_TX_QUEUES, &q);
    attr(&mut peer, IFLA_NUM_RX_QUEUES, &q);
    let mut data = Vec::new();
    nested(&mut data, VETH_INFO_PEER, &peer);
    let mut info = Vec::new();
    attr(&mut info, IFLA_INFO_KIND, b"veth");
    nested(&mut info, IFLA_INFO_DATA, &data);
    let mut body = ifinfo(0, 0, 0);
    attr(&mut body, IFLA_IFNAME, &name_bytes(a));
    attr(&mut body, IFLA_NUM_TX_QUEUES, &q);
    attr(&mut body, IFLA_NUM_RX_QUEUES, &q);
    nested(&mut body, IFLA_LINKINFO, &info);
    request(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, &body)
}

fn index(name: &str) -> io::Result<i32> {
    crate::linux::xdp::if_index(name).map(|i| i as i32)
}

/// Brings interface `name` up.
pub fn set_up(name: &str) -> io::Result<()> {
    request(RTM_NEWLINK, 0, &ifinfo(index(name)?, IFF_UP, IFF_UP))
}

/// Gives interface `name` address `ip/prefix`.
pub fn add_address(name: &str, ip: Ipv4Addr, prefix: u8) -> io::Result<()> {
    let mut body = vec![AF_INET as u8, prefix, 0, 0];
    body.extend_from_slice(&(index(name)? as u32).to_ne_bytes());
    attr(&mut body, IFA_LOCAL, &ip.octets());
    attr(&mut body, IFA_ADDRESS, &ip.octets());
    request(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, &body)
}

/// Moves interface `name` into the namespace `ns`.
pub fn move_to(name: &str, ns: &NetNs) -> io::Result<()> {
    let mut body = ifinfo(index(name)?, 0, 0);
    attr(&mut body, IFLA_NET_NS_FD, &(ns.fd.as_raw_fd() as u32).to_ne_bytes());
    request(RTM_NEWLINK, 0, &body)
}

/// Turns off the checksums interface `name` leaves to the hardware on send, so that every frame
/// it sends carries its own (`ethtool -K <name> tx off`): a `veth` leaves them to its peer, and
/// an AF_XDP socket gets the frame as it is.
pub fn disable_tx_checksum(name: &str) -> io::Result<()> {
    let fd = sock(AF_INET, SOCK_DGRAM, 0)?;
    let mut value = [0u8; 8];
    value[0..4].copy_from_slice(&ETHTOOL_STXCSUM.to_ne_bytes());
    let mut ifr = [0u8; 40];
    let n = name.len().min(IFNAMSIZ - 1);
    ifr[..n].copy_from_slice(&name.as_bytes()[..n]);
    ifr[IFNAMSIZ..IFNAMSIZ + 8].copy_from_slice(&(value.as_mut_ptr() as u64).to_ne_bytes());
    // SAFETY: `ifr` is a `struct ifreq` whose data points to `value`, both alive for the call.
    if unsafe { ioctl(fd.as_raw_fd(), SIOCETHTOOL, ifr.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A network namespace of its own, alive while this lives.
#[derive(Debug)]
pub struct NetNs {
    fd: OwnedFd,
}

impl NetNs {
    /// A new namespace.
    pub fn new() -> io::Result<NetNs> {
        // A thread of its own moves into the new namespace; its descriptor keeps it.
        thread::spawn(|| {
            // SAFETY: no pointers; it only changes this thread's namespace.
            if unsafe { unshare(CLONE_NEWNET) } < 0 {
                return Err(io::Error::last_os_error());
            }
            let f = std::fs::File::open("/proc/thread-self/ns/net")?;
            Ok(NetNs { fd: OwnedFd::from(f) })
        })
        .join()
        .map_err(|_| io::Error::other("the namespace's thread panicked"))?
    }

    /// Runs `f` on a thread inside the namespace.
    pub fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> io::Result<T> {
        let fd: RawFd = self.fd.as_raw_fd();
        let dup = self.fd.try_clone()?;
        thread::spawn(move || {
            let _keep = dup;
            // SAFETY: `fd` is a namespace descriptor kept open by `dup`; it only changes this
            // thread's namespace.
            if unsafe { setns(fd, CLONE_NEWNET) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(f())
        })
        .join()
        .map_err(|_| io::Error::other("the thread in the namespace panicked"))?
    }
}
