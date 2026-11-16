//! The DPDK network device of the native stack (built with the `dpdk` feature).
//!
//! [`DpdkOptions`] are always available so that the command line is the same with and
//! without DPDK. With the feature, `DpdkDevice` initializes one DPDK Ethernet port (RSS
//! across one queue pair per shard, checksum offload, TSO, LRO, hardware flow control, port
//! statistics as metrics) and `DpdkQp` is the queue pair of one shard: it receives bursts
//! of mbufs from the PMD and turns them into packets (copying, or zero-copy when the memory
//! is backed by hugetlbfs and can be mapped to IO addresses), and sends packets as mbuf
//! clusters built by its `TxBufFactory`. The DPDK structures, symbols and inline helpers
//! it uses are in `crate::engine::net::dpdk_rte`.
//!
//! The device and its queues refer to one another through raw pointers kept by their
//! pollers, timers and metrics; they live in stable heap allocations and never move once
//! those pointers are taken.

#[cfg(not(feature = "dpdk"))]
use crate::engine::util::program_options::Unused;
use crate::engine::util::program_options::{OptionGroup, Value};

/// DPDK configuration.
#[repr(C)]
pub struct DpdkOptions {
    /// The option group these options belong to.
    pub group: OptionGroup,
    /// DPDK Port Index.
    ///
    /// Default: 0.
    pub dpdk_port_index: Value<u32>,
    /// Enable hardware flow control (`on` / `off`).
    ///
    /// Default: `on`.
    pub hw_fc: Value<String>,
}

#[cfg(feature = "dpdk")]
pub use have_dpdk::{
    DpdkDevice, DpdkQp, TxBuf, TxBufFactory, create_dpdk_net_device,
    create_dpdk_net_device_from_config, qp_mempool_obj_size,
};

#[cfg(feature = "dpdk")]
mod have_dpdk {
    use std::cell::UnsafeCell;
    use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
    use std::io::Write;
    use std::mem::{MaybeUninit, offset_of, size_of};
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use crate::engine::core::align::align_up_usize;
    use crate::engine::core::circular_buffer::CircularBuffer;
    use crate::engine::core::deleter::{Deleter, make_deleter_with, make_free_deleter};
    use crate::engine::core::dpdk_rte::dpdk::INITIALIZED as EAL_INITIALIZED;
    use crate::engine::core::future::InvalidArgument;
    use crate::engine::core::future::{
        ExceptionPtr, Future, LogicError, Promise, make_exception_future, make_ready_future,
    };
    use crate::engine::core::internal::poll::Poller;
    use crate::engine::core::memory::{HUGE_PAGE_SIZE, PAGE_BITS, PAGE_SIZE, get_memory_layout};
    use crate::engine::core::metrics::{self as sm, MetricGroups};
    use crate::engine::core::shard_id::this_shard_id;
    use crate::engine::core::smp::Smp;
    use crate::engine::core::timer::Timer;
    use crate::engine::core::units::kib;
    use crate::engine::net::config::HwConfig;
    use crate::engine::net::const_::IpProtocolNum;
    use crate::engine::net::dpdk_rte::*;
    use crate::engine::net::ethernet::EthernetAddress;
    use crate::engine::net::native_stack::NativeStackOptions;
    use crate::engine::net::net::{
        Device, DeviceBase, HwFeatures, Qp, QpBase, QpPtr, QpStats, qp_init,
    };
    use crate::engine::net::node_ptr::{CrossShard, NodePtr};
    use crate::engine::net::packet::{Fragment, OffloadInfo, Packet};
    use crate::engine::net::toeplitz::{
        DEFAULT_RSSKEY_40BYTES, DEFAULT_RSSKEY_52BYTES, RssKeyType,
    };
    use crate::engine::net::virtio::MallocBuf;
    use std::pin::Pin;

    unsafe extern "C" {
        fn malloc(size: usize) -> *mut c_void;
        fn free(p: *mut c_void);
        fn posix_memalign(memptr: *mut *mut c_void, alignment: usize, size: usize) -> c_int;
        fn strerror(errnum: c_int) -> *const c_char;
    }

    // The mbuf-pool cookie is the address of the pool's private data.
    #[inline]
    fn as_cookie(p: &mut rte_pktmbuf_pool_private) -> *mut c_void {
        (p as *mut rte_pktmbuf_pool_private).cast()
    }

    /// A zero-sized marker for a point in a structure.
    type Marker = [*mut c_void; 0];

    // Calculate maximum amount of memory required to store given number of objects
    fn get_mempool_xmem_size(elt_num: u32, total_elt_sz: usize, pg_shift: u32) -> usize {
        if total_elt_sz == 0 {
            return 0;
        }

        if pg_shift == 0 {
            return total_elt_sz * elt_num as usize;
        }

        let pg_sz: usize = 1usize << pg_shift;
        let obj_per_page: usize = pg_sz / total_elt_sz;
        if obj_per_page == 0 {
            return align_up_usize(total_elt_sz, pg_sz) * elt_num as usize;
        }

        let pg_num: usize = (elt_num as usize).div_ceil(obj_per_page);
        pg_num << pg_shift
    }

    /* Net device related constants */
    const DEFAULT_RING_SIZE: u16 = 512;

    //
    // We need 2 times the ring size of buffers because of the way PMDs
    // refill the ring.
    //
    const MBUFS_PER_QUEUE_RX: u16 = 2 * DEFAULT_RING_SIZE;
    const RX_GC_THRESH: u16 = 64;

    //
    // No need to keep more descriptors in the air than can be sent in a single
    // rte_eth_tx_burst() call.
    //
    const MBUFS_PER_QUEUE_TX: u16 = 2 * DEFAULT_RING_SIZE;

    const MBUF_CACHE_SIZE: u16 = 512;
    const MBUF_OVERHEAD: u16 = size_of::<rte_mbuf>() as u16 + RTE_PKTMBUF_HEADROOM;
    //
    // We'll allocate 2K data buffers for an inline case because this would require
    // a single page per mbuf. If we used 4K data buffers here it would require 2
    // pages for a single buffer (due to "mbuf_overhead") and this is a much more
    // demanding memory constraint.
    //
    const INLINE_MBUF_DATA_SIZE: usize = 2048;

    //
    // Size of the data buffer in the non-inline case.
    //
    // We may want to change (increase) this value in future, while the
    // inline_mbuf_data_size value will unlikely change due to reasons described
    // above.
    //
    const MBUF_DATA_SIZE: usize = 2048;

    // (INLINE_MBUF_DATA_SIZE(2K)*32 = 64K = Max TSO/LRO size) + 1 mbuf for headers
    const MAX_FRAGS: u8 = 32 + 1;

    //
    // The i40e (40G) NICs' hardware limit on the number of fragments in a transmit segment.
    //
    // See Chapter 8.4.1 "Transmit Packet in System Memory" of the xl710 devices
    // spec. for more details.
    //
    const I40E_MAX_XMIT_SEGMENT_FRAGS: u8 = 8;

    //
    // VMWare's virtual NIC limit for a number of fragments in an xmit segment.
    //
    // see drivers/net/vmxnet3/base/vmxnet3_defs.h VMXNET3_MAX_TXD_PER_PKT
    //
    const VMXNET3_MAX_XMIT_SEGMENT_FRAGS: u8 = 16;

    const INLINE_MBUF_SIZE: u16 = INLINE_MBUF_DATA_SIZE as u16 + MBUF_OVERHEAD;

    /// The number of bytes needed for the mempool objects of each queue pair, with or
    /// without a hugetlbfs memory backend.
    pub fn qp_mempool_obj_size(hugetlbfs_membackend: bool) -> u32 {
        let mut mp_size: u32 = 0;
        let mut mp_obj_sz = rte_mempool_objsz::default();

        //
        // We will align each size to huge page size because DPDK allocates
        // physically contiguous memory region for each pool object.
        //

        // Rx
        if hugetlbfs_membackend {
            // SAFETY: rte_mempool_calc_obj_size() only computes sizes into `mp_obj_sz`.
            let obj = unsafe { rte_mempool_calc_obj_size(MBUF_OVERHEAD as u32, 0, &mut mp_obj_sz) };
            mp_size += align_up_usize(
                obj as usize + size_of::<rte_pktmbuf_pool_private>(),
                HUGE_PAGE_SIZE,
            ) as u32;
        } else {
            // SAFETY: as above.
            let obj =
                unsafe { rte_mempool_calc_obj_size(INLINE_MBUF_SIZE as u32, 0, &mut mp_obj_sz) };
            mp_size += align_up_usize(
                obj as usize + size_of::<rte_pktmbuf_pool_private>(),
                HUGE_PAGE_SIZE,
            ) as u32;
        }
        //Tx
        mp_obj_sz = rte_mempool_objsz::default();
        // SAFETY: as above.
        let obj = unsafe { rte_mempool_calc_obj_size(INLINE_MBUF_SIZE as u32, 0, &mut mp_obj_sz) };
        mp_size +=
            align_up_usize(obj as usize + size_of::<rte_pktmbuf_pool_private>(), HUGE_PAGE_SIZE)
                as u32;
        mp_size
    }

    const PKTMBUF_POOL_NAME: &str = "dpdk_pktmbuf_pool";

    /// The batch size of reads from the NIC queues.
    const PACKET_READ_SIZE: u8 = 32;

    #[derive(Clone, Copy, Debug, Default)]
    struct PortStatsRxGood {
        mcast: u64,      // number of received multicast packets
        pause_xon: u64,  // number of received PAUSE XON frames
        pause_xoff: u64, // number of received PAUSE XOFF frames
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct PortStatsRxBad {
        dropped: u64, // missed packets (e.g. full FIFO)
        crc: u64,     // packets with CRC error
        len: u64,     // packets with a bad length
        total: u64,   // total number of erroneous received packets
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct PortStatsRx {
        good: PortStatsRxGood,
        bad: PortStatsRxBad,
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct PortStatsTxGood {
        pause_xon: u64,  // number of sent PAUSE XON frames
        pause_xoff: u64, // number of sent PAUSE XOFF frames
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct PortStatsTxBad {
        total: u64, // total number of failed transmitted packets
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct PortStatsTx {
        good: PortStatsTxGood,
        bad: PortStatsTxBad,
    }

    /// Port statistics, read periodically from the device.
    #[derive(Clone, Copy, Debug, Default)]
    struct PortStats {
        rx: PortStatsRx,
        tx: PortStatsTx,
    }

    /// Hands the list of extended-statistics names the device reads to the macro `$m`.
    macro_rules! xstats_id_list {
        ($m:ident) => {
            $m! {
                rx_multicast_packets,
                rx_xon_packets,
                rx_xoff_packets,
                rx_crc_errors,
                rx_length_errors,
                rx_undersize_errors,
                rx_oversize_errors,
                tx_xon_packets,
                tx_xoff_packets
            }
        };
    }

    /// Defines `XstatId`, one variant per extended statistic, with its count, the ids in
    /// list order and each id's name.
    macro_rules! define_xstat_id {
        ($($elem:ident),*) => {
            #[allow(non_camel_case_types)]
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[repr(i32)]
            enum XstatId {
                $($elem),*
            }

            impl XstatId {
                /// Number of ids.
                const COUNT: usize = [$(XstatId::$elem),*].len();
                /// The ids in list order.
                const ALL: [XstatId; Self::COUNT] = [$(XstatId::$elem),*];

                /// The name of the statistic, as the device reports it.
                #[inline]
                fn to_str(self) -> &'static str {
                    match self {
                        $(XstatId::$elem => stringify!($elem)),*
                    }
                }
            }
        };
    }

    xstats_id_list!(define_xstat_id);

    /// The extended statistics of a port: their names, values and the offset of each id
    /// in the device's list.
    struct DpdkXstats {
        _port_id: u16,
        _len: c_int,
        _xstats: Box<[rte_eth_xstat]>,
        _xstat_names: Box<[rte_eth_xstat_name]>,
        // -1 (not found) until `start`.
        _offsets: [c_int; XstatId::COUNT],
    }

    /// A default-filled array of `len` elements, `len` being the count an xstats call
    /// returned.
    ///
    /// # Errors
    /// Fails if `len` is negative (a DPDK error code).
    fn new_array<T: Clone + Default>(len: c_int) -> Result<Box<[T]>, ExceptionPtr> {
        let Ok(n) = usize::try_from(len) else {
            return Err(ExceptionPtr::from(LogicError("invalid array length".to_string())));
        };
        Ok(vec![T::default(); n].into_boxed_slice())
    }

    impl DpdkXstats {
        fn new(port_id: u16) -> Self {
            DpdkXstats {
                _port_id: port_id,
                _len: 0,
                _xstats: Box::new([]),
                _xstat_names: Box::new([]),
                _offsets: [-1; XstatId::COUNT],
            }
        }

        // ~dpdk_xstats(): `delete[] _xstats` and `delete[] _xstat_names` are the boxed
        // arrays dropping themselves.

        fn start(&mut self) -> Result<(), ExceptionPtr> {
            // SAFETY: a null array of size 0 asks for the number of extended statistics.
            self._len = unsafe { rte_eth_xstats_get_names(self._port_id, ptr::null_mut(), 0) };
            self._xstats = new_array(self._len)?;
            self._xstat_names = new_array(self._len)?;
            self.update_xstats();
            self.update_xstat_names();
            self.update_offsets();
            Ok(())
        }

        fn update_xstats(&mut self) {
            // SAFETY: `_xstats` has room for `_len` entries.
            let len = unsafe {
                rte_eth_xstats_get(self._port_id, self._xstats.as_mut_ptr(), self._len as c_uint)
            };
            assert!(len == self._len);
        }

        #[inline]
        fn get_value(&self, id: XstatId) -> u64 {
            let off = self._offsets[id as usize];
            if off == -1 {
                return 0;
            }
            self._xstats[off as usize].value
        }

        // private:

        #[inline]
        fn id_to_str(id: XstatId) -> &'static str {
            id.to_str()
        }

        /// Whether the NUL-terminated `name` equals `s`.
        #[inline]
        // `c_char` is `i8` on x86_64 and `u8` on aarch64: the cast is a no-op only on the latter.
        #[allow(clippy::unnecessary_cast)]
        fn name_eq(name: &[c_char; RTE_ETH_XSTATS_NAME_SIZE], s: &str) -> bool {
            let s = s.as_bytes();
            s.len() < name.len()
                && name[s.len()] == 0
                && name.iter().zip(s.iter()).all(|(&c, &b)| c as u8 == b)
        }

        fn get_offset_by_name(&self, id: XstatId, len: c_int) -> c_int {
            for i in 0..len {
                if Self::name_eq(&self._xstat_names[i as usize].name, Self::id_to_str(id)) {
                    return i;
                }
            }
            -1
        }

        fn update_xstat_names(&mut self) {
            // SAFETY: `_xstat_names` has room for `_len` entries.
            let len = unsafe {
                rte_eth_xstats_get_names(
                    self._port_id,
                    self._xstat_names.as_mut_ptr(),
                    self._len as c_uint,
                )
            };
            assert!(len == self._len);
        }

        fn update_offsets(&mut self) {
            for id in XstatId::ALL {
                self._offsets[id as usize] = self.get_offset_by_name(id, self._len);
            }
        }
    }

    /// A DPDK Ethernet port, shared by the queues of every shard.
    ///
    /// `init_local_queue()` and the setup that follows it change the device through the
    /// `&dyn Device` the native stack shares between the shards; the data members are
    /// therefore in an `UnsafeCell`. The port statistics are boxed because the metrics
    /// registered by the constructor point at them before the device reaches its final
    /// place.
    pub struct DpdkDevice {
        base: DeviceBase,
        d: UnsafeCell<DpdkDeviceState>,
    }

    // SAFETY: the state is changed only during the port setup and by
    // the statistics timer, both on the device's home shard; the other shards only read the
    // fields that the setup has finished writing (port index, features, RSS table), each
    // through a reference to that field alone (`dpdk_st!`), and the timer writes only the
    // boxed port statistics, through place accesses.
    unsafe impl Send for DpdkDevice {}
    // SAFETY: see above.
    unsafe impl Sync for DpdkDevice {}

    /// `dpdk_st!(dev, field)`: a shared reference to one field of the device state, made from
    /// the cell's root pointer without a reference to the whole state (see the Sync impl).
    macro_rules! dpdk_st {
        ($dev:expr, $($field:tt)+) => {
            // SAFETY: the field is one the setup has finished writing and nothing writes
            // while it is read (see the Sync impl); the cell lives as long as the device.
            unsafe { &(*$dev.st()).$($field)+ }
        };
    }

    /// The data members of `dpdk_device` and its non-virtual member functions.
    struct DpdkDeviceState {
        _port_idx: u16,
        _num_queues: u16,
        _hw_features: HwFeatures,
        _queues_ready: u16,
        _home_cpu: u32,
        _use_lro: bool,
        _enable_fc: bool,
        _redir_table: Vec<u8>,
        _rss_key: RssKeyType,
        /// The port statistics, owned (a `Box` turned into its raw pointer, freed by `Drop`):
        /// the metrics and the timer write and read them through this pointer, which a move
        /// of the state does not retag as it would a `Box`.
        _stats: NonNull<PortStats>,
        _stats_collector: Timer,
        _stats_plugin_name: String,
        _stats_plugin_inst: String,
        _metrics: MetricGroups,
        _is_i40e_device: bool,
        _is_vmxnet3_device: bool,
        _xstats: DpdkXstats,

        // public:
        _dev_info: rte_eth_dev_info,
        _link_ready_promise: Promise<()>,
    }

    impl DpdkDeviceState {
        // private:
        //
        // Port initialization consists of 3 main stages:
        // 1) General port initialization which ends with a call to
        //    rte_eth_dev_configure() where we request the needed number of Rx and
        //    Tx queues.
        // 2) Individual queues initialization. This is done in the constructor of
        //    dpdk_qp class. In particular the memory pools for queues are allocated
        //    in this stage.
        // 3) The final stage of the initialization which starts with the call of
        //    rte_eth_dev_start() after which the port becomes fully functional. We
        //    will also wait for a link to get up in this stage.

        // First stage of the port initialization: `init_port_start()` (below), which
        // returns 0 on success and an error code otherwise.

        // The final stage of a port initialization: `init_port_fini()` (below), called
        // only after all the queues of stage (2) are initialized.

        // Checks the link status of our port for up to 9 s and prints it at the end:
        // `check_port_link_status()` (below).

        // Configures the hardware flow control: `set_hw_flow_control()` (below).

        #[inline(always)]
        fn hw_features_ref(&mut self) -> &mut HwFeatures {
            &mut self._hw_features
        }
    }

    impl Drop for DpdkDeviceState {
        fn drop(&mut self) {
            // SAFETY: the state lives in the device's UnsafeCell, which never moves.
            unsafe { Pin::new_unchecked(&mut self._stats_collector) }.cancel();
            // The metrics point at the statistics: unregister them before freeing those.
            self._metrics = MetricGroups::new();
            // SAFETY: `_stats` came from `Box::leak` in `new` and is freed only here; the
            // timer is cancelled and the metrics that read it are gone.
            drop(unsafe { Box::from_raw(self._stats.as_ptr()) });
        }
    }

    impl DpdkDevice {
        /// Initializes port `port_idx` with `num_queues` queue pairs, LRO and flow control as
        /// requested, and registers the port metrics.
        ///
        /// # Errors
        /// Fails if the metrics cannot be registered. Exits the process if the port cannot be
        /// initialized.
        pub fn new(
            port_idx: u16,
            num_queues: u16,
            use_lro: bool,
            enable_fc: bool,
        ) -> Result<Self, ExceptionPtr> {
            let base = DeviceBase::default();
            let mut d = DpdkDeviceState {
                _port_idx: port_idx,
                _num_queues: num_queues,
                _hw_features: HwFeatures::default(),
                _queues_ready: 0,
                _home_cpu: this_shard_id(),
                _use_lro: use_lro,
                _enable_fc: enable_fc,
                _redir_table: Vec::new(),
                _rss_key: &[],
                _stats: NonNull::from(Box::leak(Box::new(PortStats::default()))),
                _stats_collector: Timer::new(),
                _stats_plugin_name: "network".to_string(),
                _stats_plugin_inst: format!("port{}", port_idx),
                _metrics: MetricGroups::new(),
                _is_i40e_device: false,
                _is_vmxnet3_device: false,
                _xstats: DpdkXstats::new(port_idx),
                _dev_info: rte_eth_dev_info::default(),
                _link_ready_promise: Promise::new(),
            };

            // now initialise the port we will use
            let ret = d.init_port_start(&base);
            if ret != 0 {
                rte_exit_fmt(
                    EXIT_FAILURE,
                    format_args!("Cannot initialise port {}\n", d._port_idx),
                );
            }

            // Register port statistics pollers
            let stats: *const PortStats = d._stats.as_ptr();
            macro_rules! ps {
                ($($f:tt)+) => {
                    // SAFETY: the metrics belong to the device, whose boxed statistics outlive
                    // them, so `stats` is live whenever a metric closure reads it.
                    move || unsafe { (*stats).$($f)+ }
                };
            }
            let inst = d._stats_plugin_inst.clone();
            let lbl = || vec![sm::shard_label().call(inst.as_str())];
            d._metrics.add_group(
                &d._stats_plugin_name,
                vec![
                    // Rx Good
                    sm::make_counter("rx_multicast", ps!(rx.good.mcast),
                        sm::Description::new("Counts a number of received multicast packets."), lbl()),
                    // Rx Errors
                    sm::make_counter("rx_crc_errors", ps!(rx.bad.crc),
                        sm::Description::new(concat!(
                            "Counts a number of received packets with a bad CRC value. ",
                            "A non-zero value of this metric usually indicates a HW problem, e.g. a bad cable.")), lbl()),

                    sm::make_counter("rx_dropped", ps!(rx.bad.dropped),
                        sm::Description::new(concat!(
                            "Counts a number of dropped received packets. ",
                            "A non-zero value of this counter indicated the overflow of ingress HW buffers. ",
                            "This usually happens because of a rate of a sender on the other side of the link is higher than we can process as a receiver.")), lbl()),

                    sm::make_counter("rx_bad_length_errors", ps!(rx.bad.len),
                        sm::Description::new(concat!(
                            "Counts a number of received packets with a bad length value. ",
                            "A non-zero value of this metric usually indicates a HW issue: e.g. bad cable.")), lbl()),
                    // Coupled counters:
                    // Good
                    sm::make_counter("rx_pause_xon", ps!(rx.good.pause_xon),
                        sm::Description::new(concat!(
                            "Counts a number of received PAUSE XON frames (PAUSE frame with a quanta of zero). ",
                            "When PAUSE XON frame is received our port may resume sending L2 frames. ",
                            "PAUSE XON frames are sent to resume sending that was previously paused with a PAUSE XOFF frame. If ingress ",
                            "buffer falls below the low watermark threshold before the timeout configured in the original PAUSE XOFF frame the receiver may decide to send PAUSE XON frame. ",
                            "A non-zero value of this metric may mean that our sender is bursty and that the spikes overwhelm the receiver on the other side of the link.")), lbl()),

                    sm::make_counter("tx_pause_xon", ps!(tx.good.pause_xon),
                        sm::Description::new(concat!(
                            "Counts a number of sent PAUSE XON frames (L2 flow control frames). ",
                            "A non-zero value of this metric indicates that our ingress path doesn't keep up with the rate of a sender on the other side of the link. ",
                            "Note that if a sender port respects PAUSE frames this will prevent it from sending from ALL its egress queues because L2 flow control is defined ",
                            "on a per-link resolution.")), lbl()),

                    sm::make_counter("rx_pause_xoff", ps!(rx.good.pause_xoff),
                        sm::Description::new(concat!(
                            "Counts a number of received PAUSE XOFF frames. ",
                            "A non-zero value of this metric indicates that our egress overwhelms the receiver on the other side of the link and it has to send PAUSE frames to make us stop sending. ",
                            "Note that if our port respects PAUSE frames a reception of a PAUSE XOFF frame will cause ALL egress queues of this port to stop sending.")), lbl()),

                    sm::make_counter("tx_pause_xoff", ps!(tx.good.pause_xoff),
                        sm::Description::new(concat!(
                            "Counts a number of sent PAUSE XOFF frames. ",
                            "A non-zero value of this metric indicates that our ingress path (SW and HW) doesn't keep up with the rate of a sender on the other side of the link and as a result ",
                            "our ingress HW buffers overflow.")), lbl()),
                    // Errors
                    sm::make_counter("rx_errors", ps!(rx.bad.total),
                        sm::Description::new("Counts the total number of ingress errors: CRC errors, bad length errors, etc."), lbl()),

                    sm::make_counter("tx_errors", ps!(tx.bad.total),
                        sm::Description::new("Counts a total number of egress errors. A non-zero value usually indicated a problem with a HW or a SW driver."), lbl()),
                ],
            )?;

            Ok(DpdkDevice { base, d: UnsafeCell::new(d) })
        }

        /// The data members, as the root pointer of the `UnsafeCell`. Readers go through
        /// [`dpdk_st!`] to the one field they read, never through a reference to the whole
        /// state: the statistics timer on the home shard writes other fields of it while the
        /// other shards read these.
        #[inline(always)]
        fn st(&self) -> *mut DpdkDeviceState {
            self.d.get()
        }

        /// The device's hardware features, mutable during setup.
        #[inline(always)]
        pub fn hw_features_ref(&self) -> &HwFeatures {
            dpdk_st!(self, _hw_features)
        }

        /// The default receive-queue configuration of the port.
        #[inline(always)]
        pub fn def_rx_conf(&self) -> *const rte_eth_rxconf {
            dpdk_st!(self, _dev_info.default_rxconf)
        }

        /// The default transmit-queue configuration of the port.
        #[inline(always)]
        pub fn def_tx_conf(&self) -> *const rte_eth_txconf {
            dpdk_st!(self, _dev_info.default_txconf)
        }

        /// Set the RSS table in the device and store it in the internal vector.
        pub fn set_rss_table(&self) {
            // SAFETY: called on the home shard during the port setup (see the Sync impl).
            unsafe { (*self.d.get()).set_rss_table() }
        }

        /// The DPDK port index.
        #[inline(always)]
        pub fn port_idx(&self) -> u16 {
            *dpdk_st!(self, _port_idx)
        }

        /// Whether the port is an i40e-family NIC (which needs special handling).
        #[inline(always)]
        pub fn is_i40e_device(&self) -> bool {
            *dpdk_st!(self, _is_i40e_device)
        }

        /// Whether the port is a vmxnet3 virtual NIC (which needs special handling).
        #[inline(always)]
        pub fn is_vmxnet3_device(&self) -> bool {
            *dpdk_st!(self, _is_vmxnet3_device)
        }

        /// The hardware features, read directly rather than through the `Device` trait.
        #[inline(always)]
        fn hw_features_inline(&self) -> HwFeatures {
            *dpdk_st!(self, _hw_features)
        }
    }

    impl Device for DpdkDevice {
        fn base(&self) -> &DeviceBase {
            &self.base
        }

        fn hw_address(&self) -> EthernetAddress {
            let mut mac = rte_ether_addr::default();
            let port_idx = *dpdk_st!(self, _port_idx);
            // SAFETY: `mac` is valid for writes.
            unsafe { rte_eth_macaddr_get(port_idx, &mut mac) };

            EthernetAddress { mac: mac.addr_bytes }
        }

        fn hw_features(&self) -> HwFeatures {
            *dpdk_st!(self, _hw_features)
        }

        fn hw_queues_count(&self) -> u16 {
            *dpdk_st!(self, _num_queues)
        }

        fn link_ready(&self) -> Future<()> {
            // SAFETY: called on the home shard (see the Sync impl).
            unsafe { (*self.d.get())._link_ready_promise.get_future() }
        }

        fn init_local_queue(
            &self,
            net_opts: &NativeStackOptions,
            qid: u16,
        ) -> Result<QpPtr, ExceptionPtr> {
            // The options type is fixed by the signature (see `Device::init_local_queue`).

            let name = format!(
                "{}-{}",
                dpdk_st!(self, _stats_plugin_name),
                dpdk_st!(self, _stats_plugin_inst)
            );
            let qp: Result<QpPtr, ExceptionPtr> = if net_opts._hugepages {
                DpdkQp::<true>::new(self, qid, name)
                    // SAFETY: the pointer comes from the node's `into_raw`, so it is the node's
                    // root pointer, used once; the `QpPtr` takes over its ownership as a
                    // `dyn Qp`.
                    .map(|q| unsafe { NodePtr::from_raw(q.into_raw() as *mut dyn Qp) })
            } else {
                DpdkQp::<false>::new(self, qid, name)
                    // SAFETY: as in the other arm.
                    .map(|q| unsafe { NodePtr::from_raw(q.into_raw() as *mut dyn Qp) })
            };
            // a failure to create the queue is returned
            let qp = qp?;

            // FIXME: future is discarded
            // SAFETY: the device outlives its queues, and its state is touched by this work on
            // its home shard only (the other shards read the parts that are fixed before the
            // queues are made).
            let st = unsafe { CrossShard::new(self.d.get()) };
            let _ = Smp::submit_to(*dpdk_st!(self, _home_cpu), move || {
                let st: *mut DpdkDeviceState = st.into_inner();
                // SAFETY: see above; `st` is the state's root pointer (`UnsafeCell::get` of the
                // shared device), and this runs on the home shard from its reactor.
                let d = unsafe { &mut *st };
                d._queues_ready += 1;
                if d._queues_ready == d._num_queues {
                    // an error fails this (discarded) future
                    if let Err(e) = d.init_port_fini(st) {
                        return make_exception_future(e);
                    }
                }
                make_ready_future(())
            });
            Ok(qp)
        }

        fn hash2qid(&self, hash: u32) -> u32 {
            let t = dpdk_st!(self, _redir_table);
            assert!(!t.is_empty());
            // SAFETY: the table size is 1 or the RETA size, a power of two, so the mask keeps
            // the index in bounds.
            unsafe { *t.get_unchecked(hash as usize & (t.len() - 1)) as u32 }
        }

        fn rss_key(&self) -> RssKeyType {
            dpdk_st!(self, _rss_key)
        }
    }

    /// The transmit descriptor of one mbuf: lives in the first bytes of the mbuf
    /// pool element it describes; the private part overlays the mbuf's headroom.
    #[repr(C)]
    pub struct TxBuf<const HUGETLBFS_MEM_BACKEND: bool> {
        _mbuf: rte_mbuf,
        private_start: Marker,
        _p: Option<Packet>,
        _buf_iova: rte_iova_t,
        _data_off: u16,
        // TRUE if underlying mbuf has been used in the zero-copy flow
        _is_zc: bool,
        // buffers' factory the buffer came from
        _fc: *mut TxBufFactory<HUGETLBFS_MEM_BACKEND>,
        private_end: Marker,
    }

    impl<const H: bool> TxBuf<H> {
        /// The tx_buf that lives in `mbuf`.
        #[inline(always)]
        pub fn me(mbuf: *mut rte_mbuf) -> *mut Self {
            mbuf.cast()
        }

        // private:

        /// Returns whether the packet of the cluster whose head is `head` must be
        /// linearized because of the i40e hardware limits on fragments per packet (and,
        /// with TSO, per segment).
        ///
        /// # Safety
        ///
        /// `head` is the head of a well-formed mbuf cluster.
        unsafe fn i40e_should_linearize(head: *mut rte_mbuf) -> bool {
            // SAFETY: `head` is the head of a well-formed cluster.
            unsafe {
                let is_tso = (*head).ol_flags & RTE_MBUF_F_TX_TCP_SEG != 0;

                // For a non-TSO case: number of fragments should not exceed 8
                if !is_tso {
                    return (*head).nb_segs > I40E_MAX_XMIT_SEGMENT_FRAGS as u16;
                }

                //
                // For a TSO case each MSS window should not include more than 8
                // fragments including headers.
                //

                // Calculate the number of frags containing headers.
                //
                // Note: we support neither VLAN nor tunneling thus headers size
                // accounting is super simple.
                //
                let headers_size: usize =
                    ((*head).l2_len() + (*head).l3_len() + (*head).l4_len()) as usize;
                let mut hdr_frags: u32 = 0;
                let mut cur_payload_len: usize = 0;
                let mut cur_seg: *mut rte_mbuf = head;

                while !cur_seg.is_null() && cur_payload_len < headers_size {
                    cur_payload_len += (*cur_seg).data_len as usize;
                    cur_seg = (*cur_seg).next;
                    hdr_frags += 1;
                }

                //
                // Header fragments will be used for each TSO segment, thus the
                // maximum number of data segments will be 8 minus the number of
                // header fragments.
                //
                // It's unclear from the spec how the first TSO segment is treated
                // if the last fragment with headers contains some data bytes:
                // whether this fragment will be accounted as a single fragment or
                // as two separate fragments. We prefer to play it safe and assume
                // that this fragment will be accounted as two separate fragments.
                //
                let max_win_size: usize =
                    (I40E_MAX_XMIT_SEGMENT_FRAGS as u32).wrapping_sub(hdr_frags) as usize;

                if (*head).nb_segs as usize <= max_win_size {
                    return false;
                }

                // Get the data (without headers) part of the first data fragment
                let mut prev_frag_data: usize = cur_payload_len.wrapping_sub(headers_size);
                let mss = (*head).tso_segsz() as usize;

                while !cur_seg.is_null() {
                    let mut frags_in_seg: u32 = 0;
                    let mut cur_seg_size: usize = 0;

                    if prev_frag_data != 0 {
                        cur_seg_size = prev_frag_data;
                        frags_in_seg += 1;
                        prev_frag_data = 0;
                    }

                    while cur_seg_size < mss && !cur_seg.is_null() {
                        cur_seg_size += (*cur_seg).data_len as usize;
                        cur_seg = (*cur_seg).next;
                        frags_in_seg += 1;

                        if frags_in_seg as usize > max_win_size {
                            return true;
                        }
                    }

                    if cur_seg_size > mss {
                        prev_frag_data = cur_seg_size - mss;
                    }
                }

                false
            }
        }

        /// Sets the offload information (checksum and TSO flags, header lengths) of the
        /// packet `p` in `head`, the head mbuf of the cluster built for it, according to
        /// the offloads the queue `qp`'s port supports.
        ///
        /// # Safety
        ///
        /// `head` is the head of a cluster owned by `qp`.
        #[inline]
        unsafe fn set_cluster_offload_info(p: &Packet, qp: &DpdkQp<H>, head: *mut rte_mbuf) {
            // SAFETY: `head` is the head of a cluster owned by this queue.
            unsafe {
                // Handle TCP checksum offload
                let oi = p.get_offload_info();
                if oi.needs_ip_csum {
                    (*head).ol_flags |= RTE_MBUF_F_TX_IP_CKSUM;
                    // TODO: Take a VLAN header into an account here
                    (*head).set_l2_len(RTE_ETHER_HDR_SIZE);
                    (*head).set_l3_len(oi.ip_hdr_len as u64);
                }
                if qp.port().hw_features_inline().tx_csum_l4_offload {
                    if oi.protocol == IpProtocolNum::tcp {
                        (*head).ol_flags |= RTE_MBUF_F_TX_TCP_CKSUM;
                        // TODO: Take a VLAN header into an account here
                        (*head).set_l2_len(RTE_ETHER_HDR_SIZE);
                        (*head).set_l3_len(oi.ip_hdr_len as u64);

                        if oi.tso_seg_size != 0 {
                            assert!(oi.needs_ip_csum);
                            (*head).ol_flags |= RTE_MBUF_F_TX_TCP_SEG;
                            (*head).set_l4_len(oi.tcp_hdr_len as u64);
                            (*head).set_tso_segsz(oi.tso_seg_size as u64);
                        }
                    } else if oi.protocol == IpProtocolNum::udp {
                        (*head).ol_flags |= RTE_MBUF_F_TX_UDP_CKSUM;
                        // TODO: Take a VLAN header into an account here
                        (*head).set_l2_len(RTE_ETHER_HDR_SIZE);
                        (*head).set_l3_len(oi.ip_hdr_len as u64);
                    }
                }
            }
        }

        /// Builds a cluster of transmit buffers of the queue `qp` that points at the data
        /// of the packet `p` without copying it, and returns its head, or null on failure.
        ///
        /// A packet with too many fragments is linearized first. On success the packet
        /// moves into the cluster and `p` is left a null packet; on failure it stays in
        /// `p`.
        ///
        /// # Safety
        ///
        /// `qp` is a live queue whose transmit buffer factory owns the mbufs used.
        unsafe fn from_packet_zc(p: &mut Packet, qp: &mut DpdkQp<H>) -> *mut Self {
            // SAFETY: the mbufs come from this queue's factory and the cluster is built here.
            unsafe {
                // Too fragmented - linearize
                if p.nr_frags() > MAX_FRAGS as u32 {
                    p.linearize_all();
                    qp.base._stats.tx.linearized += 1;
                }

                // build_mbuf_cluster:
                loop {
                    let mut head: *mut rte_mbuf = ptr::null_mut();
                    let mut last_seg: *mut rte_mbuf = ptr::null_mut();
                    let mut nsegs: u32 = 0;

                    // Create a HEAD of the fragmented packet
                    if !Self::translate_one_frag(
                        qp,
                        p.frag(0),
                        &mut head,
                        &mut last_seg,
                        &mut nsegs,
                    ) {
                        return ptr::null_mut();
                    }

                    let mut total_nsegs: u32 = nsegs;

                    for i in 1..p.nr_frags() {
                        let mut h: *mut rte_mbuf = ptr::null_mut();
                        let mut new_last_seg: *mut rte_mbuf = ptr::null_mut();
                        if !Self::translate_one_frag(
                            qp,
                            p.frag(i),
                            &mut h,
                            &mut new_last_seg,
                            &mut nsegs,
                        ) {
                            Self::recycle(Self::me(head), &mut qp._tx_buf_factory);
                            return ptr::null_mut();
                        }

                        total_nsegs += nsegs;

                        // Attach a new buffers' chain to the packet chain
                        (*last_seg).next = h;
                        last_seg = new_last_seg;
                    }

                    // Update the HEAD buffer with the packet info
                    (*head).pkt_len = p.len();
                    (*head).nb_segs = total_nsegs as u16;

                    Self::set_cluster_offload_info(p, qp, head);

                    //
                    // If a packet hasn't been linearized already and the resulting
                    // cluster requires the linearisation due to HW limitation:
                    //
                    //    - Recycle the cluster.
                    //    - Linearize the packet.
                    //    - Build the cluster once again
                    //
                    if (*head).nb_segs > MAX_FRAGS as u16
                        || (p.nr_frags() > 1
                            && qp.port().is_i40e_device()
                            && Self::i40e_should_linearize(head))
                        || (p.nr_frags() > VMXNET3_MAX_XMIT_SEGMENT_FRAGS as u32
                            && qp.port().is_vmxnet3_device())
                    {
                        Self::recycle(Self::me(head), &mut qp._tx_buf_factory);
                        p.linearize_all();
                        qp.base._stats.tx.linearized += 1;

                        continue; // goto build_mbuf_cluster;
                    }

                    Self::set_packet(
                        Self::me(last_seg),
                        std::mem::replace(p, Packet::make_null_packet()),
                    );

                    return Self::me(head);
                }
            }
        }

        /// Copies the contents of the packet `p` into the mbuf cluster whose head is
        /// `head`.
        ///
        /// # Panics
        ///
        /// Panics if the cluster runs out of segments before the whole packet is copied.
        ///
        /// # Safety
        ///
        /// `head` is the head of a well-formed cluster with room for the whole packet.
        unsafe fn copy_packet_to_cluster(p: &Packet, head: *mut rte_mbuf) {
            // SAFETY: the cluster has room for the whole packet (see the note above).
            unsafe {
                let mut cur_seg: *mut rte_mbuf = head;
                let mut cur_seg_offset: usize = 0;
                let mut cur_frag_idx: u32 = 0;
                let mut cur_frag_offset: usize = 0;

                loop {
                    let frag = p.frag(cur_frag_idx);
                    let to_copy: usize =
                        (frag.size - cur_frag_offset).min(INLINE_MBUF_DATA_SIZE - cur_seg_offset);

                    ptr::copy_nonoverlapping(
                        frag.base.add(cur_frag_offset),
                        rte_pktmbuf_mtod_offset(cur_seg, cur_seg_offset),
                        to_copy,
                    );

                    cur_frag_offset += to_copy;
                    cur_seg_offset += to_copy;

                    if cur_frag_offset >= frag.size {
                        cur_frag_idx += 1;
                        if cur_frag_idx >= p.nr_frags() {
                            //
                            // We are done - set the data size of the last segment
                            // of the cluster.
                            //
                            (*cur_seg).data_len = cur_seg_offset as u16;
                            break;
                        }

                        cur_frag_offset = 0;
                    }

                    if cur_seg_offset >= INLINE_MBUF_DATA_SIZE {
                        (*cur_seg).data_len = INLINE_MBUF_DATA_SIZE as u16;
                        cur_seg = (*cur_seg).next;
                        cur_seg_offset = 0;

                        // FIXME: assert in a fast-path - remove!!!
                        assert!(!cur_seg.is_null());
                    }
                }
            }
        }

        /// Builds a cluster of transmit buffers of the queue `qp` holding a copy of the
        /// packet `p`, and returns its head, or null on failure (including an empty
        /// packet).
        ///
        /// # Safety
        ///
        /// `qp` is a live queue whose transmit buffer factory owns the mbufs used.
        unsafe fn from_packet_copy(p: &mut Packet, qp: &mut DpdkQp<H>) -> *mut Self {
            // sanity
            if p.len() == 0 {
                return ptr::null_mut();
            }

            /*
             * Here we are going to use the fact that the inline data size is a
             * power of two.
             *
             * We will first try to allocate the cluster and only if we are
             * successful - we will go and copy the data.
             */
            let aligned_len = align_up_usize(p.len() as usize, INLINE_MBUF_DATA_SIZE);
            let nsegs: u32 = (aligned_len / INLINE_MBUF_DATA_SIZE) as u32;

            // SAFETY: the mbufs come from this queue's factory and the cluster is built here.
            unsafe {
                let mut buf = qp.get_tx_buf();
                if buf.is_null() {
                    return ptr::null_mut();
                }

                let head: *mut rte_mbuf = Self::rte_mbuf_p(buf);
                let mut last_seg: *mut rte_mbuf = head;
                for _ in 1..nsegs {
                    buf = qp.get_tx_buf();
                    if buf.is_null() {
                        Self::recycle(Self::me(head), &mut qp._tx_buf_factory);
                        return ptr::null_mut();
                    }

                    (*last_seg).next = Self::rte_mbuf_p(buf);
                    last_seg = (*last_seg).next;
                }

                //
                // If we've got here means that we have succeeded already!
                // We only need to copy the data and set the head buffer with the
                // relevant info.
                //
                (*head).pkt_len = p.len();
                (*head).nb_segs = nsegs as u16;

                Self::copy_packet_to_cluster(p, head);
                Self::set_cluster_offload_info(p, qp, head);

                Self::me(head)
            }
        }

        /// Builds an mbuf cluster for the fragment `frag`, one mbuf at a time with
        /// `do_one_buf`, and returns whether it succeeded.
        ///
        /// `do_one_buf` sets up one mbuf of the queue `qp` from the data left in the
        /// fragment and returns the number of bytes it took, 0 on failure. On success the
        /// function stores the cluster's head in `head`, its last segment in `last_seg`
        /// and its number of segments in `nsegs`; on failure it recycles the partial
        /// cluster.
        ///
        /// # Panics
        ///
        /// Panics if `frag` is empty.
        ///
        /// # Safety
        ///
        /// `frag` points to `frag.size` readable bytes, and `do_one_buf` returns mbufs of
        /// `qp`.
        #[inline(always)]
        unsafe fn do_one_frag<F>(
            do_one_buf: F,
            qp: &mut DpdkQp<H>,
            frag: Fragment,
            head: &mut *mut rte_mbuf,
            last_seg: &mut *mut rte_mbuf,
            nsegs: &mut u32,
        ) -> bool
        where
            F: Fn(&mut DpdkQp<H>, &mut *mut rte_mbuf, *mut u8, usize) -> usize,
        {
            let mut len: usize;
            let mut left_to_set: usize = frag.size;
            let mut base: *mut u8 = frag.base;

            let mut m: *mut rte_mbuf = ptr::null_mut();

            // TODO: ENGINE_ASSERT() in a fast path! Remove me ASAP!
            assert!(frag.size != 0);

            // Create a HEAD of mbufs' cluster and set the first bytes into it
            len = do_one_buf(qp, head, base, left_to_set);
            if len == 0 {
                return false;
            }

            left_to_set -= len;
            // SAFETY: `len` bytes of the fragment were consumed, so `base` stays inside it.
            base = unsafe { base.add(len) };
            *nsegs = 1;

            //
            // Set the rest of the data into the new mbufs and chain them to
            // the cluster.
            //
            let mut prev_seg: *mut rte_mbuf = *head;
            while left_to_set != 0 {
                len = do_one_buf(qp, &mut m, base, left_to_set);
                if len == 0 {
                    // SAFETY: `head` is the cluster built so far.
                    unsafe { Self::recycle(Self::me(*head), &mut qp._tx_buf_factory) };
                    return false;
                }

                left_to_set -= len;
                // SAFETY: as above.
                base = unsafe { base.add(len) };
                *nsegs += 1;

                // SAFETY: `prev_seg` is the last mbuf of the cluster.
                unsafe { (*prev_seg).next = m };
                prev_seg = m;
            }

            // Return the last mbuf in the cluster
            *last_seg = prev_seg;

            true
        }

        /// Builds a zero-copy mbuf cluster that points at the data of the fragment
        /// `frag`, as `do_one_frag` with `set_one_data_buf`, and returns whether it
        /// succeeded; the outputs `head`, `last_seg` and `nsegs` are as for `do_one_frag`.
        ///
        /// # Safety
        ///
        /// As `do_one_frag`.
        #[inline(always)]
        unsafe fn translate_one_frag(
            qp: &mut DpdkQp<H>,
            frag: Fragment,
            head: &mut *mut rte_mbuf,
            last_seg: &mut *mut rte_mbuf,
            nsegs: &mut u32,
        ) -> bool {
            // SAFETY: the fragment belongs to the packet being sent.
            unsafe {
                Self::do_one_frag(
                    // SAFETY: `va` points into the fragment with `buf_len` bytes left.
                    |qp, m, va, buf_len| Self::set_one_data_buf(qp, m, va, buf_len),
                    qp,
                    frag,
                    head,
                    last_seg,
                    nsegs,
                )
            }
        }

        /// Builds an mbuf cluster holding a copy of the fragment `frag`, as `do_one_frag`
        /// with `copy_one_data_buf`, and returns whether it succeeded; the outputs `head`,
        /// `last_seg` and `nsegs` are as for `do_one_frag`.
        ///
        /// The last segment is returned so that the caller need not walk the cluster to
        /// find it.
        ///
        /// # Safety
        ///
        /// As `do_one_frag`.
        #[allow(dead_code)] // not used yet
        #[inline(always)]
        unsafe fn copy_one_frag(
            qp: &mut DpdkQp<H>,
            frag: Fragment,
            head: &mut *mut rte_mbuf,
            last_seg: &mut *mut rte_mbuf,
            nsegs: &mut u32,
        ) -> bool {
            // SAFETY: the fragment belongs to the packet being sent.
            unsafe {
                Self::do_one_frag(
                    // SAFETY: `data` points into the fragment with `buf_len` bytes left.
                    |qp, m, data, buf_len| Self::copy_one_data_buf(qp, m, data, buf_len),
                    qp,
                    frag,
                    head,
                    last_seg,
                    nsegs,
                )
            }
        }

        /// Allocates one mbuf of the queue `qp`, points it at the data buffer `va`, stores
        /// it in `m`, and returns the number of bytes of the buffer it covers (0 when no
        /// mbuf is available).
        ///
        /// An mbuf covers at most 15 KiB (82599 devices limit a single fragment to
        /// 15.5 KiB), so it may cover fewer than `buf_len` bytes. When the buffer has no IO
        /// address, the data is copied instead, as by `copy_one_data_buf`.
        ///
        /// # Safety
        /// `va` points to `buf_len` readable bytes of the packet being sent.
        #[inline(always)]
        unsafe fn set_one_data_buf(
            qp: &mut DpdkQp<H>,
            m: &mut *mut rte_mbuf,
            va: *mut u8,
            buf_len: usize,
        ) -> usize {
            let max_frag_len: usize = kib(15);

            //
            // Currently we break a buffer on a 15K boundary because 82599
            // devices have a 15.5K limitation on a maximum single fragment
            // size.
            //
            // SAFETY: rte_mem_virt2iova() only looks up the IO address; it does not access the memory.
            let iova: rte_iova_t = unsafe { rte_mem_virt2iova(va.cast()) };

            if iova == RTE_BAD_IOVA {
                // SAFETY: guaranteed by the caller.
                return unsafe { Self::copy_one_data_buf(qp, m, va, buf_len) };
            }

            let buf = qp.get_tx_buf();
            if buf.is_null() {
                return 0;
            }

            let len: usize = buf_len.min(max_frag_len);

            // SAFETY: `buf` is a free tx_buf of this queue.
            unsafe { Self::set_zc_info(buf, va.cast(), iova, len) };
            *m = Self::rte_mbuf_p(buf);

            len
        }

        /// Allocates one mbuf of the queue `qp`, copies into it as much of the `buf_len`
        /// bytes at `data` as it holds, stores it in `m`, and returns the number of bytes
        /// copied (0 when no mbuf is available).
        ///
        /// # Safety
        /// `data` points to `buf_len` readable bytes of the packet being sent.
        #[inline(always)]
        unsafe fn copy_one_data_buf(
            qp: &mut DpdkQp<H>,
            m: &mut *mut rte_mbuf,
            data: *mut u8,
            buf_len: usize,
        ) -> usize {
            let buf = qp.get_tx_buf();
            if buf.is_null() {
                return 0;
            }

            let len: usize = buf_len.min(INLINE_MBUF_DATA_SIZE);

            *m = Self::rte_mbuf_p(buf);

            // SAFETY: `*m` is a free inline mbuf with INLINE_MBUF_DATA_SIZE bytes of room and
            // `data` points to at least `len` bytes of the fragment being sent.
            unsafe {
                // mbuf_put()
                (**m).data_len = len as u16;
                (**m).pkt_len = len as u32;

                qp.base._stats.tx.good.update_copy_stats(1, len as u64);

                ptr::copy_nonoverlapping(data, rte_pktmbuf_mtod(*m), len);
            }

            len
        }

        // public:

        /// Initializes a tx_buf over a freshly allocated mbuf: the mbuf itself is left as
        /// `rte_pktmbuf_alloc()` initialized it.
        ///
        /// # Safety
        /// `this` is an mbuf of the factory's pool with room for the private part in its
        /// headroom; `fc` is the factory at its final address.
        unsafe fn init_at(this: *mut Self, fc: *mut TxBufFactory<H>) {
            // SAFETY: guaranteed by the caller; the private fields are written without
            // dropping the garbage that was there.
            unsafe {
                (&raw mut (*this)._p).write(None);
                (&raw mut (*this)._is_zc).write(false);
                (&raw mut (*this)._fc).write(fc);

                (&raw mut (*this)._buf_iova).write((*this)._mbuf.buf_iova);
                (&raw mut (*this)._data_off).write((*this)._mbuf.data_off);
            }
        }

        /// The mbuf of this tx_buf.
        #[inline(always)]
        pub fn rte_mbuf_p(this: *mut Self) -> *mut rte_mbuf {
            // _mbuf is the first field (see the static assertions below).
            this.cast()
        }

        /// Points the mbuf at `len` bytes of external memory at `va` / `iova` (zero copy).
        ///
        /// # Safety
        /// `this` is a live tx_buf.
        #[inline(always)]
        pub unsafe fn set_zc_info(this: *mut Self, va: *mut c_void, iova: rte_iova_t, len: usize) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                // mbuf_put()
                (*this)._mbuf.data_len = len as u16;
                (*this)._mbuf.pkt_len = len as u32;

                // Set the mbuf to point to our data
                (*this)._mbuf.buf_addr = va;
                (*this)._mbuf.buf_iova = iova;
                (*this)._mbuf.data_off = 0;
                (*this)._is_zc = true;
            }
        }

        /// Undoes [`set_zc_info`](Self::set_zc_info) and drops the packet the mbuf carried.
        ///
        /// # Safety
        /// `this` is a live tx_buf.
        #[inline(always)]
        pub unsafe fn reset_zc(this: *mut Self) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                //
                // If this mbuf was the last in a cluster and contains an
                // packet object then drop that packet object.
                //
                if (*this)._p.is_some() {
                    //
                    // Reset the optional: this drops the packet.
                    //
                    (*this)._p = None;
                } else if !(*this)._is_zc {
                    return;
                }

                // Restore the rte_mbuf fields we trashed in set_zc_info()
                (*this)._mbuf.buf_iova = (*this)._buf_iova;
                (*this)._mbuf.buf_addr = rte_mbuf_to_baddr(&raw mut (*this)._mbuf).cast();
                (*this)._mbuf.data_off = (*this)._data_off;

                (*this)._is_zc = false;
            }
        }

        /// Gives every buffer of the cluster back to the factory.
        ///
        /// The factory comes from the caller instead of `_fc`: every caller is building a
        /// cluster inside `send_batch(&mut self)`, whose protected reference covers the
        /// factory (a field of the queue), so a write through the pointer the tx_buf keeps
        /// would alias it. `_fc` is the same factory and is checked against it.
        ///
        /// # Safety
        /// `this` is the head of a cluster of tx_bufs of `fc`.
        pub unsafe fn recycle(this: *mut Self, fc: &mut TxBufFactory<H>) {
            // SAFETY: guaranteed by the caller; every segment is a tx_buf of `fc`.
            unsafe {
                let mut m: *mut rte_mbuf = Self::rte_mbuf_p(this);
                debug_assert!(ptr::eq((*this)._fc, fc), "tx_buf recycled into a foreign factory");

                while !m.is_null() {
                    let m_next = (*m).next;
                    rte_pktmbuf_reset(m);
                    // SAFETY: `m` is a segment of a cluster of tx_bufs of `fc` (contract of
                    // `recycle`), taken out of the cluster and not yet in the factory's ring.
                    fc.put(Self::me(m));
                    m = m_next;
                }
            }
        }

        /// Attaches `p` to the tx_buf; it is dropped when the mbuf is recycled.
        ///
        /// # Safety
        /// `this` is a live tx_buf.
        #[inline(always)]
        pub unsafe fn set_packet(this: *mut Self, p: Packet) {
            // SAFETY: guaranteed by the caller.
            unsafe { (*this)._p = Some(p) };
        }
    }

    /// The pool of tx_bufs of one queue, with the completed buffers waiting to be reused.
    pub struct TxBufFactory<const HUGETLBFS_MEM_BACKEND: bool> {
        _ring: Vec<*mut TxBuf<HUGETLBFS_MEM_BACKEND>>,
        _pool: *mut rte_mempool,
        _xmem: MallocBuf,
    }

    impl<const H: bool> TxBufFactory<H> {
        //
        // Number of buffers to free in each GC iteration:
        // We want the buffers to be allocated from the mempool as many as
        // possible.
        //
        // On the other hand if there is no Tx for some time we want the
        // completions to be eventually handled. Thus we choose the smallest
        // possible packets count number here.
        //
        const GC_COUNT: i32 = 1;

        /// Creates the factory with its fields only; [`TxBufFactory::init`] completes it, run once
        /// the factory has its final address (the tx_bufs point back at it).
        fn new() -> Self {
            TxBufFactory { _ring: Vec::new(), _pool: ptr::null_mut(), _xmem: MallocBuf::default() }
        }

        /// Creates the factory's mbuf pool for queue `qid`.
        ///
        /// # Safety
        /// `this` is the factory at its final address; the EAL is initialized.
        unsafe fn init(this: *mut Self, qid: u16) {
            let name = format!("{}{}_tx", PKTMBUF_POOL_NAME, qid);
            println!("Creating Tx mbuf pool '{}' [{} mbufs] ...", name, MBUFS_PER_QUEUE_TX);
            let cname = CString::new(name).unwrap_or_default();

            // SAFETY: guaranteed by the caller; the pool calls get valid arguments.
            unsafe {
                let me = &mut *this;
                if H {
                    let mut xmem_size: usize = 0;

                    me._xmem = MallocBuf::from_raw(
                        DpdkQp::<H>::alloc_mempool_xmem(
                            MBUFS_PER_QUEUE_TX,
                            INLINE_MBUF_SIZE,
                            &mut xmem_size,
                        )
                        .cast(),
                    );
                    if me._xmem.get().is_null() {
                        println!("Can't allocate a memory for Tx buffers");
                        std::process::exit(1);
                    }

                    //
                    // We are going to push the buffers from the mempool into
                    // the circular_buffer and then poll them from there anyway, so
                    // we prefer to make a mempool non-atomic in this case.
                    //
                    me._pool = rte_mempool_create_empty(
                        cname.as_ptr(),
                        MBUFS_PER_QUEUE_TX as c_uint,
                        INLINE_MBUF_SIZE as c_uint,
                        MBUF_CACHE_SIZE as c_uint,
                        size_of::<rte_pktmbuf_pool_private>() as c_uint,
                        rte_socket_id() as c_int,
                        0,
                    );
                    if !me._pool.is_null() {
                        rte_pktmbuf_pool_init(me._pool, ptr::null_mut());

                        if rte_mempool_populate_virt(
                            me._pool,
                            me._xmem.get().cast(),
                            xmem_size,
                            PAGE_SIZE,
                            None,
                            ptr::null_mut(),
                        ) <= 0
                        {
                            println!("Failed to populate mempool for Tx");
                            std::process::exit(1);
                        }

                        rte_mempool_obj_iter(me._pool, Some(rte_pktmbuf_init), ptr::null_mut());
                    }
                } else {
                    me._pool = rte_mempool_create(
                        cname.as_ptr(),
                        MBUFS_PER_QUEUE_TX as c_uint,
                        INLINE_MBUF_SIZE as c_uint,
                        MBUF_CACHE_SIZE as c_uint,
                        size_of::<rte_pktmbuf_pool_private>() as c_uint,
                        Some(rte_pktmbuf_pool_init),
                        ptr::null_mut(),
                        Some(rte_pktmbuf_init),
                        ptr::null_mut(),
                        rte_socket_id() as c_int,
                        0,
                    );
                }

                if me._pool.is_null() {
                    println!("Failed to create mempool for Tx");
                    std::process::exit(1);
                }
                if let Err(e) = rte_mempool_check_layout(me._pool) {
                    println!("{}", e);
                    std::process::exit(1);
                }

                //
                // Fill the factory with the buffers from the mempool allocated
                // above.
                //
                Self::init_factory(this);
            }
        }

        /// Returns a free tx_buf, taking the buffers completed by the hardware first.
        ///
        /// Must not be called when there is no free tx_buf.
        #[inline(always)]
        pub fn get(&mut self) -> *mut TxBuf<H> {
            // Take completed from the HW first
            let pkt = self.get_one_completed();
            if !pkt.is_null() {
                if H {
                    // SAFETY: a completed mbuf of the pool is one of our tx_bufs.
                    unsafe { TxBuf::reset_zc(pkt) };
                }

                return pkt;
            }

            //
            // If there are no completed at the moment - take from the
            // factory's cache.
            //
            match self._ring.pop() {
                Some(pkt) => pkt,
                None => ptr::null_mut(),
            }
        }

        /// Returns `buf` to the factory.
        ///
        /// # Safety
        /// `buf` must point to a live tx_buf of this factory (one built in its mempool) that
        /// is not already in the factory's ring; in zero-copy mode its mbuf is reset through
        /// the pointer.
        #[inline(always)]
        pub unsafe fn put(&mut self, buf: *mut TxBuf<H>) {
            if H {
                // SAFETY: `buf` is a live tx_buf of this factory (contract of this function).
                unsafe { TxBuf::reset_zc(buf) };
            }
            self._ring.push(buf);
        }

        /// Collects the buffers the hardware has completed; whether any were collected.
        pub fn gc(&mut self) -> bool {
            for _ in 0..Self::GC_COUNT {
                let tx_buf_p = self.get_one_completed();
                if tx_buf_p.is_null() {
                    return false;
                }

                // SAFETY: a non-null completed mbuf of the pool is a live tx_buf of this
                // factory that the hardware has released and the ring does not hold.
                unsafe { self.put(tx_buf_p) };
            }

            true
        }

        // private:

        /// Moves every mbuf of the pool into the factory's ring of transmit buffers,
        /// leaving the pool empty.
        ///
        /// The empty pool then catches the completed buffers: the PMD driver frees an
        /// mbuf back to the pool once its transmission completes, and the factory takes
        /// the completed buffers back from the pool.
        ///
        /// # Safety
        ///
        /// `this` is the factory at its final address, with a live pool whose mbufs each
        /// have room for a transmit buffer.
        unsafe fn init_factory(this: *mut Self) {
            // SAFETY: `this` is the factory at its final address with a live pool; each mbuf
            // of the pool has room for a tx_buf.
            unsafe {
                loop {
                    let mbuf = rte_pktmbuf_alloc((*this)._pool);
                    if mbuf.is_null() {
                        break;
                    }
                    let b = TxBuf::<H>::me(mbuf);
                    TxBuf::init_at(b, this);
                    (*this)._ring.push(b);
                }
            }
        }

        /// Returns one transmit buffer whose transmission the hardware has completed, or
        /// null if there is none.
        ///
        /// The PMD driver puts completed buffers back into the pool they came from, so this
        /// allocates one from the pool; `rte_pktmbuf_alloc()` resets the mbuf, so it needs
        /// no `rte_pktmbuf_reset()`.
        #[inline(always)]
        fn get_one_completed(&mut self) -> *mut TxBuf<H> {
            // SAFETY: the pool is live and its mbufs are our tx_bufs.
            TxBuf::me(unsafe { rte_pktmbuf_alloc(self._pool) })
        }
    }

    /// The DPDK queue pair of one shard; `HUGETLBFS_MEM_BACKEND` selects zero-copy
    /// operation over hugetlbfs memory.
    ///
    /// The pollers are `Option`s because they keep the queue's address, which is final only
    /// once the queue is in its node; they are set in declaration order.
    pub struct DpdkQp<const HUGETLBFS_MEM_BACKEND: bool> {
        /// The queue's address with the provenance of its node, written once by `new`; the
        /// pollers keep this one.
        _this: *mut Self,
        base: QpBase,
        _dev: *const DpdkDevice,
        _qid: u16,
        _pktmbuf_pool_rx: *mut rte_mempool,
        _rx_free_pkts: Vec<*mut rte_mbuf>,
        _rx_free_bufs: Vec<*mut rte_mbuf>,
        _frags: Vec<Fragment>,
        _bufs: Vec<*mut u8>,
        _num_rx_free_segs: usize,
        _rx_gc_poller: Option<Poller>,
        _rx_xmem: MallocBuf,
        _tx_buf_factory: TxBufFactory<HUGETLBFS_MEM_BACKEND>,
        _rx_poller: Option<Poller>,
        _tx_gc_poller: Option<Poller>,
        _tx_burst: Vec<*mut rte_mbuf>,
        _tx_burst_idx: u16,
    }

    impl<const H: bool> DpdkQp<H> {
        #[allow(dead_code)] // not used yet
        const PAGE_MASK: u64 = !(PAGE_SIZE as u64 - 1);

        /// The device of this queue.
        #[inline(always)]
        pub fn port(&self) -> &DpdkDevice {
            // SAFETY: the device outlives its queues.
            unsafe { &*self._dev }
        }

        /// A free tx_buf from the factory.
        #[inline(always)]
        pub fn get_tx_buf(&mut self) -> *mut TxBuf<H> {
            self._tx_buf_factory.get()
        }

        // private:

        #[inline(always)]
        fn _send<F>(&mut self, pb: &mut CircularBuffer<Packet>, mut packet_to_tx_buf_p: F) -> u32
        where
            F: FnMut(&mut Self, &mut Packet) -> *mut TxBuf<H>,
        {
            if self._tx_burst.is_empty() {
                for p in pb.iter_mut() {
                    // TODO: ENGINE_ASSERT() in a fast path! Remove me ASAP!
                    assert!(p.len() != 0);

                    let buf = packet_to_tx_buf_p(self, p);
                    if buf.is_null() {
                        break;
                    }

                    self._tx_burst.push(TxBuf::rte_mbuf_p(buf));
                }
            }

            let idx = self._tx_burst_idx as usize;
            // SAFETY: `_tx_burst[idx..]` holds the clusters not sent yet, all of this queue.
            let sent: u16 = unsafe {
                rte_eth_tx_burst(
                    self.port().port_idx(),
                    self._qid,
                    self._tx_burst.as_mut_ptr().add(idx),
                    (self._tx_burst.len() - idx) as u16,
                )
            };

            let mut nr_frags: u64 = 0;
            let mut bytes: u64 = 0;

            for i in 0..sent as usize {
                // SAFETY: the PMD sent at most the entries it was handed, so the index is in
                // bounds; the head is read after the hand-over, before the
                // PMD can complete it.
                unsafe {
                    let m: *mut rte_mbuf = *self._tx_burst.get_unchecked(idx + i);
                    bytes += (*m).pkt_len as u64;
                    nr_frags += (*m).nb_segs as u64;
                }
                pb.pop_front();
            }

            self.base._stats.tx.good.update_frags_stats(nr_frags, bytes);

            self._tx_burst_idx += sent;

            if self._tx_burst_idx as usize == self._tx_burst.len() {
                self._tx_burst_idx = 0;
                self._tx_burst.clear();
            }

            sent as u32
        }

        /// Allocates a new data buffer of `size` bytes and points the mbuf `m` at it;
        /// returns `false` if the allocation fails.
        ///
        /// The PMD expects `buf_addr` to point `RTE_PKTMBUF_HEADROOM` bytes before the
        /// data, so the mbuf's address, length and data offset are adjusted accordingly.
        ///
        /// # Safety
        ///
        /// `m` is a valid mbuf of a receive pool.
        #[inline(always)]
        unsafe fn refill_rx_mbuf(m: *mut rte_mbuf, size: usize) -> bool {
            let mut data: *mut c_void = ptr::null_mut();

            // SAFETY: `data` is valid for writes.
            if unsafe { posix_memalign(&mut data, size, size) } != 0 {
                return false;
            }

            // SAFETY: rte_mem_virt2iova() only looks up the IO address; it does not access the memory.
            let iova: rte_iova_t = unsafe { rte_mem_virt2iova(data) };

            //
            // Set the mbuf to point to our data.
            //
            // Do some DPDK hacks to work on PMD: it assumes that the buf_addr
            // points to the private data of RTE_PKTMBUF_HEADROOM before the
            // actual data buffer.
            //
            // SAFETY: `m` is an rx mbuf of this queue.
            unsafe {
                (*m).buf_addr =
                    data.cast::<u8>().wrapping_sub(RTE_PKTMBUF_HEADROOM as usize).cast();
                (*m).buf_iova = iova.wrapping_sub(RTE_PKTMBUF_HEADROOM as u64);
            }
            true
        }

        unsafe fn init_noninline_rx_mbuf(m: *mut rte_mbuf, size: usize) -> bool {
            // SAFETY: `m` is an rx mbuf of this queue.
            unsafe {
                if !Self::refill_rx_mbuf(m, size) {
                    return false;
                }
                // The below fields stay constant during the execution.
                (*m).buf_len = (size + RTE_PKTMBUF_HEADROOM as usize) as u16;
                (*m).data_off = RTE_PKTMBUF_HEADROOM;
            }
            true
        }

        /// Allocates a memory chunk for `num_bufs` buffers of `buf_sz` bytes, to be used as
        /// the external memory of a DPDK memory pool.
        ///
        /// The chunk size is computed by `get_mempool_xmem_size()` and stored in
        /// `xmem_size`. Returns the chunk's virtual address, or null on failure.
        fn alloc_mempool_xmem(num_bufs: u16, buf_sz: u16, xmem_size: &mut usize) -> *mut c_void {
            let mut xmem: *mut c_void = ptr::null_mut();
            let mut mp_obj_sz = rte_mempool_objsz::default();

            // SAFETY: rte_mempool_calc_obj_size() only computes sizes into `mp_obj_sz`.
            unsafe { rte_mempool_calc_obj_size(buf_sz as u32, 0, &mut mp_obj_sz) };

            *xmem_size = get_mempool_xmem_size(
                num_bufs as u32,
                (mp_obj_sz.elt_size + mp_obj_sz.header_size + mp_obj_sz.trailer_size) as usize,
                PAGE_BITS as u32,
            );

            // Aligning to 2M causes the further failure in small allocations.
            // TODO: Check why - and fix.
            // SAFETY: `xmem` is valid for writes.
            if unsafe { posix_memalign(&mut xmem, PAGE_SIZE, *xmem_size) } != 0 {
                println!("Can't allocate {} bytes aligned to {}", *xmem_size, PAGE_SIZE);
                return ptr::null_mut();
            }

            xmem
        }

        /// Creates the receive mbuf pool of the queue; whether it succeeded.
        fn init_rx_mbuf_pool(&mut self) -> bool {
            let name = format!("{}{}_rx", PKTMBUF_POOL_NAME, self._qid);

            println!("Creating Rx mbuf pool '{}' [{} mbufs] ...", name, MBUFS_PER_QUEUE_RX);
            let cname = CString::new(name).unwrap_or_default();

            //
            // If we have a hugetlbfs memory backend we may perform a virt2phys
            // lookup and memory is "pinned". Therefore we may provide an external
            // memory for DPDK pools and this way significantly reduce the memory needed
            // for the DPDK in this case.
            //
            // SAFETY: the pool calls get valid arguments; the mbufs are the pool's own.
            unsafe {
                if H {
                    let mut xmem_size: usize = 0;

                    self._rx_xmem = MallocBuf::from_raw(
                        Self::alloc_mempool_xmem(MBUFS_PER_QUEUE_RX, MBUF_OVERHEAD, &mut xmem_size)
                            .cast(),
                    );
                    if self._rx_xmem.get().is_null() {
                        println!("Can't allocate a memory for Rx buffers");
                        return false;
                    }

                    //
                    // Don't pass single-producer/single-consumer flags to mbuf create as it
                    // seems faster to use a cache instead.
                    //
                    let mut roomsz = rte_pktmbuf_pool_private {
                        mbuf_data_room_size: (MBUF_DATA_SIZE + RTE_PKTMBUF_HEADROOM as usize)
                            as u16,
                        ..Default::default()
                    };
                    self._pktmbuf_pool_rx = rte_mempool_create_empty(
                        cname.as_ptr(),
                        MBUFS_PER_QUEUE_RX as c_uint,
                        MBUF_OVERHEAD as c_uint,
                        MBUF_CACHE_SIZE as c_uint,
                        size_of::<rte_pktmbuf_pool_private>() as c_uint,
                        rte_socket_id() as c_int,
                        0,
                    );
                    if self._pktmbuf_pool_rx.is_null() {
                        println!("Failed to create mempool for Rx");
                        std::process::exit(1);
                    }

                    rte_pktmbuf_pool_init(self._pktmbuf_pool_rx, as_cookie(&mut roomsz));

                    if rte_mempool_populate_virt(
                        self._pktmbuf_pool_rx,
                        self._rx_xmem.get().cast(),
                        xmem_size,
                        PAGE_SIZE,
                        None,
                        ptr::null_mut(),
                    ) < 0
                    {
                        println!("Failed to populate mempool for Rx");
                        std::process::exit(1);
                    }

                    rte_mempool_obj_iter(
                        self._pktmbuf_pool_rx,
                        Some(rte_pktmbuf_init),
                        ptr::null_mut(),
                    );

                    // reserve the memory for Rx buffers containers
                    self._rx_free_pkts.reserve(MBUFS_PER_QUEUE_RX as usize);
                    self._rx_free_bufs.reserve(MBUFS_PER_QUEUE_RX as usize);

                    //
                    // 1) Pull all entries from the pool.
                    // 2) Bind data buffers to each of them.
                    // 3) Return them back to the pool.
                    //
                    for _ in 0..MBUFS_PER_QUEUE_RX {
                        let m = rte_pktmbuf_alloc(self._pktmbuf_pool_rx);
                        assert!(!m.is_null());
                        self._rx_free_bufs.push(m);
                    }

                    for &m in self._rx_free_bufs.iter() {
                        if !Self::init_noninline_rx_mbuf(m, MBUF_DATA_SIZE) {
                            println!(
                                "Failed to allocate data buffers for Rx ring. Consider increasing the amount of memory."
                            );
                            std::process::exit(1);
                        }
                    }

                    rte_mempool_put_bulk(
                        self._pktmbuf_pool_rx,
                        self._rx_free_bufs.as_ptr().cast(),
                        self._rx_free_bufs.len() as c_uint,
                    );

                    self._rx_free_bufs.clear();
                } else {
                    let mut roomsz = rte_pktmbuf_pool_private {
                        mbuf_data_room_size: (INLINE_MBUF_DATA_SIZE + RTE_PKTMBUF_HEADROOM as usize)
                            as u16,
                        ..Default::default()
                    };
                    self._pktmbuf_pool_rx = rte_mempool_create(
                        cname.as_ptr(),
                        MBUFS_PER_QUEUE_RX as c_uint,
                        INLINE_MBUF_SIZE as c_uint,
                        MBUF_CACHE_SIZE as c_uint,
                        size_of::<rte_pktmbuf_pool_private>() as c_uint,
                        Some(rte_pktmbuf_pool_init),
                        as_cookie(&mut roomsz),
                        Some(rte_pktmbuf_init),
                        ptr::null_mut(),
                        rte_socket_id() as c_int,
                        0,
                    );
                }
            }

            if !self._pktmbuf_pool_rx.is_null() {
                // SAFETY: a pool just made by rte_mempool_create(_empty).
                if let Err(e) = unsafe { rte_mempool_check_layout(self._pktmbuf_pool_rx) } {
                    println!("{}", e);
                    std::process::exit(1);
                }
            }
            !self._pktmbuf_pool_rx.is_null()
        }

        // Map DMA address explicitly.
        // XXX: does NOT work with NICs whose PMD uses InfiniBand verbs instead of VFIO.
        fn map_dma(&self) -> Result<bool, ExceptionPtr> {
            let m = get_memory_layout()?;
            // SAFETY: rte_mem_virt2iova() only looks up the IO address; it does not access the memory.
            let iova: rte_iova_t = unsafe { rte_mem_virt2iova(m.start as *const c_void) };

            // SAFETY: the range is this shard's memory, mapped for the process lifetime.
            Ok(unsafe {
                rte_vfio_container_dma_map(
                    RTE_VFIO_DEFAULT_CONTAINER_FD,
                    m.start as u64,
                    iova,
                    (m.end - m.start) as u64,
                )
            } == 0)
        }

        /// Creates queue `qid` of `dev`, registering its metrics under `stats_plugin_name`.
        ///
        /// The queue lives in a node that is never moved: the owner handed back reasserts no
        /// uniqueness, so the pointers that the pollers and the factory keep stay valid.
        pub fn new(
            dev: *const DpdkDevice,
            qid: u16,
            stats_plugin_name: String,
        ) -> Result<NodePtr<Self>, ExceptionPtr> {
            // : qp(true, stats_plugin_name, qid), _dev(dev), _qid(qid), ...
            let tp: *mut Self = NodePtr::new(DpdkQp {
                _this: ptr::null_mut(),
                base: QpBase::new(&stats_plugin_name, qid as u8),
                _dev: dev,
                _qid: qid,
                _pktmbuf_pool_rx: ptr::null_mut(),
                _rx_free_pkts: Vec::new(),
                _rx_free_bufs: Vec::new(),
                _frags: Vec::new(),
                _bufs: Vec::new(),
                _num_rx_free_segs: 0,
                _rx_gc_poller: None,
                _rx_xmem: MallocBuf::default(),
                _tx_buf_factory: TxBufFactory::new(),
                _rx_poller: None,
                _tx_gc_poller: None,
                _tx_burst: Vec::new(),
                _tx_burst_idx: 0,
            })
            .into_raw();
            // SAFETY: `tp` is the queue at its final address (the node's root pointer); the
            // pollers and the factory capture it and live inside it. It is owned again by a
            // `NodePtr` on both paths below.
            unsafe {
                (*tp)._this = tp;
                qp_init(tp as *mut dyn Qp, true);
                (*tp)._rx_gc_poller = Some(Poller::simple(move || (*tp).rx_gc()));
                TxBufFactory::init(&raw mut (*tp)._tx_buf_factory, qid);
                let fc: *mut TxBufFactory<H> = &raw mut (*tp)._tx_buf_factory;
                (*tp)._tx_gc_poller = Some(Poller::simple(move || (*fc).gc()));

                if let Err(e) = (*tp).construct() {
                    drop(NodePtr::from_raw(tp));
                    return Err(e);
                }
                Ok(NodePtr::from_raw(tp))
            }
        }

        /// The body of the constructor.
        fn construct(&mut self) -> Result<(), ExceptionPtr> {
            if !self.init_rx_mbuf_pool() {
                rte_exit_fmt(EXIT_FAILURE, format_args!("Cannot initialize mbuf pools\n"));
            }

            if H && !self.map_dma()? {
                rte_exit_fmt(EXIT_FAILURE, format_args!("Cannot map DMA\n"));
            }

            // The static_asserts on tx_buf are below, at module level.

            let port_idx = self.port().port_idx();
            // SAFETY: the port is configured and the pool is live.
            if unsafe {
                rte_eth_rx_queue_setup(
                    port_idx,
                    self._qid,
                    DEFAULT_RING_SIZE,
                    rte_eth_dev_socket_id(port_idx) as c_uint,
                    self.port().def_rx_conf(),
                    self._pktmbuf_pool_rx,
                )
            } < 0
            {
                rte_exit_fmt(EXIT_FAILURE, format_args!("Cannot initialize rx queue\n"));
            }

            // SAFETY: the port is configured.
            if unsafe {
                rte_eth_tx_queue_setup(
                    port_idx,
                    self._qid,
                    DEFAULT_RING_SIZE,
                    rte_eth_dev_socket_id(port_idx) as c_uint,
                    self.port().def_tx_conf(),
                )
            } < 0
            {
                rte_exit_fmt(EXIT_FAILURE, format_args!("Cannot initialize tx queue\n"));
            }

            // Register error statistics: Rx total and checksum errors
            let stats: *const QpStats = &self.base._stats;
            macro_rules! qs {
                ($($f:tt)+) => {
                    // SAFETY: the metrics belong to the queue, which owns the statistics they
                    // read and never moves, so `stats` is live whenever a metric closure runs.
                    move || unsafe { (*stats).$($f)+ }
                };
            }
            let qn = &self.base._queue_name;
            self.base._metrics.add_group(
                &self.base._stats_plugin_name,
                vec![
                    sm::make_counter(&format!("{}_rx_csum_errors", qn), qs!(rx.bad.csum),
                        sm::Description::new(concat!(
                            "Counts a number of packets received by this queue that have a bad CSUM value. ",
                            "A non-zero value of this metric usually indicates a HW issue, e.g. a bad cable.")), vec![]),

                    sm::make_counter(&format!("{}_rx_errors", qn), qs!(rx.bad.total),
                        sm::Description::new("Counts a total number of errors in the ingress path for this queue: CSUM errors, etc."), vec![]),

                    sm::make_counter(&format!("{}_rx_no_memory_errors", qn), qs!(rx.bad.no_mem),
                        sm::Description::new(concat!(
                            "Counts a number of ingress packets received by this HW queue but dropped by the SW due to low memory. ",
                            "A non-zero value indicates that engine doesn't have enough memory to handle the packet reception or the memory is too fragmented.")), vec![]),
                ],
            )?;
            Ok(())
        }

        /// Converts the LRO mbuf cluster whose head is `m` into a packet, or returns `None`
        /// on failure.
        ///
        /// # Safety
        ///
        /// `m` is the head of a cluster received on this queue.
        #[inline(always)]
        // Takes `&mut self`: it allocates the packet's fragments from this queue's state.
        #[allow(clippy::wrong_self_convention)]
        unsafe fn from_mbuf_lro(&mut self, mut m: *mut rte_mbuf) -> Option<Packet> {
            if !H {
                // Without a hugetlbfs backend:
                //
                // Try to allocate a buffer for the whole packet's data.
                // If we fail - construct the packet from mbufs.
                // If we succeed - copy the data into this buffer, create a packet based on
                // this buffer and return the mbuf to its pool.
                //
                // SAFETY: `m` is a received cluster of this queue's pool.
                unsafe {
                    let pkt_len = rte_pktmbuf_pkt_len(m);
                    let buf = malloc(pkt_len as usize).cast::<u8>();
                    if !buf.is_null() {
                        // Copy the contents of the packet into the buffer we've just allocated
                        let mut offset: usize = 0;
                        let mut m1 = m;
                        while !m1.is_null() {
                            let data = rte_pktmbuf_mtod(m1);
                            let len = rte_pktmbuf_data_len(m1) as usize;

                            rte_memcpy(buf.add(offset).cast(), data.cast(), len);
                            offset += len;
                            m1 = (*m1).next;
                        }

                        rte_pktmbuf_free(m);

                        return Some(Packet::from_fragment_deleter(
                            Fragment { base: buf, size: pkt_len as usize },
                            make_free_deleter(buf.cast()),
                        ));
                    }

                    // Drop if allocation failed
                    rte_pktmbuf_free(m);

                    None
                }
            } else {
                // With a hugetlbfs backend:
                self._frags.clear();
                self._bufs.clear();

                // SAFETY: `m` is a received cluster whose data buffers are ours.
                unsafe {
                    while !m.is_null() {
                        let data = rte_pktmbuf_mtod(m);

                        self._frags
                            .push(Fragment { base: data, size: rte_pktmbuf_data_len(m) as usize });
                        self._bufs.push(data);
                        m = (*m).next;
                    }
                }

                let bufs_vec = std::mem::take(&mut self._bufs);
                Some(Packet::from_iter(
                    self._frags.iter(),
                    make_deleter_with(Deleter::new(), move || {
                        for &b in bufs_vec.iter() {
                            // SAFETY: each buffer came from posix_memalign in refill_rx_mbuf.
                            unsafe { free(b.cast()) };
                        }
                    }),
                ))
            }
        }

        /// Converts the received mbuf (or mbuf cluster) `m` into a packet, or returns
        /// `None` on failure.
        ///
        /// # Safety
        ///
        /// `m` is a cluster received on this queue's pool.
        #[inline(always)]
        // Takes `&mut self`: it allocates the packet's fragments from this queue's state.
        #[allow(clippy::wrong_self_convention)]
        unsafe fn from_mbuf(&mut self, m: *mut rte_mbuf) -> Option<Packet> {
            // SAFETY: `m` is a received cluster of this queue's pool.
            unsafe {
                if !H {
                    // Without a hugetlbfs backend:
                    if !self.port().hw_features_ref().rx_lro || rte_pktmbuf_is_contiguous(m) {
                        //
                        // Try to allocate a buffer for packet's data. If we fail - give the
                        // application an mbuf itself. If we succeed - copy the data into this
                        // buffer, create a packet based on this buffer and return the mbuf to
                        // its pool.
                        //
                        let len = rte_pktmbuf_data_len(m) as usize;
                        let buf = malloc(len).cast::<u8>();

                        if buf.is_null() {
                            // Drop if allocation failed
                            rte_pktmbuf_free(m);

                            None
                        } else {
                            rte_memcpy(buf.cast(), rte_pktmbuf_mtod(m).cast(), len);
                            rte_pktmbuf_free(m);

                            Some(Packet::from_fragment_deleter(
                                Fragment { base: buf, size: len },
                                make_free_deleter(buf.cast()),
                            ))
                        }
                    } else {
                        self.from_mbuf_lro(m)
                    }
                } else {
                    // With a hugetlbfs backend:
                    self._rx_free_pkts.push(m);
                    self._num_rx_free_segs += (*m).nb_segs as usize;

                    if !self.port().hw_features_ref().rx_lro || rte_pktmbuf_is_contiguous(m) {
                        let data = rte_pktmbuf_mtod(m);

                        Some(Packet::from_fragment_deleter(
                            Fragment { base: data, size: rte_pktmbuf_data_len(m) as usize },
                            make_free_deleter(data.cast()),
                        ))
                    } else {
                        self.from_mbuf_lro(m)
                    }
                }
            }
        }

        #[inline]
        unsafe fn refill_one_cluster(&mut self, mut head: *mut rte_mbuf) -> bool {
            // SAFETY: `head` is a cluster of this queue's rx pool.
            unsafe {
                while !head.is_null() {
                    if !Self::refill_rx_mbuf(head, MBUF_DATA_SIZE) {
                        //
                        // If we failed to allocate a new buffer - push the rest of the
                        // cluster back to the free_packets list for a later retry.
                        //
                        self._rx_free_pkts.push(head);
                        return false;
                    }
                    self._rx_free_bufs.push(head);
                    head = (*head).next;
                }
            }

            true
        }

        /// Refills the receive ring with buffers the stack has released; whether any were
        /// refilled.
        fn rx_gc(&mut self) -> bool {
            if self._num_rx_free_segs >= RX_GC_THRESH as usize {
                //
                // Use back() + pop_back() semantics to avoid an extra
                // _rx_free_pkts.clear() at the end of the function - clear() has a
                // linear complexity.
                //
                while let Some(m) = self._rx_free_pkts.pop() {
                    // SAFETY: `m` is a cluster of this queue's rx pool.
                    if !unsafe { self.refill_one_cluster(m) } {
                        break;
                    }
                }

                if !self._rx_free_bufs.is_empty() {
                    // SAFETY: the buffers are mbufs of the rx pool.
                    unsafe {
                        rte_mempool_put_bulk(
                            self._pktmbuf_pool_rx,
                            self._rx_free_bufs.as_ptr().cast(),
                            self._rx_free_bufs.len() as c_uint,
                        );
                    }

                    // TODO: ENGINE_ASSERT() in a fast path! Remove me ASAP!
                    assert!(self._num_rx_free_segs >= self._rx_free_bufs.len());

                    self._num_rx_free_segs -= self._rx_free_bufs.len();
                    self._rx_free_bufs.clear();

                    // TODO: ENGINE_ASSERT() in a fast path! Remove me ASAP!
                    assert!(
                        (self._rx_free_pkts.is_empty() && self._num_rx_free_segs == 0)
                            || (!self._rx_free_pkts.is_empty() && self._num_rx_free_segs != 0)
                    );
                }
            }

            self._num_rx_free_segs >= RX_GC_THRESH as usize
        }

        /// Converts the `count` received mbufs at `bufs` into packets and feeds them to the
        /// queue's receive stream.
        ///
        /// Through the queue's root pointer: each packet goes up the stack in line
        /// (`l2receive`), which reaches this queue's rx stream through the device's queue
        /// table, so no reference to the queue is live across that call.
        ///
        /// # Safety
        /// `this` is the queue's root pointer, with no reference to it live; the PMD filled
        /// `count` entries of `bufs`.
        unsafe fn process_packets(this: *mut Self, bufs: *mut *mut rte_mbuf, count: u16) {
            let mut nr_frags: u64 = 0;
            let mut bytes: u64 = 0;

            for i in 0..count as usize {
                // SAFETY: the PMD filled `count` entries of `bufs`. The
                // mbuf's fields are read after from_mbuf(): in the copy flow it is back in its
                // pool by then, whose memory stays mapped and is not reused before the next
                // rx burst.
                unsafe {
                    let m: *mut rte_mbuf = *bufs.add(i);
                    let mut oi = OffloadInfo::default();

                    // Short accesses through the root pointer (see above).
                    let p = (*this).from_mbuf(m);

                    // Drop the packet if the conversion above has failed
                    let Some(mut p) = p else {
                        (*this).base._stats.rx.bad.inc_no_mem();
                        continue;
                    };

                    nr_frags += (*m).nb_segs as u64;
                    bytes += (*m).pkt_len as u64;

                    // Set stripped VLAN value if available
                    if ((*m).ol_flags & RTE_MBUF_F_RX_VLAN_STRIPPED) != 0
                        && ((*m).ol_flags & RTE_MBUF_F_RX_VLAN) != 0
                    {
                        oi.vlan_tci = Some((*m).vlan_tci);
                    }

                    // The device outlives its queues; it is a separate object.
                    let dev: *const DpdkDevice = (*this)._dev;
                    // Note that when _hw_features.rx_csum_offload is on, the receive
                    // code for ip, tcp and udp will assume they don't need to check
                    // the checksum again, because we did this here.
                    if (*dev).hw_features_inline().rx_csum_offload
                        && (*m).ol_flags & (RTE_MBUF_F_RX_IP_CKSUM_BAD | RTE_MBUF_F_RX_L4_CKSUM_BAD)
                            != 0
                    {
                        // Packet with bad checksum, just drop it.
                        (*this).base._stats.rx.bad.inc_csum_err();
                        continue;
                    }

                    p.set_offload_info(oi);
                    if (*m).ol_flags & RTE_MBUF_F_RX_RSS_HASH != 0 {
                        p.set_rss_hash((*m).hash.rss);
                    }

                    let dev: &dyn Device = &*dev;
                    dev.l2receive(p);
                }
            }

            // SAFETY: guaranteed by the caller; a short access, the packets delivered.
            let st = unsafe { &mut (*this).base._stats.rx.good };
            st.update_pkts_bunch(count as u64);
            st.update_frags_stats(nr_frags, bytes);

            if !H {
                st.copy_frags = st.nr_frags;
                st.copy_bytes = st.bytes;
            }
        }

        /// Reads a burst of up to `PACKET_READ_SIZE` incoming packets and processes them;
        /// returns whether any packet was received.
        ///
        /// Never blocks: returns as soon as the available packets are processed.
        ///
        /// # Safety
        /// `this` is the queue's root pointer, with no reference to it live (the rx poller).
        unsafe fn poll_rx_once(this: *mut Self) -> bool {
            let mut buf = MaybeUninit::<[*mut rte_mbuf; PACKET_READ_SIZE as usize]>::uninit();
            let bufp: *mut *mut rte_mbuf = buf.as_mut_ptr().cast();

            /* read a port */
            // SAFETY: guaranteed by the caller (short accesses); the queue is set up and polled
            // by this shard only; `buf` has room for PACKET_READ_SIZE pointers.
            let rx_count: u16 = unsafe {
                rte_eth_rx_burst(
                    (*(*this)._dev).port_idx(),
                    (*this)._qid,
                    bufp,
                    PACKET_READ_SIZE as u16,
                )
            };

            /* Now process the NIC packets read */
            if rx_count > 0 {
                // SAFETY: the PMD wrote the first `rx_count` entries; no reference is live.
                unsafe { Self::process_packets(this, bufp, rx_count) };
            }

            rx_count != 0
        }
    }

    // This function uses offsetof with non POD types.
    const _: () = assert!(
        offset_of!(TxBuf<true>, private_end) - offset_of!(TxBuf<true>, private_start)
            <= RTE_PKTMBUF_HEADROOM as usize,
        "RTE_PKTMBUF_HEADROOM is less than dpdk_qp::tx_buf size! Increase the headroom size in the DPDK configuration"
    );
    const _: () = assert!(
        offset_of!(TxBuf<false>, private_end) - offset_of!(TxBuf<false>, private_start)
            <= RTE_PKTMBUF_HEADROOM as usize,
        "RTE_PKTMBUF_HEADROOM is less than dpdk_qp::tx_buf size! Increase the headroom size in the DPDK configuration"
    );
    const _: () = assert!(
        offset_of!(TxBuf<true>, _mbuf) == 0 && offset_of!(TxBuf<false>, _mbuf) == 0,
        "There is a pad at the beginning of the tx_buf before _mbuf field!"
    );
    const _: () = assert!(
        (INLINE_MBUF_DATA_SIZE & (INLINE_MBUF_DATA_SIZE - 1)) == 0,
        "inline_mbuf_data_size has to be a power of two!"
    );

    impl<const H: bool> Qp for DpdkQp<H> {
        fn base(&self) -> &QpBase {
            &self.base
        }

        fn base_mut(&mut self) -> &mut QpBase {
            &mut self.base
        }

        fn rx_start(&mut self) {
            // The poller runs after this call: it keeps the root pointer.
            let this = self._this;
            // SAFETY: the poller belongs to the queue, so the queue outlives it.
            // It runs from the reactor with no other reference to the queue live.
            self._rx_poller = Some(Poller::simple(move || unsafe { Self::poll_rx_once(this) }));
        }

        fn send(&mut self, _p: Packet) -> Future<()> {
            std::process::abort();
        }

        fn send_batch(&mut self, pb: &mut CircularBuffer<Packet>) -> u32 {
            if H {
                // Zero-copy send
                // SAFETY: the packet is one of the queue's pending packets.
                self._send(pb, |qp, p| unsafe { TxBuf::from_packet_zc(p, qp) })
            } else {
                // "Copy"-send
                // SAFETY: as above.
                self._send(pb, |qp, p| unsafe { TxBuf::from_packet_copy(p, qp) })
            }
        }
    }

    impl<const H: bool> Drop for DpdkQp<H> {
        /// The members go in reverse declaration order, so the
        /// pollers are gone before the factory and the pools they use.
        fn drop(&mut self) {
            self._tx_gc_poller = None;
            self._rx_poller = None;
            self._rx_gc_poller = None;
        }
    }

    impl DpdkDeviceState {
        /// First stage of the port initialization; 0 in case of
        /// success and an appropriate error code in case of an error.
        fn init_port_start(&mut self, base: &DeviceBase) -> c_int {
            // SAFETY: rte_eth_dev_count_avail() only reads the ethdev state.
            assert!(self._port_idx < unsafe { rte_eth_dev_count_avail() });

            // SAFETY: `_dev_info` is valid for writes.
            unsafe { rte_eth_dev_info_get(self._port_idx, &mut self._dev_info) };

            // SAFETY: rte_eth_dev_info_get() sets the driver name of a valid port.
            let driver_name = unsafe { CStr::from_ptr(self._dev_info.driver_name) }.to_bytes();

            //
            // This is a workaround for a missing handling of a HW limitation in the
            // DPDK i40e driver. This and all related to _is_i40e_device code should be
            // removed once this handling is added.
            //
            if driver_name == b"rte_i40evf_pmd" || driver_name == b"rte_i40e_pmd" {
                println!("Device is an Intel's 40G NIC. Enabling 8 fragments hack!");
                self._is_i40e_device = true;
            }

            if driver_name == b"rte_vmxnet3_pmd" {
                println!("Device is a VMWare Virtual NIC. Enabling 16 fragments hack!");
                self._is_vmxnet3_device = true;
            }

            //
            // Another workaround: this time for a lack of number of RSS bits.
            // ixgbe PF NICs support up to 16 RSS queues.
            // ixgbe VF NICs support up to 4 RSS queues.
            // i40e PF NICs support up to 64 RSS queues.
            // i40e VF NICs support up to 16 RSS queues.
            //
            if driver_name == b"rte_ixgbe_pmd" {
                self._dev_info.max_rx_queues = self._dev_info.max_rx_queues.min(16);
            } else if driver_name == b"rte_ixgbevf_pmd" {
                self._dev_info.max_rx_queues = self._dev_info.max_rx_queues.min(4);
            } else if driver_name == b"rte_i40e_pmd" {
                self._dev_info.max_rx_queues = self._dev_info.max_rx_queues.min(64);
            } else if driver_name == b"rte_i40evf_pmd" {
                self._dev_info.max_rx_queues = self._dev_info.max_rx_queues.min(16);
            }

            // Hardware offload capabilities
            // https://github.com/DPDK/dpdk/blob/v19.05/lib/librte_ethdev/rte_ethdev.h#L993-L1074

            // We want to support all available offload features
            // TODO: below features are implemented in 17.05, should support new ones
            let tx_offloads_wanted: u64 = RTE_ETH_TX_OFFLOAD_VLAN_INSERT
                | RTE_ETH_TX_OFFLOAD_IPV4_CKSUM
                | RTE_ETH_TX_OFFLOAD_UDP_CKSUM
                | RTE_ETH_TX_OFFLOAD_TCP_CKSUM
                | RTE_ETH_TX_OFFLOAD_SCTP_CKSUM
                | RTE_ETH_TX_OFFLOAD_TCP_TSO
                | RTE_ETH_TX_OFFLOAD_UDP_TSO
                | RTE_ETH_TX_OFFLOAD_OUTER_IPV4_CKSUM
                | RTE_ETH_TX_OFFLOAD_QINQ_INSERT
                | RTE_ETH_TX_OFFLOAD_VXLAN_TNL_TSO
                | RTE_ETH_TX_OFFLOAD_GRE_TNL_TSO
                | RTE_ETH_TX_OFFLOAD_IPIP_TNL_TSO
                | RTE_ETH_TX_OFFLOAD_GENEVE_TNL_TSO
                | RTE_ETH_TX_OFFLOAD_MACSEC_INSERT;

            self._dev_info.default_txconf.offloads =
                self._dev_info.tx_offload_capa & tx_offloads_wanted;

            /* for port configuration all features are off by default */
            let mut port_conf = rte_eth_conf::default();

            /* setting tx offloads for port */
            port_conf.txmode.offloads = self._dev_info.default_txconf.offloads;

            println!(
                "Port {}: max_rx_queues {} max_tx_queues {}",
                self._port_idx, self._dev_info.max_rx_queues, self._dev_info.max_tx_queues
            );

            self._num_queues = self
                ._num_queues
                .min(self._dev_info.max_rx_queues)
                .min(self._dev_info.max_tx_queues);

            println!(
                "Port {}: using {} {}",
                self._port_idx,
                self._num_queues,
                if self._num_queues > 1 { "queues" } else { "queue" }
            );

            // Set RSS mode: enable RSS if engine is configured with more than 1 CPU.
            // Even if port has a single queue we still want the RSS feature to be
            // available in order to make HW calculate RSS hash for us.
            if Smp::count() > 1 {
                if self._dev_info.hash_key_size == 40 {
                    self._rss_key = DEFAULT_RSSKEY_40BYTES;
                } else if self._dev_info.hash_key_size == 52 {
                    self._rss_key = DEFAULT_RSSKEY_52BYTES;
                } else if self._dev_info.hash_key_size != 0 {
                    // WTF?!!
                    rte_exit_fmt(
                        EXIT_FAILURE,
                        format_args!(
                            "Port {}: We support only 40 or 52 bytes RSS hash keys, {} bytes key requested",
                            self._port_idx, self._dev_info.hash_key_size
                        ),
                    );
                } else {
                    self._rss_key = DEFAULT_RSSKEY_40BYTES;
                    self._dev_info.hash_key_size = 40;
                }

                port_conf.rxmode.mq_mode = RTE_ETH_MQ_RX_RSS;
                /* enable all supported rss offloads */
                port_conf.rx_adv_conf.rss_conf.rss_hf = self._dev_info.flow_type_rss_offloads;
                if self._dev_info.hash_key_size != 0 {
                    port_conf.rx_adv_conf.rss_conf.rss_key = self._rss_key.as_ptr() as *mut u8;
                    port_conf.rx_adv_conf.rss_conf.rss_key_len = self._dev_info.hash_key_size;
                }
            } else {
                port_conf.rxmode.mq_mode = RTE_ETH_MQ_RX_NONE;
            }

            if self._num_queues > 1 {
                if self._dev_info.reta_size != 0 {
                    // RETA size should be a power of 2
                    assert!((self._dev_info.reta_size & (self._dev_info.reta_size - 1)) == 0);

                    // Set the RSS table to the correct size
                    self._redir_table.resize(self._dev_info.reta_size as usize, 0);
                    base._rss_table_bits
                        .set((self._dev_info.reta_size as f64).log2().round() as usize);
                    println!(
                        "Port {}: RSS table size is {}",
                        self._port_idx, self._dev_info.reta_size
                    );
                } else {
                    base._rss_table_bits
                        .set((self._dev_info.max_rx_queues as f64).log2().round() as usize);
                }
            } else {
                self._redir_table.push(0);
            }

            // Set Rx VLAN stripping
            if self._dev_info.rx_offload_capa & RTE_ETH_RX_OFFLOAD_VLAN_STRIP != 0 {
                port_conf.rxmode.offloads |= RTE_ETH_RX_OFFLOAD_VLAN_STRIP;
            }

            // LRO, when the device supports it.
            // Enable LRO
            if self._use_lro && (self._dev_info.rx_offload_capa & RTE_ETH_RX_OFFLOAD_TCP_LRO) != 0 {
                println!("LRO is on");
                port_conf.rxmode.offloads |= RTE_ETH_RX_OFFLOAD_TCP_LRO;
                self.hw_features_ref().rx_lro = true;
            } else {
                println!("LRO is off");
            }

            let rx_capa = self._dev_info.rx_offload_capa;
            let tx_capa = self._dev_info.tx_offload_capa;

            // Check that all CSUM features are either all set all together or not set
            // all together. If this assumption breaks we need to rework the below logic
            // by splitting the csum offload feature bit into separate bits for IPv4,
            // TCP and UDP.
            assert!(
                ((rx_capa & RTE_ETH_RX_OFFLOAD_IPV4_CKSUM) != 0
                    && (rx_capa & RTE_ETH_RX_OFFLOAD_UDP_CKSUM) != 0
                    && (rx_capa & RTE_ETH_RX_OFFLOAD_TCP_CKSUM) != 0)
                    || ((rx_capa & RTE_ETH_RX_OFFLOAD_IPV4_CKSUM) == 0
                        && (rx_capa & RTE_ETH_RX_OFFLOAD_UDP_CKSUM) == 0
                        && (rx_capa & RTE_ETH_RX_OFFLOAD_TCP_CKSUM) == 0)
            );

            // Set Rx checksum checking
            if (rx_capa & RTE_ETH_RX_OFFLOAD_IPV4_CKSUM) != 0
                && (rx_capa & RTE_ETH_RX_OFFLOAD_UDP_CKSUM) != 0
                && (rx_capa & RTE_ETH_RX_OFFLOAD_TCP_CKSUM) != 0
            {
                println!("RX checksum offload supported");
                port_conf.rxmode.offloads |= RTE_ETH_RX_OFFLOAD_CHECKSUM;
                self._hw_features.rx_csum_offload = true;
            }

            if (tx_capa & RTE_ETH_TX_OFFLOAD_IPV4_CKSUM) != 0 {
                println!("TX ip checksum offload supported");
                self._hw_features.tx_csum_ip_offload = true;
            }

            // TSO is supported starting from DPDK v1.8
            if tx_capa & RTE_ETH_TX_OFFLOAD_TCP_TSO != 0 {
                println!("TSO is supported");
                self._hw_features.tx_tso = true;
            }

            // There is no UFO support in the PMDs yet.

            // Check that Tx TCP and UDP CSUM features are either all set all together
            // or not set all together. If this assumption breaks we need to rework the
            // below logic by splitting the csum offload feature bit into separate bits
            // for TCP and UDP.
            assert!(
                ((tx_capa & RTE_ETH_TX_OFFLOAD_UDP_CKSUM) != 0
                    && (tx_capa & RTE_ETH_TX_OFFLOAD_TCP_CKSUM) != 0)
                    || ((tx_capa & RTE_ETH_TX_OFFLOAD_UDP_CKSUM) == 0
                        && (tx_capa & RTE_ETH_TX_OFFLOAD_TCP_CKSUM) == 0)
            );

            if (tx_capa & RTE_ETH_TX_OFFLOAD_UDP_CKSUM) != 0
                && (tx_capa & RTE_ETH_TX_OFFLOAD_TCP_CKSUM) != 0
            {
                println!("TX TCP&UDP checksum offload supported");
                self._hw_features.tx_csum_l4_offload = true;
            }

            print!("Port {} init ... ", self._port_idx);
            let _ = std::io::stdout().flush();

            /*
             * Standard DPDK port initialisation - config port, then set up
             * rx and tx rings.
             */
            // SAFETY: `port_conf` is a complete configuration that outlives the call.
            let retval = unsafe {
                rte_eth_dev_configure(
                    self._port_idx,
                    self._num_queues,
                    self._num_queues,
                    &port_conf,
                )
            };
            if retval != 0 {
                return retval;
            }

            //rte_eth_promiscuous_enable(port_num);
            println!("done: ");

            0
        }

        /// Configures hardware flow control.
        fn set_hw_flow_control(&mut self) {
            'not_supported: {
                // Read the port's current/default flow control settings
                let mut fc_conf = rte_eth_fc_conf::default();
                // SAFETY: `fc_conf` is valid for writes.
                let mut ret = unsafe { rte_eth_dev_flow_ctrl_get(self._port_idx, &mut fc_conf) };

                if ret == -ENOTSUP {
                    break 'not_supported;
                }

                if ret < 0 {
                    rte_exit_fmt(
                        EXIT_FAILURE,
                        format_args!(
                            "Port {}: failed to get hardware flow control settings: (error {})\n",
                            self._port_idx, ret
                        ),
                    );
                }

                if self._enable_fc {
                    fc_conf.mode = RTE_ETH_FC_FULL;
                } else {
                    fc_conf.mode = RTE_ETH_FC_NONE;
                }

                // SAFETY: as above.
                ret = unsafe { rte_eth_dev_flow_ctrl_set(self._port_idx, &mut fc_conf) };
                if ret == -ENOTSUP {
                    break 'not_supported;
                }

                if ret < 0 {
                    rte_exit_fmt(
                        EXIT_FAILURE,
                        format_args!(
                            "Port {}: failed to set hardware flow control (error {})\n",
                            self._port_idx, ret
                        ),
                    );
                }

                println!(
                    "Port {}: {} HW FC",
                    self._port_idx,
                    if self._enable_fc { "Enabling" } else { "Disabling" }
                );
                return;
            }

            // not_supported:
            println!("Port {}: Changing HW FC settings is not supported", self._port_idx);
        }

        /// The final stage of the port initialization. Must be called after all queues have
        /// been initialized.
        ///
        /// `this` is the state's root pointer (`UnsafeCell::get` of the shared device), which
        /// the timers keep; `self` is derived from it and the timers do not run during the
        /// call.
        fn init_port_fini(&mut self, this: *mut Self) -> Result<(), ExceptionPtr> {
            // Changing FC requires HW reset, so set it before the port is initialized.
            self.set_hw_flow_control();

            // SAFETY: the port is configured and its queues are set up.
            if unsafe { rte_eth_dev_start(self._port_idx) } < 0 {
                rte_exit_fmt(EXIT_FAILURE, format_args!("Cannot start port {}\n", self._port_idx));
            }

            /* need to defer initialize xstats since NIC specific xstat entries
            show up only after port initization */
            self._xstats.start()?;

            // SAFETY: the state lives in the device's UnsafeCell, which never moves.
            unsafe { Pin::new_unchecked(&mut self._stats_collector) }.set_callback(move || {
                // SAFETY: the timer belongs to the device, which outlives it; the callback
                // runs on the home shard. Only place accesses: the other shards read other
                // fields of the state at the same time (see the Sync impl), so no reference
                // to the whole state is made. `_port_idx` and `_xstats` are fixed once the
                // port is started; the port statistics live in their own box, touched only
                // here and by the metrics of this shard.
                let (port_idx, xs, stats) =
                    unsafe { ((*this)._port_idx, &(*this)._xstats, &mut *(*this)._stats.as_ptr()) };
                let mut rte_stats = rte_eth_stats::default();
                // SAFETY: `rte_stats` is valid for writes.
                let rc = unsafe { rte_eth_stats_get(port_idx, &mut rte_stats) };

                if rc != 0 {
                    // SAFETY: strerror() returns a NUL-terminated string.
                    let msg = unsafe { CStr::from_ptr(strerror(rc)) };
                    println!("Failed to get port statistics: {}", msg.to_string_lossy());
                }

                stats.rx.good.mcast = xs.get_value(XstatId::rx_multicast_packets);
                stats.rx.good.pause_xon = xs.get_value(XstatId::rx_xon_packets);
                stats.rx.good.pause_xoff = xs.get_value(XstatId::rx_xoff_packets);

                stats.rx.bad.crc = xs.get_value(XstatId::rx_crc_errors);
                stats.rx.bad.len = xs.get_value(XstatId::rx_length_errors)
                    + xs.get_value(XstatId::rx_undersize_errors)
                    + xs.get_value(XstatId::rx_oversize_errors);
                stats.rx.bad.total = rte_stats.ierrors;

                stats.tx.good.pause_xon = xs.get_value(XstatId::tx_xon_packets);
                stats.tx.good.pause_xoff = xs.get_value(XstatId::tx_xoff_packets);

                stats.tx.bad.total = rte_stats.oerrors;
            });

            // TODO: replace deprecated filter api with generic flow api
            if self._num_queues > 1 {
                self.set_rss_table();
            }

            // Wait for a link
            self.check_port_link_status(this);

            println!("Created DPDK device");
            Ok(())
        }

        /// Checks the link status of the port for up to 9 s and prints it.
        ///
        /// `this` is the state's root pointer, which the timer keeps (see `init_port_fini`).
        fn check_port_link_status(&mut self, this: *mut Self) {
            let mut count: i32 = 0;
            let check_interval = Duration::from_millis(100);

            println!("\nChecking link status ");
            let t: *mut Timer = Box::into_raw(Box::new(Timer::new()));
            // SAFETY: `t` came from Box::into_raw just above and is freed only by the
            // callback, after this function returned.
            // SAFETY: as above; the timer stays at its heap address until the callback frees it.
            let mut tr: Pin<&mut Timer> = unsafe { Pin::new_unchecked(&mut *t) };
            tr.as_mut().set_callback(move || {
                const MAX_CHECK_TIME: i32 = 90; /* 9s (90 * 100ms) in total */
                let mut link = rte_eth_link::default();
                // SAFETY: the device outlives the timer, which runs on its home shard.
                let me = unsafe { &mut *this };
                // SAFETY: `link` is valid for writes.
                unsafe { rte_eth_link_get_nowait(me._port_idx, &mut link) };

                if link.link_status() != 0 {
                    println!(
                        "done\nPort {} Link Up - speed {} Mbps - {}",
                        me._port_idx as u32,
                        link.link_speed,
                        if link.link_duplex() == RTE_ETH_LINK_FULL_DUPLEX {
                            "full-duplex"
                        } else {
                            "half-duplex\n"
                        }
                    );
                    me._link_ready_promise.set_value(());

                    // We may start collecting statistics only after the Link is UP.
                    // SAFETY: see `init_port_fini`.
                    unsafe { Pin::new_unchecked(&mut me._stats_collector) }
                        .arm_periodic(Duration::from_secs(2));
                } else {
                    let c = count;
                    count += 1;
                    if c < MAX_CHECK_TIME {
                        print!(".");
                        let _ = std::io::stdout().flush();
                        return;
                    } else {
                        println!("done\nPort {} Link Down", me._port_idx);
                    }
                }
                // SAFETY: `t` is this timer, freed once; neither the
                // callback nor the timer is touched after the delete, and the reactor
                // re-armed the periodic timer before calling us.
                unsafe {
                    Pin::new_unchecked(&mut *t).cancel();
                    drop(Box::from_raw(t));
                }
            });
            tr.arm_periodic(check_interval);
        }

        /// Sets the RSS redirection table in the device and stores it in the
        /// internal vector.
        fn set_rss_table(&mut self) {
            if self._dev_info.reta_size == 0 {
                return;
            }

            let reta_conf_size: i32 =
                1.max((self._dev_info.reta_size / RTE_ETH_RETA_GROUP_SIZE) as i32);
            let mut reta_conf: Vec<rte_eth_rss_reta_entry64> =
                vec![rte_eth_rss_reta_entry64::default(); reta_conf_size as usize];

            // Configure the HW indirection table
            let nq = self._num_queues as u32;
            let mut i: u32 = 0;
            for x in reta_conf.iter_mut() {
                x.mask = !0u64;
                for r in x.reta.iter_mut() {
                    *r = (i % nq) as u16;
                    i += 1;
                }
            }

            // SAFETY: `reta_conf` holds reta_size entries in groups of 64.
            if unsafe {
                rte_eth_dev_rss_reta_update(
                    self._port_idx,
                    reta_conf.as_mut_ptr(),
                    self._dev_info.reta_size,
                )
            } != 0
            {
                rte_exit_fmt(
                    EXIT_FAILURE,
                    format_args!(
                        "Port {}: Failed to update an RSS indirection table",
                        self._port_idx
                    ),
                );
            }

            // Fill our local indirection table. Make it in a separate loop to keep things simple.
            i = 0;
            for r in self._redir_table.iter_mut() {
                *r = (i % nq) as u8;
                i += 1;
            }
        }
    }

    /* Interface functions */

    /// Creates the DPDK device of port `port_idx` with `num_queues` queue pairs.
    pub fn create_dpdk_net_device(
        port_idx: u16,
        num_queues: u16,
        use_lro: bool,
        enable_fc: bool,
    ) -> Result<Box<dyn Device>, ExceptionPtr> {
        static CALLED: AtomicBool = AtomicBool::new(false);

        assert!(!CALLED.load(Ordering::Relaxed));
        assert!(EAL_INITIALIZED.load(Ordering::Relaxed));

        CALLED.store(true, Ordering::Relaxed);

        // Check that we have at least one DPDK-able port
        // SAFETY: rte_eth_dev_count_avail() only reads the ethdev state.
        let count = unsafe { rte_eth_dev_count_avail() };
        if count == 0 {
            rte_exit_fmt(EXIT_FAILURE, format_args!("No Ethernet ports - bye\n"));
        } else {
            println!("ports number: {}", count);
        }

        Ok(Box::new(DpdkDevice::new(port_idx, num_queues, use_lro, enable_fc)?))
    }

    /// Creates the DPDK device described by `hw_cfg`, with one queue pair per shard.
    ///
    /// # Errors
    /// Fails if `hw_cfg` has no DPDK port index.
    pub fn create_dpdk_net_device_from_config(
        hw_cfg: &HwConfig,
    ) -> Result<Box<dyn Device>, ExceptionPtr> {
        // A configuration with only a PCI address and no port index is an error.
        let Some(port_index) = hw_cfg.port_index else {
            return Err(ExceptionPtr::from(InvalidArgument(
                "hw_config without a DPDK port index".to_string(),
            )));
        };
        create_dpdk_net_device(port_index as u16, Smp::count() as u16, hw_cfg.lro, hw_cfg.hw_fc)
    }
}

impl Default for DpdkOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl DpdkOptions {
    /// The options with their defaults, not yet linked to a group (see
    /// [`DpdkOptions::link`]).
    pub fn new() -> Self {
        #[cfg(feature = "dpdk")]
        {
            DpdkOptions {
                group: OptionGroup::new("DPDK net options"),
                dpdk_port_index: Value::new("dpdk-port-index", Some(0u32), "DPDK Port Index"),
                hw_fc: Value::new(
                    "hw-fc",
                    Some("on".to_string()),
                    "Enable HW Flow Control (on / off)",
                ),
            }
        }
        #[cfg(not(feature = "dpdk"))]
        {
            DpdkOptions {
                group: OptionGroup::new_unused("DPDK net options", Unused),
                dpdk_port_index: Value::new_unused("dpdk-port-index", Unused),
                hw_fc: Value::new_unused("hw-fc", Unused),
            }
        }
    }

    /// Links the group under `parent_group` and registers every value with it, in place,
    /// in declaration order.
    pub fn link(
        self: std::pin::Pin<&mut Self>,
        parent_group: Option<std::pin::Pin<&mut OptionGroup>>,
    ) {
        crate::link_option_group!(self, parent_group, group, dpdk_port_index, hw_fc);
    }
}
