//! AF_XDP (012 FR-033): the XDP program that sends a port's TCP segments to the sockets of its
//! interface's queues, and those sockets with their UMEM and their four rings.
//!
//! The program is written here in eBPF instructions and loaded with `bpf(2)`: an IPv4 frame
//! without IP options or fragments, of TCP, to the port (and to the address, when the port has
//! one) goes to the socket of the queue it arrived on through an `XSKMAP`; every other frame,
//! and every frame of a queue without a socket, goes on to the kernel (`XDP_PASS`). It is attached
//! with a `bpf_link` (Linux 5.9), in the driver's mode where the driver has one and in the generic
//! one where it does not; closing the link takes it off, also when the process dies.
//!
//! An [`Xsk`] owns its UMEM (on 2 MiB pages when the system has them reserved, FR-032), the
//! fill and completion rings of the UMEM and its receive and transmit rings, mapped from the
//! kernel. Half of the UMEM's frames receive and half send. It binds in zero-copy mode where the
//! driver allows it and copies otherwise, and with `XDP_USE_NEED_WAKEUP` where the kernel has it.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(target_arch = "aarch64")]
const SYS_BPF: i64 = 280;
#[cfg(target_arch = "x86_64")]
const SYS_BPF: i64 = 321;

const BPF_MAP_CREATE: i64 = 0;
const BPF_MAP_UPDATE_ELEM: i64 = 2;
const BPF_MAP_DELETE_ELEM: i64 = 3;
const BPF_PROG_LOAD: i64 = 5;
const BPF_LINK_CREATE: i64 = 28;
const BPF_MAP_TYPE_XSKMAP: u32 = 17;
const BPF_PROG_TYPE_XDP: u32 = 6;
/// `expected_attach_type` and `attach_type` of an XDP program.
const BPF_XDP: u32 = 37;
const XDP_FLAGS_SKB_MODE: u32 = 1 << 1;
const XDP_FLAGS_DRV_MODE: u32 = 1 << 2;
/// The program's answer for a frame the kernel keeps.
const XDP_PASS: i32 = 2;
/// `bpf_redirect_map`.
const HELPER_REDIRECT_MAP: i32 = 51;
const BPF_PSEUDO_MAP_FD: u8 = 1;
/// Room for the verifier's report of a refused program.
const VERIFIER_LOG: usize = 64 * 1024;
/// `union bpf_attr`, as large as the fields used here need.
const ATTR_WORDS: usize = 16;

const AF_XDP: i32 = 44;
const SOCK_RAW: i32 = 3;
const SOCK_CLOEXEC: i32 = 0o2_000_000;
const SOL_XDP: i32 = 283;
const XDP_MMAP_OFFSETS: i32 = 1;
const XDP_RX_RING: i32 = 2;
const XDP_TX_RING: i32 = 3;
const XDP_UMEM_REG: i32 = 4;
const XDP_UMEM_FILL_RING: i32 = 5;
const XDP_UMEM_COMPLETION_RING: i32 = 6;
const XDP_STATISTICS: i32 = 7;
const XDP_COPY: u16 = 1 << 1;
const XDP_ZEROCOPY: u16 = 1 << 2;
const XDP_USE_NEED_WAKEUP: u16 = 1 << 3;
const XDP_RING_NEED_WAKEUP: u32 = 1;
const XDP_PGOFF_RX_RING: i64 = 0;
const XDP_PGOFF_TX_RING: i64 = 0x8000_0000;
const XDP_UMEM_PGOFF_FILL_RING: i64 = 0x1_0000_0000;
const XDP_UMEM_PGOFF_COMPLETION_RING: i64 = 0x1_8000_0000;
const MSG_DONTWAIT: i32 = 0x40;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_POPULATE: i32 = 0x8000;
const MAP_HUGETLB: i32 = 0x40000;
const HUGE_PAGE: usize = 2 << 20;

/// Bytes of a frame of the UMEM: a full Ethernet frame of an MTU up to 1500 fits.
pub const FRAME_SIZE: u32 = 2048;
/// Frames of a socket's UMEM; half receive, half send.
pub const FRAMES: u32 = 4096;
/// Slots of each of the four rings.
const RING_SIZE: u32 = FRAMES / 2;

unsafe extern "C" {
    fn syscall(num: i64, ...) -> i64;
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const u8, len: u32) -> i32;
    fn getsockopt(fd: i32, level: i32, name: i32, value: *mut u8, len: *mut u32) -> i32;
    fn bind(fd: i32, addr: *const u8, len: u32) -> i32;
    fn sendto(fd: i32, buf: *const u8, len: usize, flags: i32, addr: *const u8, alen: u32)
    -> isize;
    fn recvfrom(
        fd: i32,
        buf: *mut u8,
        len: usize,
        flags: i32,
        addr: *mut u8,
        alen: *mut u32,
    ) -> isize;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn if_nametoindex(name: *const u8) -> u32;
}

fn cvt(rc: i64) -> io::Result<i64> {
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

fn bpf(cmd: i64, attr: &mut [u64; ATTR_WORDS]) -> io::Result<i64> {
    // SAFETY: `attr` is a zeroed `union bpf_attr` of `ATTR_WORDS * 8` bytes that lives for the
    // call; the pointers in it point to buffers the caller keeps alive for the call.
    cvt(unsafe { syscall(SYS_BPF, cmd, attr.as_mut_ptr(), (ATTR_WORDS * 8) as u32) })
}

/// Writes `v` at byte `at` of `attr` (the fields of `union bpf_attr` are little- or big-endian
/// as the CPU is: native order).
fn put32(attr: &mut [u64; ATTR_WORDS], at: usize, v: u32) {
    let word = &mut attr[at / 8];
    let mut b = word.to_ne_bytes();
    b[at % 8..at % 8 + 4].copy_from_slice(&v.to_ne_bytes());
    *word = u64::from_ne_bytes(b);
}

/// The index of interface `name`.
pub fn if_index(name: &str) -> io::Result<u32> {
    let c = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `c` is a NUL-terminated string that lives for the call.
    let i = unsafe { if_nametoindex(c.as_ptr().cast()) };
    if i == 0 { Err(io::Error::last_os_error()) } else { Ok(i) }
}

/// The receive queues of interface `name` (`/sys/class/net/<name>/queues/rx-*`).
pub fn rx_queues(name: &str) -> io::Result<u32> {
    let dir = std::fs::read_dir(format!("/sys/class/net/{name}/queues"))?;
    let n = dir
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("rx-"))
        .count();
    Ok(u32::try_from(n).unwrap_or(u32::MAX).max(1))
}

/// The MTU of interface `name`.
pub fn mtu(name: &str) -> io::Result<usize> {
    let s = std::fs::read_to_string(format!("/sys/class/net/{name}/mtu"))?;
    s.trim().parse().map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}

/// Whether the kernel has AF_XDP: a socket of the family opens.
pub fn check_support() -> Result<(), String> {
    // SAFETY: no pointers.
    let fd = unsafe { socket(AF_XDP, SOCK_RAW | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(format!("AF_XDP socket: {}", io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just opened and is owned here.
    drop(unsafe { OwnedFd::from_raw_fd(fd) });
    Ok(())
}

/// An eBPF instruction: opcode, registers, offset and immediate.
fn insn(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> u64 {
    let regs = (dst & 0x0f) | (src << 4);
    let mut b = [0u8; 8];
    b[0] = code;
    b[1] = regs;
    b[2..4].copy_from_slice(&off.to_ne_bytes());
    b[4..8].copy_from_slice(&imm.to_ne_bytes());
    u64::from_ne_bytes(b)
}

const LDX_W: u8 = 0x61;
const LDX_H: u8 = 0x69;
const LDX_B: u8 = 0x71;
const MOV_X: u8 = 0xbf;
const MOV_K: u8 = 0xb7;
const ADD_K: u8 = 0x07;
const AND_K: u8 = 0x57;
const JGT_X: u8 = 0x2d;
const JNE_K: u8 = 0x55;
const LD_IMM64: u8 = 0x18;
const CALL: u8 = 0x85;
const EXIT: u8 = 0x95;

/// A 16-bit field of the frame as the CPU reads it from memory.
fn wire16(v: u16) -> i32 {
    i32::from(u16::from_ne_bytes(v.to_be_bytes()))
}

/// The program for `port` (and `address`, if given) with the socket map `map`.
fn program(port: u16, address: Option<Ipv4Addr>, map: RawFd) -> Vec<u64> {
    // r1: the context. Frame bounds in r2 and r3; r4 the end of the headers it reads.
    let mut p = vec![
        insn(MOV_X, 6, 1, 0, 0),
        insn(LDX_W, 2, 1, 0, 0),
        insn(LDX_W, 3, 1, 4, 0),
        insn(MOV_X, 4, 2, 0, 0),
        insn(ADD_K, 4, 0, 0, 38),
    ];
    // Each check jumps to the end, which passes the frame on; offsets are patched below.
    let mut to_pass = Vec::new();
    let mut check = |p: &mut Vec<u64>, i: u64| {
        to_pass.push(p.len());
        p.push(i);
    };
    check(&mut p, insn(JGT_X, 4, 3, 0, 0));
    p.push(insn(LDX_H, 5, 2, 12, 0));
    check(&mut p, insn(JNE_K, 5, 0, 0, wire16(0x0800)));
    p.push(insn(LDX_B, 5, 2, 14, 0));
    check(&mut p, insn(JNE_K, 5, 0, 0, 0x45));
    p.push(insn(LDX_B, 5, 2, 23, 0));
    check(&mut p, insn(JNE_K, 5, 0, 0, 6));
    p.push(insn(LDX_H, 5, 2, 20, 0));
    p.push(insn(AND_K, 5, 0, 0, wire16(0x3fff)));
    check(&mut p, insn(JNE_K, 5, 0, 0, 0));
    p.push(insn(LDX_H, 5, 2, 36, 0));
    check(&mut p, insn(JNE_K, 5, 0, 0, wire16(port)));
    if let Some(a) = address {
        p.push(insn(LDX_W, 5, 2, 30, 0));
        check(&mut p, insn(JNE_K, 5, 0, 0, i32::from_ne_bytes(a.octets())));
    }
    p.push(insn(LDX_W, 2, 6, 16, 0));
    p.push(insn(LD_IMM64, 1, BPF_PSEUDO_MAP_FD, 0, map));
    p.push(insn(0, 0, 0, 0, 0));
    p.push(insn(MOV_K, 3, 0, 0, XDP_PASS));
    p.push(insn(CALL, 0, 0, 0, HELPER_REDIRECT_MAP));
    p.push(insn(EXIT, 0, 0, 0, 0));
    let pass = p.len();
    p.push(insn(MOV_K, 0, 0, 0, XDP_PASS));
    p.push(insn(EXIT, 0, 0, 0, 0));
    for at in to_pass {
        let mut b = p[at].to_ne_bytes();
        let off = (pass - at - 1) as i16;
        b[2..4].copy_from_slice(&off.to_ne_bytes());
        p[at] = u64::from_ne_bytes(b);
    }
    p
}

/// How the program runs on the interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    /// In the driver, before the kernel makes a socket buffer.
    Driver,
    /// In the kernel's generic path, for a driver without XDP.
    Generic,
}

/// The XDP program of a port with its socket map, attached to an interface while it lives.
#[derive(Debug)]
pub struct XdpProgram {
    map: OwnedFd,
    prog: OwnedFd,
    link: Option<OwnedFd>,
    mode: Option<AttachMode>,
}

impl XdpProgram {
    /// Loads the program for TCP `port` (of `address` only, if given) with a map of `queues`
    /// sockets. The verifier's report is the error of a refused program.
    pub fn load(port: u16, address: Option<Ipv4Addr>, queues: u32) -> io::Result<XdpProgram> {
        let mut attr = [0u64; ATTR_WORDS];
        put32(&mut attr, 0, BPF_MAP_TYPE_XSKMAP);
        put32(&mut attr, 4, 4);
        put32(&mut attr, 8, 4);
        put32(&mut attr, 12, queues.max(1));
        let fd = bpf(BPF_MAP_CREATE, &mut attr)?;
        // SAFETY: the kernel returned a new descriptor, owned here.
        let map = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let insns = program(port, address, map.as_raw_fd());
        let license = c"GPL";
        let mut log = vec![0u8; VERIFIER_LOG];
        let mut attr = [0u64; ATTR_WORDS];
        put32(&mut attr, 0, BPF_PROG_TYPE_XDP);
        put32(&mut attr, 4, insns.len() as u32);
        attr[1] = insns.as_ptr() as u64;
        attr[2] = license.as_ptr() as u64;
        put32(&mut attr, 24, 1);
        put32(&mut attr, 28, VERIFIER_LOG as u32);
        attr[4] = log.as_mut_ptr() as u64;
        put32(&mut attr, 68, BPF_XDP);
        match bpf(BPF_PROG_LOAD, &mut attr) {
            Ok(fd) => {
                // SAFETY: the kernel returned a new descriptor, owned here.
                let prog = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
                Ok(XdpProgram { map, prog, link: None, mode: None })
            }
            Err(e) => {
                let end = log.iter().position(|&b| b == 0).unwrap_or(log.len());
                let report = String::from_utf8_lossy(&log[..end]);
                Err(io::Error::new(e.kind(), format!("{e}: {}", report.trim())))
            }
        }
    }

    /// Attaches the program to interface `ifindex`: in the driver's mode, or in the generic one
    /// if the driver has no XDP.
    pub fn attach(&mut self, ifindex: u32) -> io::Result<AttachMode> {
        let mut last = None;
        for (mode, flags) in
            [(AttachMode::Driver, XDP_FLAGS_DRV_MODE), (AttachMode::Generic, XDP_FLAGS_SKB_MODE)]
        {
            let mut attr = [0u64; ATTR_WORDS];
            put32(&mut attr, 0, self.prog.as_raw_fd() as u32);
            put32(&mut attr, 4, ifindex);
            put32(&mut attr, 8, BPF_XDP);
            put32(&mut attr, 12, flags);
            match bpf(BPF_LINK_CREATE, &mut attr) {
                Ok(fd) => {
                    // SAFETY: the kernel returned a new descriptor, owned here.
                    self.link = Some(unsafe { OwnedFd::from_raw_fd(fd as RawFd) });
                    self.mode = Some(mode);
                    return Ok(mode);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::Unsupported)))
    }

    /// The mode it is attached in.
    pub fn mode(&self) -> Option<AttachMode> {
        self.mode
    }

    /// Sends the frames of `queue` to socket `xsk`.
    pub fn insert(&self, queue: u32, xsk: &Xsk) -> io::Result<()> {
        let key = queue;
        let value = xsk.fd.as_raw_fd() as u32;
        let mut attr = [0u64; ATTR_WORDS];
        put32(&mut attr, 0, self.map.as_raw_fd() as u32);
        attr[1] = ptr::from_ref(&key) as u64;
        attr[2] = ptr::from_ref(&value) as u64;
        bpf(BPF_MAP_UPDATE_ELEM, &mut attr).map(drop)
    }

    /// Gives the frames of `queue` back to the kernel.
    pub fn remove(&self, queue: u32) -> io::Result<()> {
        let key = queue;
        let mut attr = [0u64; ATTR_WORDS];
        put32(&mut attr, 0, self.map.as_raw_fd() as u32);
        attr[1] = ptr::from_ref(&key) as u64;
        bpf(BPF_MAP_DELETE_ELEM, &mut attr).map(drop)
    }
}

/// A mapping of memory, unmapped on drop.
#[derive(Debug)]
struct Mapping {
    at: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain memory; who reads or writes it is decided by its owner (`Xsk`,
// which is `Send` and not `Sync`).
unsafe impl Send for Mapping {}

impl Mapping {
    fn anonymous(len: usize) -> io::Result<(Mapping, bool)> {
        let flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE;
        if len.is_multiple_of(HUGE_PAGE) {
            // SAFETY: a new anonymous mapping; no pointer is given.
            let p = unsafe {
                mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, flags | MAP_HUGETLB, -1, 0)
            };
            if let Some(at) = NonNull::new(p).filter(|p| p.as_ptr() as isize != -1) {
                return Ok((Mapping { at, len }, true));
            }
        }
        // SAFETY: as above.
        let p = unsafe { mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, flags, -1, 0) };
        match NonNull::new(p).filter(|p| p.as_ptr() as isize != -1) {
            Some(at) => Ok((Mapping { at, len }, false)),
            None => Err(io::Error::last_os_error()),
        }
    }

    fn of_socket(fd: RawFd, offset: i64, len: usize) -> io::Result<Mapping> {
        // SAFETY: maps a ring of socket `fd`, which the caller keeps open while it is mapped.
        let p = unsafe {
            mmap(
                ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE,
                fd,
                offset,
            )
        };
        match NonNull::new(p).filter(|p| p.as_ptr() as isize != -1) {
            Some(at) => Ok(Mapping { at, len }),
            None => Err(io::Error::last_os_error()),
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `at..at+len` is this mapping, unmapped once.
        unsafe { munmap(self.at.as_ptr(), self.len) };
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RingOffsets {
    producer: u64,
    consumer: u64,
    desc: u64,
    flags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct MmapOffsets {
    rx: RingOffsets,
    tx: RingOffsets,
    fr: RingOffsets,
    cr: RingOffsets,
}

/// One of the four rings, mapped from the kernel.
#[derive(Debug)]
struct XskRing {
    /// The mapping the pointers below point into, unmapped when the ring goes.
    _map: Mapping,
    producer: NonNull<AtomicU32>,
    consumer: NonNull<AtomicU32>,
    /// `None` on a kernel without the ring's flags (before 5.4).
    flags: Option<NonNull<AtomicU32>>,
    desc: NonNull<u8>,
    mask: u32,
}

// SAFETY: the pointers point into the ring's own mapping, which moves with it; one thread at a
// time uses an `Xsk` (`Send`, not `Sync`), and the kernel's side of the ring is reached through
// atomics.
unsafe impl Send for XskRing {}

impl XskRing {
    fn map(
        fd: RawFd,
        page: i64,
        off: &RingOffsets,
        entry: usize,
        with_flags: bool,
    ) -> io::Result<XskRing> {
        let len = off.desc as usize + RING_SIZE as usize * entry;
        let map = Mapping::of_socket(fd, page, len)?;
        let at = |o: u64| {
            // SAFETY: `o` is an offset the kernel gave inside this mapping.
            NonNull::new(unsafe { map.at.as_ptr().add(o as usize) })
                .ok_or(io::ErrorKind::InvalidData)
        };
        let producer = at(off.producer)?.cast();
        let consumer = at(off.consumer)?.cast();
        let flags = if with_flags { Some(at(off.flags)?.cast()) } else { None };
        let desc = at(off.desc)?;
        Ok(XskRing { producer, consumer, flags, desc, mask: RING_SIZE - 1, _map: map })
    }

    fn producer(&self) -> &AtomicU32 {
        // SAFETY: the index lives in the mapping, aligned by the kernel, while `self` does.
        unsafe { self.producer.as_ref() }
    }

    fn consumer(&self) -> &AtomicU32 {
        // SAFETY: as `producer`.
        unsafe { self.consumer.as_ref() }
    }

    fn needs_wakeup(&self) -> bool {
        self.flags.is_none_or(|f| {
            // SAFETY: as `producer`.
            unsafe { f.as_ref() }.load(Ordering::Acquire) & XDP_RING_NEED_WAKEUP != 0
        })
    }

    fn slot(&self, i: u32, entry: usize) -> *mut u8 {
        // SAFETY: `(i & mask) * entry` is inside the ring's descriptors.
        unsafe { self.desc.as_ptr().add((i & self.mask) as usize * entry) }
    }
}

/// What the kernel counted on a socket (`XDP_STATISTICS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct XskStats {
    /// Frames dropped for lack of a frame in the fill ring and others.
    pub rx_dropped: u64,
    /// Receive descriptors that were not valid.
    pub rx_invalid_descs: u64,
    /// Transmit descriptors that were not valid.
    pub tx_invalid_descs: u64,
    /// Frames dropped with the receive ring full.
    pub rx_ring_full: u64,
    /// Times the fill ring was empty.
    pub rx_fill_ring_empty_descs: u64,
    /// Times the transmit ring was empty.
    pub tx_ring_empty_descs: u64,
}

/// An AF_XDP socket of one queue of an interface, with its UMEM.
#[derive(Debug)]
pub struct Xsk {
    rx: XskRing,
    tx: XskRing,
    fill: XskRing,
    comp: XskRing,
    fd: OwnedFd,
    umem: Mapping,
    huge: bool,
    zerocopy: bool,
    /// Frames free to send from.
    free_tx: Vec<u64>,
    /// Transmit descriptors written and not published yet.
    pending_tx: u32,
}

const DESC: usize = 16;
const ADDR: usize = 8;

impl AsRawFd for Xsk {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Xsk {
    /// A socket on queue `queue` of interface `ifindex`.
    pub fn open(ifindex: u32, queue: u32) -> io::Result<Xsk> {
        let mut last = None;
        for flags in [XDP_ZEROCOPY | XDP_USE_NEED_WAKEUP, XDP_COPY | XDP_USE_NEED_WAKEUP, XDP_COPY]
        {
            match Xsk::open_with(ifindex, queue, flags) {
                Ok(x) => return Ok(x),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::Unsupported)))
    }

    fn open_with(ifindex: u32, queue: u32, bind_flags: u16) -> io::Result<Xsk> {
        // SAFETY: no pointers.
        let raw = unsafe { socket(AF_XDP, SOCK_RAW | SOCK_CLOEXEC, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new descriptor, owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let len = (FRAMES * FRAME_SIZE) as usize;
        let (umem, huge) = Mapping::anonymous(len)?;
        // `struct xdp_umem_reg` up to `headroom` (Linux 4.18).
        let mut reg = [0u8; 24];
        reg[0..8].copy_from_slice(&(umem.at.as_ptr() as u64).to_ne_bytes());
        reg[8..16].copy_from_slice(&(len as u64).to_ne_bytes());
        reg[16..20].copy_from_slice(&FRAME_SIZE.to_ne_bytes());
        let opt = |name: i32, value: &[u8]| {
            // SAFETY: `value` lives for the call and its length is given.
            let rc = unsafe { setsockopt(raw, SOL_XDP, name, value.as_ptr(), value.len() as u32) };
            if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
        };
        opt(XDP_UMEM_REG, &reg)?;
        let size = RING_SIZE.to_ne_bytes();
        opt(XDP_UMEM_FILL_RING, &size)?;
        opt(XDP_UMEM_COMPLETION_RING, &size)?;
        opt(XDP_RX_RING, &size)?;
        opt(XDP_TX_RING, &size)?;
        let mut off = MmapOffsets::default();
        let mut off_len = size_of::<MmapOffsets>() as u32;
        // SAFETY: `off` is writable for `off_len` bytes, and `off_len` for the kernel's answer.
        let rc = unsafe {
            getsockopt(raw, SOL_XDP, XDP_MMAP_OFFSETS, ptr::from_mut(&mut off).cast(), &mut off_len)
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        let with_flags = off_len as usize == size_of::<MmapOffsets>();
        if !with_flags {
            // Before 5.4 the offsets have no flags: three words per ring.
            let w: [u64; 16] = std::array::from_fn(|i| match i {
                0..=3 => [off.rx.producer, off.rx.consumer, off.rx.desc, off.rx.flags][i],
                4..=7 => [off.tx.producer, off.tx.consumer, off.tx.desc, off.tx.flags][i - 4],
                8..=11 => [off.fr.producer, off.fr.consumer, off.fr.desc, off.fr.flags][i - 8],
                _ => [off.cr.producer, off.cr.consumer, off.cr.desc, off.cr.flags][i - 12],
            });
            let ring = |k: usize| RingOffsets {
                producer: w[3 * k],
                consumer: w[3 * k + 1],
                desc: w[3 * k + 2],
                flags: 0,
            };
            off = MmapOffsets { rx: ring(0), tx: ring(1), fr: ring(2), cr: ring(3) };
        }
        let rx = XskRing::map(raw, XDP_PGOFF_RX_RING, &off.rx, DESC, with_flags)?;
        let tx = XskRing::map(raw, XDP_PGOFF_TX_RING, &off.tx, DESC, with_flags)?;
        let fill = XskRing::map(raw, XDP_UMEM_PGOFF_FILL_RING, &off.fr, ADDR, with_flags)?;
        let comp = XskRing::map(raw, XDP_UMEM_PGOFF_COMPLETION_RING, &off.cr, ADDR, with_flags)?;
        let mut x = Xsk {
            rx,
            tx,
            fill,
            comp,
            fd,
            umem,
            huge,
            zerocopy: bind_flags & XDP_ZEROCOPY != 0,
            free_tx: (RING_SIZE..FRAMES).map(|i| u64::from(i * FRAME_SIZE)).collect(),
            pending_tx: 0,
        };
        // The receiving half of the frames goes to the fill ring before the bind.
        let addrs: Vec<u64> = (0..RING_SIZE).map(|i| u64::from(i * FRAME_SIZE)).collect();
        x.refill(&addrs);
        let mut sa = [0u8; 16];
        sa[0..2].copy_from_slice(&(AF_XDP as u16).to_ne_bytes());
        sa[2..4].copy_from_slice(&bind_flags.to_ne_bytes());
        sa[4..8].copy_from_slice(&ifindex.to_ne_bytes());
        sa[8..12].copy_from_slice(&queue.to_ne_bytes());
        // SAFETY: `sa` is a `struct sockaddr_xdp` that lives for the call.
        if unsafe { bind(raw, sa.as_ptr(), sa.len() as u32) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(x)
    }

    /// The socket's descriptor, to wait on its frames with `epoll`.
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Whether the driver moves the frames without copying them.
    pub fn zerocopy(&self) -> bool {
        self.zerocopy
    }

    /// Whether the UMEM is on 2 MiB pages.
    pub fn on_huge_pages(&self) -> bool {
        self.huge
    }

    /// Gives frames `addrs` to the kernel to receive into.
    fn refill(&mut self, addrs: &[u64]) {
        let ring = &self.fill;
        let prod = ring.producer().load(Ordering::Relaxed);
        for (i, a) in addrs.iter().enumerate() {
            let slot = ring.slot(prod.wrapping_add(i as u32), ADDR).cast::<u64>();
            // SAFETY: a slot of the fill ring this side owns until the producer index moves past
            // it; the ring has room for every receiving frame, which are never more than its size.
            unsafe { slot.write(*a) };
        }
        ring.producer().store(prod.wrapping_add(addrs.len() as u32), Ordering::Release);
    }

    /// Hands every frame that arrived to `f` and gives its frame back to the kernel; how many.
    pub fn receive(&mut self, f: &mut dyn FnMut(&[u8])) -> usize {
        let cons = self.rx.consumer().load(Ordering::Relaxed);
        let prod = self.rx.producer().load(Ordering::Acquire);
        let n = prod.wrapping_sub(cons);
        let mut done = Vec::with_capacity(n as usize);
        for i in 0..n {
            let slot = self.rx.slot(cons.wrapping_add(i), DESC);
            // SAFETY: the kernel published this descriptor (Acquire above): `addr` and `len`
            // name bytes of a frame of the UMEM that the kernel no longer writes.
            let (addr, len) =
                unsafe { (slot.cast::<u64>().read(), slot.add(8).cast::<u32>().read()) };
            let base = addr & !u64::from(FRAME_SIZE - 1);
            let end = (addr + u64::from(len)) as usize;
            if end <= self.umem.len {
                // SAFETY: `addr..addr+len` is inside the UMEM, checked above.
                let frame = unsafe {
                    std::slice::from_raw_parts(
                        self.umem.at.as_ptr().add(addr as usize),
                        len as usize,
                    )
                };
                f(frame);
            }
            done.push(base);
        }
        self.rx.consumer().store(prod, Ordering::Release);
        if !done.is_empty() {
            self.refill(&done);
        }
        n as usize
    }

    /// Queues `frame` to send; `false` without a free frame or room in the ring (the next
    /// [`Xsk::flush`] frees them).
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if frame.len() > FRAME_SIZE as usize {
            return true;
        }
        let cons = self.tx.consumer().load(Ordering::Acquire);
        let prod = self.tx.producer().load(Ordering::Relaxed).wrapping_add(self.pending_tx);
        if prod.wrapping_sub(cons) >= RING_SIZE {
            return false;
        }
        let Some(addr) = self.free_tx.pop() else { return false };
        // SAFETY: `addr` is a sending frame this side owns (no descriptor names it), and the
        // frame fits in it.
        unsafe {
            ptr::copy_nonoverlapping(
                frame.as_ptr(),
                self.umem.at.as_ptr().add(addr as usize),
                frame.len(),
            );
            let slot = self.tx.slot(prod, DESC);
            slot.cast::<u64>().write(addr);
            slot.add(8).cast::<u32>().write(frame.len() as u32);
            slot.add(12).cast::<u32>().write(0);
        }
        self.pending_tx += 1;
        true
    }

    /// Publishes the frames queued to send, wakes the kernel to send them if it asks, and takes
    /// back the frames it sent.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.pending_tx > 0 {
            let prod = self.tx.producer().load(Ordering::Relaxed);
            self.tx.producer().store(prod.wrapping_add(self.pending_tx), Ordering::Release);
            self.pending_tx = 0;
        }
        let in_flight = FRAMES - RING_SIZE - self.free_tx.len() as u32;
        if in_flight > 0 && self.tx.needs_wakeup() {
            // SAFETY: no buffer: the call only tells the kernel to send.
            let rc = unsafe { sendto(self.fd(), ptr::null(), 0, MSG_DONTWAIT, ptr::null(), 0) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                // Busy or out of buffers: the frames go at the next wake-up.
                if !matches!(e.raw_os_error(), Some(11 | 16 | 105)) {
                    return Err(e);
                }
            }
        }
        let cons = self.comp.consumer().load(Ordering::Relaxed);
        let prod = self.comp.producer().load(Ordering::Acquire);
        for i in 0..prod.wrapping_sub(cons) {
            let slot = self.comp.slot(cons.wrapping_add(i), ADDR).cast::<u64>();
            // SAFETY: a completion the kernel published (Acquire above).
            let addr = unsafe { slot.read() };
            self.free_tx.push(addr & !u64::from(FRAME_SIZE - 1));
        }
        self.comp.consumer().store(prod, Ordering::Release);
        if self.fill.needs_wakeup() && self.fill.flags.is_some() {
            // SAFETY: no buffer: the call only tells the driver to take the fill ring.
            unsafe {
                recvfrom(
                    self.fd(),
                    ptr::null_mut(),
                    0,
                    MSG_DONTWAIT,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
        }
        Ok(())
    }

    /// Frames being sent.
    pub fn sending(&self) -> usize {
        (FRAMES - RING_SIZE) as usize - self.free_tx.len()
    }

    /// The kernel's counters of the socket.
    pub fn stats(&self) -> io::Result<XskStats> {
        let mut s = XskStats::default();
        let mut len = size_of::<XskStats>() as u32;
        // SAFETY: `s` is writable for `len` bytes, and `len` for the kernel's answer.
        let rc = unsafe {
            getsockopt(self.fd(), SOL_XDP, XDP_STATISTICS, ptr::from_mut(&mut s).cast(), &mut len)
        };
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(s) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_jump_of_the_program_lands_on_its_end_and_the_map_is_loaded_once() {
        let p = program(27017, Some(Ipv4Addr::new(10, 0, 0, 1)), 7);
        let pass = p.len() - 2;
        let mut jumps = 0;
        for (i, w) in p.iter().enumerate() {
            let b = w.to_ne_bytes();
            if b[0] == JGT_X || b[0] == JNE_K {
                let off = i16::from_ne_bytes([b[2], b[3]]);
                assert_eq!(i + 1 + off as usize, pass);
                jumps += 1;
            }
        }
        assert_eq!(jumps, 7);
        let loads = p.iter().filter(|w| w.to_ne_bytes()[0] == LD_IMM64).count();
        assert_eq!(loads, 1);
        assert_eq!(program(27017, None, 7).len(), p.len() - 2);
        assert_eq!(wire16(0x0800).to_ne_bytes()[..2], [0x08, 0x00]);
    }
}
