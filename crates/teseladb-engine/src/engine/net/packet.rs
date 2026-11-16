//! The zero-copy network packet of the native stack.
//!
//! A [`Packet`] is a list of [`Fragment`]s (pointer and length) plus one [`Deleter`] that
//! releases all the memory the fragments point into. Packets are built without copying from
//! device buffers or [`TemporaryBuffer`]s, combined by appending or prepending, shared
//! (reference-counted through the deleter, not copy-on-write) and trimmed at either end.
//! Small headers are prepended into an internal headroom area in the packet's own
//! allocation, so adding protocol headers does not allocate.
//!
//! The packet's metadata, its fragment array and the headroom live in one heap allocation;
//! a moved-from or [`Packet::make_null_packet`] packet holds none, and every operation other
//! than [`Packet::reset`] and [`Packet::is_valid`] requires a valid packet. A packet is
//! owned by one shard; [`Packet::free_on_cpu`] makes the memory go back to the shard that
//! allocated it when the packet is sent from another one. Packets are on every per-packet
//! hot path: most accessors are `#[inline]` and allocation-free.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

use crate::engine::core::deleter::{Deleter, make_deleter_with, make_free_deleter};
use crate::engine::core::smp::Smp;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::const_::IpProtocolNum;
use crate::engine::net::node_ptr::CrossShard;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
}

/// Asserts an internal invariant of the packet code.
macro_rules! engine_assert {
    ($e:expr) => {
        assert!($e)
    };
}

/// One contiguous piece of a packet's data.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Fragment {
    /// First byte of the fragment.
    pub base: *mut u8,
    /// Length of the fragment, in bytes.
    pub size: usize,
}

impl Fragment {
    /// The bytes of the fragment.
    ///
    /// # Safety
    /// `base` must point to `size` readable bytes that outlive the returned slice.
    pub unsafe fn as_slice<'a>(&self) -> &'a [u8] {
        if self.size == 0 {
            return &[];
        }
        // SAFETY: guaranteed by the caller.
        unsafe { std::slice::from_raw_parts(self.base, self.size) }
    }
}

/// Offload metadata carried with a packet between the protocol layers and the device.
#[derive(Clone, Copy, Debug)]
pub struct OffloadInfo {
    /// The L4 protocol of the packet.
    pub protocol: IpProtocolNum,
    /// Whether the device must compute the L4 checksum.
    pub needs_csum: bool,
    /// IP header length, in bytes.
    pub ip_hdr_len: u8,
    /// TCP header length, in bytes.
    pub tcp_hdr_len: u8,
    /// UDP header length, in bytes.
    pub udp_hdr_len: u8,
    /// Whether the device must compute the IPv4 header checksum.
    pub needs_ip_csum: bool,
    /// Whether the packet was reassembled from several received segments or fragments.
    pub reassembled: bool,
    /// Segment size for TCP segmentation offload, 0 when not segmenting.
    pub tso_seg_size: u16,
    /// VLAN tag stripped by the hardware, in host byte order.
    pub vlan_tci: Option<u16>,
}

impl Default for OffloadInfo {
    fn default() -> Self {
        OffloadInfo {
            protocol: IpProtocolNum::unused,
            needs_csum: false,
            ip_hdr_len: 20,
            tcp_hdr_len: 20,
            udp_hdr_len: 8,
            needs_ip_csum: false,
            reassembled: false,
            tso_seg_size: 0,
            vlan_tci: None,
        }
    }
}

// enough for lots of headers, not quite two cache lines:
const INTERNAL_DATA_SIZE: usize = 128 - 16;
const DEFAULT_NR_FRAGS: usize = 4;

/// A read-only view of the fragments of a packet, borrowed from it.
///
/// The view lives no longer than the packet borrow and gives shared access only (mutable
/// access to a fragment is [`Packet::frag_mut`]).
#[derive(Clone, Copy)]
pub struct PseudoVector<'a> {
    _start: *const Fragment,
    _finish: *const Fragment,
    _packet: PhantomData<&'a [Fragment]>,
}

impl<'a> PseudoVector<'a> {
    /// # Safety
    /// `start` points to `nr` initialized fragments that stay valid and unchanged for `'a`.
    #[inline]
    unsafe fn new(start: *const Fragment, nr: usize) -> Self {
        // SAFETY: guaranteed by the caller.
        PseudoVector { _start: start, _finish: unsafe { start.add(nr) }, _packet: PhantomData }
    }
    /// Pointer to the first fragment.
    #[inline]
    pub fn begin(&self) -> *const Fragment {
        self._start
    }
    /// Pointer one past the last fragment.
    #[inline]
    pub fn end(&self) -> *const Fragment {
        self._finish
    }
    /// Number of fragments.
    #[inline]
    pub fn len(&self) -> usize {
        // SAFETY: both pointers are in the same array.
        unsafe { self._finish.offset_from(self._start) as usize }
    }
    /// Whether the packet has no fragments.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self._start == self._finish
    }
    /// The fragments as a slice.
    #[inline]
    pub fn as_slice(&self) -> &'a [Fragment] {
        // SAFETY: [_start, _finish) are initialized fragments of the packet, borrowed for 'a.
        unsafe { std::slice::from_raw_parts(self._start, self.len()) }
    }
    /// Iterates over the fragments.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'a, Fragment> {
        self.as_slice().iter()
    }
}

impl std::ops::Index<usize> for PseudoVector<'_> {
    type Output = Fragment;
    #[inline]
    fn index(&self, idx: usize) -> &Fragment {
        &self.as_slice()[idx]
    }
}

/// A heap buffer of `n` bytes, left uninitialized. It is owned
/// through the raw pointer (freed by [`delete_char_array`]) rather than a `Box`, because
/// the fragment that points into it must stay valid while the owner moves into a deleter.
#[inline]
fn new_char_array(n: usize) -> *mut [MaybeUninit<u8>] {
    Box::into_raw(Box::<[u8]>::new_uninit_slice(n))
}

/// A deleter that frees `buf` (from [`new_char_array`]) and then runs `next`.
#[inline]
fn delete_char_array(next: Deleter, buf: *mut [MaybeUninit<u8>]) -> Deleter {
    make_deleter_with(next, move || {
        // SAFETY: `buf` came from `new_char_array` and this deleter, which runs once, owns it.
        drop(unsafe { Box::from_raw(buf) });
    })
}

#[repr(C)]
struct Impl {
    // when destroyed, virtual destructor will reclaim resources
    _deleter: Deleter,
    _len: u32,
    _nr_frags: u16,
    _allocated_frags: u16,
    _offload_info: OffloadInfo,
    _rss_hash: Option<u32>,
    // Left uninitialized: only the bytes a fragment covers are ever written and read.
    _data: [MaybeUninit<u8>; INTERNAL_DATA_SIZE], // only _frags[0] may use
    _headroom: u32,                               // in _data
    // FIXME: share _data/_frags space
    _frags: [Fragment; 0],
}

/// Owner of a packet's single allocation: the [`Impl`] header followed by its fragment
/// array (`_frags`, a trailing array of `_allocated_frags` entries).
struct ImplPtr(NonNull<Impl>);

impl ImplPtr {
    fn layout(nr_frags: usize) -> Layout {
        let size = std::mem::size_of::<Impl>() + nr_frags * std::mem::size_of::<Fragment>();
        Layout::from_size_align(size, std::mem::align_of::<Impl>()).expect("packet impl layout")
    }

    /// Allocates an impl with room for `nr_frags` fragments and no data.
    fn new_impl(nr_frags: usize) -> ImplPtr {
        engine_assert!(nr_frags == nr_frags as u16 as usize);
        let layout = Self::layout(nr_frags);
        // SAFETY: the layout has a non-zero size.
        let p = unsafe { alloc(layout) } as *mut Impl;
        let Some(nn) = NonNull::new(p) else { handle_alloc_error(layout) };
        // SAFETY: `p` is a fresh allocation large enough for an Impl. Every field but `_data`
        // (`MaybeUninit`, left uninitialized) is written; the trailing fragments are written
        // before being read (`_nr_frags` is 0).
        unsafe {
            ptr::addr_of_mut!((*p)._deleter).write(Deleter::default());
            ptr::addr_of_mut!((*p)._len).write(0);
            ptr::addr_of_mut!((*p)._nr_frags).write(0);
            ptr::addr_of_mut!((*p)._allocated_frags).write(nr_frags as u16);
            ptr::addr_of_mut!((*p)._offload_info).write(OffloadInfo::default());
            ptr::addr_of_mut!((*p)._rss_hash).write(None);
            ptr::addr_of_mut!((*p)._headroom).write(INTERNAL_DATA_SIZE as u32);
        }
        ImplPtr(nn)
    }

    /// Allocates an impl with room for `nr_frags` fragments holding a copy of `frag`
    /// (in the internal data area when it fits, otherwise in a separate heap buffer).
    fn new_impl_with_frag(frag: Fragment, nr_frags: usize) -> ImplPtr {
        let mut n = Self::new_impl(nr_frags);
        n._len = frag.size as u32;
        engine_assert!(n._allocated_frags > n._nr_frags);
        if frag.size <= INTERNAL_DATA_SIZE {
            n._headroom -= frag.size as u32;
            let base = n.data_ptr().wrapping_add(n._headroom as usize);
            *n.frag_mut(0) = Fragment { base, size: frag.size };
        } else {
            // SAFETY: plain malloc of frag.size bytes.
            let buf = unsafe { malloc(frag.size) } as *mut u8;
            if buf.is_null() {
                handle_alloc_error(Layout::array::<u8>(frag.size).unwrap());
            }
            let d = make_free_deleter(buf as *mut c_void);
            *n.frag_mut(0) = Fragment { base: buf, size: frag.size };
            n._deleter.append(d);
        }
        // SAFETY: frag.base has frag.size readable bytes, and _frags[0].base has room for them.
        unsafe { ptr::copy(frag.base, n.frag(0).base, frag.size) };
        n._nr_frags += 1;
        n
    }

    /// `_data`, projected from the impl's root pointer without forming a reference: the
    /// fragment that points into it must keep the root's provenance, which a temporary
    /// `&mut` to the array (or to the impl) would cut off at the next access to the impl.
    #[inline]
    fn data_ptr(&self) -> *mut u8 {
        // SAFETY: the Impl is alive; `addr_of_mut!` creates no reference.
        unsafe { ptr::addr_of_mut!((*self.0.as_ptr())._data).cast::<u8>() }
    }

    fn frags_ptr(&self) -> *mut Fragment {
        // SAFETY: the Impl is alive; `_frags` is the start of the trailing array.
        unsafe { ptr::addr_of_mut!((*self.0.as_ptr())._frags) as *mut Fragment }
    }

    fn frag(&self, i: usize) -> Fragment {
        debug_assert!(i < self._allocated_frags as usize);
        // SAFETY: `i` is within the allocated trailing array.
        unsafe { *self.frags_ptr().add(i) }
    }

    fn frag_mut(&mut self, i: usize) -> &mut Fragment {
        debug_assert!(i < self._allocated_frags as usize);
        // SAFETY: `i` is within the allocated trailing array.
        unsafe { &mut *self.frags_ptr().add(i) }
    }

    fn fragments(&self) -> PseudoVector<'_> {
        // SAFETY: the impl holds `_nr_frags` initialized fragments, which do not change while
        // it is borrowed.
        unsafe { PseudoVector::new(self.frags_ptr(), self._nr_frags as usize) }
    }

    fn allocate(nr_frags: usize) -> ImplPtr {
        let nr_frags = std::cmp::max(nr_frags, DEFAULT_NR_FRAGS);
        Self::new_impl(nr_frags)
    }

    fn copy_n(old: &mut ImplPtr, nr: usize) -> ImplPtr {
        let mut n = Self::allocate(nr);
        n._deleter = std::mem::take(&mut old._deleter);
        n._len = old._len;
        n._nr_frags = old._nr_frags;
        n._headroom = old._headroom;
        n._offload_info = old._offload_info;
        n._rss_hash = old._rss_hash;
        // SAFETY: both arrays hold at least old._nr_frags fragments.
        unsafe { ptr::copy_nonoverlapping(old.frags_ptr(), n.frags_ptr(), old._nr_frags as usize) };
        old.copy_internal_fragment_to(&mut n);
        n
    }

    fn copy(old: &mut ImplPtr) -> ImplPtr {
        let nr = old._nr_frags as usize;
        Self::copy_n(old, nr)
    }

    fn allocate_if_needed(mut old: ImplPtr, extra_frags: usize) -> ImplPtr {
        if old._allocated_frags as usize >= old._nr_frags as usize + extra_frags {
            return old;
        }
        let nr = std::cmp::max(old._nr_frags as usize + extra_frags, 2 * old._nr_frags as usize);
        Self::copy_n(&mut old, nr)
    }

    fn using_internal_data(&self) -> bool {
        if self._nr_frags == 0 {
            return false;
        }
        let data = self.data_ptr() as *const u8;
        let base = self.frag(0).base as *const u8;
        base >= data && base < data.wrapping_add(INTERNAL_DATA_SIZE)
    }

    fn unuse_internal_data(&mut self) {
        if !self.using_internal_data() {
            return;
        }
        let f0 = self.frag(0);
        // SAFETY: plain malloc.
        let buf = unsafe { malloc(f0.size) } as *mut u8;
        if buf.is_null() {
            // allocation failure
            handle_alloc_error(Layout::array::<u8>(f0.size.max(1)).unwrap());
        }
        let mut d = make_free_deleter(buf as *mut c_void);
        // SAFETY: f0 has f0.size bytes in _data; buf has room for them.
        unsafe { ptr::copy_nonoverlapping(f0.base, buf, f0.size) };
        self.frag_mut(0).base = buf;
        d.append(std::mem::take(&mut self._deleter));
        self._deleter = d;
        self._headroom = INTERNAL_DATA_SIZE as u32;
    }

    fn copy_internal_fragment_to(&mut self, to: &mut ImplPtr) {
        if !self.using_internal_data() {
            return;
        }
        let base = to.data_ptr().wrapping_add(self._headroom as usize);
        to.frag_mut(0).base = base;
        let f0 = self.frag(0);
        // SAFETY: both buffers are internal data areas of INTERNAL_DATA_SIZE bytes.
        unsafe { ptr::copy_nonoverlapping(f0.base, base, f0.size) };
    }
}

impl std::ops::Deref for ImplPtr {
    type Target = Impl;
    fn deref(&self) -> &Impl {
        // SAFETY: the pointer is owned and valid.
        unsafe { self.0.as_ref() }
    }
}

impl std::ops::DerefMut for ImplPtr {
    fn deref_mut(&mut self) -> &mut Impl {
        // SAFETY: the pointer is owned and valid.
        unsafe { self.0.as_mut() }
    }
}

impl Drop for ImplPtr {
    fn drop(&mut self) {
        let layout = Self::layout(self._allocated_frags as usize);
        // SAFETY: the Impl was written by new_impl with this layout and is dropped once.
        unsafe {
            ptr::drop_in_place(self.0.as_ptr());
            dealloc(self.0.as_ptr() as *mut u8, layout);
        }
    }
}

/// A network packet: fragments of data plus the deleter that frees them.
///
/// Zero copy needs a flexible destructor: packet data may be freed by decrementing a
/// reference count or by a `free()`-like function, and different fragments of one packet
/// may need different ones (a header and a trailer may be heap buffers while the payload is
/// a reference-counted device buffer). Each packet therefore has a single [`Deleter`], and
/// composing a packet from another packet plus a fragment chains the fragment's deleter onto
/// it. The cost is that a deleter may duplicate information the fragment already has.
///
/// Small fragments are allocated with extra space in front, so prepending headers does not
/// allocate.
pub struct Packet {
    _impl: Option<ImplPtr>,
}

// A packet has `len` but no `is_empty`: an empty packet is still a valid packet.
#[allow(clippy::len_without_is_empty)]
impl Packet {
    fn from_impl(impl_: ImplPtr) -> Self {
        Packet { _impl: Some(impl_) }
    }

    /// The packet's allocation. Using a null packet (other than `reset`, `is_valid` and
    /// assignment) is a precondition violation.
    #[inline]
    fn i(&self) -> &ImplPtr {
        debug_assert!(self._impl.is_some(), "null packet");
        // SAFETY: precondition of every packet operation: the packet is not null.
        unsafe { self._impl.as_ref().unwrap_unchecked() }
    }

    /// The packet's allocation, mutable.
    #[inline]
    fn im(&mut self) -> &mut ImplPtr {
        debug_assert!(self._impl.is_some(), "null packet");
        // SAFETY: as in `i`.
        unsafe { self._impl.as_mut().unwrap_unchecked() }
    }

    /// Takes the packet's allocation, leaving a null packet.
    #[inline]
    fn take_impl(&mut self) -> ImplPtr {
        debug_assert!(self._impl.is_some(), "null packet");
        // SAFETY: as in `i`.
        unsafe { self._impl.take().unwrap_unchecked() }
    }

    /// A zero-copy packet over static bytes (no deleter is needed).
    pub fn from_static_data(data: &'static [u8]) -> Packet {
        Packet::from_fragment_deleter(
            Fragment { base: data.as_ptr() as *mut u8, size: data.len() },
            Deleter::default(),
        )
    }

    #[inline]
    /// Total length of the packet's data, in bytes.
    pub fn len(&self) -> u32 {
        self.i()._len
    }

    #[inline]
    /// Approximate memory use: the data length plus the packet's metadata.
    pub fn memory(&self) -> u32 {
        self.len() + std::mem::size_of::<Impl>() as u32
    }

    #[inline]
    /// Fragment number `idx`.
    pub fn frag(&self, idx: u32) -> Fragment {
        self.i().frag(idx as usize)
    }

    #[inline]
    /// Fragment number `idx`, mutable.
    pub fn frag_mut(&mut self, idx: u32) -> &mut Fragment {
        self.im().frag_mut(idx as usize)
    }

    #[inline]
    /// Number of fragments.
    pub fn nr_frags(&self) -> u32 {
        self.i()._nr_frags as u32
    }

    #[inline]
    /// A view of the fragments.
    pub fn fragments(&self) -> PseudoVector<'_> {
        self.i().fragments()
    }

    #[inline]
    /// Raw pointer to the fragment array (`nr_frags` entries), for device drivers.
    pub fn fragment_array(&self) -> *mut Fragment {
        self.i().frags_ptr()
    }

    #[inline]
    /// Copies the whole packet into a single fragment (a no-op if it already is one).
    pub fn linearize_all(&mut self) {
        let l = self.len() as usize;
        self.linearize(0, l)
    }

    #[inline]
    /// Releases the packet's data, leaving a null packet.
    pub fn reset(&mut self) {
        self._impl = None;
    }

    /// Makes room for at least `n_frags` fragments, reallocating the metadata if needed.
    pub fn reserve(&mut self, n_frags: i32) {
        if n_frags > self.i()._nr_frags as i32 {
            let extra = n_frags - self.i()._nr_frags as i32;
            let old = self.take_impl();
            self._impl = Some(ImplPtr::allocate_if_needed(old, extra as usize));
        }
    }

    #[inline]
    /// The RSS hash of the packet, if the device or the stack set one.
    pub fn rss_hash(&self) -> Option<u32> {
        self.i()._rss_hash
    }

    #[inline]
    /// Sets the RSS hash of the packet and returns it.
    pub fn set_rss_hash(&mut self, hash: u32) -> Option<u32> {
        self.im()._rss_hash = Some(hash);
        self.im()._rss_hash
    }

    /// Calls `func` with each fragment as a temporary buffer, avoiding copies when possible:
    /// a fragment in the internal data area is copied, the others share the packet's
    /// deleter.
    pub fn release_into<F: FnMut(TemporaryBuffer<u8>)>(&mut self, mut func: F) {
        let mut idx = 0u32;
        if self.i().using_internal_data() {
            let f = self.frag(idx);
            idx += 1;
            // SAFETY: the fragment points to f.size bytes of the packet.
            func(TemporaryBuffer::<u8>::copy_from(unsafe { f.as_slice() }));
        }
        while idx < self.nr_frags() {
            let f = self.frag(idx);
            idx += 1;
            let d = self.im()._deleter.share();
            // SAFETY: an external fragment's bytes live as long as the packet's deleter, and
            // the buffer holds a share of it.
            func(unsafe { TemporaryBuffer::<u8>::with_deleter(f.base, f.size, d) });
        }
    }

    /// Returns the fragments as temporary buffers (see [`release_into`](Self::release_into)).
    pub fn release(&mut self) -> Vec<TemporaryBuffer<u8>> {
        let mut ret = Vec::with_capacity(self.i()._nr_frags as usize);
        self.release_into(|frag| {
            ret.push(frag);
        });
        ret
    }

    /// Whether the packet holds data (is not null).
    #[inline]
    pub fn is_valid(&self) -> bool {
        self._impl.is_some()
    }

    /// A null packet: holds no allocation; only `reset`, `is_valid` and assignment may be
    /// used on it.
    pub fn make_null_packet() -> Packet {
        Packet { _impl: None }
    }

    #[inline]
    /// A copy of the offload metadata.
    pub fn get_offload_info(&self) -> OffloadInfo {
        self.i()._offload_info
    }
    #[inline]
    /// The offload metadata, mutable.
    pub fn offload_info_ref(&mut self) -> &mut OffloadInfo {
        &mut self.im()._offload_info
    }
    #[inline]
    /// Replaces the offload metadata.
    pub fn set_offload_info(&mut self, oi: OffloadInfo) {
        self.im()._offload_info = oi;
    }
}

impl Default for Packet {
    /// An empty packet.
    fn default() -> Self {
        Packet::from_impl(ImplPtr::allocate(1))
    }
}

impl Packet {
    /// An empty packet.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty packet with room for `nr_frags` fragments.
    pub fn with_nr_frags(nr_frags: usize) -> Self {
        Packet::from_impl(ImplPtr::allocate(nr_frags))
    }

    /// A packet holding a copy of the bytes of `frag`.
    pub fn from_fragment(frag: Fragment) -> Self {
        Packet::from_impl(ImplPtr::new_impl_with_frag(frag, DEFAULT_NR_FRAGS))
    }

    /// A packet holding a copy of `data`.
    pub fn from_data(data: &[u8]) -> Self {
        Self::from_fragment(Fragment { base: data.as_ptr() as *mut u8, size: data.len() })
    }

    /// A zero-copy packet of one fragment, whose memory `d` frees.
    pub fn from_fragment_deleter(frag: Fragment, d: Deleter) -> Self {
        let mut i = ImplPtr::allocate(1);
        i._deleter = d;
        let n = i._nr_frags as usize;
        *i.frag_mut(n) = frag;
        i._nr_frags += 1;
        i._len = frag.size as u32;
        Packet::from_impl(i)
    }

    /// A zero-copy packet of several fragments, whose memory `d` frees.
    pub fn from_fragments(frag: Vec<Fragment>, d: Deleter) -> Self {
        let mut i = ImplPtr::allocate(frag.len());
        i._deleter = d;
        // SAFETY: the impl has room for frag.len() fragments.
        unsafe { ptr::copy_nonoverlapping(frag.as_ptr(), i.frags_ptr(), frag.len()) };
        i._nr_frags = frag.len() as u16;
        i._len = 0;
        let mut len = 0u32;
        for f in i.fragments().iter() {
            len += f.size as u32;
        }
        i._len = len;
        Packet::from_impl(i)
    }

    /// A zero-copy packet of the fragments `it` yields, whose memory `del` frees.
    pub fn from_iter<'a, I>(it: I, del: Deleter) -> Self
    where
        I: Iterator<Item = &'a Fragment> + Clone,
    {
        let mut len: u32 = 0;
        let nr_frags = it.clone().count() as u32;
        it.clone().for_each(|frag| len += frag.size as u32);
        let mut i = ImplPtr::allocate(nr_frags as usize);
        i._deleter = del;
        i._len = len;
        i._nr_frags = nr_frags as u16;
        for (k, f) in it.enumerate() {
            *i.frag_mut(k) = *f;
        }
        Packet::from_impl(i)
    }

    /// Appends a copy of the bytes of `frag` to `x`.
    pub fn append_copy(mut x: Packet, frag: Fragment) -> Self {
        let mut i = ImplPtr::allocate_if_needed(x.take_impl(), 1);
        i._len += frag.size as u32;
        let buf = new_char_array(frag.size);
        // SAFETY: frag.base has frag.size readable bytes; buf has room for them.
        unsafe { ptr::copy_nonoverlapping(frag.base, buf.cast::<u8>(), frag.size) };
        let n = i._nr_frags as usize;
        *i.frag_mut(n) = Fragment { base: buf.cast::<u8>(), size: frag.size };
        i._nr_frags += 1;
        let old = std::mem::take(&mut i._deleter);
        i._deleter = delete_char_array(old, buf);
        Packet::from_impl(i)
    }

    fn allocate_headroom(&mut self, size: usize) -> bool {
        if self.i()._headroom as usize >= size {
            self.im()._len += size as u32;
            if !self.i().using_internal_data() {
                let old = self.take_impl();
                let mut i = ImplPtr::allocate_if_needed(old, 1);
                let n = i._nr_frags as usize;
                // SAFETY: the impl has room for n + 1 fragments (copy_backward).
                unsafe { ptr::copy(i.frags_ptr(), i.frags_ptr().add(1), n) };
                let base = i.data_ptr().wrapping_add(INTERNAL_DATA_SIZE);
                *i.frag_mut(0) = Fragment { base, size: 0 };
                i._nr_frags += 1;
                self._impl = Some(i);
            }
            let i = self.im();
            i._headroom -= size as u32;
            let f0 = i.frag_mut(0);
            f0.base = f0.base.wrapping_sub(size);
            f0.size += size;
            true
        } else {
            false
        }
    }

    /// Prepends a copy of the bytes of `frag` to `x`, into the headroom when it fits.
    pub fn prepend_copy(frag: Fragment, mut x: Packet) -> Self {
        let mut p = Packet { _impl: x._impl.take() };
        // try to prepend into existing internal fragment
        if p.allocate_headroom(frag.size) {
            // SAFETY: the headroom just reserved has frag.size bytes.
            unsafe { ptr::copy(frag.base, p.i().frag(0).base, frag.size) };
            p
        } else {
            // didn't work out, allocate and copy
            p.im().unuse_internal_data();
            let mut i = ImplPtr::allocate_if_needed(p.take_impl(), 1);
            i._len += frag.size as u32;
            let buf = new_char_array(frag.size);
            // SAFETY: frag.base has frag.size readable bytes; buf has room for them.
            unsafe { ptr::copy_nonoverlapping(frag.base, buf.cast::<u8>(), frag.size) };
            let n = i._nr_frags as usize;
            // SAFETY: the impl has room for n + 1 fragments.
            unsafe { ptr::copy(i.frags_ptr(), i.frags_ptr().add(1), n) };
            i._nr_frags += 1;
            *i.frag_mut(0) = Fragment { base: buf.cast::<u8>(), size: frag.size };
            let old = std::mem::take(&mut i._deleter);
            i._deleter = delete_char_array(old, buf);
            Packet::from_impl(i)
        }
    }

    /// Prepends `frag` to `x` without copying; `del` frees its memory.
    pub fn prepend_zero_copy(frag: Fragment, del: Deleter, x: Packet) -> Self {
        let mut p = Packet::from_fragment_deleter(frag, del);
        p.append(x);
        p
    }

    /// Appends `frag` to `x` without copying; `d` frees its memory.
    pub fn append_zero_copy(mut x: Packet, frag: Fragment, mut d: Deleter) -> Self {
        let mut i = ImplPtr::allocate_if_needed(x.take_impl(), 1);
        i._len += frag.size as u32;
        let n = i._nr_frags as usize;
        *i.frag_mut(n) = frag;
        i._nr_frags += 1;
        d.append(std::mem::take(&mut i._deleter));
        i._deleter = d;
        Packet::from_impl(i)
    }

    /// Chains `d` onto the deleter of `x`, so it runs when `x`'s data is freed.
    pub fn append_deleter(mut x: Packet, d: Deleter) -> Self {
        let mut i = x.take_impl();
        i._deleter.append(d);
        Packet::from_impl(i)
    }

    /// Appends the bytes of `buf` to `x` without copying.
    pub fn append_buffer(x: Packet, mut buf: TemporaryBuffer<u8>) -> Self {
        let frag = Fragment { base: buf.get_write(), size: buf.size() };
        Self::append_zero_copy(x, frag, buf.release())
    }

    /// A packet over the bytes of `buf`, without copying.
    pub fn from_buffer(mut buf: TemporaryBuffer<u8>) -> Self {
        let frag = Fragment { base: buf.get_write(), size: buf.size() };
        Self::from_fragment_deleter(frag, buf.release())
    }

    /// Appends the fragments of `p` (without copying their data), taking over its deleter.
    pub fn append(&mut self, mut p: Packet) {
        if self.i()._len == 0 {
            *self = p;
            return;
        }
        let pn = p.i()._nr_frags as usize;
        let mut i = ImplPtr::allocate_if_needed(self.take_impl(), pn);
        i._len += p.i()._len;
        p.im().unuse_internal_data();
        let n = i._nr_frags as usize;
        // SAFETY: `i` has room for n + pn fragments; p has pn of them.
        unsafe { ptr::copy_nonoverlapping(p.i().frags_ptr(), i.frags_ptr().add(n), pn) };
        i._nr_frags += pn as u16;
        let mine = std::mem::take(&mut i._deleter);
        p.im()._deleter.append(mine);
        i._deleter = std::mem::take(&mut p.im()._deleter);
        self._impl = Some(i);
    }

    /// A pointer to `size` contiguous bytes at `offset`, linearizing the fragments that span
    /// them if needed; null if the packet is shorter than `offset + size`.
    ///
    /// Linearizing replaces fragments, so pointers obtained earlier into the affected range
    /// become invalid.
    pub fn get_header_raw(&mut self, mut offset: usize, size: usize) -> *mut u8 {
        if offset + size > self.i()._len as usize {
            return ptr::null_mut();
        }
        let mut i = 0usize;
        let nr = self.i()._nr_frags as usize;
        while i != nr && offset >= self.i().frag(i).size {
            offset -= self.i().frag(i).size;
            i += 1;
        }
        if i == nr {
            return ptr::null_mut();
        }
        if offset + size > self.i().frag(i).size {
            self.linearize(i, offset + size);
        }
        self.i().frag(i).base.wrapping_add(offset)
    }

    /// A pointer to a header of type `H` at `offset` (see
    /// [`get_header_raw`](Self::get_header_raw)); the pointer may be unaligned.
    pub fn get_header<H>(&mut self, offset: usize) -> *mut H {
        self.get_header_raw(offset, std::mem::size_of::<H>()) as *mut H
    }

    /// Removes `how_much` bytes from the front.
    ///
    /// # Panics
    /// If `how_much` exceeds the packet length.
    pub fn trim_front(&mut self, mut how_much: usize) {
        let im = self.im();
        engine_assert!(how_much <= im._len as usize);
        im._len -= how_much as u32;
        let mut i = 0usize;
        while how_much != 0 && how_much >= im.frag(i).size {
            how_much -= im.frag(i).size;
            i += 1;
        }
        let nr = im._nr_frags as usize;
        // SAFETY: [i, nr) are valid fragments; the regions may overlap.
        unsafe { ptr::copy(im.frags_ptr().add(i), im.frags_ptr(), nr - i) };
        im._nr_frags -= i as u16;
        if !im.using_internal_data() {
            im._headroom = INTERNAL_DATA_SIZE as u32;
        }
        if how_much != 0 {
            if im.using_internal_data() {
                im._headroom += how_much as u32;
            }
            let f0 = im.frag_mut(0);
            f0.base = f0.base.wrapping_add(how_much);
            f0.size -= how_much;
        }
    }

    /// Removes `how_much` bytes from the back.
    ///
    /// # Panics
    /// If `how_much` exceeds the packet length.
    pub fn trim_back(&mut self, mut how_much: usize) {
        let im = self.im();
        engine_assert!(how_much <= im._len as usize);
        im._len -= how_much as u32;
        let mut i: isize = im._nr_frags as isize - 1;
        while how_much != 0 && how_much >= im.frag(i as usize).size {
            how_much -= im.frag(i as usize).size;
            i -= 1;
        }
        im._nr_frags = (i + 1) as u16;
        if how_much != 0 {
            im.frag_mut(i as usize).size -= how_much;
            if i == 0 && im.using_internal_data() {
                im._headroom += how_much as u32;
            }
        }
    }

    /// Prepends a default-initialized header of type `H` followed by `extra_size`
    /// uninitialized bytes, and returns a (possibly unaligned) pointer to it.
    pub fn prepend_header<H: Default>(&mut self, extra_size: usize) -> *mut H {
        let h = self.prepend_uninitialized_header(std::mem::size_of::<H>() + extra_size) as *mut H;
        // SAFETY: `h` points to at least size_of::<H>() writable bytes, possibly unaligned.
        unsafe { ptr::write_unaligned(h, H::default()) };
        h
    }

    /// Prepends `size` uninitialized bytes and returns a pointer to them; uses the headroom
    /// when it fits, otherwise allocates a new first fragment.
    pub fn prepend_uninitialized_header(&mut self, size: usize) -> *mut u8 {
        if !self.allocate_headroom(size) {
            // didn't work out, allocate and copy
            self.im().unuse_internal_data();
            // try again, after unuse_internal_data we may have space after all
            if !self.allocate_headroom(size) {
                // failed
                self.im()._len += size as u32;
                let mut i = ImplPtr::allocate_if_needed(self.take_impl(), 1);
                let buf = new_char_array(size);
                let n = i._nr_frags as usize;
                // SAFETY: the impl has room for n + 1 fragments.
                unsafe { ptr::copy(i.frags_ptr(), i.frags_ptr().add(1), n) };
                i._nr_frags += 1;
                *i.frag_mut(0) = Fragment { base: buf.cast::<u8>(), size };
                let old = std::mem::take(&mut i._deleter);
                i._deleter = delete_char_array(old, buf);
                self._impl = Some(i);
            }
        }
        self.i().frag(0).base
    }

    /// Shares the whole packet: a new packet over the same data (reference counted, not
    /// copy-on-write).
    pub fn share(&mut self) -> Packet {
        let l = self.i()._len as usize;
        self.share_range(0, l)
    }

    /// Shares `len` bytes starting at `offset`: a new packet over the same data, holding a
    /// share of the deleter and a copy of the offload metadata.
    pub fn share_range(&mut self, mut offset: usize, len: usize) -> Packet {
        self.im().unuse_internal_data(); // FIXME: eliminate?
        let mut n = Packet::new();
        let nr = self.i()._nr_frags as usize;
        n._impl = Some(ImplPtr::allocate_if_needed(n.take_impl(), nr));
        let mut idx = 0usize;
        while offset > 0 && offset >= self.i().frag(idx).size {
            offset -= self.i().frag(idx).size;
            idx += 1;
        }
        while (n.i()._len as usize) < len {
            let f = self.i().frag(idx);
            idx += 1;
            let fsize = std::cmp::min(len - n.i()._len as usize, f.size - offset);
            let ni = n.im();
            let k = ni._nr_frags as usize;
            *ni.frag_mut(k) = Fragment { base: f.base.wrapping_add(offset), size: fsize };
            ni._nr_frags += 1;
            ni._len += fsize as u32;
            offset = 0;
        }
        n.im()._offload_info = self.i()._offload_info;
        engine_assert!(!n.i()._deleter.is_some());
        n.im()._deleter = self.im()._deleter.share();
        n
    }
}

impl Packet {
    /// Copies the `desired_size` bytes starting at fragment `at_frag` into one new
    /// fragment.
    fn linearize(&mut self, at_frag: usize, desired_size: usize) {
        self.im().unuse_internal_data();
        let mut nr_frags = 0usize;
        let mut accum_size = 0usize;
        while accum_size < desired_size {
            accum_size += self.i().frag(at_frag + nr_frags).size;
            nr_frags += 1;
        }
        let new_frag = new_char_array(accum_size);
        let mut p = 0usize;
        for i in 0..nr_frags {
            let f = self.i().frag(at_frag + i);
            // SAFETY: f has f.size readable bytes; new_frag has room for accum_size.
            unsafe { ptr::copy_nonoverlapping(f.base, new_frag.cast::<u8>().add(p), f.size) };
            p += f.size;
        }
        let total = self.len() as usize;
        let im = self.im();
        // collapse nr_frags into one fragment
        let nr = im._nr_frags as usize;
        // SAFETY: the ranges are within the fragment array; they may overlap.
        unsafe {
            ptr::copy(
                im.frags_ptr().add(at_frag + nr_frags),
                im.frags_ptr().add(at_frag + 1),
                nr - (at_frag + nr_frags),
            )
        };
        im._nr_frags -= (nr_frags - 1) as u16;
        *im.frag_mut(at_frag) = Fragment { base: new_frag.cast::<u8>(), size: accum_size };
        if at_frag == 0 && desired_size == total {
            // We can drop the old buffer safely
            let x = std::mem::take(&mut im._deleter);
            im._deleter = delete_char_array(Deleter::default(), new_frag);
            drop(x);
        } else {
            let old = std::mem::take(&mut im._deleter);
            im._deleter = delete_char_array(old, new_frag);
        }
    }

    /// Returns a packet over the same data whose deleter, when it runs, submits the previous
    /// deleter and then `cb` to shard `cpu` and runs them there: memory allocated on `cpu`
    /// is freed on `cpu`. The callback is a type parameter, stored inline in the deleter and
    /// the submitted work.
    pub fn free_on_cpu<F: FnOnce() + 'static>(&mut self, cpu: u32, cb: F) -> Packet {
        // make new deleter that runs old deleter on an origin cpu
        let d = std::mem::take(&mut self.im()._deleter);
        self.im()._deleter = make_deleter_with(Deleter::default(), move || {
            // SAFETY: the deleter and the callback move to `cpu`, the shard that made them,
            // and are run and destroyed there only; this shard keeps nothing that refers to
            // them.
            let captured = unsafe { CrossShard::new((d, cb)) };
            // FIXME: future is discarded
            let _ = Smp::submit_to(cpu, move || {
                // deleter needs to be moved out of the closure capture to be destroyed here
                // otherwise deleter destructor will be called on the shard that submitted the
                // work, when the work item is destroyed.
                let (xxx, cb) = captured.into_inner();
                cb();
                drop(xxx);
            });
        });

        Packet::from_impl(ImplPtr::copy(self.im()))
    }

    /// [`free_on_cpu`](Self::free_on_cpu) with no callback.
    pub fn free_on_cpu_default(&mut self, cpu: u32) -> Packet {
        self.free_on_cpu(cpu, || {})
    }
}

/// Whether `c` is a printable ASCII character.
fn isprint(c: u8) -> bool {
    (0x20..0x7f).contains(&c)
}

impl fmt::Display for Packet {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        os.write_str("packet{")?;
        let mut first = true;
        for frag in self.fragments().iter() {
            if !first {
                os.write_str(", ")?;
            }
            first = false;
            // SAFETY: the fragment points to frag.size bytes of the packet.
            let bytes = unsafe { frag.as_slice() };
            // Printed as text if every byte is in 9..=0x7f, otherwise as hex.
            if bytes.iter().all(|&c| (9..=0x7f).contains(&c)) {
                os.write_str("\"")?;
                for &c in bytes {
                    if isprint(c) {
                        write!(os, "{}", c as char)?;
                    } else if c == b'\r' {
                        os.write_str("\\r")?;
                    } else if c == b'\n' {
                        os.write_str("\\n")?;
                    } else if c == b'\t' {
                        os.write_str("\\t")?;
                    } else {
                        let b: u8 = c;
                        write!(os, "\\x{}{}", b / 16, b % 16)?;
                    }
                }
                os.write_str("\"")?;
            } else {
                os.write_str("{")?;
                let mut nfirst = true;
                for &b in bytes {
                    if !nfirst {
                        os.write_str(" ")?;
                    }
                    nfirst = false;
                    write!(os, "{:02x}", b as u32)?;
                }
                os.write_str("}")?;
            }
        }
        os.write_str("}")
    }
}
