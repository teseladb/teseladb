//! Proportional-share I/O scheduling.
//!
//! A [`FairQueue`] lives on each shard's I/O queue and serves the requests of several
//! priority classes in proportion to their shares: the class with the least accumulated
//! (share-normalized) capacity is served first, so classes that lag behind catch up within
//! a time window `tau`. Every shard's queue draws capacity tokens from one [`FairGroup`]
//! per device, a token bucket shared across shards that keeps the device within its
//! configured bandwidth and IOPS limits.
//!
//! Request costs are [`CapacityT`] tokens in fixed point (see
//! [`FairGroup::FIXED_POINT_FACTOR`]). Queued requests are [`FairQueueEntry`]s linked
//! intrusively into their class, so queueing allocates nothing; the entries are owned by
//! the caller and must not move while queued.

use std::collections::BinaryHeap;
use std::ptr::NonNull;
use std::time::Duration;

use crate::engine::core::future::{ExceptionPtr, RuntimeError};
use crate::engine::core::metrics;
use crate::engine::core::reactor_backend::engine_assert;
use crate::engine::core::sstring::Sstring;
use crate::engine::util::shared_token_bucket::internal::{
    Period, SharedTokenBucket, SteadyClock, TokenBucketClock,
};

/// Describes a request that passes through a [`FairQueue`].
///
/// A ticket is a `weight` and a `size`. For example, a request can have weight 1 and size
/// 16 KiB; if the queue accepts one such request per second, it sustains 1 IOPS at 16 KiB/s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FairQueueTicket {
    /// The total weight of these requests for capacity purposes (IOPS).
    _weight: u32,
    /// The total effective size of these requests.
    _size: u32,
}

const _: () = assert!(
    std::mem::size_of::<FairQueueTicket>() == std::mem::size_of::<u64>(),
    "unexpected fair_queue_ticket size"
);

impl FairQueueTicket {
    /// A ticket of `weight` and `size`.
    #[inline]
    pub const fn new(weight: u32, size: u32) -> Self {
        FairQueueTicket { _weight: weight, _size: size }
    }

    /// Whether the ticket represents a non-zero quantity: at least one of weight and size
    /// is non-zero.
    #[inline]
    pub fn as_bool(&self) -> bool {
        (self._weight > 0) || (self._size > 0)
    }

    /// Whether both weight and size are non-zero.
    #[inline]
    pub fn is_non_zero(&self) -> bool {
        (self._weight > 0) && (self._size > 0)
    }

    /// The value of this ticket normalized along the base axis `denominator`:
    /// `weight / denominator.weight + size / denominator.size`.
    ///
    /// Weight or size weighs more depending on which dimension of the denominator is
    /// relatively higher. This ticket may have one quantity at zero, in which case only the
    /// other counts; the axis must have both quantities non-zero.
    #[inline]
    pub fn normalize(&self, denominator: FairQueueTicket) -> f32 {
        self._weight as f32 / denominator._weight as f32
            + self._size as f32 / denominator._size as f32
    }
}

impl std::ops::Add for FairQueueTicket {
    type Output = FairQueueTicket;
    #[inline]
    fn add(self, desc: FairQueueTicket) -> FairQueueTicket {
        FairQueueTicket::new(
            self._weight.wrapping_add(desc._weight),
            self._size.wrapping_add(desc._size),
        )
    }
}

impl std::ops::Sub for FairQueueTicket {
    type Output = FairQueueTicket;
    #[inline]
    fn sub(self, desc: FairQueueTicket) -> FairQueueTicket {
        FairQueueTicket::new(
            self._weight.wrapping_sub(desc._weight),
            self._size.wrapping_sub(desc._size),
        )
    }
}

impl std::ops::AddAssign for FairQueueTicket {
    /// Adds the weight and size of `desc` (wrapping).
    #[inline]
    fn add_assign(&mut self, desc: FairQueueTicket) {
        self._weight = self._weight.wrapping_add(desc._weight);
        self._size = self._size.wrapping_add(desc._size);
    }
}

impl std::ops::SubAssign for FairQueueTicket {
    /// Subtracts the weight and size of `desc` (wrapping).
    #[inline]
    fn sub_assign(&mut self, desc: FairQueueTicket) {
        self._weight = self._weight.wrapping_sub(desc._weight);
        self._size = self._size.wrapping_sub(desc._size);
    }
}

impl std::fmt::Display for FairQueueTicket {
    fn fmt(&self, os: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(os, "{}:{}", self._weight, self._size)
    }
}

/// For each dimension, how far the rover `a` is ahead of `b` (wrapping arithmetic); zero
/// where `a` is behind.
#[inline]
pub fn wrapping_difference(a: &FairQueueTicket, b: &FairQueueTicket) -> FairQueueTicket {
    FairQueueTicket::new(
        (a._weight.wrapping_sub(b._weight) as i32).max(0) as u32,
        (a._size.wrapping_sub(b._size) as i32).max(0) as u32,
    )
}

/// The intrusive link of a [`FairQueueEntry`] in its class's list.
#[repr(C)]
#[derive(Debug)]
pub struct SlistMemberHook {
    next: *mut FairQueueEntry,
}

impl Default for SlistMemberHook {
    fn default() -> Self {
        SlistMemberHook { next: std::ptr::null_mut() }
    }
}

/// A request queued in a [`FairQueue`]: its cost and its intrusive list link. The owner
/// embeds it in the request and keeps it in place while it is queued.
#[repr(C)]
#[derive(Debug)]
pub struct FairQueueEntry {
    _capacity: u64,
    _hook: SlistMemberHook,
}

/// The tokens an entry needs to be dispatched, in normalized form: converted from floating
/// point to fixed point and scaled to the group's token bucket period.
pub type CapacityT = u64;

impl FairQueueEntry {
    /// An unlinked entry costing `c`.
    #[inline]
    pub fn new(c: CapacityT) -> Self {
        FairQueueEntry { _capacity: c, _hook: SlistMemberHook::default() }
    }

    /// The entry's cost.
    #[inline]
    pub fn capacity(&self) -> CapacityT {
        self._capacity
    }
}

/// An intrusive singly linked list of entries with a pointer to its last element, so both
/// appending and popping the front are O(1).
#[derive(Debug)]
pub struct ContainerListT {
    head: *mut FairQueueEntry,
    tail: *mut FairQueueEntry,
}

const _: () = assert!(
    std::mem::size_of::<FairQueueEntry>() <= 3 * std::mem::size_of::<usize>(),
    "unexpected fair_queue_entry::_hook size"
);
const _: () = assert!(
    std::mem::size_of::<ContainerListT>() == 2 * std::mem::size_of::<usize>(),
    "unexpected priority_class::_queue size"
);

impl Default for ContainerListT {
    fn default() -> Self {
        ContainerListT { head: std::ptr::null_mut(), tail: std::ptr::null_mut() }
    }
}

impl ContainerListT {
    /// Whether the list is empty.
    #[inline]
    pub fn empty(&self) -> bool {
        self.head.is_null()
    }
    /// Appends `ent`.
    ///
    /// # Safety
    /// `ent` is a live, unlinked entry that neither moves nor is freed while it is linked,
    /// with the provenance of its owner's pointer (for an entry embedded in a request, the
    /// `addr_of_mut!` of the field on the pointer to the whole request): the list stores it
    /// as given.
    #[inline]
    pub unsafe fn push_back(&mut self, ent: *mut FairQueueEntry) {
        // SAFETY: the contract above.
        unsafe { (*ent)._hook.next = std::ptr::null_mut() };
        if self.tail.is_null() {
            self.head = ent;
        } else {
            // SAFETY: `tail` is a linked entry, alive while it is queued.
            unsafe { (*self.tail)._hook.next = ent };
        }
        self.tail = ent;
    }
    /// The first entry (null if the list is empty).
    #[inline]
    pub fn front(&self) -> *mut FairQueueEntry {
        self.head
    }
    /// Unlinks the first entry; the list must not be empty.
    #[inline]
    pub fn pop_front(&mut self) {
        let h = self.head;
        // SAFETY: called on a non-empty list; `h` is a linked entry.
        self.head = unsafe { (*h)._hook.next };
        if self.head.is_null() {
            self.tail = std::ptr::null_mut();
        }
        // SAFETY: as above.
        unsafe { (*h)._hook.next = std::ptr::null_mut() };
    }
}

/// The rate resolution of the token bucket: rates are per millisecond.
pub type RateResolution = crate::engine::util::shared_token_bucket::internal::Milli;

/// The group's token bucket, shared by the shards, with uncapped release.
pub type TokenBucketT = SharedTokenBucket<
    RateResolution,
    crate::engine::util::shared_token_bucket::internal::RoversUncapped,
>;

/// A time point of the token bucket clock: nanoseconds of `CLOCK_MONOTONIC`.
pub type ClockTypeTimePoint = i64;

/// Converts a token bucket duration (fractional units of period `P`) to whole microseconds,
/// truncated toward zero.
#[inline]
pub(crate) fn duration_cast_micros<P: Period>(
    d: crate::engine::util::shared_token_bucket::internal::RateResolution<P>,
) -> Duration {
    Duration::from_micros((d.count() * P::NUM / P::DEN * 1e6) as u64)
}

/// Configuration of a [`FairGroup`].
#[derive(Clone, Debug)]
pub struct FairGroupConfig {
    /// Name used in metrics and messages.
    pub label: Sstring,
    /// The smallest request cost, in tokens, the upper layer submits; it is the
    /// replenish threshold of the bucket.
    ///
    /// The upper layer may submit larger costs, and the queue must accept at least those
    /// up to [`FairGroupConfig::limit_min_tokens`] (larger ones too, of course).
    pub min_tokens: f64,
    /// The cost, in tokens, the bucket limit must at least hold.
    pub limit_min_tokens: f64,
    /// How long the bucket accumulates tokens for: its limit is the refill over this
    /// duration (default 1 ms).
    pub rate_limit_duration: Duration,
}

impl Default for FairGroupConfig {
    fn default() -> Self {
        FairGroupConfig {
            label: Sstring::new(),
            min_tokens: 0.0,
            limit_min_tokens: 0.0,
            rate_limit_duration: Duration::from_millis(1),
        }
    }
}

/// A group of fair queues sharing one device's capacity.
///
/// One or more fair queues attach to a group. On machines with a big number of shards
/// (big meaning that when all shards submit requests together the disk cannot dispatch
/// them efficiently, either because of the number of requests or because their total size
/// exceeds the disk throughput), the queues use the group to borrow and lend the capacity
/// needed to dispatch requests.
pub struct FairGroup {
    /*
     * The dF/dt <= K limitation is managed by the modified token bucket
     * algo where tokens are ticket.normalize(cost_capacity), the refill
     * rate is K.
     *
     * The token bucket algorithm must have the limit on the number of tokens
     * accumulated. Here it's configured so that it accumulates for the
     * latency_goal duration.
     *
     * The replenish threshold is the minimal number of tokens to put back.
     * It's reserved for future use to reduce the load on the replenish
     * timestamp.
     *
     * The timestamp, in turn, is the time when the bucket was replenished
     * last. Every time a shard tries to get tokens from bucket it first
     * tries to convert the time that had passed since this timestamp
     * into more tokens in the bucket.
     */
    _token_bucket: TokenBucketT,
    _per_tick_threshold: CapacityT,
}

impl FairGroup {
    /*
     * tldr; The math
     *
     *    Bw, Br -- write/read bandwidth (bytes per second)
     *    Ow, Or -- write/read iops (ops per second)
     *
     *    xx_max -- their maximum values (configured)
     *
     * Throttling formula:
     *
     *    Bw/Bw_max + Br/Br_max + Ow/Ow_max + Or/Or_max <= K
     *
     * where K is the scalar value <= 1.0 (also configured)
     *
     * Bandwidth is bytes time derivative, iops is ops time derivative, i.e.
     * Bx = d(bx)/dt, Ox = d(ox)/dt. Then the formula turns into
     *
     *   d(bw/Bw_max + br/Br_max + ow/Ow_max + or/Or_max)/dt <= K
     *
     * Fair queue tickets are {w, s} weight-size pairs that are
     *
     *   s = read_base_count * br, for reads
     *       Br_max/Bw_max * read_base_count * bw, for writes
     *
     *   w = read_base_count, for reads
     *       Or_max/Ow_max * read_base_count, for writes
     *
     * Thus the formula turns into
     *
     *   d(sum(w/W + s/S))/dr <= K
     *
     * where {w, s} is the ticket value if a request and sum summarizes the
     * ticket values from all the requests seen so far, {W, S} is the ticket
     * value that corresponds to a virtual summary of Or_max requests of
     * Br_max size total.
     */

    /*
     * The normalization results in a float of the 2^-30 seconds order of
     * magnitude. Not to invent float point atomic arithmetics, the result
     * is converted to an integer by multiplying by a factor that's large
     * enough to turn these values into a non-zero integer.
     *
     * Also, the rates in bytes/sec when adjusted by io-queue according to
     * multipliers become too large to be stored in 32-bit ticket value.
     * Thus the rate resolution is applied. The t.bucket is configured with a
     * time period for which the speeds from F (in above formula) are taken.
     */
    /// The factor that converts normalized ticket values (floats of the order of 2^-30) to
    /// integer capacity.
    pub const FIXED_POINT_FACTOR: f32 = (1u32 << 24) as f32;

    /// Converts an internal capacity value back into real tokens.
    #[inline]
    pub fn capacity_tokens(cap: CapacityT) -> f64 {
        cap as f64
            / Self::FIXED_POINT_FACTOR as f64
            / TokenBucketT::rate_cast(Duration::from_secs(1)).count()
    }

    /// Converts floating-point tokens into token bucket capacity.
    #[inline]
    pub fn tokens_capacity(tokens: f64) -> CapacityT {
        (tokens
            * TokenBucketT::rate_cast(Duration::from_secs(1)).count()
            * Self::FIXED_POINT_FACTOR as f64) as CapacityT
    }

    /// How long the bucket takes to accumulate `cap`.
    #[inline]
    pub fn capacity_duration(
        &self,
        cap: CapacityT,
    ) -> crate::engine::util::shared_token_bucket::internal::RateResolution<RateResolution> {
        self._token_bucket.duration_for(cap)
    }

    /// A group for `nr_queues` queues; each may grab up to a `1 / nr_queues` share of the
    /// bucket limit per tick.
    ///
    /// # Errors
    /// A runtime error when the bucket's replenish threshold ends up below `min_tokens`
    /// (the bucket limit is too low).
    pub fn new(cfg: FairGroupConfig, nr_queues: u32) -> Result<Self, ExceptionPtr> {
        let limit = ((Self::FIXED_POINT_FACTOR as f64
            * TokenBucketT::rate_cast(cfg.rate_limit_duration).count())
            as CapacityT)
            .max(Self::tokens_capacity(cfg.limit_min_tokens));
        let token_bucket = TokenBucketT::new(
            Self::FIXED_POINT_FACTOR as CapacityT,
            limit,
            Self::tokens_capacity(cfg.min_tokens),
            true,
        );
        let per_tick_threshold = token_bucket.limit() / nr_queues as CapacityT;
        if Self::tokens_capacity(cfg.min_tokens) > token_bucket.threshold() {
            return Err(ExceptionPtr::new(RuntimeError::new(
                "Fair-group replenisher limit is lower than threshold".to_string(),
            )));
        }
        Ok(FairGroup { _token_bucket: token_bucket, _per_tick_threshold: per_tick_threshold })
    }

    /// The bucket limit: the most capacity that can be grabbed at once.
    #[inline]
    pub fn maximum_capacity(&self) -> CapacityT {
        self._token_bucket.limit()
    }

    /// The most capacity one queue reserves per tick.
    #[inline]
    pub fn per_tick_grab_threshold(&self) -> CapacityT {
        self._per_tick_threshold
    }

    /// Reserves `cap` from the bucket tail and returns the new tail.
    ///
    /// # Panics
    /// Asserts that `cap` does not exceed the bucket limit.
    #[inline]
    pub fn grab_capacity(&self, cap: CapacityT) -> CapacityT {
        engine_assert!(cap <= self._token_bucket.limit());
        self._token_bucket.grab(cap)
    }

    /// When the bucket was last replenished.
    #[inline]
    pub fn replenished_ts(&self) -> ClockTypeTimePoint {
        self._token_bucket.replenished_ts()
    }

    /// Returns `cap` tokens to the bucket.
    #[inline]
    pub fn refund_tokens(&self, cap: CapacityT) {
        self._token_bucket.refund(cap);
    }

    /// Refills the bucket for the time elapsed up to `now`.
    #[inline]
    pub fn replenish_capacity(&self, now: ClockTypeTimePoint) {
        self._token_bucket.replenish(now);
    }

    /// Refills the bucket if enough time has passed since `local_ts` to accumulate at least
    /// the replenish threshold, and then advances `local_ts`.
    #[inline]
    pub fn maybe_replenish_capacity(&self, local_ts: &mut ClockTypeTimePoint) {
        let now = SteadyClock::now();
        // The clock is monotonic, so `now - local_ts` is not negative.
        let extra =
            self._token_bucket.accumulated_in(Duration::from_nanos((now - *local_ts) as u64));
        if extra >= self._token_bucket.threshold() {
            *local_ts = now;
            self.replenish_capacity(now);
        }
    }

    /// How many tokens the bucket head still lacks to reach `from`.
    #[inline]
    pub fn capacity_deficiency(&self, from: CapacityT) -> CapacityT {
        self._token_bucket.deficiency(from)
    }

    /// The duration the bucket limit corresponds to at the refill rate.
    pub fn rate_limit_duration(&self) -> Duration {
        // The rate is per millisecond.
        Duration::from_secs_f64(
            self._token_bucket.limit() as f64 / self._token_bucket.rate() as f64 / 1000.0,
        )
    }

    /// The shared token bucket.
    #[inline]
    pub fn token_bucket(&self) -> &TokenBucketT {
        &self._token_bucket
    }
}

/// The operation parameters of a [`FairQueue`].
#[derive(Clone, Debug)]
pub struct FairQueueConfig {
    /// Name used in metrics and messages.
    pub label: Sstring,
    /// The time window over which a class that was idle may catch up: a newly active class
    /// is not let to monopolize the disk for more than this (default 5 ms).
    pub tau: Duration,
}

impl Default for FairQueueConfig {
    fn default() -> Self {
        FairQueueConfig { label: Sstring::new(), tau: Duration::from_millis(5) }
    }
}

/// Identifier of a priority class within a queue.
pub type ClassId = u32;
/// Signed capacity, for deviations that may be negative.
pub type SignedCapacityT = i64;

/// A priority class of a [`FairQueue`]: its shares, accumulated consumption and queue of
/// entries.
pub struct PriorityClassData {
    _shares: u32,
    _accumulated: CapacityT,
    _pure_accumulated: CapacityT,
    _queue: ContainerListT,
    _queued: bool,
    _plugged: bool,
    _activations: u32,
}

impl PriorityClassData {
    /// A class with `shares` shares (at least 1).
    pub fn new(shares: u32) -> Self {
        PriorityClassData {
            _shares: shares.max(1),
            _accumulated: 0,
            _pure_accumulated: 0,
            _queue: ContainerListT::default(),
            _queued: false,
            _plugged: true,
            _activations: 0,
        }
    }

    /// Sets the shares (at least 1).
    #[inline]
    pub fn update_shares(&mut self, shares: u32) {
        self._shares = shares.max(1);
    }
}

/// A class pointer ordered so that the top of the max-heap is the class with the least
/// accumulated capacity.
#[derive(Clone, Copy)]
struct PriorityClassPtr(*mut PriorityClassData);

impl PriorityClassPtr {
    #[inline]
    fn acc(&self) -> CapacityT {
        // SAFETY: the heap holds pointers to classes owned by `_priority_classes`.
        unsafe { (*self.0)._accumulated }
    }
}

impl PartialEq for PriorityClassPtr {
    fn eq(&self, o: &Self) -> bool {
        self.acc() == o.acc()
    }
}
impl Eq for PriorityClassPtr {}
impl PartialOrd for PriorityClassPtr {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for PriorityClassPtr {
    #[inline]
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // A class with more accumulated capacity has lower priority.
        o.acc().cmp(&self.acc())
    }
}

/// The heap of active classes, least accumulated capacity on top.
struct PriorityQueue {
    c: BinaryHeap<PriorityClassPtr>,
}

impl PriorityQueue {
    fn reserve(&mut self, len: usize) {
        self.c.reserve(len.saturating_sub(self.c.len()));
    }
    fn assert_enough_capacity(&self) {
        engine_assert!(self.c.len() < self.c.capacity());
    }
    #[inline]
    fn push(&mut self, p: *mut PriorityClassData) {
        self.c.push(PriorityClassPtr(p));
    }
    #[inline]
    fn pop(&mut self) {
        self.c.pop();
    }
    #[inline]
    fn top(&self) -> *mut PriorityClassData {
        self.c.peek().map_or(std::ptr::null_mut(), |p| p.0)
    }
    #[inline]
    fn empty(&self) -> bool {
        self.c.is_empty()
    }
}

// _pending represents a reservation of tokens from the bucket.
//
// In the "dispatch timeline" defined by the growing bucket head of the group,
// tokens in the range [_pending.head - cap, _pending.head) belong
// to this queue.
//
// For example, if:
//    _group._token_bucket.head == 300
//    _pending.head == 700
//    _pending.cap == 500
// then the reservation is [200, 700), 100 tokens are ready to be dispatched by this queue,
// and another 400 tokens are going to be appear soon. (And after that, this queue
// will be able to make its next reservation).
#[derive(Clone, Copy, Debug, Default)]
struct Pending {
    head: CapacityT,
    cap: CapacityT,
}

// Shaves off the fulfilled frontal part from `_pending` (if any),
// and returns the fulfilled tokens in `ready_tokens`.
// Sets `our_turn_has_come` to the truth value of "`_pending` is empty or
// there are no unfulfilled reservations (from other shards) earlier than `_pending`".
//
// Assumes that `_group.maybe_replenish_capacity()` was called recently.
#[derive(Clone, Copy, Debug)]
struct ReapResult {
    ready_tokens: CapacityT,
    our_turn_has_come: bool,
}

/// A fair queue: multiple request producers queue requests, which are served in
/// proportion to their classes' shares.
///
/// To each request, a weight can also be associated. A request of weight 1 will consume
/// 1 share. Higher weights for a request will consume a proportionally higher amount of
/// shares.
///
/// The user registers several priority classes, each with its shares.
///
/// Internally, each priority class may keep a separate queue of requests.
/// Requests pertaining to a class can go through even if they are over its
/// share limit, provided that the other classes have empty queues.
///
/// When the classes that lag behind start seeing requests, the fair queue will serve
/// them first, until balance is restored. This balancing is expected to happen within
/// a certain time window that obeys an exponential decay.
pub struct FairQueue {
    _config: FairQueueConfig,
    _group: *const FairGroup,
    _group_replenish: ClockTypeTimePoint,
    _resources_executing: FairQueueTicket,
    _resources_queued: FairQueueTicket,
    _handles: PriorityQueue,
    /// The classes by id. They are reached through the pointers kept here and in `_handles`, so they are owned as raw pointers made by
    /// `Box::into_raw` (and freed by `unregister_priority_class`), never through a `Box`
    /// reference that would invalidate those pointers.
    _priority_classes: Vec<Option<NonNull<PriorityClassData>>>,
    _nr_classes: usize,
    _last_accumulated: CapacityT,
    _pending: Pending,
    // Total capacity of all requests waiting in the queue.
    _queued_capacity: CapacityT,
}

impl FairQueue {
    #[inline]
    fn group(&self) -> &FairGroup {
        // SAFETY: by `new`'s contract the group outlives its queues.
        unsafe { &*self._group }
    }

    /// The class `cid`, unchecked in release builds: `cid` is a registered class.
    #[inline]
    fn pc(&self, cid: ClassId) -> *mut PriorityClassData {
        debug_assert!(
            self._priority_classes.get(cid as usize).is_some_and(|p| p.is_some()),
            "_priority_classes[cid]"
        );
        // SAFETY: the callers pass registered classes only (the io_queue registers a class
        // before it queues requests of it, and unregisters it after the last one).
        unsafe { self._priority_classes.get_unchecked(cid as usize).unwrap_unchecked().as_ptr() }
    }

    /// A fair queue with configuration `cfg`, drawing capacity from `group`.
    ///
    /// # Safety
    /// `group` outlives the queue.
    pub unsafe fn new(group: *const FairGroup, cfg: FairQueueConfig) -> Self {
        FairQueue {
            _config: cfg,
            _group: group,
            _group_replenish: SteadyClock::now(),
            _resources_executing: FairQueueTicket::default(),
            _resources_queued: FairQueueTicket::default(),
            _handles: PriorityQueue { c: BinaryHeap::new() },
            _priority_classes: Vec::new(),
            _nr_classes: 0,
            _last_accumulated: 0,
            _pending: Pending::default(),
            _queued_capacity: 0,
        }
    }

    /// The queue's label.
    pub fn label(&self) -> Sstring {
        self._config.label.clone()
    }

    // The private class operations take the class as the pointer `_priority_classes` owns
    // (`pc()`), which the heap stores; a reference to the class is never kept across them.
    //
    // # Safety (all of them)
    // `pc` is a registered class of this queue, as returned by `pc()`, and no reference to it
    // is live.

    unsafe fn push_priority_class(&mut self, pc: *mut PriorityClassData) {
        // SAFETY: see above; short accesses to the class's fields.
        unsafe {
            engine_assert!((*pc)._plugged && !(*pc)._queued);
            self._handles.assert_enough_capacity();
            self._handles.push(pc);
            (*pc)._queued = true;
        }
    }

    unsafe fn push_priority_class_from_idle(&mut self, pc: *mut PriorityClassData) {
        // SAFETY: see above; `pc` is reborrowed only for this body, and `_handles` stores the
        // owner's pointer, not one taken from this reference.
        let pcr = unsafe { &mut *pc };
        if !pcr._queued {
            // Don't let the newcomer monopolize the disk for more than tau
            // duration. For this estimate how many capacity units can be
            // accumulated with the current class shares per rate resolution
            // and scale it up to tau.
            let max_deviation = ((FairGroup::FIXED_POINT_FACTOR / pcr._shares as f32) as f64
                * TokenBucketT::rate_cast(self._config.tau).count())
                as CapacityT;
            // On start this deviation can go to negative values, so not to
            // introduce extra if's for that short corner case, use signed
            // arithmetic and make sure the _accumulated value doesn't grow
            // over signed maximum (see overflow check below)
            pcr._accumulated =
                (self._last_accumulated.wrapping_sub(max_deviation) as SignedCapacityT)
                    .max(pcr._accumulated as SignedCapacityT) as CapacityT;
            self._handles.assert_enough_capacity();
            self._handles.push(pc);
            pcr._queued = true;
            pcr._activations += 1;
        }
    }

    // ATTN: This can only be called on pc that is from _handles.top()
    unsafe fn pop_priority_class(&mut self, pc: *mut PriorityClassData) {
        // SAFETY: see above.
        unsafe {
            engine_assert!((*pc)._queued);
            (*pc)._queued = false;
        }
        self._handles.pop();
    }

    unsafe fn plug_priority_class(&mut self, pc: *mut PriorityClassData) {
        // SAFETY: see above.
        unsafe {
            engine_assert!(!(*pc)._plugged);
            (*pc)._plugged = true;
            if !(*pc)._queue.empty() {
                self.push_priority_class_from_idle(pc);
            }
        }
    }

    /// Lets class `cid` be dispatched again; if it has queued requests it becomes active.
    pub fn plug_class(&mut self, cid: ClassId) {
        let p = self.pc(cid);
        // SAFETY: `p` is the class owned by `_priority_classes`, not otherwise borrowed.
        unsafe { self.plug_priority_class(p) };
    }

    unsafe fn unplug_priority_class(&mut self, pc: *mut PriorityClassData) {
        // SAFETY: see above.
        unsafe {
            engine_assert!((*pc)._plugged);
            (*pc)._plugged = false;
        }
    }

    /// Stops dispatching class `cid` (its requests stay queued) until it is plugged again.
    pub fn unplug_class(&mut self, cid: ClassId) {
        let p = self.pc(cid);
        // SAFETY: as in plug_class().
        unsafe { self.unplug_priority_class(p) };
    }

    fn reap_pending_capacity(&mut self) -> ReapResult {
        let mut result = ReapResult { ready_tokens: 0, our_turn_has_come: true };
        if self._pending.cap != 0 {
            let deficiency = self.group().capacity_deficiency(self._pending.head);
            result.our_turn_has_come = deficiency <= self._pending.cap;
            if result.our_turn_has_come {
                result.ready_tokens = self._pending.cap - deficiency;
                self._pending.cap = deficiency;
            }
        }
        result
    }

    // Replaces _pending with a new reservation starting at the current
    // group bucket tail.
    fn grab_capacity(&mut self, cap: CapacityT) {
        let want_head = self.group().grab_capacity(cap);
        self._pending = Pending { head: want_head, cap };
    }

    /// Registers priority class `id` with `shares` shares.
    ///
    /// # Panics
    /// Fails an assertion if `id` is already registered.
    pub fn register_priority_class(&mut self, id: ClassId, shares: u32) {
        let id = id as usize;
        if id >= self._priority_classes.len() {
            self._priority_classes.resize_with(id + 1, || None);
        } else {
            engine_assert!(self._priority_classes[id].is_none());
        }
        self._handles.reserve(self._nr_classes + 1);
        self._priority_classes[id] =
            Some(NonNull::from(Box::leak(Box::new(PriorityClassData::new(shares)))));
        self._nr_classes += 1;
    }

    /// Unregisters priority class `id`, which must have no queued requests.
    pub fn unregister_priority_class(&mut self, id: ClassId) {
        let pclass = self._priority_classes[id as usize].take();
        engine_assert!(pclass.is_some());
        if let Some(p) = pclass {
            // SAFETY: the class was made by Box::leak in register_priority_class; a class
            // without pending requests is not in `_handles`.
            drop(unsafe { Box::from_raw(p.as_ptr()) });
        }
        self._nr_classes -= 1;
    }

    /// Sets the shares of class `id`.
    pub fn update_shares_for_class(&mut self, id: ClassId, shares: u32) {
        engine_assert!((id as usize) < self._priority_classes.len());
        let pc = self._priority_classes[id as usize];
        engine_assert!(pc.is_some());
        if let Some(pc) = pc {
            // SAFETY: a registered class is live and reached by this reference only here.
            unsafe { (*pc.as_ptr()).update_shares(shares) };
        }
    }

    /// The resources (weight, size) currently queued for all classes.
    pub fn resources_currently_waiting(&self) -> FairQueueTicket {
        self._resources_queued
    }

    /// The resources (weight, size) currently executing.
    pub fn resources_currently_executing(&self) -> FairQueueTicket {
        self._resources_executing
    }

    /// Converts `tokens` to capacity, as [`FairGroup::tokens_capacity`].
    #[inline]
    pub fn tokens_capacity(&self, tokens: f64) -> CapacityT {
        FairGroup::tokens_capacity(tokens)
    }

    /// The group's maximum capacity.
    #[inline]
    pub fn maximum_capacity(&self) -> CapacityT {
        self.group().maximum_capacity()
    }

    /// Queues the entry `ent` in class `id`.
    ///
    /// The caller is expected to call [`FairQueue::notify_request_finished`] when the
    /// request finishes executing, whether it succeeds or fails.
    ///
    /// # Safety
    /// `id` is a registered class, and `ent` is a live entry that neither moves nor is
    /// dropped while it is queued: until `dispatch_requests` hands it out, or until it is
    /// cancelled (`notify_request_cancelled`) and unlinked. The queue stores `ent` as given
    /// and hands that same pointer to the `dispatch_requests` callback, so it must carry the
    /// provenance the callback needs (for an entry inside a request, `addr_of_mut!` of the
    /// field on the pointer to the whole request).
    pub unsafe fn queue(&mut self, id: ClassId, ent: *mut FairQueueEntry) {
        let p = self.pc(id);
        // We need to return a future in this function on which the caller can wait.
        // Since we don't know which queue we will use to execute the next request - if ours or
        // someone else's, we need a separate promise at this point.
        // SAFETY: `p` is a live registered class, reached through its owner's pointer; `ent`
        // follows the contract above.
        unsafe {
            if (*p)._plugged {
                self.push_priority_class_from_idle(p);
            }
            (*p)._queue.push_back(ent);
            self._queued_capacity += (*ent).capacity();
        }
    }

    /// Notifies that a request of cost `_cap` finished. The capacity was already accounted
    /// when the request was dispatched, so this does nothing.
    #[inline]
    pub fn notify_request_finished(&mut self, _cap: CapacityT) {}

    /// Marks the queued entry `ent` as cancelled: its cost becomes zero so it is dispatched
    /// (and discarded by the caller) without consuming capacity.
    ///
    /// # Safety
    /// `ent` is a live entry queued in this queue; no reference to it is live.
    #[inline]
    pub unsafe fn notify_request_cancelled(&mut self, ent: *mut FairQueueEntry) {
        // SAFETY: the contract above.
        unsafe {
            self._queued_capacity -= (*ent)._capacity;
            (*ent)._capacity = 0;
        }
    }

    /// When the pending reservation is expected to be fulfilled, in nanoseconds of the
    /// monotonic clock; `i64::MAX` when there is no reservation.
    pub fn next_pending_aio(&self) -> ClockTypeTimePoint {
        if self._pending.cap != 0 {
            /*
             * We expect the disk to release the ticket within some time,
             * but it's ... OK if it doesn't -- the pending wait still
             * needs the head rover value to be ahead of the needed value.
             *
             * It may happen that the capacity gets released before we think
             * it will, in this case we will wait for the full value again,
             * which's sub-optimal. The expectation is that we think disk
             * works faster, than it really does.
             */
            let over = self.group().capacity_deficiency(self._pending.head);
            let ticks = self.group().capacity_duration(over);
            let d = duration_cast_micros(ticks);
            return SteadyClock::now()
                .saturating_add(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
        }
        ClockTypeTimePoint::MAX
    }

    // This function is called by the shard on every poll.
    // It picks up tokens granted by the group, spends available tokens on IO dispatches,
    // and makes a reservation for more tokens, if needed.
    //
    // Reservations are done in batches of size `_group.per_tick_grab_threshold()`.
    // During contention, in an average moment in time each contending shard can be expected to
    // be holding a reservation of such size after the current head of the token bucket.
    //
    // A shard which is currently calling `dispatch_requests()` can expect a latency
    // of at most `nr_contenders * (_group.per_tick_grab_threshold() + max_request_cap)` before its next reservation is fulfilled.
    // If a shard calls `dispatch_requests()` at least once per X total tokens, it should receive bandwidth
    // of at least `_group.per_tick_grab_threshold() / (X + nr_contenders * (_group.per_tick_grab_threshold() + max_request_cap))`.
    //
    // A shard which is polling continuously should be able to grab its fair share of the disk for itself.
    //
    // Given a task quota of 500us and IO latency goal of 750 us,
    // a CPU-starved shard should still be able to grab at least ~30% of its fair share in the worst case.
    // This is far from ideal, but it's something.
    ///
    /// The callback may re-enter this queue (the I/O queue plugs and unplugs classes while a
    /// request is dispatched), so the
    /// queue is taken as a pointer and no reference to it, or to the class being served, is
    /// live while `cb` runs. The callback receives the entry pointer given to [`queue`](Self::queue).
    ///
    /// # Safety
    /// `this` points to a live queue that the callback does not destroy; while `cb` runs,
    /// the queue may be reached only through pointers with the provenance of `this` (or of
    /// the owner it came from) and only for `plug_class`, `unplug_class` and the read-only
    /// accessors.
    pub unsafe fn dispatch_requests<F: FnMut(*mut FairQueueEntry)>(this: *mut Self, mut cb: F) {
        // SAFETY: the contract above makes `this` a live queue; this reborrow (like each one
        // below) lives only up to the next call of `cb`, and is formed again after it.
        let me = unsafe { &mut *this };
        let group = me._group;
        // SAFETY: as in group().
        unsafe { (*group).maybe_replenish_capacity(&mut me._group_replenish) };
        let max_unamortized_reservation: u64 = me.group().per_tick_grab_threshold();
        let mut available = me.reap_pending_capacity();
        loop {
            // SAFETY: see above.
            let me = unsafe { &mut *this };
            if me._handles.empty() {
                break;
            }
            // The heap holds the classes' owner pointers (see `pc()`).
            let h = me._handles.top();
            // SAFETY: `h` is a live class; short reads of its fields.
            if unsafe { (*h)._queue.empty() || !(*h)._plugged } {
                // SAFETY: `h` is the top of the heap.
                unsafe { me.pop_priority_class(h) };
                continue;
            }
            // SAFETY: the queue is not empty; its entries are alive while queued.
            let req = unsafe { (*h)._queue.front() };
            // SAFETY: as above.
            let req_cap = unsafe { (*req)._capacity };
            if req_cap <= available.ready_tokens {
                // We can dispatch the request immediately.
                // We do that after the if-else.
            } else if req_cap <= available.ready_tokens + me._pending.cap
                || me._pending.cap >= max_unamortized_reservation
            {
                // We can't dispatch the request yet, but we already have a pending reservation
                // which will provide us with enough tokens for it eventually,
                // or our reservation is already max-size and we can't reserve more tokens until we reap some.
                // So we should just wait.
                // We return any immediately-available tokens back to `_pending`
                // and we bail. The next `dispatch_request` will again take those tokens
                // (possibly joined by some newly-granted tokens) and retry.
                me._pending.cap += available.ready_tokens;
                available.ready_tokens = 0;
                break;
            } else if available.our_turn_has_come {
                // The current reservation isn't enough to fulfill the next request,
                // and we can cancel it (because `our_turn_has_come == true`) and make a bigger one
                // (because `_pending.cap < can_grab_this_tick`).
                // So we cancel it and do a bigger one.
                // We do token recycling here: we return the tokens which we have available, and the tokens we have reserved
                // immediately after the group head, and we return them to the bucket, immediately grabbing the same amount from the tail.
                // This is neutral to fairness. The bandwidth we consume is still influenced only by the
                // `max_unarmortized_reservation` portions.
                let recycled = available.ready_tokens + me._pending.cap;
                let mut grab_amount =
                    (recycled + max_unamortized_reservation).min(me._queued_capacity);
                // There's technically nothing wrong with grabbing more than `_group.maximum_capacity()`,
                // but the token bucket has an assert for that, and its a reasonable expectation, so let's respect that limit.
                // It shouldn't matter in practice.
                grab_amount = grab_amount.min(me.group().maximum_capacity());
                me.group().refund_tokens(recycled);
                me.grab_capacity(grab_amount);
                available = me.reap_pending_capacity();
                continue;
            } else {
                // We can already see that our current reservation is going to be insufficient
                // for the highest-priority request as of now. But since group head didn't touch
                // it yet, there's no good way to cancel it, so we have no choice but to wait
                // until the touch time.
                engine_assert!(available.ready_tokens == 0);
                break;
            }
            available.ready_tokens -= req_cap;
            // SAFETY: `h` is a live class, the top of the heap; `req` is its front entry, which
            // `pop_front` unlinks through the list's own pointers (no reference to the entry is
            // live); every access below is a short raw place.
            unsafe {
                me._last_accumulated = (*h)._accumulated.max(me._last_accumulated);
                me.pop_priority_class(h);
                (*h)._queue.pop_front();
            }
            // Usually the cost of request is tens to hundreds of thousands. However, for
            // unrestricted queue it can be as low as 2k. With large enough shares this
            // may be converted into zero cost which, in turn, will make the
            // class show no progress and monopolize the queue.
            // SAFETY: as above.
            let req_cost = (req_cap / unsafe { (*h)._shares } as CapacityT).max(1);
            // signed overflow check to make push_priority_class_from_idle math work
            // SAFETY: as above.
            if unsafe { (*h)._accumulated } >= (SignedCapacityT::MAX as CapacityT) - req_cost {
                for pc in me._priority_classes.iter().flatten() {
                    let pc = pc.as_ptr();
                    // SAFETY: registered classes are live; `h` is one of them and is read
                    // through its own pointer in each round, so `h._accumulated` is read
                    // afresh each round (it is 0 once `h` itself was reset).
                    unsafe {
                        if (*pc)._queued {
                            (*pc)._accumulated = (*pc)._accumulated.wrapping_sub((*h)._accumulated);
                        } else {
                            // this includes h
                            (*pc)._accumulated = 0;
                        }
                    }
                }
                me._last_accumulated = 0;
            }
            // SAFETY: as above.
            unsafe {
                (*h)._accumulated += req_cost;
                (*h)._pure_accumulated += req_cap;
            }
            me._queued_capacity -= req_cap;
            // No reference to the queue, to `h` or to the entry is live across the callback.
            cb(req);
            // SAFETY: see above; the class is still registered (the io_queue unregisters a
            // class only when the queue is destroyed).
            let me = unsafe { &mut *this };
            // SAFETY: as above.
            if unsafe { (*h)._plugged && !(*h)._queue.empty() } {
                // SAFETY: `h` is a registered class with no reference live.
                unsafe { me.push_priority_class(h) };
            }
        }

        // SAFETY: see above.
        let me = unsafe { &mut *this };
        engine_assert!(me._handles.empty() || available.ready_tokens == 0);
        // Note: if IO cancellation happens, it's possible that we are still holding some tokens in `ready` here.
        //
        // We could refund them to the bucket, but permanently refunding tokens (as opposed to only
        // "rotating" the bucket like the earlier refund() calls in this function do) is theoretically
        // unpleasant (it can bloat the bucket beyond its size limit, and its hard to write a correct
        // countermeasure for that), so we just discard the tokens. There's no harm in it, IO cancellation
        // can't have resource-saving guarantees anyway.
    }

    /// The metric definitions of class `c`: consumption, adjusted consumption and
    /// activations.
    pub fn metrics(&mut self, c: ClassId) -> Vec<metrics::impl_::MetricDefinitionImpl> {
        use crate::engine::core::metrics as sm;
        let pc = self.pc(c) as *const PriorityClassData;
        vec![
            sm::make_counter(
                "consumption",
                // SAFETY: the metrics are unregistered before the class is destroyed; a read of a
                // counter on the class's shard.
                move || FairGroup::capacity_tokens(unsafe { (*pc)._pure_accumulated }),
                sm::description(
                    "Accumulated disk capacity units consumed by this class; an increment per-second rate indicates full utilization",
                ),
                vec![],
            ),
            sm::make_counter(
                "adjusted_consumption",
                // SAFETY: as above.
                move || FairGroup::capacity_tokens(unsafe { (*pc)._accumulated }),
                sm::description(
                    "Consumed disk capacity units adjusted for class shares and idling preemption",
                ),
                vec![],
            ),
            sm::make_counter(
                "activations",
                // SAFETY: as above.
                move || unsafe { (*pc)._activations } as u64,
                sm::description("The number of times the class was woken up from idle"),
                vec![],
            ),
        ]
    }
}

impl Drop for FairQueue {
    fn drop(&mut self) {
        for fq in &self._priority_classes {
            engine_assert!(fq.is_none());
        }
    }
}
