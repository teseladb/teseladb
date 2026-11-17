//! The per-shard disk I/O queue and the per-device I/O group.
//!
//! Every read and write a shard issues to a device goes through that device's [`IoQueue`]
//! on the shard. The queue costs each request in capacity tokens (from its direction and
//! length, see [`IoQueueConfig`]), queues it in a [`FairQueue`] per stream (one, or one per
//! direction for duplex devices) under the priority class of the caller's scheduling
//! group, and dispatches it to the reactor's I/O sink when the shared [`IoGroup`] grants
//! the capacity. Large requests are split to the device's saturation length, requests may
//! be cancelled through an [`IoIntent`], and
//! classes can be bandwidth-limited with a per-class token bucket shared by all shards.
//!
//! The queue, its classes and its queued requests point at one another through raw root
//! pointers: the queue is built in place and never moves, classes and requests live in
//! their own allocations, and the unsafe functions state which pointer they must be given.
//! A shard's queue is only used on that shard; the group is shared by the shards of a
//! device and synchronizes its class table with a mutex and its token buckets atomically.

use std::cell::Cell;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use crate::engine::core::fair_queue::{
    CapacityT, ClassId, ClockTypeTimePoint, FairGroup, FairGroupConfig, FairQueue, FairQueueConfig,
    FairQueueEntry, duration_cast_micros,
};
use crate::engine::core::future::{
    ExceptionPtr, Future, LogicError, Promise, RuntimeError, make_exception_future,
    make_ready_future,
};
use crate::engine::core::internal::io_intent::{CancellableQueue, Link};
use crate::engine::core::internal::io_request::{IoDirectionAndLength, IoRequest, Operation, Part};
use crate::engine::core::internal::io_sink::{IoSink, PendingIoRequest};
use crate::engine::core::io_intent::IoIntent;
use crate::engine::core::io_priority_class::IoPriorityClassId;
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::metrics;
use crate::engine::core::reactor::{IoCompletion, IoCompletionImpl, engine, engine_logger};
use crate::engine::core::reactor_backend::{Iovec, engine_assert};
use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::timer::Timer;
use crate::engine::net::node_ptr::NodePtr;
use crate::engine::util::log::{FormatInfo, LogLevel, Logger};
use crate::engine::util::shared_token_bucket::internal::{
    Ratio1, RoversUncapped, SharedTokenBucket, SteadyClock, TokenBucketClock,
};

/// The logger of the I/O subsystem, named `io`.
pub static IO_LOG: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("io").expect("logger registered once"));

/// The I/O logger.
#[inline]
pub fn io_log() -> &'static Logger {
    &IO_LOG
}

/// Identifier of a shard.
pub type ShardId = u32;
/// Index of a stream of a queue: 0, or the direction index on duplex devices.
pub type StreamId = u32;
/// Shared ownership of an [`IoGroup`].
pub type IoGroupPtr = Arc<IoGroup>;
/// The `iovec` array a vectored request keeps alive until it completes.
pub type IovecKeeper = Vec<Iovec>;

const IO_DIRECTION_READ: i32 = IoDirectionAndLength::READ_IDX;
const IO_DIRECTION_WRITE: i32 = IoDirectionAndLength::WRITE_IDX;

/// Helpers shared with the reactor and the tests.
pub mod internal {
    use super::*;

    /// The fair group of stream `stream` of `ioq`'s group.
    pub fn get_fair_group(ioq: &IoQueue, stream: u32) -> &FairGroup {
        &ioq._group._fgs[stream as usize]
    }

    /// An I/O priority class: the index of a scheduling group.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PriorityClass {
        _id: u32,
    }

    impl PriorityClass {
        /// The class of scheduling group `sg`.
        #[inline]
        pub fn new(sg: &SchedulingGroup) -> Self {
            PriorityClass {
                _id: crate::engine::core::scheduling::internal::scheduling_group_index(*sg),
            }
        }

        /// The class of the current scheduling group; the argument carries no priority.
        #[inline]
        pub fn from_maybe(
            _pc: crate::engine::core::io_priority_class::internal::MaybePriorityClassRef,
        ) -> Self {
            PriorityClass::new(&current_scheduling_group())
        }

        /// The class index.
        #[inline]
        pub fn id(&self) -> u32 {
            self._id
        }
    }

    impl From<SchedulingGroup> for PriorityClass {
        #[inline]
        fn from(sg: SchedulingGroup) -> Self {
            PriorityClass::new(&sg)
        }
    }

    /// The cost of a request in floating-point tokens: its weight over the request rate
    /// plus its size in 512-byte blocks over the block rate, with writes scaled by the
    /// configured write-to-read multipliers.
    #[inline]
    pub fn request_tokens(dnl: IoDirectionAndLength, cfg: &IoQueueConfig) -> f64 {
        #[derive(Clone, Copy, Default)]
        struct Mult {
            weight: u32,
            size: u32,
        }
        let mut mult = [Mult::default(); 2];

        mult[IO_DIRECTION_WRITE as usize] = Mult {
            weight: cfg.disk_req_write_to_read_multiplier,
            size: cfg.disk_blocks_write_to_read_multiplier,
        };
        mult[IO_DIRECTION_READ as usize] = Mult {
            weight: IoQueue::READ_REQUEST_BASE_COUNT,
            size: IoQueue::READ_REQUEST_BASE_COUNT,
        };

        let m = &mult[dnl.rw_idx() as usize];

        m.weight as f64 / cfg.req_count_rate as f64
            + m.size as f64 * (dnl.length() >> IoQueue::BLOCK_SIZE_SHIFT) as f64
                / cfg.blocks_count_rate as f64
    }
}

use internal::PriorityClass;

/// Makes the errors the I/O queue reports.
pub struct DefaultIoExceptionFactory;

impl DefaultIoExceptionFactory {
    /// The error of a cancelled request: a [`CancelledError`](crate::engine::core::file::CancelledError).
    pub fn cancelled() -> ExceptionPtr {
        ExceptionPtr::from(crate::engine::core::file::CancelledError)
    }
}

/// Configuration of a device's I/O queues and group.
#[derive(Clone, Debug)]
pub struct IoQueueConfig {
    /// Identifier of the device (the group and its queues).
    pub id: u32,
    /// Maximum requests per second the device sustains.
    pub req_count_rate: u64,
    /// Maximum 512-byte blocks per second the device sustains.
    pub blocks_count_rate: u64,
    /// Weight of a write request relative to a read, scaled by
    /// [`IoQueue::READ_REQUEST_BASE_COUNT`].
    pub disk_req_write_to_read_multiplier: u32,
    /// Weight of a written block relative to a read one, scaled by
    /// [`IoQueue::READ_REQUEST_BASE_COUNT`].
    pub disk_blocks_write_to_read_multiplier: u32,
    /// Read length beyond which the device does not get faster; longer reads are split.
    pub disk_read_saturation_length: u64,
    /// Write length beyond which the device does not get faster; longer writes are split.
    pub disk_write_saturation_length: u64,
    /// Mount point of the device, for metrics and messages.
    pub mountpoint: String,
    /// Whether reads and writes are scheduled as separate streams.
    pub duplex: bool,
    /// The latency goal: the period the fair group's token bucket accumulates for.
    pub rate_limit_duration: Duration,
    /// Minimum number of blocks the bucket limit must hold for one request.
    pub block_count_limit_min: u64,
    /// Period of the flow-ratio averaging, in latency goals.
    pub averaging_decay_ticks: u32,
    /// Smoothing factor of the exponential moving average of the flow ratio.
    pub flow_ratio_ema_factor: f64,
    /// Flow ratio (dispatched over completed requests) above which request costs are
    /// inflated to apply back-pressure.
    pub flow_ratio_backpressure_threshold: f64,
    /// Initial execution time above which a request is reported as stalled; it doubles
    /// on each report and decays back over time.
    pub stall_threshold: Duration,
}

impl Default for IoQueueConfig {
    fn default() -> Self {
        IoQueueConfig {
            id: 0,
            // Unbounded: an unconfigured queue does not throttle (requests cost nothing).
            req_count_rate: u64::MAX,
            blocks_count_rate: u64::MAX,
            disk_req_write_to_read_multiplier: IoQueue::READ_REQUEST_BASE_COUNT,
            disk_blocks_write_to_read_multiplier: IoQueue::READ_REQUEST_BASE_COUNT,
            disk_read_saturation_length: u64::MAX,
            disk_write_saturation_length: u64::MAX,
            mountpoint: "undefined".to_string(),
            duplex: false,
            rate_limit_duration: Duration::from_millis(1),
            block_count_limit_min: 1,
            averaging_decay_ticks: 100,
            flow_ratio_ema_factor: 0.95,
            flow_ratio_backpressure_threshold: 1.1,
            stall_threshold: Duration::from_millis(100),
        }
    }
}

/// The longest read and write a queue issues in one request; longer ones are split.
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestLimits {
    /// Longest read, in bytes.
    pub max_read: usize,
    /// Longest write, in bytes.
    pub max_write: usize,
}

/// The token bucket of a class's bandwidth limit, in 512-byte blocks per second.
pub type GroupTokenBucketT = SharedTokenBucket<Ratio1, RoversUncapped>;

/// The per-device state of a priority class, shared by the shards: its bandwidth
/// bucket.
pub struct IoGroupPriorityClassData {
    /// The class's bandwidth token bucket (unlimited until a bandwidth is set).
    pub tb: GroupTokenBucketT,
}

impl IoGroupPriorityClassData {
    /// The bucket limit: a burst of 10 MiB, in blocks.
    pub const BANDWIDTH_BURST_IN_BLOCKS: u64 = 10 << (20 - IoQueue::BLOCK_SIZE_SHIFT);
    /// The bucket's replenish threshold: 128 KiB, in blocks.
    pub const BANDWIDTH_THRESHOLD_IN_BLOCKS: u64 = 128 << (10 - IoQueue::BLOCK_SIZE_SHIFT);

    /// The bucket tokens of `length` bytes: whole 512-byte blocks.
    #[inline]
    pub fn tokens(&self, length: usize) -> u64 {
        (length >> IoQueue::BLOCK_SIZE_SHIFT) as u64
    }

    /// Sets the class's bandwidth limit to `bandwidth` bytes per second.
    ///
    /// # Errors
    /// A runtime error when the rate exceeds the bucket's maximum.
    ///
    /// Takes `&self`: the other shards of the group read the bucket at the same time, and
    /// its rate is atomic.
    pub fn update_bandwidth(&self, bandwidth: u64) -> Result<(), ExceptionPtr> {
        if bandwidth >> IoQueue::BLOCK_SIZE_SHIFT > GroupTokenBucketT::max_rate() {
            // The rate is beyond what the bucket can represent.
            return Err(ExceptionPtr::new(RuntimeError::new(format!(
                "Too large rate, maximum is {}MB/s",
                GroupTokenBucketT::max_rate() >> (20 - IoQueue::BLOCK_SIZE_SHIFT)
            ))));
        }

        let tokens = self.tokens(bandwidth as usize);
        self.tb.update_rate(tokens);
        Ok(())
    }

    /// A class with no bandwidth limit.
    pub fn new() -> Self {
        IoGroupPriorityClassData {
            tb: GroupTokenBucketT::new(
                u64::MAX,
                Self::BANDWIDTH_BURST_IN_BLOCKS,
                Self::BANDWIDTH_THRESHOLD_IN_BLOCKS,
                true,
            ),
        }
    }
}

impl Default for IoGroupPriorityClassData {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RwStat {
    bytes: usize,
    ops: u64,
}

impl RwStat {
    #[inline]
    fn add(&mut self, len: usize) {
        self.ops += 1;
        self.bytes += len;
    }
}

/// The per-queue state of a priority class: its fair-queue shares, statistics, metrics
/// and the timer that unthrottles it when its bandwidth bucket refills.
pub struct IoQueuePriorityClassData {
    _queue: *mut IoQueue,
    _pc: PriorityClass,
    _shares: u32,
    _rwstat: [RwStat; 2],
    _splits: RwStat,
    _nr_queued: u32,
    _nr_executing: u32,
    _queue_time: Duration,
    _total_queue_time: Duration,
    _total_execution_time: Duration,
    _starvation_time: Duration,
    _activated: Instant,

    _group: *mut IoGroupPriorityClassData,
    _replenish_head: u64,
    _replenish: Timer<LowresClock>,
    pub(crate) metric_groups: metrics::MetricGroups,
}

impl IoQueuePriorityClassData {
    #[inline]
    fn group(&self) -> &IoGroupPriorityClassData {
        // SAFETY: the group's class data lives in the io_group, which outlives the queue.
        unsafe { &*self._group }
    }

    /// The replenish timer, pinned: the class is boxed (see `new`) and never moves.
    #[inline]
    fn replenish_timer(&mut self) -> Pin<&mut Timer<LowresClock>> {
        // SAFETY: the class lives in its box until it is dropped, and the timer's destructor
        // cancels it.
        unsafe { Pin::new_unchecked(&mut self._replenish) }
    }

    fn try_to_replenish(&mut self) {
        self.group().tb.replenish(SteadyClock::now());
        let delta = self.group().tb.deficiency(self._replenish_head);
        if delta > 0 {
            let d = self.group().tb.duration_for(delta);
            self.replenish_timer().arm_in(duration_cast_micros(d));
        } else {
            let cid = self.fq_class();
            // SAFETY: `_queue` is the queue's root pointer; the
            // class lives in its own node, outside the queue, and `unthrottle_priority_class`
            // touches only the queue's fair queues.
            unsafe { IoQueue::unthrottle_priority_class(self._queue, cid) };
        }
    }

    /// Sets the class's shares in every stream's fair queue.
    #[inline]
    pub fn update_shares(&mut self, shares: u32) {
        self._shares = shares.max(1);
    }

    /// Sets the class's bandwidth limit; see [`IoGroupPriorityClassData::update_bandwidth`].
    ///
    /// # Errors
    /// As [`IoGroupPriorityClassData::update_bandwidth`].
    pub fn update_bandwidth(&mut self, bandwidth: u64) -> Result<(), ExceptionPtr> {
        // SAFETY: the group class data is boxed in the io_group and never removed (see
        // `IoGroup::find_or_create_class`), so the pointer is valid; only a shared
        // reference is formed, as the other shards of the group read it concurrently and
        // the rate it changes is atomic.
        unsafe { (*self._group).update_bandwidth(bandwidth)? };
        io_log().debug(
            FormatInfo::new("Updated {} class bandwidth to {}MB/s"),
            format_args!("Updated {} class bandwidth to {}MB/s", self._pc.id(), bandwidth >> 20),
        );
        Ok(())
    }

    /// A class `pc` with `shares` shares of queue `q`, sharing the group state `pg`.
    /// In a node with a stable address: the replenish timer and the requests refer to the
    /// class through the node's pointer.
    ///
    /// # Safety
    /// `q` is the queue that will own the class (its root pointer, see [`IoQueue::construct_in_place`]) and
    /// `pg` the group's class data; both outlive the class.
    pub unsafe fn new(
        pc: PriorityClass,
        shares: u32,
        q: *mut IoQueue,
        pg: *mut IoGroupPriorityClassData,
    ) -> NodePtr<Self> {
        let this = NodePtr::new(IoQueuePriorityClassData {
            _queue: q,
            _pc: pc,
            _shares: shares,
            _rwstat: [RwStat::default(); 2],
            _splits: RwStat::default(),
            _nr_queued: 0,
            _nr_executing: 0,
            _queue_time: Duration::ZERO,
            _total_queue_time: Duration::ZERO,
            _total_execution_time: Duration::ZERO,
            _starvation_time: Duration::ZERO,
            _activated: Instant::now(),
            _group: pg,
            _replenish_head: 0,
            _replenish: Timer::new(),
            metric_groups: metrics::MetricGroups::new(),
        });
        let p = this.as_ptr();
        // SAFETY: `p` is the node's pointer, the class's address for its whole life; the
        // timer is owned by the class, so `p` outlives every callback, which runs from the
        // lowres timer poller with no reference to the class live.
        unsafe { (*p).replenish_timer().set_callback(move || (*p).try_to_replenish()) };
        this
    }

    /// Counts a newly queued request; an idle class starts its starvation clock.
    #[inline]
    pub fn on_queue(&mut self) {
        self._nr_queued += 1;
        if self._nr_executing == 0 && self._nr_queued == 1 {
            self._activated = Instant::now();
        }
    }

    /// Accounts a dispatched request of `dnl` that waited `lat`, and charges its blocks to
    /// the bandwidth bucket; when the bucket runs short the class is throttled until the
    /// replenish timer fires.
    #[inline]
    pub fn on_dispatch(&mut self, dnl: IoDirectionAndLength, lat: Duration) {
        self._rwstat[dnl.rw_idx() as usize].add(dnl.length());
        self._queue_time = lat;
        self._total_queue_time += lat;
        self._nr_queued -= 1;
        self._nr_executing += 1;
        if self._nr_executing == 1 {
            self._starvation_time += Instant::now().saturating_duration_since(self._activated);
        }

        let tokens = self.group().tokens(dnl.length());
        let ph = self.group().tb.grab(tokens);
        let delta = self.group().tb.deficiency(ph);
        if delta > 0 {
            let cid = self.fq_class();
            // SAFETY: as in try_to_replenish(). This runs inside the fair queue's dispatch
            // (`poll_io_queue`), which keeps no reference to the queue or to the fair queue
            // across its callback, so unplugging the class re-enters it soundly.
            unsafe { IoQueue::throttle_priority_class(self._queue, cid) };
            self._replenish_head = ph;
            let d = self.group().tb.duration_for(delta);
            self.replenish_timer().arm_in(duration_cast_micros(d));
        }
    }

    /// Counts a queued request that was cancelled.
    #[inline]
    pub fn on_cancel(&mut self) {
        self._nr_queued -= 1;
    }

    /// Accounts a request that completed after executing for `lat`.
    #[inline]
    pub fn on_complete(&mut self, lat: Duration) {
        self._total_execution_time += lat;
        self._nr_executing -= 1;
        if self._nr_executing == 0 && self._nr_queued != 0 {
            self._activated = Instant::now();
        }
    }

    /// Accounts a request that failed.
    #[inline]
    pub fn on_error(&mut self) {
        self._nr_executing -= 1;
        if self._nr_executing == 0 && self._nr_queued != 0 {
            self._activated = Instant::now();
        }
    }

    /// Counts a request that had to be split.
    #[inline]
    pub fn on_split(&mut self, dnl: IoDirectionAndLength) {
        self._splits.add(dnl.length());
    }

    /// The class's id in the fair queues.
    #[inline]
    pub fn fq_class(&self) -> ClassId {
        self._pc.id()
    }

    /// The class's metric definitions.
    ///
    /// # Safety
    /// `this` is the class's node pointer (the metrics keep it), and the metrics are dropped
    /// before the class.
    pub unsafe fn metrics(this: *const Self) -> Vec<metrics::impl_::MetricDefinitionImpl> {
        use crate::engine::core::metrics as sm;
        macro_rules! s {
            () => {
                // SAFETY: the metrics are owned by the class (metric_groups) and die with it.
                unsafe { &*this }
            };
        }
        vec![
            sm::make_counter(
                "total_bytes",
                move || {
                    (s!()._rwstat[IO_DIRECTION_READ as usize].bytes
                        + s!()._rwstat[IO_DIRECTION_WRITE as usize].bytes)
                        as u64
                },
                sm::description("Total bytes passed in the queue"),
                vec![],
            ),
            sm::make_counter(
                "total_operations",
                move || {
                    s!()._rwstat[IO_DIRECTION_READ as usize].ops
                        + s!()._rwstat[IO_DIRECTION_WRITE as usize].ops
                },
                sm::description("Total operations passed in the queue"),
                vec![],
            ),
            sm::make_counter(
                "total_read_bytes",
                move || s!()._rwstat[IO_DIRECTION_READ as usize].bytes as u64,
                sm::description("Total read bytes passed in the queue"),
                vec![],
            ),
            sm::make_counter(
                "total_read_ops",
                move || s!()._rwstat[IO_DIRECTION_READ as usize].ops,
                sm::description("Total read operations passed in the queue"),
                vec![],
            ),
            sm::make_counter(
                "total_write_bytes",
                move || s!()._rwstat[IO_DIRECTION_WRITE as usize].bytes as u64,
                sm::description("Total write bytes passed in the queue"),
                vec![],
            ),
            sm::make_counter(
                "total_write_ops",
                move || s!()._rwstat[IO_DIRECTION_WRITE as usize].ops,
                sm::description("Total write operations passed in the queue"),
                vec![],
            ),
            sm::make_counter(
                "total_split_ops",
                move || s!()._splits.ops,
                sm::description("Total number of requests split"),
                vec![],
            ),
            sm::make_counter(
                "total_split_bytes",
                move || s!()._splits.bytes as u64,
                sm::description("Total number of bytes split"),
                vec![],
            ),
            sm::make_counter(
                "total_delay_sec",
                move || s!()._total_queue_time.as_secs_f64(),
                sm::description("Total time spent in the queue"),
                vec![],
            ),
            sm::make_counter(
                "total_exec_sec",
                move || s!()._total_execution_time.as_secs_f64(),
                sm::description("Total time spent in disk"),
                vec![],
            ),
            sm::make_counter(
                "starvation_time_sec",
                move || {
                    let mut st = s!()._starvation_time;
                    if s!()._nr_queued != 0 && s!()._nr_executing == 0 {
                        st += Instant::now().saturating_duration_since(s!()._activated);
                    }
                    st.as_secs_f64()
                },
                sm::description("Total time spent starving for disk"),
                vec![],
            ),
            // Note: the counter below is not the same as the reactor's queued-io-requests:
            // queued-io-requests shows us how many requests in total exist in this I/O Queue.
            //
            // This counter lives in the priority class, so it will count only queued requests
            // that belong to that class.
            //
            // In other words: the new counter tells you how busy a class is, and the
            // old counter tells you how busy the system is.
            sm::make_queue_length(
                "queue_length",
                move || s!()._nr_queued as u64,
                sm::description("Number of requests in the queue"),
                vec![],
            ),
            sm::make_queue_length(
                "disk_queue_length",
                move || s!()._nr_executing as u64,
                sm::description("Number of requests in the disk"),
                vec![],
            ),
            sm::make_gauge(
                "delay",
                move || s!()._queue_time.as_secs_f64(),
                sm::description("random delay time in the queue"),
                vec![],
            ),
            sm::make_gauge(
                "shares",
                move || s!()._shares as f64,
                sm::description("current amount of shares"),
                vec![],
            ),
        ]
    }
}

/// The completion of a dispatched read or write: it accounts the request in its class and
/// queue and resolves the request's future with the byte count or the error.
#[repr(C)]
pub struct IoDescReadWrite {
    base: IoCompletion,
    _ioq: *mut IoQueue,
    _pclass: *mut IoQueuePriorityClassData,
    _ts: Instant,
    _stream: StreamId,
    _dnl: IoDirectionAndLength,
    _fq_capacity: CapacityT,
    _pr: Promise<usize>,
    _iovs: IovecKeeper,
    _dispatched_polls: u64,
}

impl IoDescReadWrite {
    /// A completion for a request of `dnl` and cost `cap` on stream `stream` of `ioq`,
    /// keeping `iovs` alive.
    ///
    /// # Safety
    /// `ioq` and `pc` (a class of `ioq`) are live and outlive the request.
    pub unsafe fn new(
        ioq: *mut IoQueue,
        pc: *mut IoQueuePriorityClassData,
        stream: StreamId,
        dnl: IoDirectionAndLength,
        cap: CapacityT,
        iovs: IovecKeeper,
    ) -> Box<Self> {
        let this = Box::new(IoDescReadWrite {
            base: IoCompletion::new::<IoDescReadWrite>(),
            _ioq: ioq,
            _pclass: pc,
            _ts: Instant::now(),
            _stream: stream,
            _dnl: dnl,
            _fq_capacity: cap,
            _pr: Promise::new(),
            _iovs: iovs,
            _dispatched_polls: 0,
        });
        io_log().trace(
            FormatInfo::new("dev {} : req {:p} queue  len {} capacity {}"),
            format_args!(
                "dev {} : req {:p} queue  len {} capacity {}",
                // SAFETY: `ioq` is a live queue.
                unsafe { (*ioq).id() },
                &*this,
                dnl.length(),
                cap
            ),
        );
        this
    }

    /// The queue's id.
    #[inline]
    fn ioq_id(&self) -> u32 {
        // SAFETY: the constructor's contract: the queue outlives its requests; a short
        // shared access.
        unsafe { (*self._ioq).id() }
    }

    /// The request's class, for one call.
    #[inline]
    fn pclass(&mut self) -> &mut IoQueuePriorityClassData {
        // SAFETY: the constructor's contract: the class is owned by the queue and outlives
        // its requests; the reference is tied to this request's exclusive borrow, and no
        // other reference to the class is live while a request runs it.
        unsafe { &mut *self._pclass }
    }

    /// Completes a queued request as cancelled, consuming the completion.
    pub fn cancel(self: Box<Self>) {
        let mut me = self;
        me.pclass().on_cancel();
        me._pr.set_exception(DefaultIoExceptionFactory::cancelled());
    }

    /// Accounts the dispatch in the class and starts the execution clock.
    #[inline]
    pub fn dispatch(&mut self) {
        io_log().trace(
            FormatInfo::new("dev {} : req {:p} submit"),
            format_args!("dev {} : req {:p} submit", self.ioq_id(), self),
        );
        let now = Instant::now();
        let dnl = self._dnl;
        let lat = now.saturating_duration_since(self._ts);
        self.pclass().on_dispatch(dnl, lat);
        self._ts = now;
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        self._dispatched_polls = unsafe { (*engine()).polls() };
    }

    /// The future of the request's result.
    #[inline]
    pub fn get_future(&mut self) -> Future<usize> {
        self._pr.get_future()
    }

    /// The request's cost.
    #[inline]
    pub fn capacity(&self) -> CapacityT {
        self._fq_capacity
    }
    /// The stream the request is queued on.
    #[inline]
    pub fn stream(&self) -> StreamId {
        self._stream
    }
    /// The reactor's poll count when the request was dispatched.
    #[inline]
    pub fn polls(&self) -> u64 {
        self._dispatched_polls
    }
}

// SAFETY: `#[repr(C)]` with the `IoCompletion` built for `Self` at offset 0.
unsafe impl IoCompletionImpl for IoDescReadWrite {
    unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        io_log().trace(
            FormatInfo::new("dev {} : req {:p} error"),
            format_args!("dev {} : req {:p} error", me.ioq_id(), me),
        );
        me.pclass().on_error();
        let ioq = me._ioq;
        // SAFETY: as in ioq().
        unsafe { (*ioq).complete_request(me, Duration::ZERO) };
        me._pr.set_exception(eptr);
        // SAFETY: `delete this`: the descriptor was released to the sink by `dispatch`.
        drop(unsafe { Box::from_raw(this) });
    }

    unsafe fn complete(this: *mut Self, res: usize) {
        // SAFETY: the trait's contract: `this` is live and no reference to it is live;
        // `me` is not used after the object is destroyed.
        let me = unsafe { &mut *this };
        io_log().trace(
            FormatInfo::new("dev {} : req {:p} complete"),
            format_args!("dev {} : req {:p} complete", me.ioq_id(), me),
        );
        let now = Instant::now();
        let delay = now.saturating_duration_since(me._ts);
        me.pclass().on_complete(delay);
        let ioq = me._ioq;
        // SAFETY: as in ioq().
        unsafe { (*ioq).complete_request(me, delay) };
        me._pr.set_value(res);
        // SAFETY: `delete this`.
        drop(unsafe { Box::from_raw(this) });
    }
}

/// A request waiting in a queue: the I/O request, its fair-queue entry, its completion and
/// its link into the intent's cancellable queue.
#[repr(C)]
pub struct QueuedIoRequest {
    req: IoRequest,
    _ioq: *mut IoQueue,
    _stream: StreamId,
    _fq_entry: FairQueueEntry,
    _intent: Link,
    _desc: Option<Box<IoDescReadWrite>>,
}

impl QueuedIoRequest {
    #[inline]
    fn is_cancelled(&self) -> bool {
        self._desc.is_none()
    }

    /// A queued request for `req` of cost `cap` in class `pc` of queue `q`.
    ///
    /// The request is handed over with `Box::into_raw` before it is linked anywhere (see
    /// `IoQueue::queue_one_request`): the links store pointers derived from that pointer.
    ///
    /// # Safety
    /// `q` is the queue's root pointer (see [`IoQueue::construct_in_place`]) and `pc` a class of it; the
    /// queue outlives the request.
    pub unsafe fn new(
        req: IoRequest,
        q: *mut IoQueue,
        cap: CapacityT,
        pc: *mut IoQueuePriorityClassData,
        dnl: IoDirectionAndLength,
        iovs: IovecKeeper,
    ) -> Box<Self> {
        // SAFETY: `q` is live; a short shared access.
        let stream = unsafe { (*q).request_stream(dnl) };
        let qp = q;
        Box::new(QueuedIoRequest {
            req,
            _ioq: qp,
            _stream: stream,
            _fq_entry: FairQueueEntry::new(cap),
            _intent: Link::default(),
            // SAFETY: guaranteed by the caller.
            _desc: Some(unsafe { IoDescReadWrite::new(qp, pc, stream, dnl, cap, iovs) }),
        })
    }

    /// Submits the request to the sink, or completes it as cancelled, and frees it.
    ///
    /// # Safety
    /// `this` is a request made by `IoQueue::queue_one_request` (`Box::into_raw`), just
    /// handed out by its fair queue, with no reference to it live; it is freed here.
    pub unsafe fn dispatch(this: *mut Self) {
        // SAFETY: the contract above; every access is a short raw place, and the request is
        // freed last, after its link left the cancellable queue.
        unsafe {
            let ioq = (*this)._ioq;
            if (*this).is_cancelled() {
                IoQueue::complete_cancelled_request(ioq, this);
                drop(Box::from_raw(this));
                return;
            }

            Link::maybe_dequeue(std::ptr::addr_of_mut!((*this)._intent));
            // A request that is not cancelled has its descriptor (`is_cancelled`).
            let desc = Box::into_raw((*this)._desc.take().unwrap_unchecked());
            (*desc).dispatch();
            let req = (*this).req;
            IoQueue::submit_request(ioq, desc, req);
            drop(Box::from_raw(this));
        }
    }

    /// Cancels the request: it is unlinked from its intent and dispatched later as
    /// cancelled, without consuming capacity.
    ///
    /// # Safety
    /// `this` is a live queued request (reached from its cancellable queue's link), with no
    /// reference to it live.
    pub unsafe fn cancel(this: *mut Self) {
        // SAFETY: the contract above; short raw places.
        unsafe {
            IoQueue::cancel_request((*this)._ioq, this);
            if let Some(d) = (*this)._desc.take() {
                d.cancel();
            }
        }
    }

    /// Links the request into the intent's cancellable queue `cq`.
    ///
    /// # Safety
    /// `this` is the request's pointer from `Box::into_raw`, and `cq` a queue of a live,
    /// pinned intent reached through the intent pointer
    /// (`IoIntent::find_or_create_cancellable_queue_raw`).
    #[inline]
    pub unsafe fn set_intent(this: *mut Self, cq: *mut CancellableQueue) {
        // SAFETY: the link is derived from the pointer to the whole request, so the queue's
        // destructor can go back to the request (`from_cq_link`).
        unsafe { Link::enqueue(std::ptr::addr_of_mut!((*this)._intent), cq) };
    }

    /// The future of the request's result.
    #[inline]
    pub fn get_future(&mut self) -> Future<usize> {
        match self._desc.as_mut() {
            Some(d) => d.get_future(),
            None => make_exception_future(DefaultIoExceptionFactory::cancelled()),
        }
    }

    /// The request's fair-queue entry, as a pointer derived from the pointer to the whole
    /// request, so that the fair queue can store it and `from_fq_entry` can go back.
    ///
    /// # Safety
    /// `this` points to a live request.
    #[inline]
    pub unsafe fn queue_entry(this: *mut Self) -> *mut FairQueueEntry {
        // SAFETY: forwarded from the caller; no reference is formed.
        unsafe { std::ptr::addr_of_mut!((*this)._fq_entry) }
    }

    /// The stream the request is queued on.
    #[inline]
    pub fn stream(&self) -> StreamId {
        self._stream
    }

    /// The request that embeds the fair-queue entry `ent`.
    ///
    /// # Safety
    ///
    /// `ent` must be the `_fq_entry` of a live, heap-allocated `QueuedIoRequest`, derived
    /// from the pointer to the whole request (`queue_entry`).
    #[inline]
    pub unsafe fn from_fq_entry(ent: *mut FairQueueEntry) -> *mut QueuedIoRequest {
        // SAFETY: forwarded from the caller; the offset is that of the field.
        unsafe { ent.byte_sub(std::mem::offset_of!(QueuedIoRequest, _fq_entry)).cast() }
    }

    /// The request that embeds the cancellable-queue link `link`.
    ///
    /// # Safety
    ///
    /// `link` must be the `_intent` of a live `QueuedIoRequest`, derived from the pointer to
    /// the whole request (as `set_intent` links it).
    #[inline]
    pub unsafe fn from_cq_link(link: *mut Link) -> *mut QueuedIoRequest {
        // SAFETY: forwarded from the caller; the offset is that of the field.
        unsafe {
            link.byte_sub(std::mem::offset_of!(QueuedIoRequest, _intent)).cast::<QueuedIoRequest>()
        }
    }
}

// (`PriorityClass` is in this module's `internal`; the cancellable queue and intent
// references are in core/internal/io_intent.rs.)

impl IoSink {
    /// Queues `req` with its completion `desc` for the reactor to submit to the kernel.
    #[inline]
    pub fn submit(&mut self, desc: *mut IoCompletion, req: IoRequest) {
        self._pending_io.push_back(PendingIoRequest::new(req, desc));
    }
}

impl IoRequest {
    /// Splits the request into parts of at most `max_length` bytes, each with its own
    /// buffer range or `iovec` array.
    pub fn split(&self, max_length: usize) -> Vec<Part> {
        let op = self.opcode();
        if op == Operation::Read || op == Operation::Write {
            return self.split_buffer(max_length);
        }
        if op == Operation::Readv || op == Operation::Writev {
            return self.split_iovec(max_length);
        }

        engine_logger().error(
            FormatInfo::new("Invalid operation for split: {}"),
            format_args!("Invalid operation for split: {}", op as i32),
        );
        std::process::abort();
    }

    fn split_buffer(&self, max_length: usize) -> Vec<Part> {
        // The layouts of the read and write forms must be identical, otherwise we need to
        // have two different implementations for each of them
        let op_size = if self.opcode() == Operation::Read {
            self.as_read().size
        } else {
            self.as_write().size
        };
        let mut ret = Vec::with_capacity(op_size.div_ceil(max_length));

        let mut off = 0usize;
        loop {
            let len = (op_size - off).min(max_length);
            ret.push(Part { req: self.sub_req_buffer(off, len), size: len, iovecs: Vec::new() });
            off += len;
            if off >= op_size {
                break;
            }
        }

        ret
    }

    fn split_iovec(&self, max_length: usize) -> Vec<Part> {
        let mut parts = Vec::new();
        let mut vecs: Vec<Iovec> = Vec::new();
        // The layouts of the vectored read and write forms must be identical, otherwise we need to
        // have two different implementations for each of them
        let op = if self.opcode() == Operation::Readv { self.as_readv() } else { self.as_writev() };
        let mut cur = op.iovec;
        let mut pos = 0usize;
        let mut off = 0usize;
        // SAFETY: `iovec` points to `iov_len` entries owned by the caller of the request.
        let end = unsafe { cur.add(op.iov_len) };
        let mut remaining = max_length;

        while cur != end {
            // SAFETY: `cur` is within [iovec, end).
            let c = unsafe { &*cur };
            let mut iov = Iovec {
                iov_base: (c.iov_base as *mut u8).wrapping_add(off).cast(),
                iov_len: c.iov_len - off,
            };

            if iov.iov_len <= remaining {
                remaining -= iov.iov_len;
                vecs.push(iov);
                // SAFETY: as above.
                cur = unsafe { cur.add(1) };
                off = 0;
                continue;
            }

            if remaining > 0 {
                iov.iov_len = remaining;
                off += remaining;
                vecs.push(iov);
            }

            let mut v = std::mem::take(&mut vecs);
            let req = self.sub_req_iovec(pos, &mut v);
            parts.push(Part { req, size: max_length, iovecs: v });
            pos += max_length;
            remaining = max_length;
        }

        if !vecs.is_empty() {
            engine_assert!(remaining < max_length);
            let mut v = vecs;
            let req = self.sub_req_iovec(pos, &mut v);
            parts.push(Part { req, size: max_length - remaining, iovecs: v });
        }

        parts
    }

    /// The name of the operation, for messages.
    pub fn opname(&self) -> Sstring {
        Sstring::from(match self.opcode() {
            Operation::Fdatasync => "fdatasync",
            Operation::Write => "write",
            Operation::Writev => "vectored write",
            Operation::Read => "read",
            Operation::Readv => "vectored read",
            Operation::Recv => "recv",
            Operation::Recvmsg => "recvmsg",
            Operation::Send => "send",
            Operation::Sendmsg => "sendmsg",
            Operation::Accept => "accept",
            Operation::Connect => "connect",
            Operation::PollAdd => "poll add",
            Operation::PollRemove => "poll remove",
            Operation::Cancel => "cancel",
        })
    }
}

/// Updates the exponential moving average `result` with `value`:
/// `result = result * factor + value * (1 - factor)`.
#[inline]
fn update_moving_average(result: &mut f64, value: f64, factor: f64) {
    *result = *result * factor + value * (1.0 - factor);
}

/// The I/O queue of one device on one shard (see the module docs).
pub struct IoQueue {
    _priority_classes: Vec<Option<NodePtr<IoQueuePriorityClassData>>>,
    pub(crate) _group: IoGroupPtr,
    _id: u32,
    _streams: StaticVector<FairQueue, 2>,
    _sink: *mut IoSink,
    // The fields below are going away, they are just here so we can implement deprecated
    // functions that used to be provided by the fair_queue and are going away (from both
    // the fair_queue and the io_queue). Double-accounting for now will allow for easier
    // decoupling and is temporary
    _queued_requests: usize,
    _requests_executing: usize,
    _requests_dispatched: u64,
    _requests_completed: u64,

    // Flow monitor
    _prev_dispatched: u64,
    _prev_completed: u64,
    _flow_ratio: f64,
    _averaging_decay_timer: Timer<LowresClock>,

    _stall_threshold_min: Duration,
    _stall_threshold: Duration,
    _metric_groups: metrics::MetricGroups,
}

/// A time point of the queue's clock.
pub type IoQueueClockTimePoint = Instant;

impl IoQueue {
    /// The weight of one read request.
    ///
    /// Write requests may be more expensive than reads. To avoid floating-point math, one
    /// read counts as this amount: a write 30% more expensive than a read counts as
    /// `READ_REQUEST_BASE_COUNT * 130 / 100`. Reads may also be the expensive ones, in
    /// which case writes count less than this.
    pub const READ_REQUEST_BASE_COUNT: u32 = 128;
    /// Log2 of the block size requests are costed in (512 bytes).
    pub const BLOCK_SIZE_SHIFT: u32 = 9;

    fn update_flow_ratio(&mut self) {
        if self._requests_completed > self._prev_completed {
            let instant = (self._requests_dispatched - self._prev_dispatched) as f64
                / (self._requests_completed - self._prev_completed) as f64;
            let f = self.get_config().flow_ratio_ema_factor;
            update_moving_average(&mut self._flow_ratio, instant, f);
            self._prev_dispatched = self._requests_dispatched;
            self._prev_completed = self._requests_completed;
        }
    }

    fn lower_stall_threshold(&mut self) {
        let new_threshold = self._stall_threshold.saturating_sub(Duration::from_millis(1));
        self._stall_threshold = self._stall_threshold_min.max(new_threshold);
    }

    /// Accounts a completed request that executed for `delay`, and warns about a stall
    /// when `delay` exceeds the current threshold.
    pub fn complete_request(&mut self, desc: &IoDescReadWrite, delay: Duration) {
        self._requests_executing -= 1;
        self._requests_completed += 1;
        // SAFETY: the stream index of a request is one the queue assigned (request_stream),
        // below the number of streams.
        unsafe { self._streams.get_unchecked_mut(desc.stream() as usize) }
            .notify_request_finished(desc.capacity());

        if delay > self._stall_threshold {
            self._stall_threshold *= 2;
            io_log().warn(
                FormatInfo::new(
                    "Request took {:.3}ms ({} polls) to execute, queued {} executing {}",
                ),
                format_args!(
                    "Request took {:.3}ms ({} polls) to execute, queued {} executing {}",
                    delay.as_secs_f64() * 1000.0,
                    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                    unsafe { (*engine()).polls() } - desc.polls(),
                    self._queued_requests,
                    self._requests_executing
                ),
            );
        }
    }

    fn make_fair_queue_config(_iocfg: &IoQueueConfig, label: &str) -> FairQueueConfig {
        FairQueueConfig { label: Sstring::from(label), ..FairQueueConfig::default() }
    }

    /// Builds a queue of `group` that submits to `sink`, in place at its final address:
    /// the decay timer, the classes and the queued requests keep `this`.
    ///
    /// # Errors
    /// A metric registration error; the queue is then dropped again and `this` is left
    /// uninitialized.
    ///
    /// # Safety
    /// `this` is valid for writes, suitably aligned, uninitialized, and stays at that address
    /// until the queue is dropped in place; it is the root pointer of the queue: every later
    /// access goes through pointers with its provenance (no `Box` or `&mut` owner retakes the
    /// memory). `sink` is the reactor's I/O sink, which outlives the queue.
    pub unsafe fn construct_in_place(
        this: *mut IoQueue,
        group: IoGroupPtr,
        sink: *mut IoSink,
    ) -> Result<(), ExceptionPtr> {
        let id = group._config.id;
        let stall_min = group._config.stall_threshold.max(Duration::from_millis(1));
        // SAFETY: the contract above.
        unsafe {
            this.write(IoQueue {
                _priority_classes: Vec::new(),
                _group: group,
                _id: id,
                _streams: StaticVector::new(),
                _sink: sink,
                _queued_requests: 0,
                _requests_executing: 0,
                _requests_dispatched: 0,
                _requests_completed: 0,
                _prev_dispatched: 0,
                _prev_completed: 0,
                _flow_ratio: 1.0,
                _averaging_decay_timer: Timer::new(),
                _stall_threshold_min: stall_min,
                _stall_threshold: stall_min,
                _metric_groups: metrics::MetricGroups::new(),
            })
        };
        // SAFETY: the queue is initialized at its final address.
        if let Err(e) = unsafe { Self::construct_body(this) } {
            // SAFETY: as above; nothing refers to the queue any longer.
            unsafe { std::ptr::drop_in_place(this) };
            return Err(e);
        }
        Ok(())
    }

    /// The second half of construction: streams, decay timer and metrics.
    ///
    /// # Errors
    /// A metric registration error.
    ///
    /// # Safety
    /// As in [`IoQueue::construct_in_place`], with the queue initialized.
    unsafe fn construct_body(p: *mut IoQueue) -> Result<(), ExceptionPtr> {
        // SAFETY: the contract above; `this` is reborrowed for the body only, and the
        // pointers the timer keeps are `p` itself.
        let this = unsafe { &mut *p };
        // SAFETY: the queue never moves, so its timer is pinned; the timer is owned by the
        // queue, so `p` outlives every callback, which runs with no reference to the queue
        // live.
        unsafe { Pin::new_unchecked(&mut this._averaging_decay_timer) }.set_callback(
            move || unsafe {
                (*p).update_flow_ratio();
                (*p).lower_stall_threshold();
            },
        );

        let cfg = this.get_config().clone();
        let group = this._group.clone();
        if cfg.duplex {
            const _: () = assert!(IoDirectionAndLength::WRITE_IDX == 0);
            // SAFETY: the queue holds the group (an Arc) for its whole life, so the fair
            // groups outlive the fair queues, as `FairQueue::new` requires.
            this._streams.push(unsafe {
                FairQueue::new(&group._fgs[0], Self::make_fair_queue_config(&cfg, "write"))
            });
            const _: () = assert!(IoDirectionAndLength::READ_IDX == 1);
            // SAFETY: as above, the group outlives the fair queue.
            this._streams.push(unsafe {
                FairQueue::new(&group._fgs[1], Self::make_fair_queue_config(&cfg, "read"))
            });
        } else {
            // SAFETY: as above.
            this._streams.push(unsafe {
                FairQueue::new(&group._fgs[0], Self::make_fair_queue_config(&cfg, "rw"))
            });
        }
        let period = group.io_latency_goal().mul_f64(cfg.averaging_decay_ticks as f64);
        // SAFETY: as above.
        unsafe { Pin::new_unchecked(&mut this._averaging_decay_timer) }
            .arm_periodic(Duration::from_millis(period.as_millis() as u64));

        use crate::engine::core::metrics as sm;
        let owner_l = sm::shard_label().call(crate::engine::core::shard_id::this_shard_id());
        let mnt_l = sm::label("mountpoint").call(this.mountpoint());
        let group_l = sm::label("iogroup").call(group._allocated_on);
        // SAFETY: the metric is owned by the queue and dies with it.
        this._metric_groups.add_group("io_queue", vec![
            // SAFETY: `p` is the queue, which never moves and owns the metric group, so it is
            // live whenever the gauge is read; the read is on the owning shard.
            sm::make_gauge("flow_ratio", move || unsafe { (*p)._flow_ratio },
                sm::description("Ratio of dispatch rate to completion rate. Is expected to be 1.0+ growing larger on reactor stalls or (!) disk problems"),
                vec![owner_l, mnt_l, group_l]),
        ])?;
        Ok(())
    }

    /// The stream a request of `dnl` goes to: its direction on duplex devices, else 0.
    #[inline]
    pub fn request_stream(&self, dnl: IoDirectionAndLength) -> StreamId {
        if self.get_config().duplex { dnl.rw_idx() as StreamId } else { 0 }
    }

    /// `pc` is a class of this queue, its node pointer, with no reference to it live.
    unsafe fn register_stats(
        &mut self,
        name: &Sstring,
        pc: *mut IoQueuePriorityClassData,
    ) -> Result<(), ExceptionPtr> {
        use crate::engine::core::metrics as sm;
        let mut new_metrics = metrics::MetricGroups::new();

        let owner_l = sm::shard_label().call(crate::engine::core::shard_id::this_shard_id());
        let mnt_l = sm::label("mountpoint").call(self.mountpoint());
        let class_l = sm::label("class").call(name);
        let group_l = sm::label("iogroup").call(self._group._allocated_on);

        let mut metrics_v: Vec<sm::MetricDefinition> = Vec::new();
        // SAFETY: the contract above; the metrics live in the class.
        for m in unsafe { IoQueuePriorityClassData::metrics(pc) } {
            metrics_v.push(m.label(&owner_l).label(&mnt_l).label(&class_l).label(&group_l).into());
        }

        // SAFETY: as above; a short shared access.
        let cid = unsafe { (*pc).fq_class() };
        for s in self._streams.iter_mut() {
            let stream_l = sm::label("stream").call(s.label());
            for m in s.metrics(cid) {
                metrics_v.push(
                    m.label(&owner_l)
                        .label(&mnt_l)
                        .label(&class_l)
                        .label(&group_l)
                        .label(&stream_l)
                        .into(),
                );
            }
        }

        new_metrics.add_group("io_queue", metrics_v)?;
        // SAFETY: as above.
        unsafe { (*pc).metric_groups = new_metrics };
        Ok(())
    }

    /// # Safety
    /// `p` is the queue's root pointer (see [`IoQueue::construct_in_place`]), with no
    /// reference to the queue live: a new class keeps it.
    unsafe fn find_or_create_class(
        p: *mut Self,
        pc: PriorityClass,
    ) -> Result<*mut IoQueuePriorityClassData, ExceptionPtr> {
        // SAFETY: the contract above; the reborrow does not outlive this call, and the class
        // gets `p`, not a pointer taken from it.
        let this = unsafe { &mut *p };
        let id = pc.id() as usize;
        if id >= this._priority_classes.len() {
            this._priority_classes.resize_with(id + 1, || None);
        }
        if this._priority_classes[id].is_none() {
            let (shares, name) = get_class_info(pc.id());

            // A note on naming:
            //
            // We could just add the owner as the instance id and have something like:
            //  io_queue-<class_owner>-<counter>-<class_name>
            //
            // However, when there are more than one shard per I/O queue, it is very useful
            // to know which shards are being served by the same queue. Therefore, a better name
            // scheme is:
            //
            //  io_queue-<queue_owner>-<counter>-<class_name>, shard=<class_owner>
            //  using the shard label to hold the owner number
            //
            // This conveys all the information we need and allows one to easily group all classes from
            // the same I/O queue (by filtering by shard)
            for s in this._streams.iter_mut() {
                s.register_priority_class(pc.id(), shares);
            }
            let group = this._group.clone();
            let pg = group.find_or_create_class(pc);
            // SAFETY: the queue owns the class; the group's class data lives as long as the
            // group, which the queue holds; `p` is the queue's root pointer.
            let pc_data = unsafe { IoQueuePriorityClassData::new(pc, shares, p, pg) };
            // SAFETY: the node pointer of the new class, with no reference to it live.
            unsafe { this.register_stats(&name, pc_data.as_ptr())? };

            this._priority_classes[id] = Some(pc_data);
        }
        match &this._priority_classes[id] {
            Some(n) => Ok(n.as_ptr()),
            None => Err(ExceptionPtr::new(LogicError::new("io_queue priority class".to_string()))),
        }
    }

    /// The cost of a request of `dnl` in its stream's capacity. When the flow ratio is above
    /// the back-pressure threshold the cost is inflated by it, up to the stream's maximum.
    #[inline]
    pub fn request_capacity(&self, dnl: IoDirectionAndLength) -> CapacityT {
        let cfg = self.get_config();
        let tokens = internal::request_tokens(dnl, cfg);
        // SAFETY: request_stream() gives the index of one of the queue's streams.
        let stream = unsafe { self._streams.get_unchecked(self.request_stream(dnl) as usize) };
        if self._flow_ratio <= cfg.flow_ratio_backpressure_threshold {
            return stream.tokens_capacity(tokens);
        }

        let cap = stream.tokens_capacity(tokens * self._flow_ratio);
        let max_cap = stream.maximum_capacity();
        cap.min(max_cap)
    }

    /// The longest read and write issued in one request: the saturation lengths, bounded by
    /// the group's maximum request lengths and aligned down.
    pub fn get_request_limits(&self) -> RequestLimits {
        let bs = 1usize << Self::BLOCK_SIZE_SHIFT;
        let align_down = |v: usize| v & !(bs - 1);
        RequestLimits {
            max_read: align_down(
                (self.get_config().disk_read_saturation_length as usize)
                    .min(self._group._max_request_length[IO_DIRECTION_READ as usize].get()),
            ),
            max_write: align_down(
                (self.get_config().disk_write_saturation_length as usize)
                    .min(self._group._max_request_length[IO_DIRECTION_WRITE as usize].get()),
            ),
        }
    }

    /// # Safety
    /// As `IoQueue::find_or_create_class`.
    unsafe fn queue_one_request(
        this: *mut Self,
        pc: PriorityClass,
        dnl: IoDirectionAndLength,
        req: IoRequest,
        intent: *mut IoIntent,
        iovs: IovecKeeper,
    ) -> Future<usize> {
        // First time will hit here, and then we create the class. It is important
        // that we create the shared pointer in the same shard it will be used at later.
        // SAFETY: the contract above.
        let pclass =
            crate::engine::core::reactor::fut_try!(unsafe { Self::find_or_create_class(this, pc) });
        // SAFETY: the contract above; every access to the queue below is a short raw place.
        unsafe {
            let cap = (*this).request_capacity(dnl);
            // `pclass` is a class of this queue (find_or_create_class). The request is leaked
            // before anything links it, so every stored pointer derives from `raw`.
            let raw = Box::into_raw(QueuedIoRequest::new(req, this, cap, pclass, dnl, iovs));
            let fut = (*raw).get_future();
            if !intent.is_null() {
                // a non-null intent is pinned and outlives the requests attached to it (it
                // cancels them when it dies)
                let cq =
                    IoIntent::find_or_create_cancellable_queue_raw(intent, (*this)._id, pc.id());
                QueuedIoRequest::set_intent(raw, cq);
            }

            let stream = (*raw).stream() as usize;
            let fq_class = (*pclass).fq_class();
            // `raw` stays in place until it is dispatched or cancelled and unlinked;
            // `fq_class` is registered with every stream.
            (*Self::stream_ptr(this, stream)).queue(fq_class, QueuedIoRequest::queue_entry(raw));
            (*pclass).on_queue();
            (*this)._queued_requests += 1;
            fut
        }
    }

    /// Queues `req` in class `pc`, splitting it into parts of at most the request limit;
    /// the future resolves with the total bytes transferred. Public only for the queue
    /// tests; use the submit functions.
    ///
    /// # Safety
    /// `this` is the queue's root pointer (see [`IoQueue::construct_in_place`]), with no
    /// reference to the queue live: a new class keeps it.
    #[doc(hidden)]
    pub unsafe fn queue_request(
        this: *mut Self,
        pc: PriorityClass,
        dnl: IoDirectionAndLength,
        req: IoRequest,
        intent: *mut IoIntent,
        iovs: IovecKeeper,
    ) -> Future<usize> {
        // SAFETY: the contract above; a short shared access.
        let group: &IoGroup = unsafe { &(*this)._group };
        let max_length = group._max_request_length[dnl.rw_idx() as usize].get();

        if dnl.length() <= max_length {
            // SAFETY: forwarded.
            return unsafe { Self::queue_one_request(this, pc, dnl, req, intent, iovs) };
        }

        let parts = req.split(max_length);
        let mut p: Vec<Future<usize>> = Vec::with_capacity(parts.len());
        // SAFETY: forwarded.
        let pcd =
            crate::engine::core::reactor::fut_try!(unsafe { Self::find_or_create_class(this, pc) });
        // SAFETY: `pcd` is owned by `_priority_classes`.
        unsafe { (*pcd).on_split(dnl) };
        // SAFETY: on this shard's reactor thread; a short access to a counter.
        unsafe { (*engine())._io_stats.aio_outsizes += 1 };

        // No errors from now on: if queue_one_request fails it resolves into a failed
        // future, which when_all() below picks up.
        for part in parts {
            // SAFETY: forwarded.
            let f = unsafe {
                Self::queue_one_request(
                    this,
                    pc,
                    IoDirectionAndLength::new(dnl.rw_idx(), part.size),
                    part.req,
                    intent,
                    part.iovecs,
                )
            };
            p.push(f);
        }

        crate::engine::core::when_all::when_all_iter(p).then(move |results: Vec<Future<usize>>| {
            let mut prev_ok = true;
            let mut total = 0usize;
            let mut ex: Option<ExceptionPtr> = None;

            for mut res in results {
                if !res.failed() {
                    if prev_ok {
                        // The future is ready and did not fail, so get() gives its value.
                        let sz = match res.get() {
                            Ok(sz) => sz,
                            Err(e) => {
                                ex.get_or_insert(e);
                                prev_ok = false;
                                continue;
                            }
                        };
                        total += sz;
                        prev_ok &= sz == max_length;
                    }
                } else {
                    if ex.is_none() {
                        ex = Some(res.get_exception());
                    } else {
                        res.ignore_ready_future();
                    }
                    prev_ok = false;
                }
            }

            if total > 0 {
                make_ready_future(total)
            } else if let Some(ex) = ex {
                make_exception_future(ex)
            } else {
                make_ready_future(0)
            }
        })
    }

    /// Queues a read of `len` bytes in class `priority_class`, cancellable through `intent`
    /// (may be null); resolves with the bytes read.
    ///
    /// # Safety
    /// `this` is the queue's root pointer (as [`Reactor::get_io_queue`](crate::engine::core::reactor::Reactor::get_io_queue)
    /// gives it), with no reference to the queue live: the queued request keeps it.
    pub unsafe fn submit_io_read(
        this: *mut Self,
        pc: PriorityClass,
        len: usize,
        req: IoRequest,
        intent: *mut IoIntent,
        iovs: IovecKeeper,
    ) -> Future<usize> {
        let r = engine();
        // SAFETY: on this shard's reactor thread; the reactor is reached for these accesses only (see `engine()`).
        unsafe {
            (*r)._io_stats.aio_reads += 1;
            (*r)._io_stats.aio_read_bytes += len as u64;
        }
        // SAFETY: the contract above.
        unsafe {
            Self::queue_request(
                this,
                pc,
                IoDirectionAndLength::new(IO_DIRECTION_READ, len),
                req,
                intent,
                iovs,
            )
        }
    }

    /// Queues a write of `len` bytes in class `priority_class`, cancellable through `intent`
    /// (may be null); resolves with the bytes written.
    ///
    /// # Safety
    /// `this` is the queue's root pointer (as [`Reactor::get_io_queue`](crate::engine::core::reactor::Reactor::get_io_queue)
    /// gives it), with no reference to the queue live: the queued request keeps it.
    pub unsafe fn submit_io_write(
        this: *mut Self,
        pc: PriorityClass,
        len: usize,
        req: IoRequest,
        intent: *mut IoIntent,
        iovs: IovecKeeper,
    ) -> Future<usize> {
        let r = engine();
        // SAFETY: on this shard's reactor thread; the reactor is reached for these accesses only (see `engine()`).
        unsafe {
            (*r)._io_stats.aio_writes += 1;
            (*r)._io_stats.aio_write_bytes += len as u64;
        }
        // SAFETY: the contract above.
        unsafe {
            Self::queue_request(
                this,
                pc,
                IoDirectionAndLength::new(IO_DIRECTION_WRITE, len),
                req,
                intent,
                iovs,
            )
        }
    }

    /// Dispatches the requests that the fair queues can serve now.
    ///
    ///
    /// Dispatching re-enters the queue (a request unplugs its class from the fair queue
    /// being dispatched, and submits itself to the sink), so the queue is reached by its
    /// root pointer and the fair queues through pointers into the streams' buffer; no
    /// reference to either is live across a dispatch.
    ///
    /// # Safety
    /// `this` is the queue's root pointer, with no reference to the queue live.
    pub unsafe fn poll_io_queue(this: *mut Self) {
        // SAFETY: the contract above. The streams are inline in the queue and filled by the
        // constructor only; each fair queue is reached by a place of the root pointer.
        unsafe {
            let n = StaticVector::len_of(std::ptr::addr_of!((*this)._streams));
            for i in 0..n {
                FairQueue::dispatch_requests(
                    Self::stream_ptr(this, i),
                    |fqe: *mut FairQueueEntry| {
                        // every queued entry is the `_fq_entry` of a leaked QueuedIoRequest,
                        // queued with the request's own pointer (`queue_one_request`)
                        QueuedIoRequest::dispatch(QueuedIoRequest::from_fq_entry(fqe));
                    },
                );
            }
        }
    }

    /// The fair queue of stream `i`, a place of the queue's root pointer (see `poll_io_queue`).
    ///
    /// # Safety
    /// `this` is the queue's root pointer and `i` a stream index.
    #[inline]
    unsafe fn stream_ptr(this: *mut Self, i: usize) -> *mut FairQueue {
        // SAFETY: the contract above; no reference to the queue or its streams is formed.
        unsafe { StaticVector::slot_ptr(std::ptr::addr_of_mut!((*this)._streams), i) }
    }

    /// Hands a dispatched request to the sink and accounts it as executing.
    ///
    /// # Safety
    /// `this` is the queue's root pointer, with no reference to the queue live (this runs
    /// inside `poll_io_queue`).
    #[inline]
    pub unsafe fn submit_request(this: *mut Self, desc: *mut IoDescReadWrite, req: IoRequest) {
        // SAFETY: the contract above; short raw places. `_sink` is a reference in the
        // reactor; the reactor's sink outlives the queue.
        unsafe {
            (*this)._queued_requests -= 1;
            (*this)._requests_executing += 1;
            (*this)._requests_dispatched += 1;
            (*(*this)._sink).submit(desc.cast::<IoCompletion>(), req);
        }
    }

    /// Cancels the queued request `req` in its fair queue.
    ///
    /// # Safety
    /// `this` is the queue's root pointer and `req` a live request queued in it; no
    /// reference to either is live.
    pub unsafe fn cancel_request(this: *mut Self, req: *mut QueuedIoRequest) {
        // SAFETY: the contract above.
        unsafe {
            (*this)._queued_requests -= 1;
            let s = (*req).stream() as usize;
            (*Self::stream_ptr(this, s))
                .notify_request_cancelled(QueuedIoRequest::queue_entry(req));
        }
    }

    /// Accounts a cancelled request when the fair queue hands it out.
    ///
    /// # Safety
    /// As [`IoQueue::cancel_request`] (this runs inside `poll_io_queue`).
    pub unsafe fn complete_cancelled_request(this: *mut Self, req: *mut QueuedIoRequest) {
        // SAFETY: the contract above.
        unsafe {
            let s = (*req).stream() as usize;
            (*Self::stream_ptr(this, s)).notify_request_finished((*req)._fq_entry.capacity());
        }
    }

    /// When the earliest stream expects its pending reservation to be fulfilled, in
    /// nanoseconds of `CLOCK_MONOTONIC` (the reactor's and the token bucket's clock);
    /// `i64::MAX` when none is pending.
    pub fn next_pending_aio(&self) -> ClockTypeTimePoint {
        let mut next = ClockTypeTimePoint::MAX;

        for s in self._streams.iter() {
            next = next.min(s.next_pending_aio());
        }

        next
    }

    /// Sets the shares of class `pc`, creating the class if needed.
    ///
    /// # Errors
    /// A metric registration error of a new class.
    ///
    /// # Safety
    /// As `IoQueue::find_or_create_class`.
    pub unsafe fn update_shares_for_class(
        this: *mut Self,
        pc: PriorityClass,
        new_shares: u32,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the contract above.
        let pclass = unsafe { Self::find_or_create_class(this, pc)? };
        // SAFETY: `pclass` is owned by `_priority_classes`; short raw places.
        unsafe {
            (*pclass).update_shares(new_shares);
            let cid = (*pclass).fq_class();
            for s in (*this)._streams.iter_mut() {
                s.update_shares_for_class(cid, new_shares);
            }
        }
        Ok(())
    }

    /// Sets the bandwidth limit of class `pc`, creating the class if needed; the future
    /// fails when the class cannot be created or the rate is too large.
    ///
    /// # Safety
    /// As `IoQueue::find_or_create_class`.
    pub unsafe fn update_bandwidth_for_class(
        this: *mut Self,
        pc: PriorityClass,
        new_bandwidth: u64,
    ) -> Future<()> {
        // SAFETY: the contract above; a short shared access.
        let group: &IoGroup = unsafe { &(*this)._group };
        if group._allocated_on == crate::engine::core::shard_id::this_shard_id() {
            // SAFETY: forwarded.
            let pclass = crate::engine::core::reactor::fut_try!(unsafe {
                Self::find_or_create_class(this, pc)
            });
            // SAFETY: as above.
            crate::engine::core::reactor::fut_try!(unsafe {
                (*pclass).update_bandwidth(new_bandwidth)
            });
        }
        make_ready_future(())
    }

    /// Re-registers the metrics of class `pc` under its scheduling group's new name.
    pub fn rename_priority_class(&mut self, pc: PriorityClass, new_name: Sstring) {
        let id = pc.id() as usize;
        if self._priority_classes.len() > id
            && let Some(p) = self._priority_classes[id].as_ref().map(NodePtr::as_ptr)
        {
            // Ignore this error: a class already created with the new name may be
            // renamed again, which fails with a double registration.
            // SAFETY: `p` is the class's node pointer, owned by `_priority_classes`,
            // which register_stats does not touch.
            let _ = unsafe { self.register_stats(&new_name, p) };
        }
    }

    /// Stops dispatching class `cid` in every stream (its bandwidth bucket ran short).
    ///
    /// # Safety
    /// `this` is the queue's root pointer; no reference to the queue or to its fair queues
    /// is live (this runs from inside a fair queue's dispatch).
    pub unsafe fn throttle_priority_class(this: *mut Self, cid: ClassId) {
        // SAFETY: the contract above; each fair queue is reborrowed for one call.
        unsafe {
            for i in 0..StaticVector::len_of(std::ptr::addr_of!((*this)._streams)) {
                (*Self::stream_ptr(this, i)).unplug_class(cid);
            }
        }
    }

    /// Resumes dispatching class `cid` in every stream.
    ///
    /// # Safety
    /// As [`IoQueue::throttle_priority_class`] (this runs from the replenish timer).
    pub unsafe fn unthrottle_priority_class(this: *mut Self, cid: ClassId) {
        // SAFETY: as above.
        unsafe {
            for i in 0..StaticVector::len_of(std::ptr::addr_of!((*this)._streams)) {
                (*Self::stream_ptr(this, i)).plug_class(cid);
            }
        }
    }

    /// The number of requests queued and not yet dispatched (deprecated).
    #[deprecated(
        note = "I/O queue users should not track individual requests, but resources (weight, size) passing through the queue"
    )]
    pub fn queued_requests(&self) -> usize {
        self._queued_requests
    }

    /// How many requests are sent to the disk but not yet returned (deprecated).
    #[deprecated(
        note = "I/O queue users should not track individual requests, but resources (weight, size) passing through the queue"
    )]
    pub fn requests_currently_executing(&self) -> usize {
        self._requests_executing
    }

    /// The queue's device id.
    #[inline]
    pub fn id(&self) -> u32 {
        self._id
    }

    /// The device configuration.
    #[inline]
    pub fn get_config(&self) -> &IoQueueConfig {
        &self._group._config
    }

    /// The device's mount point.
    #[inline]
    pub fn mountpoint(&self) -> String {
        self.get_config().mountpoint.clone()
    }
}

impl Drop for IoQueue {
    fn drop(&mut self) {
        // It is illegal to stop the I/O queue with pending requests. A gate could guarantee
        // that, but it is not needed: the queue is dropped only after the reactor stops,
        // when no more tasks run. If that ever changes, this has to change.
        engine_assert!(self._queued_requests == 0);
        for pc_data in self._priority_classes.iter().flatten() {
            // SAFETY: the class is live in its node; a short shared access.
            let cid = unsafe { (*pc_data.as_ptr()).fq_class() };
            for s in self._streams.iter_mut() {
                s.unregister_priority_class(cid);
            }
        }
        self._priority_classes.clear();
    }
}

/// The shares and name of the scheduling group behind priority class `pc`.
pub fn get_class_info(pc: IoPriorityClassId) -> (u32, Sstring) {
    let sg = crate::engine::core::scheduling::internal::scheduling_group_from_index(pc);
    (sg.get_shares() as u32, sg.name().clone())
}

/// The per-device state shared by the I/O queues of all shards: configuration, fair groups
/// (one per stream), maximum request lengths and the classes' bandwidth buckets.
pub struct IoGroup {
    pub(crate) _config: IoQueueConfig,
    pub(crate) _max_request_length: [Cell<usize>; 2],
    pub(crate) _fgs: Vec<FairGroup>,
    _priority_classes: std::sync::Mutex<Vec<Option<Box<IoGroupPriorityClassData>>>>,
    pub(crate) _allocated_on: ShardId,
}

impl IoGroup {
    /// The cut-off of the search for the maximum request length: the largest power of 2
    /// whose capacity is within the maximum capacity is looked for up to this length, which is
    /// the maximum request length when the search gets past it.
    pub const REQUEST_LENGTH_LIMIT: usize = 16 << 20; // 16 MiB

    /// The maximum length of a request of direction `dnl_idx` (0: write, 1: read); longer
    /// requests are split.
    #[inline]
    pub fn max_request_length(&self, dnl_idx: usize) -> usize {
        self._max_request_length[dnl_idx].get()
    }
}

// SAFETY: an io_group is shared by the shards of a NUMA node; its mutable state is the
// priority class table, guarded by the lock, and the token buckets, which are atomic.
// `_max_request_length` is written only while the group is constructed.
unsafe impl Sync for IoGroup {}
// SAFETY: as above.
unsafe impl Send for IoGroup {}

impl IoGroup {
    fn make_fair_group_config(qcfg: &IoQueueConfig) -> FairGroupConfig {
        let mut cfg = FairGroupConfig {
            label: Sstring::from(format!("io-queue-{}", qcfg.id)),
            ..FairGroupConfig::default()
        };
        let min_weight =
            IoQueue::READ_REQUEST_BASE_COUNT.min(qcfg.disk_req_write_to_read_multiplier) as f64;
        let min_size =
            IoQueue::READ_REQUEST_BASE_COUNT.min(qcfg.disk_blocks_write_to_read_multiplier) as f64;
        cfg.min_tokens =
            min_weight / qcfg.req_count_rate as f64 + min_size / qcfg.blocks_count_rate as f64;
        let limit_min_weight =
            IoQueue::READ_REQUEST_BASE_COUNT.max(qcfg.disk_req_write_to_read_multiplier) as f64;
        let limit_min_size = (IoQueue::READ_REQUEST_BASE_COUNT
            .max(qcfg.disk_blocks_write_to_read_multiplier) as u64
            * qcfg.block_count_limit_min) as f64;
        cfg.limit_min_tokens = limit_min_weight / qcfg.req_count_rate as f64
            + limit_min_size / qcfg.blocks_count_rate as f64;
        cfg.rate_limit_duration = qcfg.rate_limit_duration;
        cfg
    }

    /// The fair groups; public only for the queue tests.
    #[doc(hidden)]
    pub fn fair_groups(&self) -> &[FairGroup] {
        &self._fgs
    }

    /// The latency goal: the duration of the first fair group's bucket limit.
    pub fn io_latency_goal(&self) -> Duration {
        self._fgs[0].rate_limit_duration()
    }

    /// A group for `nr_queues` shard queues configured by `io_cfg`; it also computes the
    /// longest request whose cost fits in the bucket limit.
    ///
    /// # Errors
    /// A runtime error when the fair group cannot be configured, or when even a one-block
    /// request exceeds the bucket limit.
    pub fn new(io_cfg: IoQueueConfig, nr_queues: u32) -> Result<Self, ExceptionPtr> {
        let fg_cfg = Self::make_fair_group_config(&io_cfg);
        let mut fgs = Vec::with_capacity(2);
        fgs.push(FairGroup::new(fg_cfg.clone(), nr_queues)?);
        if io_cfg.duplex {
            fgs.push(FairGroup::new(fg_cfg, nr_queues)?);
        }
        let this = IoGroup {
            _config: io_cfg,
            _max_request_length: [
                Cell::new(Self::REQUEST_LENGTH_LIMIT), // write
                Cell::new(Self::REQUEST_LENGTH_LIMIT), // read
            ],
            _fgs: fgs,
            _priority_classes: std::sync::Mutex::new(Vec::new()),
            _allocated_on: crate::engine::core::shard_id::this_shard_id(),
        };

        let goal = this.io_latency_goal();
        let lvl = if goal.as_secs_f64() > 1.1 * this._config.rate_limit_duration.as_secs_f64() {
            LogLevel::Warn
        } else {
            LogLevel::Debug
        };
        engine_logger().log(
            lvl,
            FormatInfo::new("IO queue uses {:.2}ms latency goal for {}"),
            format_args!(
                "IO queue uses {:.2}ms latency goal for {}",
                goal.as_secs_f64() * 1000.0,
                this._config.mountpoint
            ),
        );

        /*
         * The maximum request size shouldn't result in the capacity that would
         * be larger than the group's replenisher limit.
         *
         * To get the correct value check the 2^N sizes and find the largest one
         * with little enough capacity. FIXME: requests should compute capacities
         * directly so this math would go away.
         */
        let update_max_size = |idx: i32| -> Result<(), ExceptionPtr> {
            let g_idx = if this._config.duplex { idx as usize } else { 0 };
            let fg = &this._fgs[g_idx];
            let max_cap = fg.maximum_capacity();
            let capacity_of = |len: usize| {
                FairGroup::tokens_capacity(internal::request_tokens(
                    IoDirectionAndLength::new(idx, len),
                    &this._config,
                ))
            };
            let mut shift: u32 = 0;
            loop {
                let req_length = 1usize << (shift + IoQueue::BLOCK_SIZE_SHIFT);
                let cap = capacity_of(req_length);
                if cap > max_cap {
                    if shift == 0 {
                        return Err(ExceptionPtr::new(RuntimeError::new(
                            "IO-group limits are too low".to_string(),
                        )));
                    }
                    // The current shift gives the biggest power of 2 request length whose
                    // capacity is still within max_cap, but REQUEST_LENGTH_LIMIT may be a better
                    // fit if its capacity is closer to max_cap.
                    let fitting_req_length = 1usize << (shift - 1 + IoQueue::BLOCK_SIZE_SHIFT);
                    let fitting_cap = capacity_of(fitting_req_length);
                    let cap_limit = capacity_of(Self::REQUEST_LENGTH_LIMIT);
                    let length = if fitting_cap < cap_limit && cap_limit < max_cap {
                        Self::REQUEST_LENGTH_LIMIT
                    } else {
                        fitting_req_length
                    };
                    this._max_request_length[idx as usize].set(length);
                    break;
                }
                // Stop once the request length passes the limit: with huge configured rates
                // the capacity may stay 0 for every length, and the search would never end.
                // The maximum request length keeps its default, REQUEST_LENGTH_LIMIT.
                if req_length > Self::REQUEST_LENGTH_LIMIT {
                    engine_logger().info(
                        FormatInfo::new("IO queue was unable to find a suitable maximum request length, the search was cut-off early at: {}MB"),
                        format_args!(
                            "IO queue was unable to find a suitable maximum request length, the search was cut-off early at: {}MB",
                            Self::REQUEST_LENGTH_LIMIT >> 20
                        ),
                    );
                    break;
                }
                shift += 1;
            }
            Ok(())
        };

        update_max_size(IO_DIRECTION_WRITE)?;
        update_max_size(IO_DIRECTION_READ)?;
        Ok(this)
    }

    fn find_or_create_class(&self, pc: PriorityClass) -> *mut IoGroupPriorityClassData {
        let mut classes = self._priority_classes.lock().unwrap_or_else(|e| e.into_inner());

        let id = pc.id() as usize;
        if id >= classes.len() {
            classes.resize_with(id + 1, || None);
        }
        if classes[id].is_none() {
            classes[id] = Some(Box::new(IoGroupPriorityClassData::new()));
        }

        // The class is boxed and never removed, so its address is stable.
        classes[id]
            .as_deref_mut()
            .map_or(std::ptr::null_mut(), |p| p as *mut IoGroupPriorityClassData)
    }
}

/// A fixed-capacity vector: up to `N` values stored inline, in order, with their count. The values never move while the vector does not, so pointers into it stay
/// valid; they are dropped in order with the vector.
struct StaticVector<T, const N: usize> {
    _len: usize,
    _items: [std::mem::MaybeUninit<T>; N],
}

impl<T, const N: usize> StaticVector<T, N> {
    /// An empty vector.
    #[inline]
    fn new() -> Self {
        StaticVector { _len: 0, _items: [const { std::mem::MaybeUninit::uninit() }; N] }
    }

    /// Appends `v`.
    ///
    /// # Panics
    /// Fails an assertion past the capacity; the callers push at most `N` values (a fixed
    /// number of streams).
    #[inline]
    fn push(&mut self, v: T) {
        engine_assert!(self._len < N, "static_vector capacity exceeded");
        // SAFETY: `_len < N` (asserted), and that slot is not initialized.
        unsafe { self._items.get_unchecked_mut(self._len) }.write(v);
        self._len += 1;
    }

    /// The number of values of the vector at `this`, read as a place of `this`.
    ///
    /// # Safety
    /// `this` points to a live vector.
    #[inline]
    unsafe fn len_of(this: *const Self) -> usize {
        // SAFETY: the contract above; one field read.
        unsafe { (*this)._len }
    }

    /// The `i`-th slot of the vector at `this`, as a place of `this` (no reference formed).
    ///
    /// # Safety
    /// `this` points to a live vector and `i < N`.
    #[inline]
    unsafe fn slot_ptr(this: *mut Self, i: usize) -> *mut T {
        // SAFETY: the contract above; `MaybeUninit<T>` has the layout of `T`.
        unsafe { std::ptr::addr_of_mut!((*this)._items).cast::<T>().add(i) }
    }
}

impl<T, const N: usize> std::ops::Deref for StaticVector<T, N> {
    type Target = [T];
    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: the first `_len` slots are initialized.
        unsafe { std::slice::from_raw_parts(self._items.as_ptr().cast::<T>(), self._len) }
    }
}

impl<T, const N: usize> std::ops::DerefMut for StaticVector<T, N> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: the first `_len` slots are initialized.
        unsafe { std::slice::from_raw_parts_mut(self._items.as_mut_ptr().cast::<T>(), self._len) }
    }
}

impl<T, const N: usize> Drop for StaticVector<T, N> {
    fn drop(&mut self) {
        // SAFETY: the first `_len` slots are initialized and dropped once, here.
        unsafe { std::ptr::drop_in_place(&mut **self as *mut [T]) };
    }
}
