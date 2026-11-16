//! Bindings to the parts of DPDK the DPDK network device uses (built with the `dpdk`
//! feature): its structures, the functions it exports and the helpers its headers define
//! inline.
//!
//! The bindings target DPDK 23.07 built with `-Dmbuf_refcnt_atomic=false` (the mbuf
//! reference count follows the library actually linked, see `build.rs`) and the default
//! `rte_config.h` (`RTE_MAX_LCORE` 128, `RTE_MAX_ETHPORTS` 32, `RTE_PKTMBUF_HEADROOM` 128,
//! `RTE_MEMPOOL_CACHE_MAX_SIZE` 512, `RTE_IOVA_IN_MBUF` 1, `RTE_ETHDEV_RXTX_CALLBACKS` on,
//! mempool statistics and debug cookies off, fast-path trace points compiled out).
//!
//! The structures are `#[repr(C)]` with the field order, alignment and padding of the DPDK
//! 23.07 headers; the compile-time assertions at the end of each one pin its size and the
//! offsets the driver reads. The functions DPDK exports as symbols are declared
//! `extern "C"`; the ones its headers define `static inline` (`rte_pktmbuf_*`,
//! `rte_mempool_get/put*`, `rte_eth_rx_burst/tx_burst` and their helpers) cannot be linked,
//! so they are implemented here with the same algorithm as DPDK's definitions. They are on
//! the per-packet hot path.

#![allow(non_camel_case_types)]

use std::cell::Cell;
use std::ffi::{CString, c_char, c_int, c_uint, c_void};
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU16, Ordering};

// ---------------------------------------------------------------------------
// rte_config.h, rte_common.h, rte_lcore.h
// ---------------------------------------------------------------------------

/// `RTE_MAX_LCORE` of the default configuration.
pub const RTE_MAX_LCORE: usize = 128;
/// `RTE_MAX_ETHPORTS` of the default configuration.
pub const RTE_MAX_ETHPORTS: usize = 32;
/// `RTE_CACHE_LINE_SIZE`.
pub const RTE_CACHE_LINE_SIZE: usize = 64;
/// `LCORE_ID_ANY`: the lcore id of a thread that is not an EAL lcore.
pub const LCORE_ID_ANY: c_uint = c_uint::MAX;
/// `EXIT_FAILURE` of `<stdlib.h>`.
pub const EXIT_FAILURE: c_int = 1;
/// `ENOTSUP` of `<errno.h>` (Linux).
pub const ENOTSUP: c_int = 95;

/// `rte_iova_t`
pub type rte_iova_t = u64;
/// `RTE_BAD_IOVA`
pub const RTE_BAD_IOVA: rte_iova_t = !0;

// ---------------------------------------------------------------------------
// rte_mbuf_core.h, rte_mbuf.h
// ---------------------------------------------------------------------------

/// `RTE_PKTMBUF_HEADROOM`
pub const RTE_PKTMBUF_HEADROOM: u16 = 128;
/// `RTE_MBUF_PORT_INVALID`
pub const RTE_MBUF_PORT_INVALID: u16 = u16::MAX;

/// `RTE_MBUF_F_RX_VLAN`: the RX packet is a 802.1q VLAN packet.
pub const RTE_MBUF_F_RX_VLAN: u64 = 1 << 0;
/// `RTE_MBUF_F_RX_RSS_HASH`: the RX packet carries an RSS hash.
pub const RTE_MBUF_F_RX_RSS_HASH: u64 = 1 << 1;
/// `RTE_MBUF_F_RX_L4_CKSUM_BAD`: the L4 checksum of the RX packet is wrong.
pub const RTE_MBUF_F_RX_L4_CKSUM_BAD: u64 = 1 << 3;
/// `RTE_MBUF_F_RX_IP_CKSUM_BAD`: the IP checksum of the RX packet is wrong.
pub const RTE_MBUF_F_RX_IP_CKSUM_BAD: u64 = 1 << 4;
/// `RTE_MBUF_F_RX_VLAN_STRIPPED`: the VLAN tag was stripped into `vlan_tci`.
pub const RTE_MBUF_F_RX_VLAN_STRIPPED: u64 = 1 << 6;
/// `RTE_MBUF_F_TX_TCP_SEG`: TCP segmentation offload.
pub const RTE_MBUF_F_TX_TCP_SEG: u64 = 1 << 50;
/// `RTE_MBUF_F_TX_TCP_CKSUM`: TCP checksum offload.
pub const RTE_MBUF_F_TX_TCP_CKSUM: u64 = 1 << 52;
/// `RTE_MBUF_F_TX_UDP_CKSUM`: UDP checksum offload.
pub const RTE_MBUF_F_TX_UDP_CKSUM: u64 = 3 << 52;
/// `RTE_MBUF_F_TX_IP_CKSUM`: IP checksum offload.
pub const RTE_MBUF_F_TX_IP_CKSUM: u64 = 1 << 54;
/// `RTE_MBUF_F_EXTERNAL`: the mbuf has an external buffer attached.
pub const RTE_MBUF_F_EXTERNAL: u64 = 1 << 61;
/// `RTE_MBUF_F_INDIRECT`: the mbuf is an indirect (cloned) one.
pub const RTE_MBUF_F_INDIRECT: u64 = 1 << 62;

/// `RTE_PKTMBUF_POOL_F_PINNED_EXT_BUF`
pub const RTE_PKTMBUF_POOL_F_PINNED_EXT_BUF: u32 = 1 << 0;

/// The `hash` union of `struct rte_mbuf`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union rte_mbuf_hash {
    /// RSS hash result if RSS enabled.
    pub rss: u32,
    /// Filter identifier if FDIR enabled (`hash`/`id`/`lo`, `hi`).
    pub fdir: [u32; 2],
    /// Hierarchical scheduler (`struct rte_mbuf_sched`).
    pub sched: [u32; 2],
    /// Eventdev Tx adapter (`reserved1`, `reserved2`, `txq`).
    pub txadapter: [u32; 2],
    /// User defined tags.
    pub usr: u32,
}

/// `struct rte_mbuf_ext_shared_info`
#[repr(C)]
pub struct rte_mbuf_ext_shared_info {
    /// Free callback function.
    pub free_cb: Option<unsafe extern "C" fn(addr: *mut c_void, opaque: *mut c_void)>,
    /// Free callback argument.
    pub fcb_opaque: *mut c_void,
    /// Atomically accessed refcnt.
    pub refcnt: AtomicU16,
}

/// `struct rte_mbuf` (two cache lines).
#[repr(C, align(64))]
pub struct rte_mbuf {
    /// Virtual address of segment buffer.
    pub buf_addr: *mut c_void,
    /// Physical address of segment buffer.
    pub buf_iova: rte_iova_t,
    // next 8 bytes are initialised on RX descriptor rearm
    /// Data offset in the segment buffer.
    pub data_off: u16,
    /// Reference counter (`refcnt_atomic` in the union). Atomic: DPDK's own functions (the
    /// non-inline frees of the library and the PMDs) use atomic operations on it unless DPDK
    /// was built without `RTE_MBUF_REFCNT_ATOMIC`, which `build.rs` detects (see
    /// `rte_mbuf_refcnt_update`). Same size and alignment as `uint16_t`.
    pub refcnt: AtomicU16,
    /// Number of segments.
    pub nb_segs: u16,
    /// Input port.
    pub port: u16,
    /// Offload features.
    pub ol_flags: u64,
    // remaining bytes are set on RX when pulling packet from descriptor
    /// L2/L3/L4 and tunnel information.
    pub packet_type: u32,
    /// Total pkt len: sum of all segments.
    pub pkt_len: u32,
    /// Amount of data in segment buffer.
    pub data_len: u16,
    /// VLAN TCI (CPU order), valid if `RTE_MBUF_F_RX_VLAN` is set.
    pub vlan_tci: u16,
    /// Hash information.
    pub hash: rte_mbuf_hash,
    /// Outer VLAN TCI (CPU order), valid if `RTE_MBUF_F_RX_QINQ` is set.
    pub vlan_tci_outer: u16,
    /// Length of segment buffer.
    pub buf_len: u16,
    /// Pool from which mbuf was allocated.
    pub pool: *mut rte_mempool,
    // second cache line - fields only used in slow path or on TX
    /// Next segment of scattered packet.
    pub next: *mut rte_mbuf,
    /// The `tx_offload` bit fields (`l2_len:7`, `l3_len:9`, `l4_len:8`, `tso_segsz:16`,
    /// `outer_l3_len:9`, `outer_l2_len:7`).
    pub tx_offload: u64,
    /// Shared data for external buffer attached to mbuf.
    pub shinfo: *mut rte_mbuf_ext_shared_info,
    /// Size of the application private data.
    pub priv_size: u16,
    /// Timesync flags for use with IEEE1588.
    pub timesync: u16,
    /// Reserved for dynamic fields.
    pub dynfield1: [u32; 9],
}

const _: () = assert!(size_of::<rte_mbuf>() == 128);
const _: () = assert!(offset_of!(rte_mbuf, data_off) == 16);
const _: () = assert!(offset_of!(rte_mbuf, ol_flags) == 24);
const _: () = assert!(offset_of!(rte_mbuf, pkt_len) == 36);
const _: () = assert!(offset_of!(rte_mbuf, hash) == 44);
const _: () = assert!(offset_of!(rte_mbuf, pool) == 56);
const _: () = assert!(offset_of!(rte_mbuf, next) == 64);
const _: () = assert!(offset_of!(rte_mbuf, dynfield1) == 92);

const L2_LEN_SHIFT: u32 = 0;
const L2_LEN_MASK: u64 = (1 << 7) - 1;
const L3_LEN_SHIFT: u32 = 7;
const L3_LEN_MASK: u64 = (1 << 9) - 1;
const L4_LEN_SHIFT: u32 = 16;
const L4_LEN_MASK: u64 = (1 << 8) - 1;
const TSO_SEGSZ_SHIFT: u32 = 24;
const TSO_SEGSZ_MASK: u64 = (1 << 16) - 1;

impl rte_mbuf {
    #[inline(always)]
    fn tx_offload_field(&self, shift: u32, mask: u64) -> u64 {
        (self.tx_offload >> shift) & mask
    }

    #[inline(always)]
    fn set_tx_offload_field(&mut self, shift: u32, mask: u64, v: u64) {
        self.tx_offload = (self.tx_offload & !(mask << shift)) | ((v & mask) << shift);
    }

    /// `l2_len`: L2 (MAC) header length.
    #[inline(always)]
    pub fn l2_len(&self) -> u64 {
        self.tx_offload_field(L2_LEN_SHIFT, L2_LEN_MASK)
    }
    /// `l2_len = v`
    #[inline(always)]
    pub fn set_l2_len(&mut self, v: u64) {
        self.set_tx_offload_field(L2_LEN_SHIFT, L2_LEN_MASK, v)
    }
    /// `l3_len`: L3 (IP) header length.
    #[inline(always)]
    pub fn l3_len(&self) -> u64 {
        self.tx_offload_field(L3_LEN_SHIFT, L3_LEN_MASK)
    }
    /// `l3_len = v`
    #[inline(always)]
    pub fn set_l3_len(&mut self, v: u64) {
        self.set_tx_offload_field(L3_LEN_SHIFT, L3_LEN_MASK, v)
    }
    /// `l4_len`: L4 (TCP/UDP) header length.
    #[inline(always)]
    pub fn l4_len(&self) -> u64 {
        self.tx_offload_field(L4_LEN_SHIFT, L4_LEN_MASK)
    }
    /// `l4_len = v`
    #[inline(always)]
    pub fn set_l4_len(&mut self, v: u64) {
        self.set_tx_offload_field(L4_LEN_SHIFT, L4_LEN_MASK, v)
    }
    /// `tso_segsz`: TCP TSO segment size.
    #[inline(always)]
    pub fn tso_segsz(&self) -> u64 {
        self.tx_offload_field(TSO_SEGSZ_SHIFT, TSO_SEGSZ_MASK)
    }
    /// `tso_segsz = v`
    #[inline(always)]
    pub fn set_tso_segsz(&mut self, v: u64) {
        self.set_tx_offload_field(TSO_SEGSZ_SHIFT, TSO_SEGSZ_MASK, v)
    }
}

/// `struct rte_pktmbuf_pool_private`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_pktmbuf_pool_private {
    /// Size of data space in each mbuf.
    pub mbuf_data_room_size: u16,
    /// Size of private area in each mbuf.
    pub mbuf_priv_size: u16,
    /// reserved for future use.
    pub flags: u32,
}

// ---------------------------------------------------------------------------
// rte_mempool.h
// ---------------------------------------------------------------------------

/// `RTE_MEMPOOL_NAMESIZE` (`RTE_RING_NAMESIZE - sizeof("MP_") + 1`).
pub const RTE_MEMPOOL_NAMESIZE: usize = 26;
/// `RTE_MEMPOOL_CACHE_MAX_SIZE`
pub const RTE_MEMPOOL_CACHE_MAX_SIZE: usize = 512;
/// `RTE_MEMPOOL_MAX_OPS_IDX`
pub const RTE_MEMPOOL_MAX_OPS_IDX: usize = 16;
/// `RTE_MEMPOOL_OPS_NAMESIZE`
pub const RTE_MEMPOOL_OPS_NAMESIZE: usize = 32;

/// `struct rte_mempool_cache`: a per-lcore cache of objects.
#[repr(C, align(64))]
pub struct rte_mempool_cache {
    /// Size of the cache
    pub size: u32,
    /// Threshold before we flush excess elements
    pub flushthresh: u32,
    /// Current cache count
    pub len: u32,
    _pad: [u8; RTE_CACHE_LINE_SIZE - 12],
    /// Cache objects (cache line aligned).
    ///
    /// Cache is allocated to this size to allow it to overflow in certain
    /// cases to avoid needless emptying of cache.
    pub objs: [*mut c_void; RTE_MEMPOOL_CACHE_MAX_SIZE * 2],
}

const _: () = assert!(offset_of!(rte_mempool_cache, objs) == 64);
const _: () = assert!(size_of::<rte_mempool_cache>() == 64 + 8 * 1024);

/// `struct rte_memzone` of DPDK 23.07, up to `len` (the only field read).
#[repr(C)]
pub struct rte_memzone {
    /// Name of the memory zone.
    pub name: [c_char; 32],
    /// Start IO address.
    pub iova: rte_iova_t,
    /// Start virtual address.
    pub addr: *mut c_void,
    /// Length of the memzone.
    pub len: usize,
}

const _: () = assert!(offset_of!(rte_memzone, len) == 48);

/// Checks, against the linked library, the size of `struct rte_mempool_cache` that the inline
/// mempool functions of this module assume (`RTE_MEMPOOL_CACHE_MAX_SIZE * 2` objects, which
/// changed across DPDK releases).
///
/// `rte_mempool_create_empty` reserves the pool's memzone with
/// `RTE_ALIGN_CEIL(RTE_MEMPOOL_HEADER_SIZE(mp, cache_size) + private_data_size,
/// RTE_MEMPOOL_ALIGN)` bytes, where `RTE_MEMPOOL_HEADER_SIZE` is
/// `sizeof(struct rte_mempool) + RTE_MAX_LCORE * sizeof(struct rte_mempool_cache)` for a
/// pool with a cache, and sets
/// `local_cache` right after the `rte_mempool`. Both are compared with this module's sizes.
/// The library's own headers are not available at build time, so the layout is checked
/// at run time.
///
/// # Safety
/// `mp` is a pool made by `rte_mempool_create` or `rte_mempool_create_empty`.
pub unsafe fn rte_mempool_check_layout(mp: *const rte_mempool) -> Result<(), String> {
    // SAFETY: guaranteed by the caller; `mz` is the pool's memzone, set by DPDK.
    unsafe {
        let base = mp as usize;
        if (*mp).local_cache as usize != base + size_of::<rte_mempool>() {
            return Err(format!(
                "DPDK mempool layout mismatch: local_cache at +{}, expected +{} (struct rte_mempool)",
                ((*mp).local_cache as usize).wrapping_sub(base),
                size_of::<rte_mempool>()
            ));
        }
        if (*mp).cache_size == 0 || (*mp).mz.is_null() {
            return Ok(());
        }
        let header = size_of::<rte_mempool>() + RTE_MAX_LCORE * size_of::<rte_mempool_cache>();
        let expected =
            (header + (*mp).private_data_size as usize).next_multiple_of(RTE_CACHE_LINE_SIZE);
        let len = (*((*mp).mz as *const rte_memzone)).len;
        if len != expected {
            return Err(format!(
                "DPDK mempool layout mismatch: memzone of {len} bytes, expected {expected} \
                 (sizeof(struct rte_mempool_cache) = {} assumed, RTE_MAX_LCORE = {RTE_MAX_LCORE})",
                size_of::<rte_mempool_cache>()
            ));
        }
        Ok(())
    }
}

/// `struct rte_mempool_objhdr`: the header in front of every mempool object.
#[repr(C)]
pub struct rte_mempool_objhdr {
    /// Next in list.
    pub next: *mut rte_mempool_objhdr,
    /// The mempool owning the object.
    pub mp: *mut rte_mempool,
    /// IO address of the object.
    pub iova: rte_iova_t,
}

/// `struct rte_mempool_objsz`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_mempool_objsz {
    /// Size of an element.
    pub elt_size: u32,
    /// Size of header (before elt).
    pub header_size: u32,
    /// Size of trailer (after elt).
    pub trailer_size: u32,
    /// Total size of an object (header + elt + trailer).
    pub total_size: u32,
}

/// `struct rte_mempool`
#[repr(C, align(64))]
pub struct rte_mempool {
    /// Name of mempool.
    pub name: [c_char; RTE_MEMPOOL_NAMESIZE],
    /// Ring or pool to store objects (union with `uint64_t pool_id`).
    pub pool_data: *mut c_void,
    /// optional args for ops alloc.
    pub pool_config: *mut c_void,
    /// Memzone where pool is alloc'd.
    pub mz: *const c_void,
    /// Flags of the mempool.
    pub flags: c_uint,
    /// Socket id passed at create.
    pub socket_id: c_int,
    /// Max size of the mempool.
    pub size: u32,
    /// Size of per-lcore default local cache.
    pub cache_size: u32,
    /// Size of an element.
    pub elt_size: u32,
    /// Size of header (before elt).
    pub header_size: u32,
    /// Size of trailer (after elt).
    pub trailer_size: u32,
    /// Size of private data.
    pub private_data_size: c_uint,
    /// Index into rte_mempool_ops_table array of mempool ops structs.
    pub ops_index: i32,
    /// Per-lcore local cache
    pub local_cache: *mut rte_mempool_cache,
    /// Number of populated objects.
    pub populated_size: u32,
    /// List of objects in pool (`STAILQ_HEAD`).
    pub elt_list: [*mut c_void; 2],
    /// Number of memory chunks
    pub nb_mem_chunks: u32,
    /// List of memory chunks (`STAILQ_HEAD`).
    pub mem_list: [*mut c_void; 2],
}

const _: () = assert!(offset_of!(rte_mempool, pool_data) == 32);
const _: () = assert!(offset_of!(rte_mempool, cache_size) == 68);
const _: () = assert!(offset_of!(rte_mempool, ops_index) == 88);
const _: () = assert!(offset_of!(rte_mempool, local_cache) == 96);
const _: () = assert!(size_of::<rte_mempool>() == 192);

/// `rte_mempool_enqueue_t`
pub type rte_mempool_enqueue_t =
    unsafe extern "C" fn(mp: *mut rte_mempool, obj_table: *const *mut c_void, n: c_uint) -> c_int;
/// `rte_mempool_dequeue_t`
pub type rte_mempool_dequeue_t =
    unsafe extern "C" fn(mp: *mut rte_mempool, obj_table: *mut *mut c_void, n: c_uint) -> c_int;

/// `struct rte_mempool_ops`
#[repr(C, align(64))]
pub struct rte_mempool_ops {
    /// Name of mempool ops struct.
    pub name: [c_char; RTE_MEMPOOL_OPS_NAMESIZE],
    /// Allocate private data.
    pub alloc: *const c_void,
    /// Free the external pool.
    pub free: *const c_void,
    /// Enqueue an object.
    pub enqueue: Option<rte_mempool_enqueue_t>,
    /// Dequeue an object.
    pub dequeue: Option<rte_mempool_dequeue_t>,
    /// Get qty of available objs.
    pub get_count: *const c_void,
    /// Optional callback to calculate memory size required to store specified number of
    /// objects.
    pub calc_mem_size: *const c_void,
    /// Optional callback to populate mempool objects using provided memory chunk.
    pub populate: *const c_void,
    /// Get mempool info
    pub get_info: *const c_void,
    /// Dequeue a number of contiguous object blocks.
    pub dequeue_contig_blocks: *const c_void,
}

const _: () = assert!(size_of::<rte_mempool_ops>() == 128);

/// `struct rte_mempool_ops_table`
#[repr(C, align(64))]
pub struct rte_mempool_ops_table {
    /// Spinlock for add/delete (`rte_spinlock_t`).
    pub sl: c_int,
    /// Number of used ops structs in the table.
    pub num_ops: u32,
    /// Storage for all possible ops structs.
    pub ops: [rte_mempool_ops; RTE_MEMPOOL_MAX_OPS_IDX],
}

const _: () = assert!(offset_of!(rte_mempool_ops_table, ops) == 64);

/// `rte_mempool_ctor_t`
pub type rte_mempool_ctor_t = unsafe extern "C" fn(mp: *mut rte_mempool, arg: *mut c_void);
/// `rte_mempool_obj_cb_t`
pub type rte_mempool_obj_cb_t = unsafe extern "C" fn(
    mp: *mut rte_mempool,
    opaque: *mut c_void,
    obj: *mut c_void,
    obj_idx: c_uint,
);
/// `rte_mempool_memchunk_free_cb_t`
pub type rte_mempool_memchunk_free_cb_t =
    unsafe extern "C" fn(memhdr: *mut c_void, opaque: *mut c_void);

// ---------------------------------------------------------------------------
// rte_ether.h, rte_ethdev.h, ethdev_driver.h (rte_ethdev_core.h)
// ---------------------------------------------------------------------------

/// `RTE_ETHER_ADDR_LEN`
pub const RTE_ETHER_ADDR_LEN: usize = 6;
/// `sizeof(struct rte_ether_hdr)`: destination and source addresses and the ether type.
pub const RTE_ETHER_HDR_SIZE: u64 = 14;

/// `struct rte_ether_addr`
#[repr(C, align(2))]
#[derive(Clone, Copy, Default)]
pub struct rte_ether_addr {
    /// Addr bytes in tx order
    pub addr_bytes: [u8; RTE_ETHER_ADDR_LEN],
}

/// `RTE_ETH_XSTATS_NAME_SIZE`
pub const RTE_ETH_XSTATS_NAME_SIZE: usize = 64;
/// `RTE_ETHDEV_QUEUE_STAT_CNTRS`
pub const RTE_ETHDEV_QUEUE_STAT_CNTRS: usize = 16;
/// `RTE_ETH_RETA_GROUP_SIZE`
pub const RTE_ETH_RETA_GROUP_SIZE: u16 = 64;
/// `RTE_ETH_VMDQ_MAX_VLAN_FILTERS`
pub const RTE_ETH_VMDQ_MAX_VLAN_FILTERS: usize = 64;
/// `RTE_ETH_DCB_NUM_USER_PRIORITIES`
pub const RTE_ETH_DCB_NUM_USER_PRIORITIES: usize = 8;
/// `RTE_VFIO_DEFAULT_CONTAINER_FD`
pub const RTE_VFIO_DEFAULT_CONTAINER_FD: c_int = -1;

/// `RTE_ETH_MQ_RX_NONE` of `enum rte_eth_rx_mq_mode`.
pub const RTE_ETH_MQ_RX_NONE: u32 = 0;
/// `RTE_ETH_MQ_RX_RSS` of `enum rte_eth_rx_mq_mode`.
pub const RTE_ETH_MQ_RX_RSS: u32 = 1;

/// `RTE_ETH_FC_NONE` of `enum rte_eth_fc_mode`.
pub const RTE_ETH_FC_NONE: u32 = 0;
/// `RTE_ETH_FC_FULL` of `enum rte_eth_fc_mode`.
pub const RTE_ETH_FC_FULL: u32 = 3;

/// `RTE_ETH_LINK_FULL_DUPLEX`
pub const RTE_ETH_LINK_FULL_DUPLEX: u16 = 1;

/// `RTE_ETH_RX_OFFLOAD_VLAN_STRIP`
pub const RTE_ETH_RX_OFFLOAD_VLAN_STRIP: u64 = 1 << 0;
/// `RTE_ETH_RX_OFFLOAD_IPV4_CKSUM`
pub const RTE_ETH_RX_OFFLOAD_IPV4_CKSUM: u64 = 1 << 1;
/// `RTE_ETH_RX_OFFLOAD_UDP_CKSUM`
pub const RTE_ETH_RX_OFFLOAD_UDP_CKSUM: u64 = 1 << 2;
/// `RTE_ETH_RX_OFFLOAD_TCP_CKSUM`
pub const RTE_ETH_RX_OFFLOAD_TCP_CKSUM: u64 = 1 << 3;
/// `RTE_ETH_RX_OFFLOAD_TCP_LRO`
pub const RTE_ETH_RX_OFFLOAD_TCP_LRO: u64 = 1 << 4;
/// `RTE_ETH_RX_OFFLOAD_CHECKSUM`
pub const RTE_ETH_RX_OFFLOAD_CHECKSUM: u64 =
    RTE_ETH_RX_OFFLOAD_IPV4_CKSUM | RTE_ETH_RX_OFFLOAD_UDP_CKSUM | RTE_ETH_RX_OFFLOAD_TCP_CKSUM;

/// `RTE_ETH_TX_OFFLOAD_VLAN_INSERT`
pub const RTE_ETH_TX_OFFLOAD_VLAN_INSERT: u64 = 1 << 0;
/// `RTE_ETH_TX_OFFLOAD_IPV4_CKSUM`
pub const RTE_ETH_TX_OFFLOAD_IPV4_CKSUM: u64 = 1 << 1;
/// `RTE_ETH_TX_OFFLOAD_UDP_CKSUM`
pub const RTE_ETH_TX_OFFLOAD_UDP_CKSUM: u64 = 1 << 2;
/// `RTE_ETH_TX_OFFLOAD_TCP_CKSUM`
pub const RTE_ETH_TX_OFFLOAD_TCP_CKSUM: u64 = 1 << 3;
/// `RTE_ETH_TX_OFFLOAD_SCTP_CKSUM`
pub const RTE_ETH_TX_OFFLOAD_SCTP_CKSUM: u64 = 1 << 4;
/// `RTE_ETH_TX_OFFLOAD_TCP_TSO`
pub const RTE_ETH_TX_OFFLOAD_TCP_TSO: u64 = 1 << 5;
/// `RTE_ETH_TX_OFFLOAD_UDP_TSO`
pub const RTE_ETH_TX_OFFLOAD_UDP_TSO: u64 = 1 << 6;
/// `RTE_ETH_TX_OFFLOAD_OUTER_IPV4_CKSUM`
pub const RTE_ETH_TX_OFFLOAD_OUTER_IPV4_CKSUM: u64 = 1 << 7;
/// `RTE_ETH_TX_OFFLOAD_QINQ_INSERT`
pub const RTE_ETH_TX_OFFLOAD_QINQ_INSERT: u64 = 1 << 8;
/// `RTE_ETH_TX_OFFLOAD_VXLAN_TNL_TSO`
pub const RTE_ETH_TX_OFFLOAD_VXLAN_TNL_TSO: u64 = 1 << 9;
/// `RTE_ETH_TX_OFFLOAD_GRE_TNL_TSO`
pub const RTE_ETH_TX_OFFLOAD_GRE_TNL_TSO: u64 = 1 << 10;
/// `RTE_ETH_TX_OFFLOAD_IPIP_TNL_TSO`
pub const RTE_ETH_TX_OFFLOAD_IPIP_TNL_TSO: u64 = 1 << 11;
/// `RTE_ETH_TX_OFFLOAD_GENEVE_TNL_TSO`
pub const RTE_ETH_TX_OFFLOAD_GENEVE_TNL_TSO: u64 = 1 << 12;
/// `RTE_ETH_TX_OFFLOAD_MACSEC_INSERT`
pub const RTE_ETH_TX_OFFLOAD_MACSEC_INSERT: u64 = 1 << 13;

/// `struct rte_eth_xstat`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_xstat {
    /// The index in xstats name array.
    pub id: u64,
    /// The statistic counter value.
    pub value: u64,
}

/// `struct rte_eth_xstat_name`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_xstat_name {
    /// The statistic name.
    pub name: [c_char; RTE_ETH_XSTATS_NAME_SIZE],
}

impl Default for rte_eth_xstat_name {
    fn default() -> Self {
        rte_eth_xstat_name { name: [0; RTE_ETH_XSTATS_NAME_SIZE] }
    }
}

/// `struct rte_eth_stats`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_stats {
    /// Total number of successfully received packets.
    pub ipackets: u64,
    /// Total number of successfully transmitted packets.
    pub opackets: u64,
    /// Total number of successfully received bytes.
    pub ibytes: u64,
    /// Total number of successfully transmitted bytes.
    pub obytes: u64,
    /// Total of Rx packets dropped by the HW, because there are no available buffer.
    pub imissed: u64,
    /// Total number of erroneous received packets.
    pub ierrors: u64,
    /// Total number of failed transmitted packets.
    pub oerrors: u64,
    /// Total number of Rx mbuf allocation failures.
    pub rx_nombuf: u64,
    /// Total number of queue Rx packets.
    pub q_ipackets: [u64; RTE_ETHDEV_QUEUE_STAT_CNTRS],
    /// Total number of queue Tx packets.
    pub q_opackets: [u64; RTE_ETHDEV_QUEUE_STAT_CNTRS],
    /// Total number of successfully received queue bytes.
    pub q_ibytes: [u64; RTE_ETHDEV_QUEUE_STAT_CNTRS],
    /// Total number of successfully transmitted queue bytes.
    pub q_obytes: [u64; RTE_ETHDEV_QUEUE_STAT_CNTRS],
    /// Total number of queue packets received that are dropped.
    pub q_errors: [u64; RTE_ETHDEV_QUEUE_STAT_CNTRS],
}

/// `struct rte_eth_link`
#[repr(C, align(8))]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_link {
    /// RTE_ETH_SPEED_NUM_
    pub link_speed: u32,
    /// The bit fields `link_duplex:1`, `link_autoneg:1`, `link_status:1`.
    pub bits: u16,
}

impl rte_eth_link {
    /// `link_duplex`: RTE_ETH_LINK_[HALF/FULL]_DUPLEX
    #[inline]
    pub fn link_duplex(&self) -> u16 {
        self.bits & 1
    }
    /// `link_status`: RTE_ETH_LINK_[DOWN/UP]
    #[inline]
    pub fn link_status(&self) -> u16 {
        (self.bits >> 2) & 1
    }
}

const _: () = assert!(size_of::<rte_eth_link>() == 8);

/// `struct rte_eth_fc_conf`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_fc_conf {
    /// High threshold value to trigger XOFF
    pub high_water: u32,
    /// Low threshold value to trigger XON
    pub low_water: u32,
    /// Pause quota in the Pause frame
    pub pause_time: u16,
    /// Is XON frame need be sent
    pub send_xon: u16,
    /// Link flow control mode (`enum rte_eth_fc_mode`).
    pub mode: u32,
    /// Forward MAC control frames
    pub mac_ctrl_frame_fwd: u8,
    /// Use Pause autoneg
    pub autoneg: u8,
}

const _: () = assert!(size_of::<rte_eth_fc_conf>() == 20);

/// `struct rte_eth_rss_reta_entry64`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_rss_reta_entry64 {
    /// Mask bits indicate which entries need to be updated/queried.
    pub mask: u64,
    /// Group of 64 redirection table entries.
    pub reta: [u16; RTE_ETH_RETA_GROUP_SIZE as usize],
}

impl Default for rte_eth_rss_reta_entry64 {
    fn default() -> Self {
        rte_eth_rss_reta_entry64 { mask: 0, reta: [0; RTE_ETH_RETA_GROUP_SIZE as usize] }
    }
}

/// `struct rte_eth_thresh`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_thresh {
    /// Ring prefetch threshold.
    pub pthresh: u8,
    /// Ring host threshold.
    pub hthresh: u8,
    /// Ring writeback threshold.
    pub wthresh: u8,
}

/// `struct rte_eth_rxconf`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_rxconf {
    /// Rx ring threshold registers.
    pub rx_thresh: rte_eth_thresh,
    /// Drives the freeing of Rx descriptors.
    pub rx_free_thresh: u16,
    /// Drop packets if no descriptors are available.
    pub rx_drop_en: u8,
    /// Do not start queue with rte_eth_dev_start().
    pub rx_deferred_start: u8,
    /// Number of descriptions in rx_seg array.
    pub rx_nseg: u16,
    /// Share group index in Rx domain and switch domain.
    pub share_group: u16,
    /// Shared Rx queue ID in group
    pub share_qid: u16,
    /// Per-queue Rx offloads to be set using RTE_ETH_RX_OFFLOAD_* flags.
    pub offloads: u64,
    /// Points to the array of segment descriptions for an entire packet.
    pub rx_seg: *mut c_void,
    /// Array of mempools to allocate Rx buffers from.
    pub rx_mempools: *mut *mut rte_mempool,
    /// Number of Rx mempools
    pub rx_nmempool: u16,
    /// Reserved for future fields
    pub reserved_64s: [u64; 2],
    /// Reserved for future fields
    pub reserved_ptrs: [*mut c_void; 2],
}

const _: () = assert!(size_of::<rte_eth_rxconf>() == 80);

/// `struct rte_eth_txconf`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_txconf {
    /// Tx ring threshold registers.
    pub tx_thresh: rte_eth_thresh,
    /// Drives the setting of RS bit on TXDs.
    pub tx_rs_thresh: u16,
    /// Start freeing Tx buffers if there are less free descriptors than this value.
    pub tx_free_thresh: u16,
    /// Do not start queue with rte_eth_dev_start().
    pub tx_deferred_start: u8,
    /// Per-queue Tx offloads to be set using RTE_ETH_TX_OFFLOAD_* flags.
    pub offloads: u64,
    /// Reserved for future fields
    pub reserved_64s: [u64; 2],
    /// Reserved for future fields
    pub reserved_ptrs: [*mut c_void; 2],
}

const _: () = assert!(size_of::<rte_eth_txconf>() == 56);

/// `struct rte_eth_rxseg_capa` (bit fields `multi_pools:1`, `offset_allowed:1`,
/// `offset_align_log2:4` in `bits`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_rxseg_capa {
    /// The bit fields.
    pub bits: u32,
    /// Maximum amount of segments to split.
    pub max_nseg: u16,
    /// Reserved field.
    pub reserved: u16,
}

/// `struct rte_eth_desc_lim`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_desc_lim {
    /// Max allowed number of descriptors.
    pub nb_max: u16,
    /// Min allowed number of descriptors.
    pub nb_min: u16,
    /// Number of descriptors should be aligned to.
    pub nb_align: u16,
    /// Max number of segments per whole packet.
    pub nb_seg_max: u16,
    /// Max number of segments per one MTU.
    pub nb_mtu_seg_max: u16,
}

/// `struct rte_eth_dev_portconf`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct rte_eth_dev_portconf {
    /// Device-preferred burst size
    pub burst_size: u16,
    /// Device-preferred size of queue rings
    pub ring_size: u16,
    /// Device-preferred number of queues
    pub nb_queues: u16,
}

/// `struct rte_eth_switch_info`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_switch_info {
    /// switch name
    pub name: *const c_char,
    /// switch domain ID
    pub domain_id: u16,
    /// Mapping to the devices physical switch port as enumerated from the perspective of
    /// the embedded interconnect/switch.
    pub port_id: u16,
    /// Shared Rx queue sub-domain boundary.
    pub rx_domain: u16,
}

/// `struct rte_eth_dev_info`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_dev_info {
    /// Generic device information
    pub device: *mut c_void,
    /// Device Driver name.
    pub driver_name: *const c_char,
    /// Index to bound host interface, or 0 if none.
    pub if_index: c_uint,
    /// Minimum MTU allowed
    pub min_mtu: u16,
    /// Maximum MTU allowed
    pub max_mtu: u16,
    /// Device flags
    pub dev_flags: *const u32,
    /// Minimum Rx buffer size per descriptor supported by HW.
    pub min_rx_bufsize: u32,
    /// Maximum configurable length of Rx pkt.
    pub max_rx_pktlen: u32,
    /// Maximum configurable size of LRO aggregated packet.
    pub max_lro_pkt_size: u32,
    /// Maximum number of Rx queues.
    pub max_rx_queues: u16,
    /// Maximum number of Tx queues.
    pub max_tx_queues: u16,
    /// Maximum number of MAC addresses.
    pub max_mac_addrs: u32,
    /// Maximum number of hash MAC addresses for MTA and UTA.
    pub max_hash_mac_addrs: u32,
    /// Maximum number of VFs.
    pub max_vfs: u16,
    /// Maximum number of VMDq pools.
    pub max_vmdq_pools: u16,
    /// Segmentation capability.
    pub rx_seg_capa: rte_eth_rxseg_capa,
    /// All Rx offload capabilities including all per-queue ones
    pub rx_offload_capa: u64,
    /// All Tx offload capabilities including all per-queue ones
    pub tx_offload_capa: u64,
    /// Device per-queue Rx offload capabilities.
    pub rx_queue_offload_capa: u64,
    /// Device per-queue Tx offload capabilities.
    pub tx_queue_offload_capa: u64,
    /// Device redirection table size, the total number of entries.
    pub reta_size: u16,
    /// Hash key size in bytes
    pub hash_key_size: u8,
    /// Bit mask of RSS offloads, the bit offset also means flow type
    pub flow_type_rss_offloads: u64,
    /// Default Rx configuration
    pub default_rxconf: rte_eth_rxconf,
    /// Default Tx configuration
    pub default_txconf: rte_eth_txconf,
    /// First queue ID for VMDq pools.
    pub vmdq_queue_base: u16,
    /// Queue number for VMDq pools.
    pub vmdq_queue_num: u16,
    /// First ID of VMDq pools.
    pub vmdq_pool_base: u16,
    /// Rx descriptors limits
    pub rx_desc_lim: rte_eth_desc_lim,
    /// Tx descriptors limits
    pub tx_desc_lim: rte_eth_desc_lim,
    /// Supported speeds bitmap (RTE_ETH_LINK_SPEED_).
    pub speed_capa: u32,
    /// Number of Rx queues.
    pub nb_rx_queues: u16,
    /// Number of Tx queues.
    pub nb_tx_queues: u16,
    /// Maximum number of Rx mempools supported per Rx queue.
    pub max_rx_mempools: u16,
    /// Rx parameter recommendations
    pub default_rxportconf: rte_eth_dev_portconf,
    /// Tx parameter recommendations
    pub default_txportconf: rte_eth_dev_portconf,
    /// Generic device capabilities (RTE_ETH_DEV_CAPA_).
    pub dev_capa: u64,
    /// Switching information for ports on a device with a embedded managed
    /// interconnect/switch.
    pub switch_info: rte_eth_switch_info,
    /// Supported error handling mode (`enum rte_eth_err_handle_mode`).
    pub err_handle_mode: u32,
    /// Reserved for future fields
    pub reserved_64s: [u64; 2],
    /// Reserved for future fields
    pub reserved_ptrs: [*mut c_void; 2],
}

const _: () = assert!(offset_of!(rte_eth_dev_info, max_rx_queues) == 44);
const _: () = assert!(offset_of!(rte_eth_dev_info, rx_offload_capa) == 72);
const _: () = assert!(offset_of!(rte_eth_dev_info, reta_size) == 104);
const _: () = assert!(offset_of!(rte_eth_dev_info, flow_type_rss_offloads) == 112);
const _: () = assert!(offset_of!(rte_eth_dev_info, default_rxconf) == 120);
const _: () = assert!(offset_of!(rte_eth_dev_info, default_txconf) == 200);

impl Default for rte_eth_dev_info {
    /// `rte_eth_dev_info _dev_info = {}`: all zero.
    fn default() -> Self {
        // SAFETY: every field is an integer, a raw pointer or an aggregate of them, for
        // which all-zero is a valid value (the C `= {}`).
        unsafe { std::mem::zeroed() }
    }
}

/// `struct rte_eth_rxmode`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_rxmode {
    /// The multi-queue packet distribution mode to be used, e.g. RSS.
    pub mq_mode: u32,
    /// Requested MTU.
    pub mtu: u32,
    /// Maximum allowed size of LRO aggregated packet.
    pub max_lro_pkt_size: u32,
    /// Per-port Rx offloads to be set using RTE_ETH_RX_OFFLOAD_* flags.
    pub offloads: u64,
    /// Reserved for future fields
    pub reserved_64s: [u64; 2],
    /// Reserved for future fields
    pub reserved_ptrs: [*mut c_void; 2],
}

/// `struct rte_eth_txmode`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_txmode {
    /// Tx multi-queues mode.
    pub mq_mode: u32,
    /// Per-port Tx offloads to be set using RTE_ETH_TX_OFFLOAD_* flags.
    pub offloads: u64,
    /// VLAN id
    pub pvid: u16,
    /// The bit fields `hw_vlan_reject_tagged:1`, `hw_vlan_reject_untagged:1`,
    /// `hw_vlan_insert_pvid:1`.
    pub hw_vlan_bits: u8,
    /// Reserved for future fields
    pub reserved_64s: [u64; 2],
    /// Reserved for future fields
    pub reserved_ptrs: [*mut c_void; 2],
}

/// `struct rte_eth_rss_conf`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_rss_conf {
    /// If not NULL, 40-byte hash key.
    pub rss_key: *mut u8,
    /// hash key length in bytes.
    pub rss_key_len: u8,
    /// Hash functions to apply.
    pub rss_hf: u64,
}

/// The `pool_map` entries of the VMDq configurations.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_vmdq_pool_map {
    /// The VLAN ID of the received frame
    pub vlan_id: u16,
    /// Bitmask of pools for packet Rx
    pub pools: u64,
}

/// `struct rte_eth_vmdq_dcb_conf`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_vmdq_dcb_conf {
    /// With DCB, 16 or 32 pools (`enum rte_eth_nb_pools`).
    pub nb_queue_pools: u32,
    /// If non-zero, use a default pool
    pub enable_default_pool: u8,
    /// The default pool, if applicable
    pub default_pool: u8,
    /// We can have up to 64 filters/mappings
    pub nb_pool_maps: u8,
    /// VMDq VLAN pool maps.
    pub pool_map: [rte_eth_vmdq_pool_map; RTE_ETH_VMDQ_MAX_VLAN_FILTERS],
    /// Selects a queue in a pool
    pub dcb_tc: [u8; RTE_ETH_DCB_NUM_USER_PRIORITIES],
}

/// `struct rte_eth_dcb_rx_conf`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_dcb_rx_conf {
    /// Possible DCB TCs, 4 or 8 TCs (`enum rte_eth_nb_tcs`).
    pub nb_tcs: u32,
    /// Traffic class each UP mapped to.
    pub dcb_tc: [u8; RTE_ETH_DCB_NUM_USER_PRIORITIES],
}

/// `struct rte_eth_vmdq_rx_conf`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_vmdq_rx_conf {
    /// VMDq only mode, 8 or 64 pools (`enum rte_eth_nb_pools`).
    pub nb_queue_pools: u32,
    /// If non-zero, use a default pool
    pub enable_default_pool: u8,
    /// The default pool, if applicable
    pub default_pool: u8,
    /// Enable VT loop back
    pub enable_loop_back: u8,
    /// We can have up to 64 filters/mappings
    pub nb_pool_maps: u8,
    /// Flags from RTE_ETH_VMDQ_ACCEPT_*
    pub rx_mode: u32,
    /// VMDq VLAN pool maps.
    pub pool_map: [rte_eth_vmdq_pool_map; RTE_ETH_VMDQ_MAX_VLAN_FILTERS],
}

/// The `rx_adv_conf` member of `struct rte_eth_conf`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_rx_adv_conf {
    /// Port RSS configuration
    pub rss_conf: rte_eth_rss_conf,
    /// Port VMDq+DCB configuration.
    pub vmdq_dcb_conf: rte_eth_vmdq_dcb_conf,
    /// Port DCB Rx configuration.
    pub dcb_rx_conf: rte_eth_dcb_rx_conf,
    /// Port VMDq Rx configuration.
    pub vmdq_rx_conf: rte_eth_vmdq_rx_conf,
}

/// The `tx_adv_conf` union of `struct rte_eth_conf` (`rte_eth_vmdq_dcb_tx_conf`,
/// `rte_eth_dcb_tx_conf`, `rte_eth_vmdq_tx_conf`).
#[repr(C)]
#[derive(Clone, Copy)]
pub union rte_eth_tx_adv_conf {
    /// Port VMDq+DCB Tx configuration (`nb_queue_pools`, `dcb_tc[8]`).
    pub vmdq_dcb_tx_conf: [u32; 3],
    /// Port DCB Tx configuration (`nb_tcs`, `dcb_tc[8]`).
    pub dcb_tx_conf: [u32; 3],
    /// Port VMDq Tx configuration (`nb_queue_pools`).
    pub vmdq_tx_conf: u32,
}

/// `struct rte_eth_conf`
#[repr(C)]
#[derive(Clone, Copy)]
pub struct rte_eth_conf {
    /// bitmap of RTE_ETH_LINK_SPEED_XXX of speeds to be used.
    pub link_speeds: u32,
    /// Port Rx configuration.
    pub rxmode: rte_eth_rxmode,
    /// Port Tx configuration.
    pub txmode: rte_eth_txmode,
    /// Loopback operation mode.
    pub lpbk_mode: u32,
    /// Port Rx filtering configuration.
    pub rx_adv_conf: rte_eth_rx_adv_conf,
    /// Port Tx DCB configuration (union).
    pub tx_adv_conf: rte_eth_tx_adv_conf,
    /// Currently,Priority Flow Control(PFC) are supported,if DCB with PFC is needed,and the
    /// variable must be set RTE_ETH_DCB_PFC_SUPPORT.
    pub dcb_capability_en: u32,
    /// Interrupt mode configuration (bit fields `lsc:1`, `rxq:1`, `rmv:1`).
    pub intr_conf: u32,
}

const _: () = assert!(size_of::<rte_eth_rxmode>() == 56);
const _: () = assert!(size_of::<rte_eth_txmode>() == 56);
const _: () = assert!(size_of::<rte_eth_vmdq_dcb_conf>() == 1040);
const _: () = assert!(size_of::<rte_eth_vmdq_rx_conf>() == 1040);

impl Default for rte_eth_conf {
    /// `rte_eth_conf port_conf = { 0 }`: all zero.
    fn default() -> Self {
        // SAFETY: every field is an integer, a raw pointer or an aggregate of them, for
        // which all-zero is a valid value (the C `= { 0 }`).
        unsafe { std::mem::zeroed() }
    }
}

/// `eth_rx_burst_t`
pub type eth_rx_burst_t =
    unsafe extern "C" fn(rxq: *mut c_void, rx_pkts: *mut *mut rte_mbuf, nb_pkts: u16) -> u16;
/// `eth_tx_burst_t`
pub type eth_tx_burst_t =
    unsafe extern "C" fn(txq: *mut c_void, tx_pkts: *mut *mut rte_mbuf, nb_pkts: u16) -> u16;

/// `struct rte_ethdev_qdata`
#[repr(C)]
pub struct rte_ethdev_qdata {
    /// Pointer to the array of internal queue private data structures.
    pub data: *mut *mut c_void,
    /// Points to array of Rx/Tx callback data.
    pub clbk: *mut *mut c_void,
}

/// `struct rte_eth_fp_ops`: the fast-path functions and data of a port.
#[repr(C, align(64))]
pub struct rte_eth_fp_ops {
    /// PMD receive function.
    pub rx_pkt_burst: Option<eth_rx_burst_t>,
    /// Get the number of used Rx descriptors.
    pub rx_queue_count: *const c_void,
    /// Check the status of a Rx descriptor.
    pub rx_descriptor_status: *const c_void,
    /// Rx queues data.
    pub rxq: rte_ethdev_qdata,
    /// Reserved.
    pub reserved1: [usize; 3],
    /// PMD transmit function.
    pub tx_pkt_burst: Option<eth_tx_burst_t>,
    /// PMD transmit prepare function.
    pub tx_pkt_prepare: *const c_void,
    /// Check the status of a Tx descriptor.
    pub tx_descriptor_status: *const c_void,
    /// Tx queues data.
    pub txq: rte_ethdev_qdata,
    /// Reserved.
    pub reserved2: [usize; 3],
}

const _: () = assert!(offset_of!(rte_eth_fp_ops, tx_pkt_burst) == 64);
const _: () = assert!(size_of::<rte_eth_fp_ops>() == 128);

// ---------------------------------------------------------------------------
// Exported DPDK symbols
// ---------------------------------------------------------------------------

unsafe extern "C" {
    /// `struct rte_eth_fp_ops rte_eth_fp_ops[RTE_MAX_ETHPORTS]`
    pub static mut rte_eth_fp_ops: [rte_eth_fp_ops; RTE_MAX_ETHPORTS];
    /// `struct rte_mempool_ops_table rte_mempool_ops_table`
    pub static mut rte_mempool_ops_table: rte_mempool_ops_table;

    /// `void rte_exit(int exit_code, const char *format, ...)`
    pub fn rte_exit(exit_code: c_int, format: *const c_char, ...) -> !;
    /// `void __rte_panic(const char *funcname, const char *format, ...)`
    pub fn __rte_panic(funcname: *const c_char, format: *const c_char, ...) -> !;
    /// `int rte_lcore_index(int lcore_id)`
    pub fn rte_lcore_index(lcore_id: c_int) -> c_int;
    /// `unsigned int rte_socket_id(void)`
    pub fn rte_socket_id() -> c_uint;
    /// `rte_iova_t rte_mem_virt2iova(const void *virt)`
    pub fn rte_mem_virt2iova(virt: *const c_void) -> rte_iova_t;
    /// `int rte_vfio_container_dma_map(int container_fd, uint64_t vaddr, uint64_t iova,
    /// uint64_t len)`
    pub fn rte_vfio_container_dma_map(
        container_fd: c_int,
        vaddr: u64,
        iova: u64,
        len: u64,
    ) -> c_int;

    /// `uint32_t rte_mempool_calc_obj_size(uint32_t elt_size, uint32_t flags,
    /// struct rte_mempool_objsz *sz)`
    pub fn rte_mempool_calc_obj_size(elt_size: u32, flags: u32, sz: *mut rte_mempool_objsz) -> u32;
    /// `struct rte_mempool *rte_mempool_create(...)`
    pub fn rte_mempool_create(
        name: *const c_char,
        n: c_uint,
        elt_size: c_uint,
        cache_size: c_uint,
        private_data_size: c_uint,
        mp_init: Option<rte_mempool_ctor_t>,
        mp_init_arg: *mut c_void,
        obj_init: Option<rte_mempool_obj_cb_t>,
        obj_init_arg: *mut c_void,
        socket_id: c_int,
        flags: c_uint,
    ) -> *mut rte_mempool;
    /// `struct rte_mempool *rte_mempool_create_empty(...)`
    pub fn rte_mempool_create_empty(
        name: *const c_char,
        n: c_uint,
        elt_size: c_uint,
        cache_size: c_uint,
        private_data_size: c_uint,
        socket_id: c_int,
        flags: c_uint,
    ) -> *mut rte_mempool;
    /// `int rte_mempool_populate_virt(struct rte_mempool *mp, char *addr, size_t len,
    /// size_t pg_sz, rte_mempool_memchunk_free_cb_t *free_cb, void *opaque)`
    pub fn rte_mempool_populate_virt(
        mp: *mut rte_mempool,
        addr: *mut c_char,
        len: usize,
        pg_sz: usize,
        free_cb: Option<rte_mempool_memchunk_free_cb_t>,
        opaque: *mut c_void,
    ) -> c_int;
    /// `uint32_t rte_mempool_obj_iter(struct rte_mempool *mp, rte_mempool_obj_cb_t *obj_cb,
    /// void *obj_cb_arg)`
    pub fn rte_mempool_obj_iter(
        mp: *mut rte_mempool,
        obj_cb: Option<rte_mempool_obj_cb_t>,
        obj_cb_arg: *mut c_void,
    ) -> u32;
    /// `void rte_pktmbuf_pool_init(struct rte_mempool *mp, void *opaque_arg)`
    pub fn rte_pktmbuf_pool_init(mp: *mut rte_mempool, opaque_arg: *mut c_void);
    /// `void rte_pktmbuf_init(struct rte_mempool *mp, void *opaque_arg, void *m, unsigned i)`
    pub fn rte_pktmbuf_init(
        mp: *mut rte_mempool,
        opaque_arg: *mut c_void,
        m: *mut c_void,
        i: c_uint,
    );

    /// `uint16_t rte_eth_dev_count_avail(void)`
    pub fn rte_eth_dev_count_avail() -> u16;
    /// `int rte_eth_dev_info_get(uint16_t port_id, struct rte_eth_dev_info *dev_info)`
    pub fn rte_eth_dev_info_get(port_id: u16, dev_info: *mut rte_eth_dev_info) -> c_int;
    /// `int rte_eth_macaddr_get(uint16_t port_id, struct rte_ether_addr *mac_addr)`
    pub fn rte_eth_macaddr_get(port_id: u16, mac_addr: *mut rte_ether_addr) -> c_int;
    /// `int rte_eth_dev_configure(uint16_t port_id, uint16_t nb_rx_queue,
    /// uint16_t nb_tx_queue, const struct rte_eth_conf *eth_conf)`
    pub fn rte_eth_dev_configure(
        port_id: u16,
        nb_rx_queue: u16,
        nb_tx_queue: u16,
        eth_conf: *const rte_eth_conf,
    ) -> c_int;
    /// `int rte_eth_dev_flow_ctrl_get(uint16_t port_id, struct rte_eth_fc_conf *fc_conf)`
    pub fn rte_eth_dev_flow_ctrl_get(port_id: u16, fc_conf: *mut rte_eth_fc_conf) -> c_int;
    /// `int rte_eth_dev_flow_ctrl_set(uint16_t port_id, struct rte_eth_fc_conf *fc_conf)`
    pub fn rte_eth_dev_flow_ctrl_set(port_id: u16, fc_conf: *mut rte_eth_fc_conf) -> c_int;
    /// `int rte_eth_dev_start(uint16_t port_id)`
    pub fn rte_eth_dev_start(port_id: u16) -> c_int;
    /// `int rte_eth_stats_get(uint16_t port_id, struct rte_eth_stats *stats)`
    pub fn rte_eth_stats_get(port_id: u16, stats: *mut rte_eth_stats) -> c_int;
    /// `int rte_eth_xstats_get_names(uint16_t port_id, struct rte_eth_xstat_name *xstats_names,
    /// unsigned int size)`
    pub fn rte_eth_xstats_get_names(
        port_id: u16,
        xstats_names: *mut rte_eth_xstat_name,
        size: c_uint,
    ) -> c_int;
    /// `int rte_eth_xstats_get(uint16_t port_id, struct rte_eth_xstat *xstats, unsigned int n)`
    pub fn rte_eth_xstats_get(port_id: u16, xstats: *mut rte_eth_xstat, n: c_uint) -> c_int;
    /// `int rte_eth_link_get_nowait(uint16_t port_id, struct rte_eth_link *link)`
    pub fn rte_eth_link_get_nowait(port_id: u16, link: *mut rte_eth_link) -> c_int;
    /// `int rte_eth_dev_rss_reta_update(uint16_t port_id,
    /// struct rte_eth_rss_reta_entry64 *reta_conf, uint16_t reta_size)`
    pub fn rte_eth_dev_rss_reta_update(
        port_id: u16,
        reta_conf: *mut rte_eth_rss_reta_entry64,
        reta_size: u16,
    ) -> c_int;
    /// `int rte_eth_dev_socket_id(uint16_t port_id)`
    pub fn rte_eth_dev_socket_id(port_id: u16) -> c_int;
    /// `int rte_eth_rx_queue_setup(uint16_t port_id, uint16_t rx_queue_id, uint16_t nb_rx_desc,
    /// unsigned int socket_id, const struct rte_eth_rxconf *rx_conf,
    /// struct rte_mempool *mb_pool)`
    pub fn rte_eth_rx_queue_setup(
        port_id: u16,
        rx_queue_id: u16,
        nb_rx_desc: u16,
        socket_id: c_uint,
        rx_conf: *const rte_eth_rxconf,
        mb_pool: *mut rte_mempool,
    ) -> c_int;
    /// `int rte_eth_tx_queue_setup(uint16_t port_id, uint16_t tx_queue_id, uint16_t nb_tx_desc,
    /// unsigned int socket_id, const struct rte_eth_txconf *tx_conf)`
    pub fn rte_eth_tx_queue_setup(
        port_id: u16,
        tx_queue_id: u16,
        nb_tx_desc: u16,
        socket_id: c_uint,
        tx_conf: *const rte_eth_txconf,
    ) -> c_int;
    /// `uint16_t rte_eth_call_rx_callbacks(uint16_t port_id, uint16_t queue_id,
    /// struct rte_mbuf **rx_pkts, uint16_t nb_rx, uint16_t nb_pkts, void *opaque)`
    pub fn rte_eth_call_rx_callbacks(
        port_id: u16,
        queue_id: u16,
        rx_pkts: *mut *mut rte_mbuf,
        nb_rx: u16,
        nb_pkts: u16,
        opaque: *mut c_void,
    ) -> u16;
    /// `uint16_t rte_eth_call_tx_callbacks(uint16_t port_id, uint16_t queue_id,
    /// struct rte_mbuf **tx_pkts, uint16_t nb_pkts, void *opaque)`
    pub fn rte_eth_call_tx_callbacks(
        port_id: u16,
        queue_id: u16,
        tx_pkts: *mut *mut rte_mbuf,
        nb_pkts: u16,
        opaque: *mut c_void,
    ) -> u16;
}

/// `rte_exit(exit_code, format, ...)` with the message formatted on the Rust side and
/// handed to DPDK as `"%s"`.
#[cold]
pub fn rte_exit_fmt(exit_code: c_int, msg: std::fmt::Arguments<'_>) -> ! {
    let s = CString::new(std::fmt::format(msg)).unwrap_or_default();
    // SAFETY: "%s" takes one NUL-terminated string, which `s` is and outlives the call.
    unsafe { rte_exit(exit_code, c"%s".as_ptr(), s.as_ptr()) }
}

// ---------------------------------------------------------------------------
// rte_lcore.h: rte_lcore_id()
// ---------------------------------------------------------------------------

/// The lcore id of this thread, or `LCORE_ID_UNKNOWN` before the first lookup.
const LCORE_ID_UNKNOWN: c_uint = c_uint::MAX - 1;

thread_local! {
    static LCORE_ID: Cell<c_uint> = const { Cell::new(LCORE_ID_UNKNOWN) };
}

/// Finds the lcore id of this thread with the EAL's exported functions: the EAL lcore whose
/// core index is the index of this thread, or `LCORE_ID_ANY` for a non-EAL thread.
#[cold]
fn lookup_lcore_id() -> c_uint {
    // SAFETY: rte_lcore_index() only reads the EAL's lcore configuration.
    let index = unsafe { rte_lcore_index(-1) };
    if index < 0 {
        return LCORE_ID_ANY;
    }
    for lcore_id in 0..RTE_MAX_LCORE as c_int {
        // SAFETY: as above, with an lcore id below RTE_MAX_LCORE.
        if unsafe { rte_lcore_index(lcore_id) } == index {
            return lcore_id as c_uint;
        }
    }
    LCORE_ID_ANY
}

/// `rte_lcore_id()`: the id of the lcore this thread runs on.
///
/// DPDK reads its per-thread `per_lcore__lcore_id`; a thread-local symbol of a C library
/// cannot be linked from stable Rust, so the id is looked up once per thread through
/// `rte_lcore_index()` and kept in a thread-local of our own. The EAL never changes the
/// lcore of a thread, so the cached value stays exact.
#[inline(always)]
pub fn rte_lcore_id() -> c_uint {
    LCORE_ID.with(|c| {
        let id = c.get();
        if id != LCORE_ID_UNKNOWN {
            return id;
        }
        let id = lookup_lcore_id();
        c.set(id);
        id
    })
}

// ---------------------------------------------------------------------------
// rte_mempool.h: inline functions
// ---------------------------------------------------------------------------

/// `RTE_MEMPOOL_HEADER_SIZE(mp, cs)`: the size of the mempool header and its local caches.
#[inline(always)]
pub const fn rte_mempool_header_size(cs: u32) -> usize {
    size_of::<rte_mempool>()
        + if cs == 0 { 0 } else { size_of::<rte_mempool_cache>() * RTE_MAX_LCORE }
}

/// `rte_mempool_get_ops(ops_index)`
///
/// # Safety
/// The EAL must be initialized (the table is DPDK's).
#[inline(always)]
pub unsafe fn rte_mempool_get_ops(ops_index: c_int) -> *mut rte_mempool_ops {
    // RTE_VERIFY((ops_index >= 0) && (ops_index < RTE_MEMPOOL_MAX_OPS_IDX));
    if !(ops_index >= 0 && (ops_index as usize) < RTE_MEMPOOL_MAX_OPS_IDX) {
        // SAFETY: rte_panic with its format and the matching arguments.
        unsafe {
            __rte_panic(
                c"rte_mempool_get_ops".as_ptr(),
                c"line %d\tassert \"%s\" failed\n".as_ptr(),
                line!() as c_int,
                c"(ops_index >= 0) && (ops_index < RTE_MEMPOOL_MAX_OPS_IDX)".as_ptr(),
            )
        }
    }
    // SAFETY: the index was checked against the size of the table.
    unsafe {
        (&raw mut rte_mempool_ops_table.ops).cast::<rte_mempool_ops>().add(ops_index as usize)
    }
}

/// `rte_mempool_ops_dequeue_bulk(mp, obj_table, n)`: wrapper for the mempool_ops dequeue
/// callback.
///
/// # Safety
/// `mp` is a live mempool and `obj_table` has room for `n` pointers.
#[inline(always)]
pub unsafe fn rte_mempool_ops_dequeue_bulk(
    mp: *mut rte_mempool,
    obj_table: *mut *mut c_void,
    n: c_uint,
) -> c_int {
    // rte_mempool_trace_ops_dequeue_bulk(mp, obj_table, n): fast-path trace point, compiled
    // out.
    // SAFETY: guaranteed by the caller; every registered ops has a dequeue callback.
    unsafe {
        let ops = rte_mempool_get_ops((*mp).ops_index);
        // RTE_MEMPOOL_STAT_ADD on success: mempool statistics are compiled out.
        ((*ops).dequeue.unwrap_unchecked())(mp, obj_table, n)
    }
}

/// `rte_mempool_ops_enqueue_bulk(mp, obj_table, n)`: wrapper for the mempool_ops enqueue
/// callback.
///
/// # Safety
/// `mp` is a live mempool and `obj_table` holds `n` objects of it.
#[inline(always)]
pub unsafe fn rte_mempool_ops_enqueue_bulk(
    mp: *mut rte_mempool,
    obj_table: *const *mut c_void,
    n: c_uint,
) -> c_int {
    // RTE_MEMPOOL_STAT_ADD(mp, put_common_pool_bulk/objs): compiled out.
    // rte_mempool_trace_ops_enqueue_bulk(mp, obj_table, n): compiled out.
    // SAFETY: guaranteed by the caller; every registered ops has an enqueue callback.
    unsafe {
        let ops = rte_mempool_get_ops((*mp).ops_index);
        ((*ops).enqueue.unwrap_unchecked())(mp, obj_table, n)
    }
}

/// `rte_mempool_default_cache(mp, lcore_id)`: the default cache of the mempool for the
/// given lcore, or null.
///
/// # Safety
/// `mp` is a live mempool.
#[inline(always)]
pub unsafe fn rte_mempool_default_cache(
    mp: *mut rte_mempool,
    lcore_id: c_uint,
) -> *mut rte_mempool_cache {
    // SAFETY: guaranteed by the caller; a pool with a cache has RTE_MAX_LCORE of them.
    unsafe {
        if (*mp).cache_size == 0 {
            return ptr::null_mut();
        }

        if lcore_id as usize >= RTE_MAX_LCORE {
            return ptr::null_mut();
        }

        // rte_mempool_trace_default_cache(...): compiled out.
        (*mp).local_cache.add(lcore_id as usize)
    }
}

/// `rte_mempool_do_generic_put(mp, obj_table, n, cache)`: put several objects back in the
/// mempool, through the cache when there is one.
///
/// # Safety
/// `mp` is a live mempool, `obj_table` holds `n` objects of it and `cache` is null or a cache
/// of `mp` that only this thread uses.
#[inline(always)]
pub unsafe fn rte_mempool_do_generic_put(
    mp: *mut rte_mempool,
    obj_table: *const *mut c_void,
    n: c_uint,
    cache: *mut rte_mempool_cache,
) {
    // SAFETY: guaranteed by the caller; the cache has room for flushthresh objects.
    unsafe {
        let cache_objs: *mut *mut c_void;

        /* No cache provided */
        if cache.is_null() {
            // goto driver_enqueue;
            // RTE_MEMPOOL_STAT_ADD(mp, put_bulk/put_objs): compiled out.
            /* push objects to the backend */
            rte_mempool_ops_enqueue_bulk(mp, obj_table, n);
            return;
        }

        /* increment stat now, adding in mempool always success */
        // RTE_MEMPOOL_CACHE_STAT_ADD(cache, put_bulk/put_objs): compiled out.

        /* The request itself is too big for the cache */
        if n > (*cache).flushthresh {
            // goto driver_enqueue_stats_incremented;
            /* push objects to the backend */
            rte_mempool_ops_enqueue_bulk(mp, obj_table, n);
            return;
        }

        /*
         * The cache follows the following algorithm:
         *   1. If the objects cannot be added to the cache without crossing
         *      the flush threshold, flush the cache to the backend.
         *   2. Add the objects to the cache.
         */

        let objs = (&raw mut (*cache).objs).cast::<*mut c_void>();
        if (*cache).len + n <= (*cache).flushthresh {
            cache_objs = objs.add((*cache).len as usize);
            (*cache).len += n;
        } else {
            cache_objs = objs;
            rte_mempool_ops_enqueue_bulk(mp, cache_objs, (*cache).len);
            (*cache).len = n;
        }

        /* Add the objects to the cache. */
        rte_memcpy(cache_objs.cast(), obj_table.cast(), size_of::<*mut c_void>() * n as usize);
    }
}

/// `rte_mempool_generic_put(mp, obj_table, n, cache)`
///
/// # Safety
/// As [`rte_mempool_do_generic_put`].
#[inline(always)]
pub unsafe fn rte_mempool_generic_put(
    mp: *mut rte_mempool,
    obj_table: *const *mut c_void,
    n: c_uint,
    cache: *mut rte_mempool_cache,
) {
    // rte_mempool_trace_generic_put(...): compiled out.
    // RTE_MEMPOOL_CHECK_COOKIES(mp, obj_table, n, 0): mempool debug is compiled out.
    // SAFETY: guaranteed by the caller.
    unsafe { rte_mempool_do_generic_put(mp, obj_table, n, cache) }
}

/// `rte_mempool_put_bulk(mp, obj_table, n)`: put several objects back in the mempool.
///
/// # Safety
/// `mp` is a live mempool and `obj_table` holds `n` objects of it.
#[inline(always)]
pub unsafe fn rte_mempool_put_bulk(mp: *mut rte_mempool, obj_table: *const *mut c_void, n: c_uint) {
    // SAFETY: guaranteed by the caller; the lcore's cache is only used by this thread.
    unsafe {
        let cache = rte_mempool_default_cache(mp, rte_lcore_id());
        // rte_mempool_trace_put_bulk(...): compiled out.
        rte_mempool_generic_put(mp, obj_table, n, cache);
    }
}

/// `rte_mempool_put(mp, obj)`: put one object back in the mempool.
///
/// # Safety
/// `mp` is a live mempool and `obj` an object of it.
#[inline(always)]
pub unsafe fn rte_mempool_put(mp: *mut rte_mempool, obj: *mut c_void) {
    // SAFETY: guaranteed by the caller.
    unsafe { rte_mempool_put_bulk(mp, &obj, 1) }
}

/// `rte_mempool_do_generic_get(mp, obj_table, n, cache)`: get several objects from the
/// mempool, through the cache when there is one.
///
/// DPDK has a first branch for an `n` known at build time (`__builtin_constant_p(n)`)
/// that gives the same result as the general one; after inlining the compiler folds the
/// general branch the same way.
///
/// # Safety
/// `mp` is a live mempool, `obj_table` has room for `n` pointers and `cache` is null or a
/// cache of `mp` that only this thread uses.
#[inline(always)]
pub unsafe fn rte_mempool_do_generic_get(
    mp: *mut rte_mempool,
    mut obj_table: *mut *mut c_void,
    n: c_uint,
    cache: *mut rte_mempool_cache,
) -> c_int {
    // SAFETY: guaranteed by the caller; the cache holds 2 * RTE_MEMPOOL_CACHE_MAX_SIZE
    // objects and the fill below never asks for more.
    unsafe {
        let remaining: c_uint;

        'driver_dequeue: {
            /* No cache provided */
            if cache.is_null() {
                remaining = n;
                break 'driver_dequeue;
            }

            /* The cache is a stack, so copy will be in reverse order. */
            let objs = (&raw mut (*cache).objs).cast::<*mut c_void>();
            let mut cache_objs = objs.add((*cache).len as usize);

            /*
             * Use the cache as much as we have to return hot objects first.
             */
            let len = n.min((*cache).len);
            (*cache).len -= len;
            remaining = n - len;
            for _ in 0..len {
                cache_objs = cache_objs.sub(1);
                *obj_table = *cache_objs;
                obj_table = obj_table.add(1);
            }

            if remaining == 0 {
                /* The entire request is satisfied from the cache. */
                // RTE_MEMPOOL_CACHE_STAT_ADD(cache, get_success_bulk/objs): compiled out.
                return 0;
            }

            /* if dequeue below would overflow mem allocated for cache */
            if remaining as usize > RTE_MEMPOOL_CACHE_MAX_SIZE {
                break 'driver_dequeue;
            }

            /* Fill the cache from the backend; fetch size + remaining objects. */
            let r = rte_mempool_ops_dequeue_bulk(mp, objs, (*cache).size + remaining);
            if r < 0 {
                /*
                 * We are buffer constrained, and not able to allocate
                 * cache + remaining.
                 * Do not fill the cache, just satisfy the remaining part of
                 * the request directly from the backend.
                 */
                break 'driver_dequeue;
            }

            /* Satisfy the remaining part of the request from the filled cache. */
            cache_objs = objs.add(((*cache).size + remaining) as usize);
            for _ in 0..remaining {
                cache_objs = cache_objs.sub(1);
                *obj_table = *cache_objs;
                obj_table = obj_table.add(1);
            }

            (*cache).len = (*cache).size;

            // RTE_MEMPOOL_CACHE_STAT_ADD(cache, get_success_bulk/objs): compiled out.
            return 0;
        }

        // driver_dequeue:

        /* Get remaining objects directly from the backend. */
        let ret: c_int = rte_mempool_ops_dequeue_bulk(mp, obj_table, remaining);

        // On failure: RTE_MEMPOOL_STAT_ADD(mp, get_fail_bulk/objs): compiled out.
        if ret < 0 && !cache.is_null() {
            (*cache).len = n - remaining;
            /*
             * No further action is required to roll the first part
             * of the request back into the cache, as objects in
             * the cache are intact.
             */
        }
        // else: RTE_MEMPOOL_[CACHE_]STAT_ADD(..., get_success_bulk/objs): compiled out.

        ret
    }
}

/// `rte_mempool_generic_get(mp, obj_table, n, cache)`
///
/// # Safety
/// As [`rte_mempool_do_generic_get`].
#[inline(always)]
pub unsafe fn rte_mempool_generic_get(
    mp: *mut rte_mempool,
    obj_table: *mut *mut c_void,
    n: c_uint,
    cache: *mut rte_mempool_cache,
) -> c_int {
    // SAFETY: guaranteed by the caller.
    // RTE_MEMPOOL_CHECK_COOKIES(mp, obj_table, n, 1) on success: compiled out.
    // rte_mempool_trace_generic_get(...): compiled out.
    unsafe { rte_mempool_do_generic_get(mp, obj_table, n, cache) }
}

/// `rte_mempool_get_bulk(mp, obj_table, n)`: get several objects from the mempool.
///
/// # Safety
/// `mp` is a live mempool and `obj_table` has room for `n` pointers.
#[inline(always)]
pub unsafe fn rte_mempool_get_bulk(
    mp: *mut rte_mempool,
    obj_table: *mut *mut c_void,
    n: c_uint,
) -> c_int {
    // SAFETY: guaranteed by the caller; the lcore's cache is only used by this thread.
    unsafe {
        let cache = rte_mempool_default_cache(mp, rte_lcore_id());
        // rte_mempool_trace_get_bulk(...): compiled out.
        rte_mempool_generic_get(mp, obj_table, n, cache)
    }
}

/// `rte_mempool_get(mp, obj_p)`: get one object from the mempool.
///
/// # Safety
/// `mp` is a live mempool and `obj_p` is valid for writes.
#[inline(always)]
pub unsafe fn rte_mempool_get(mp: *mut rte_mempool, obj_p: *mut *mut c_void) -> c_int {
    // SAFETY: guaranteed by the caller.
    unsafe { rte_mempool_get_bulk(mp, obj_p, 1) }
}

/// `rte_mempool_get_priv(mp)`: the private data area of the mempool.
///
/// # Safety
/// `mp` is a live mempool.
#[inline(always)]
pub unsafe fn rte_mempool_get_priv(mp: *mut rte_mempool) -> *mut c_void {
    // SAFETY: the private area follows the header and the caches in the same allocation.
    unsafe { mp.cast::<u8>().add(rte_mempool_header_size((*mp).cache_size)).cast() }
}

/// `rte_mempool_virt2iova(elt)`: the IO address of a mempool object.
///
/// # Safety
/// `elt` is an object of a mempool.
#[inline(always)]
pub unsafe fn rte_mempool_virt2iova(elt: *const c_void) -> rte_iova_t {
    // SAFETY: every mempool object is preceded by its header.
    unsafe {
        let hdr =
            elt.cast::<u8>().sub(size_of::<rte_mempool_objhdr>()).cast::<rte_mempool_objhdr>();
        (*hdr).iova
    }
}

/// `rte_memcpy(dst, src, n)`: DPDK's vectorized memcpy; `copy_nonoverlapping` lowers to the
/// platform's vectorized memcpy the same way.
///
/// # Safety
/// `dst` and `src` are valid for `n` bytes and do not overlap.
#[inline(always)]
pub unsafe fn rte_memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
    // SAFETY: guaranteed by the caller.
    unsafe { ptr::copy_nonoverlapping(src.cast::<u8>(), dst.cast::<u8>(), n) };
    dst
}

// ---------------------------------------------------------------------------
// rte_mbuf.h: inline functions
// ---------------------------------------------------------------------------

/// `rte_pktmbuf_priv_size(mp)`: the size of the private area of each mbuf of the pool.
///
/// # Safety
/// `mp` is a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_pktmbuf_priv_size(mp: *mut rte_mempool) -> u16 {
    // SAFETY: the private area of a pktmbuf pool is a rte_pktmbuf_pool_private.
    unsafe { (*rte_mempool_get_priv(mp).cast::<rte_pktmbuf_pool_private>()).mbuf_priv_size }
}

/// `rte_pktmbuf_priv_flags(mp)`: the flags of the pool.
///
/// # Safety
/// `mp` is a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_pktmbuf_priv_flags(mp: *mut rte_mempool) -> u32 {
    // SAFETY: as in rte_pktmbuf_priv_size.
    unsafe { (*rte_mempool_get_priv(mp).cast::<rte_pktmbuf_pool_private>()).flags }
}

/// `rte_pktmbuf_data_room_size(mp)`: the data room size of each mbuf of the pool.
///
/// # Safety
/// `mp` is a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_pktmbuf_data_room_size(mp: *mut rte_mempool) -> u16 {
    // SAFETY: as in rte_pktmbuf_priv_size.
    unsafe { (*rte_mempool_get_priv(mp).cast::<rte_pktmbuf_pool_private>()).mbuf_data_room_size }
}

/// `rte_mbuf_to_baddr(md)`: the address of the buffer embedded in the given mbuf.
///
/// # Safety
/// `md` is an mbuf of a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_mbuf_to_baddr(md: *mut rte_mbuf) -> *mut c_char {
    // SAFETY: the buffer follows the mbuf and its private area in the pool element.
    unsafe {
        md.cast::<c_char>().add(size_of::<rte_mbuf>() + rte_pktmbuf_priv_size((*md).pool) as usize)
    }
}

/// `rte_mbuf_iova_set(m, iova)`
///
/// # Safety
/// `m` is valid for writes.
#[inline(always)]
pub unsafe fn rte_mbuf_iova_set(m: *mut rte_mbuf, iova: rte_iova_t) {
    // SAFETY: guaranteed by the caller.
    unsafe { (*m).buf_iova = iova };
}

/// `rte_mbuf_refcnt_read(m)`: `rte_atomic_load_explicit(&m->refcnt, relaxed)` (a plain
/// load, `m->refcnt`, without `RTE_MBUF_REFCNT_ATOMIC`; a relaxed load compiles to the same).
///
/// # Safety
/// `m` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_mbuf_refcnt_read(m: *const rte_mbuf) -> u16 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*m).refcnt.load(Ordering::Relaxed) }
}

/// `rte_mbuf_refcnt_set(m, new_value)`: a relaxed store (a plain store without
/// `RTE_MBUF_REFCNT_ATOMIC`).
///
/// # Safety
/// `m` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_mbuf_refcnt_set(m: *mut rte_mbuf, new_value: u16) {
    // SAFETY: guaranteed by the caller.
    unsafe { (*m).refcnt.store(new_value, Ordering::Relaxed) };
}

/// `rte_mbuf_refcnt_update(m, value)`.
///
/// With `RTE_MBUF_REFCNT_ATOMIC` (DPDK's default): "The atomic_add is an expensive
/// operation, so we don't want to call it in the case where we know we are the unique holder
/// of this mbuf (i.e. ref_cnt == 1). Otherwise, an atomic operation has to be used because
/// concurrent accesses on the reference counter can occur."
///
/// # Safety
/// `m` is a valid mbuf.
#[cfg(not(dpdk_mbuf_refcnt_nonatomic))]
#[inline(always)]
pub unsafe fn rte_mbuf_refcnt_update(m: *mut rte_mbuf, value: i16) -> u16 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        if rte_mbuf_refcnt_read(m) == 1 {
            let v = (value as u16).wrapping_add(1);
            rte_mbuf_refcnt_set(m, v);
            return v;
        }
        // `__rte_mbuf_refcnt_update`: rte_atomic_fetch_add_explicit(.., acq_rel) + value
        (*m).refcnt.fetch_add(value as u16, Ordering::AcqRel).wrapping_add(value as u16)
    }
}

/// `rte_mbuf_refcnt_update(m, value)` for a DPDK built without `RTE_MBUF_REFCNT_ATOMIC`
/// (Engine's recommended `-Dmbuf_refcnt_atomic=false`): `__rte_mbuf_refcnt_update`, a plain
/// read-modify-write (relaxed load and store, no atomic RMW).
///
/// # Safety
/// `m` is a valid mbuf.
#[cfg(dpdk_mbuf_refcnt_nonatomic)]
#[inline(always)]
pub unsafe fn rte_mbuf_refcnt_update(m: *mut rte_mbuf, value: i16) -> u16 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let v = rte_mbuf_refcnt_read(m).wrapping_add(value as u16);
        rte_mbuf_refcnt_set(m, v);
        v
    }
}

/// `rte_mbuf_ext_refcnt_read(shinfo)`
///
/// # Safety
/// `shinfo` is a valid shared info.
#[inline(always)]
pub unsafe fn rte_mbuf_ext_refcnt_read(shinfo: *const rte_mbuf_ext_shared_info) -> u16 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*shinfo).refcnt.load(Ordering::Relaxed) }
}

/// `rte_mbuf_ext_refcnt_set(shinfo, new_value)`
///
/// # Safety
/// `shinfo` is a valid shared info.
#[inline(always)]
pub unsafe fn rte_mbuf_ext_refcnt_set(shinfo: *const rte_mbuf_ext_shared_info, new_value: u16) {
    // SAFETY: guaranteed by the caller.
    unsafe { (*shinfo).refcnt.store(new_value, Ordering::Relaxed) };
}

/// `rte_mbuf_ext_refcnt_update(shinfo, value)`
///
/// # Safety
/// `shinfo` is a valid shared info.
#[inline(always)]
pub unsafe fn rte_mbuf_ext_refcnt_update(
    shinfo: *const rte_mbuf_ext_shared_info,
    value: i16,
) -> u16 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        if rte_mbuf_ext_refcnt_read(shinfo) == 1 {
            let v = 1u16.wrapping_add(value as u16);
            rte_mbuf_ext_refcnt_set(shinfo, v);
            return v;
        }

        (*shinfo).refcnt.fetch_add(value as u16, Ordering::AcqRel).wrapping_add(value as u16)
    }
}

/// `RTE_MBUF_DIRECT(mb)`: the mbuf owns its data buffer.
///
/// # Safety
/// `mb` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_mbuf_direct(mb: *const rte_mbuf) -> bool {
    // SAFETY: guaranteed by the caller.
    unsafe { (*mb).ol_flags & (RTE_MBUF_F_INDIRECT | RTE_MBUF_F_EXTERNAL) == 0 }
}

/// `RTE_MBUF_HAS_EXTBUF(mb)`
///
/// # Safety
/// `mb` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_mbuf_has_extbuf(mb: *const rte_mbuf) -> bool {
    // SAFETY: guaranteed by the caller.
    unsafe { (*mb).ol_flags & RTE_MBUF_F_EXTERNAL != 0 }
}

/// `RTE_MBUF_HAS_PINNED_EXTBUF(mb)`
///
/// # Safety
/// `mb` is an mbuf of a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_mbuf_has_pinned_extbuf(mb: *const rte_mbuf) -> bool {
    // SAFETY: guaranteed by the caller.
    unsafe { rte_pktmbuf_priv_flags((*mb).pool) & RTE_PKTMBUF_POOL_F_PINNED_EXT_BUF != 0 }
}

/// `rte_mbuf_from_indirect(mi)`: the direct mbuf an indirect one is attached to.
///
/// # Safety
/// `mi` is an indirect mbuf.
#[inline(always)]
pub unsafe fn rte_mbuf_from_indirect(mi: *mut rte_mbuf) -> *mut rte_mbuf {
    // SAFETY: the buffer of an indirect mbuf is the one embedded in its direct mbuf.
    unsafe {
        (*mi).buf_addr.cast::<u8>().sub(size_of::<rte_mbuf>() + (*mi).priv_size as usize).cast()
    }
}

/// `rte_mbuf_raw_alloc(mp)`: allocate an uninitialized mbuf from mempool `mp`.
///
/// # Safety
/// `mp` is a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_mbuf_raw_alloc(mp: *mut rte_mempool) -> *mut rte_mbuf {
    let mut m: *mut rte_mbuf = ptr::null_mut();

    // SAFETY: guaranteed by the caller; `m` is valid for one pointer.
    if unsafe { rte_mempool_get(mp, (&raw mut m).cast()) } < 0 {
        return ptr::null_mut();
    }
    // __rte_mbuf_raw_sanity_check(m): RTE_ASSERT, compiled out.
    m
}

/// `rte_mbuf_raw_free(m)`: put mbuf back into the mempool it came from.
///
/// # Safety
/// `m` is a direct mbuf with a reference count of 1 and no next segment.
#[inline(always)]
pub unsafe fn rte_mbuf_raw_free(m: *mut rte_mbuf) {
    // RTE_ASSERT(!RTE_MBUF_CLONED(m) && ...) and __rte_mbuf_raw_sanity_check(m): compiled out.
    // SAFETY: guaranteed by the caller.
    unsafe { rte_mempool_put((*m).pool, m.cast()) };
}

/// `rte_pktmbuf_reset_headroom(m)`: reset the data_off field of a packet mbuf to its default
/// value.
///
/// # Safety
/// `m` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_pktmbuf_reset_headroom(m: *mut rte_mbuf) {
    // SAFETY: guaranteed by the caller.
    unsafe { (*m).data_off = RTE_PKTMBUF_HEADROOM.min((*m).buf_len) };
}

/// `rte_pktmbuf_reset(m)`: reset the fields of a packet mbuf to their default values.
///
/// # Safety
/// `m` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_pktmbuf_reset(m: *mut rte_mbuf) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        (*m).next = ptr::null_mut();
        (*m).pkt_len = 0;
        (*m).tx_offload = 0;
        (*m).vlan_tci = 0;
        (*m).vlan_tci_outer = 0;
        (*m).nb_segs = 1;
        (*m).port = RTE_MBUF_PORT_INVALID;

        (*m).ol_flags &= RTE_MBUF_F_EXTERNAL;
        (*m).packet_type = 0;
        rte_pktmbuf_reset_headroom(m);

        (*m).data_len = 0;
        // __rte_mbuf_sanity_check(m, 1): compiled out.
    }
}

/// `rte_pktmbuf_alloc(mp)`: allocate a new mbuf from a mempool.
///
/// # Safety
/// `mp` is a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_pktmbuf_alloc(mp: *mut rte_mempool) -> *mut rte_mbuf {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let m = rte_mbuf_raw_alloc(mp);
        if !m.is_null() {
            rte_pktmbuf_reset(m);
        }
        m
    }
}

/// `__rte_pktmbuf_free_extbuf(m)`
///
/// # Safety
/// `m` has an external buffer attached.
#[inline(always)]
unsafe fn __rte_pktmbuf_free_extbuf(m: *mut rte_mbuf) {
    // RTE_ASSERT(RTE_MBUF_HAS_EXTBUF(m)); RTE_ASSERT(m->shinfo != NULL): compiled out.
    // SAFETY: guaranteed by the caller; an external buffer comes with its free callback.
    unsafe {
        if rte_mbuf_ext_refcnt_update((*m).shinfo, -1) == 0 {
            ((*(*m).shinfo).free_cb.unwrap_unchecked())((*m).buf_addr, (*(*m).shinfo).fcb_opaque);
        }
    }
}

/// `__rte_pktmbuf_free_direct(m)`
///
/// # Safety
/// `m` is an indirect mbuf.
#[inline(always)]
unsafe fn __rte_pktmbuf_free_direct(m: *mut rte_mbuf) {
    // RTE_ASSERT(RTE_MBUF_CLONED(m)): compiled out.
    // SAFETY: guaranteed by the caller.
    unsafe {
        let md = rte_mbuf_from_indirect(m);

        if rte_mbuf_refcnt_update(md, -1) == 0 {
            (*md).next = ptr::null_mut();
            (*md).nb_segs = 1;
            rte_mbuf_refcnt_set(md, 1);
            rte_mbuf_raw_free(md);
        }
    }
}

/// `rte_pktmbuf_detach(m)`: detach a packet mbuf from external buffer or direct buffer.
///
/// # Safety
/// `m` is an mbuf of a live pktmbuf pool that is not direct.
#[inline]
pub unsafe fn rte_pktmbuf_detach(m: *mut rte_mbuf) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let mp = (*m).pool;

        if rte_mbuf_has_extbuf(m) {
            /*
             * The mbuf has the external attached buffer,
             * we should check the type of the memory pool where
             * the mbuf was allocated from to detect the pinned
             * external buffer.
             */
            let flags = rte_pktmbuf_priv_flags(mp);

            if flags & RTE_PKTMBUF_POOL_F_PINNED_EXT_BUF != 0 {
                /*
                 * The pinned external buffer should not be
                 * detached from its backing mbuf, just exit.
                 */
                return;
            }
            __rte_pktmbuf_free_extbuf(m);
        } else {
            __rte_pktmbuf_free_direct(m);
        }
        let priv_size = rte_pktmbuf_priv_size(mp);
        let mbuf_size = (size_of::<rte_mbuf>() + priv_size as usize) as u32;
        let buf_len = rte_pktmbuf_data_room_size(mp) as u32;

        (*m).priv_size = priv_size;
        (*m).buf_addr = m.cast::<u8>().add(mbuf_size as usize).cast();
        rte_mbuf_iova_set(m, rte_mempool_virt2iova(m.cast()) + mbuf_size as u64);
        (*m).buf_len = buf_len as u16;
        rte_pktmbuf_reset_headroom(m);
        (*m).data_len = 0;
        (*m).ol_flags = 0;
    }
}

/// `__rte_pktmbuf_pinned_extbuf_decref(m)`: decrease the reference counter of a pinned
/// external buffer; nonzero when the buffer is still in use.
///
/// # Safety
/// `m` has a pinned external buffer attached.
#[inline]
unsafe fn __rte_pktmbuf_pinned_extbuf_decref(m: *mut rte_mbuf) -> c_int {
    // SAFETY: guaranteed by the caller.
    unsafe {
        /* Clear flags, mbuf is being freed. */
        (*m).ol_flags = RTE_MBUF_F_EXTERNAL;
        let shinfo = (*m).shinfo;

        /* Optimize for performance - do not dec/reinit */
        if rte_mbuf_ext_refcnt_read(shinfo) == 1 {
            return 0;
        }

        /*
         * Direct usage of add primitive to avoid
         * duplication of comparing with one.
         */
        if (*shinfo).refcnt.fetch_add(u16::MAX, Ordering::AcqRel).wrapping_sub(1) != 0 {
            return 1;
        }

        /* Reinitialize counter before mbuf freeing. */
        rte_mbuf_ext_refcnt_set(shinfo, 1);
        0
    }
}

/// `rte_pktmbuf_prefree_seg(m)`: decrease reference counter and unlink a mbuf segment; the
/// segment when it can be put back into its pool, null otherwise.
///
/// # Safety
/// `m` is an mbuf of a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_pktmbuf_prefree_seg(m: *mut rte_mbuf) -> *mut rte_mbuf {
    // __rte_mbuf_sanity_check(m, 0): compiled out.
    // SAFETY: guaranteed by the caller.
    unsafe {
        if rte_mbuf_refcnt_read(m) == 1 {
            if !rte_mbuf_direct(m) {
                rte_pktmbuf_detach(m);
                if rte_mbuf_has_extbuf(m)
                    && rte_mbuf_has_pinned_extbuf(m)
                    && __rte_pktmbuf_pinned_extbuf_decref(m) != 0
                {
                    return ptr::null_mut();
                }
            }

            if !(*m).next.is_null() {
                (*m).next = ptr::null_mut();
            }
            if (*m).nb_segs != 1 {
                (*m).nb_segs = 1;
            }

            return m;
        } else if rte_mbuf_refcnt_update(m, -1) == 0 {
            if !rte_mbuf_direct(m) {
                rte_pktmbuf_detach(m);
                if rte_mbuf_has_extbuf(m)
                    && rte_mbuf_has_pinned_extbuf(m)
                    && __rte_pktmbuf_pinned_extbuf_decref(m) != 0
                {
                    return ptr::null_mut();
                }
            }

            if !(*m).next.is_null() {
                (*m).next = ptr::null_mut();
            }
            if (*m).nb_segs != 1 {
                (*m).nb_segs = 1;
            }
            rte_mbuf_refcnt_set(m, 1);

            return m;
        }
        ptr::null_mut()
    }
}

/// `rte_pktmbuf_free_seg(m)`: free a segment of a packet mbuf into the mempool it came from.
///
/// # Safety
/// `m` is an mbuf of a live pktmbuf pool.
#[inline(always)]
pub unsafe fn rte_pktmbuf_free_seg(m: *mut rte_mbuf) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let m = rte_pktmbuf_prefree_seg(m);
        if !m.is_null() {
            rte_mbuf_raw_free(m);
        }
    }
}

/// `rte_pktmbuf_free(m)`: free a packet mbuf back into the mempool it came from, with all its
/// segments.
///
/// # Safety
/// `m` is null or the head of a chain of mbufs of live pktmbuf pools.
#[inline]
pub unsafe fn rte_pktmbuf_free(mut m: *mut rte_mbuf) {
    // __rte_mbuf_sanity_check(m, 1) when m != NULL: compiled out.
    // SAFETY: guaranteed by the caller.
    unsafe {
        while !m.is_null() {
            let m_next = (*m).next;
            rte_pktmbuf_free_seg(m);
            m = m_next;
        }
    }
}

/// `rte_pktmbuf_mtod_offset(m, t, o)`: a pointer to the data of the mbuf at offset `o`.
///
/// # Safety
/// `m` is a valid mbuf; the result is valid as far as its buffer goes.
#[inline(always)]
pub unsafe fn rte_pktmbuf_mtod_offset(m: *const rte_mbuf, o: usize) -> *mut u8 {
    // SAFETY: guaranteed by the caller; the data starts data_off bytes into the buffer.
    unsafe { (*m).buf_addr.cast::<u8>().add((*m).data_off as usize + o) }
}

/// `rte_pktmbuf_mtod(m, t)`: a pointer to the start of the data of the mbuf.
///
/// # Safety
/// As [`rte_pktmbuf_mtod_offset`].
#[inline(always)]
pub unsafe fn rte_pktmbuf_mtod(m: *const rte_mbuf) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe { rte_pktmbuf_mtod_offset(m, 0) }
}

/// `rte_pktmbuf_pkt_len(m)`: the length of the packet.
///
/// # Safety
/// `m` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_pktmbuf_pkt_len(m: *const rte_mbuf) -> u32 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*m).pkt_len }
}

/// `rte_pktmbuf_data_len(m)`: the amount of data in the segment.
///
/// # Safety
/// `m` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_pktmbuf_data_len(m: *const rte_mbuf) -> u16 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*m).data_len }
}

/// `rte_pktmbuf_is_contiguous(m)`: the packet has a single segment.
///
/// # Safety
/// `m` is a valid mbuf.
#[inline(always)]
pub unsafe fn rte_pktmbuf_is_contiguous(m: *const rte_mbuf) -> bool {
    // __rte_mbuf_sanity_check(m, 1): compiled out.
    // SAFETY: guaranteed by the caller.
    unsafe { (*m).nb_segs == 1 }
}

// ---------------------------------------------------------------------------
// rte_ethdev.h: rte_eth_rx_burst(), rte_eth_tx_burst()
// ---------------------------------------------------------------------------

/// `rte_eth_rx_burst(port_id, queue_id, rx_pkts, nb_pkts)`: retrieve a burst of input
/// packets from a receive queue of an Ethernet device.
///
/// # Safety
/// The port is started, the queue is set up and used by this thread only, and `rx_pkts`
/// has room for `nb_pkts` pointers.
#[inline(always)]
pub unsafe fn rte_eth_rx_burst(
    port_id: u16,
    queue_id: u16,
    rx_pkts: *mut *mut rte_mbuf,
    nb_pkts: u16,
) -> u16 {
    // SAFETY: guaranteed by the caller; ethdev keeps every port's fast-path ops set.
    unsafe {
        /* fetch pointer to queue data */
        let p = (&raw mut rte_eth_fp_ops).cast::<rte_eth_fp_ops>().add(port_id as usize);
        let qd = *(*p).rxq.data.add(queue_id as usize);

        let mut nb_rx = ((*p).rx_pkt_burst.unwrap_unchecked())(qd, rx_pkts, nb_pkts);

        // RTE_ETHDEV_RXTX_CALLBACKS
        {
            /* rte_memory_order_release memory order was used when the
             * call back was inserted into the list.
             * Since there is a clear dependency between loading
             * cb and cb->fn/cb->next, rte_memory_order_acquire memory order is
             * not required.
             */
            let cb = (*(*p).rxq.clbk.add(queue_id as usize).cast::<AtomicPtr<c_void>>())
                .load(Ordering::Relaxed);
            if !cb.is_null() {
                nb_rx = rte_eth_call_rx_callbacks(port_id, queue_id, rx_pkts, nb_rx, nb_pkts, cb);
            }
        }

        // rte_ethdev_trace_rx_burst(...): compiled out.
        nb_rx
    }
}

/// `rte_eth_tx_burst(port_id, queue_id, tx_pkts, nb_pkts)`: send a burst of output packets
/// on a transmit queue of an Ethernet device.
///
/// # Safety
/// The port is started, the queue is set up and used by this thread only, and `tx_pkts`
/// holds `nb_pkts` packets.
#[inline(always)]
pub unsafe fn rte_eth_tx_burst(
    port_id: u16,
    queue_id: u16,
    tx_pkts: *mut *mut rte_mbuf,
    mut nb_pkts: u16,
) -> u16 {
    // SAFETY: guaranteed by the caller; ethdev keeps every port's fast-path ops set.
    unsafe {
        /* fetch pointer to queue data */
        let p = (&raw mut rte_eth_fp_ops).cast::<rte_eth_fp_ops>().add(port_id as usize);
        let qd = *(*p).txq.data.add(queue_id as usize);

        // RTE_ETHDEV_RXTX_CALLBACKS
        {
            /* rte_memory_order_release memory order was used when the
             * call back was inserted into the list.
             * Since there is a clear dependency between loading
             * cb and cb->fn/cb->next, rte_memory_order_acquire memory order is
             * not required.
             */
            let cb = (*(*p).txq.clbk.add(queue_id as usize).cast::<AtomicPtr<c_void>>())
                .load(Ordering::Relaxed);
            if !cb.is_null() {
                nb_pkts = rte_eth_call_tx_callbacks(port_id, queue_id, tx_pkts, nb_pkts, cb);
            }
        }

        nb_pkts = ((*p).tx_pkt_burst.unwrap_unchecked())(qd, tx_pkts, nb_pkts);

        // rte_ethdev_trace_tx_burst(...): compiled out.
        nb_pkts
    }
}
