//! The kernel structures of the ring's network operations (012 FR-024, FR-026): provided buffer
//! rings (`IORING_REGISTER_PBUF_RING`), the `msghdr` of a `sendmsg`, and what a send hands the
//! kernel.
//!
//! A provided buffer ring is an array of `struct io_uring_buf` the process fills and the kernel
//! takes from: each multishot `recv` completion names the buffer it filled, which then belongs to
//! the process until it puts it back at the ring's tail. [`BufRing`] keeps which buffers the
//! process holds, so a buffer the kernel may write is never read.

use std::io;
use std::sync::atomic::{AtomicU16, Ordering};

use crate::aligned::AlignedBuf;

/// Bytes a send hands the kernel. The ring owns them until the operation's last completion, and
/// the slice `as_ref` gives must point into the value itself or into memory nobody changes while
/// the value lives (a `Vec<u8>`, a `Box<[u8]>`, an `Arc<[u8]>`, a `&'static [u8]`).
pub struct Part(Box<dyn AsRef<[u8]> + Send>);

impl Part {
    /// The bytes of `data`.
    pub fn new(data: impl AsRef<[u8]> + Send + 'static) -> Part {
        Part(Box::new(data))
    }

    /// The bytes.
    pub fn bytes(&self) -> &[u8] {
        (*self.0).as_ref()
    }
}

impl std::fmt::Debug for Part {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Part({} bytes)", self.bytes().len())
    }
}

/// Where an [`super::Op::SendZc`] sends from.
pub enum SendData {
    /// Bytes the ring keeps until the notification.
    Owned(Part),
    /// `len` bytes of registered buffer `buf` from byte `at`, taken until the notification.
    Fixed {
        /// Registered buffer.
        buf: u16,
        /// First byte.
        at: u32,
        /// Bytes.
        len: u32,
    },
}

impl std::fmt::Debug for SendData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendData::Owned(p) => write!(f, "Owned({} bytes)", p.bytes().len()),
            SendData::Fixed { buf, at, len } => write!(f, "Fixed({buf}, {at}, {len})"),
        }
    }
}

/// `struct user_msghdr` of the kernel on 64-bit targets, with pointers as integers.
#[repr(C)]
#[derive(Debug)]
pub(super) struct MsgHdr {
    name: u64,
    namelen: u32,
    _pad1: u32,
    iov: u64,
    iovlen: u64,
    control: u64,
    controllen: u64,
    flags: u32,
    _pad2: u32,
}

impl MsgHdr {
    /// A header of the `iovec`s `iov` (address and length each), with no address or control data.
    pub(super) fn of(iov: &[[u64; 2]]) -> MsgHdr {
        MsgHdr {
            name: 0,
            namelen: 0,
            _pad1: 0,
            iov: iov.as_ptr() as u64,
            iovlen: iov.len() as u64,
            control: 0,
            controllen: 0,
            flags: 0,
            _pad2: 0,
        }
    }
}

/// `struct io_uring_buf_reg`.
#[repr(C)]
pub(super) struct BufReg {
    ring_addr: u64,
    ring_entries: u32,
    bgid: u16,
    flags: u16,
    resv: [u64; 3],
}

/// Bytes of a `struct io_uring_buf`: address, length, identifier, and 2 bytes that in the first
/// entry hold the ring's tail.
const BUF_ENTRY: usize = 16;
/// Offset of the tail inside the first entry.
const TAIL_AT: usize = 14;
/// Most entries the kernel takes in one ring.
pub const MAX_BUF_RING_ENTRIES: u16 = 1 << 15;

/// A provided buffer ring: the entries the kernel reads and the buffers they point to.
pub(super) struct BufRing {
    /// The entries, never borrowed as a slice: the kernel reads them at any time.
    entries: AlignedBuf,
    /// The buffers, one after the other, never borrowed whole: the kernel writes those it holds.
    data: AlignedBuf,
    count: u16,
    buf_len: u32,
    /// The tail the process publishes.
    tail: u16,
    /// Bytes the kernel left in each buffer the process holds; `None`: the kernel has it.
    held: Vec<Option<u32>>,
}

impl BufRing {
    /// `count` buffers of `buf_len` bytes, all in the ring.
    pub(super) fn new(count: u16, buf_len: u32) -> io::Result<BufRing> {
        if !count.is_power_of_two() || count > MAX_BUF_RING_ENTRIES || buf_len == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut ring = BufRing {
            entries: AlignedBuf::new(usize::from(count) * BUF_ENTRY),
            data: AlignedBuf::new(usize::from(count) * buf_len as usize),
            count,
            buf_len,
            tail: 0,
            held: vec![Some(0); usize::from(count)],
        };
        for bid in 0..count {
            ring.give_back(bid);
        }
        Ok(ring)
    }

    /// What `IORING_REGISTER_PBUF_RING` takes to register this ring as group `group`.
    pub(super) fn registration(&self, group: u16) -> BufReg {
        BufReg {
            ring_addr: self.entries.addr(),
            ring_entries: u32::from(self.count),
            bgid: group,
            flags: 0,
            resv: [0; 3],
        }
    }

    /// A completion handed buffer `bid` with `len` bytes to the process.
    pub(super) fn hand_out(&mut self, bid: u16, len: u32) {
        if let Some(h) = self.held.get_mut(usize::from(bid)) {
            *h = Some(len.min(self.buf_len));
        }
    }

    /// The bytes of buffer `bid`, if the process holds it.
    pub(super) fn held(&self, bid: u16) -> Option<&[u8]> {
        let len = (*self.held.get(usize::from(bid))?)?;
        let at = usize::from(bid) * self.buf_len as usize;
        // SAFETY: the process holds buffer `bid`: the kernel handed it over in a completion and
        // writes it again only after `give_back`, which takes `&mut self` and so cannot run while
        // this borrow lives. `at + len` lies inside `data` (`len <= buf_len`).
        Some(unsafe {
            std::slice::from_raw_parts((self.data.addr() as *const u8).add(at), len as usize)
        })
    }

    /// Buffers the process holds.
    pub(super) fn held_count(&self) -> usize {
        self.held.iter().filter(|h| h.is_some()).count()
    }

    /// Puts buffer `bid` back in the ring; false if the process did not hold it.
    pub(super) fn give_back(&mut self, bid: u16) -> bool {
        match self.held.get_mut(usize::from(bid)) {
            Some(h @ Some(_)) => *h = None,
            _ => return false,
        }
        let slot = usize::from(self.tail & (self.count - 1)) * BUF_ENTRY;
        let addr = self.data.addr() + u64::from(bid) * u64::from(self.buf_len);
        let base = self.entries.addr() as *mut u8;
        // SAFETY: entry `slot` lies inside `entries`; it is past the tail the kernel sees, so the
        // kernel does not read it until the store below publishes it. Only the address, length and
        // identifier are written, never the tail's 2 bytes of the first entry.
        unsafe {
            base.add(slot).cast::<u64>().write_volatile(addr);
            base.add(slot + 8).cast::<u32>().write_volatile(self.buf_len);
            base.add(slot + 12).cast::<u16>().write_volatile(bid);
        }
        self.tail = self.tail.wrapping_add(1);
        // SAFETY: the tail is the aligned `u16` at byte 14 of the first entry, shared with the
        // kernel; the release store publishes the entry written above.
        unsafe { (*base.add(TAIL_AT).cast::<AtomicU16>()).store(self.tail, Ordering::Release) };
        true
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests;
