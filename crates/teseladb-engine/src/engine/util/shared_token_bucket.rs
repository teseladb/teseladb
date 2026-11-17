//! A token bucket shared by several shards.
//!
//! [`internal::SharedTokenBucket`] rate-limits a resource (for example the bandwidth of
//! one disk, shared by the shards of an I/O group) without locks: grabbing and returning
//! tokens are atomic additions on two wrapping counters, and replenishing is a
//! compare-and-swap on the last replenish time that only one shard wins per period. The
//! rate unit is set by a [`Period`](internal::Period) marker (tokens per second,
//! millisecond or microsecond).

/// The bucket and its parts; not part of the stable API path.
pub mod internal {
    use std::marker::PhantomData;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    use std::time::Duration;

    /// `a - b` on wrapping counters, clamped at 0 when `b` is ahead of `a`.
    #[inline]
    pub fn wrapping_difference(a: &u64, b: &u64) -> u64 {
        std::cmp::max::<i64>(a.wrapping_sub(*b) as i64, 0) as u64
    }

    /// Atomically adds `b` to `a` (sequentially consistent) and returns the previous value.
    #[inline]
    pub fn fetch_add(a: &AtomicU64, b: u64) -> u64 {
        a.fetch_add(b, Ordering::SeqCst)
    }

    /// Counter types with the wrapping operations above (only `u64`).
    pub trait SupportsWrappingArithmetics {}
    impl SupportsWrappingArithmetics for u64 {}

    /// Whether replenishing is capped by tokens explicitly released back (a second bucket).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum CappedRelease {
        /// Replenishment is limited by the `ceil` rover, advanced by `release`.
        Yes,
        /// Replenishment is limited only by the bucket limit.
        No,
    }

    /// One wrapping monotonic counter of the bucket.
    pub type AtomicRover = AtomicU64;

    /// The counters of a bucket: `tail` (tokens grabbed), `head` (tokens available) and,
    /// when capped, `ceil` (tokens released back).
    pub trait Rovers {
        /// Whether this layout caps replenishment.
        const CAPPED: CappedRelease;
        /// Creates the rovers for a bucket of `limit` tokens.
        fn new(limit: u64) -> Self;
        /// The counter advanced by grabbing tokens.
        fn tail(&self) -> &AtomicRover;
        /// The counter advanced by replenishing tokens.
        fn head(&self) -> &AtomicRover;
        /// How far `head` may advance on replenish, for a bucket of `limit` tokens.
        fn max_extra(&self, limit: u64) -> u64;
        /// Returns `tokens` to the second bucket (capped layout only).
        fn release(&self, tokens: u64);
    }

    /// Rovers of a capped bucket: replenishment cannot pass `ceil`.
    pub struct RoversCapped {
        /// Tokens grabbed.
        pub tail: AtomicRover,
        /// Tokens made available.
        pub head: AtomicRover,
        /// Tokens released back; `head` never passes it.
        pub ceil: AtomicRover,
    }

    impl Rovers for RoversCapped {
        const CAPPED: CappedRelease = CappedRelease::Yes;
        fn new(limit: u64) -> Self {
            RoversCapped {
                tail: AtomicU64::new(0),
                head: AtomicU64::new(0),
                ceil: AtomicU64::new(limit),
            }
        }
        fn tail(&self) -> &AtomicRover {
            &self.tail
        }
        fn head(&self) -> &AtomicRover {
            &self.head
        }
        fn max_extra(&self, _: u64) -> u64 {
            wrapping_difference(
                &self.ceil.load(Ordering::Relaxed),
                &self.head.load(Ordering::Relaxed),
            )
        }
        fn release(&self, tokens: u64) {
            fetch_add(&self.ceil, tokens);
        }
    }

    /// Rovers of an uncapped bucket: `head` may run at most `limit` ahead of `tail`.
    pub struct RoversUncapped {
        /// Tokens grabbed.
        pub tail: AtomicRover,
        /// Tokens made available.
        pub head: AtomicRover,
    }

    impl Rovers for RoversUncapped {
        const CAPPED: CappedRelease = CappedRelease::No;
        fn new(_: u64) -> Self {
            RoversUncapped { tail: AtomicU64::new(0), head: AtomicU64::new(0) }
        }
        fn tail(&self) -> &AtomicRover {
            &self.tail
        }
        fn head(&self) -> &AtomicRover {
            &self.head
        }
        fn max_extra(&self, limit: u64) -> u64 {
            wrapping_difference(
                &self.tail.load(Ordering::Relaxed).wrapping_add(limit),
                &self.head.load(Ordering::Relaxed),
            )
        }
        /// Not supported by uncapped buckets.
        ///
        /// # Panics
        ///
        /// Always panics.
        fn release(&self, _: u64) {
            unreachable!("release() is deleted for uncapped rovers")
        }
    }

    /// The time unit of the bucket's rate, as the ratio `NUM / DEN` of a second.
    pub trait Period {
        /// Numerator of the unit in seconds.
        const NUM: f64;
        /// Denominator of the unit in seconds.
        const DEN: f64;
    }

    /// Seconds.
    pub struct Ratio1;
    impl Period for Ratio1 {
        const NUM: f64 = 1.0;
        const DEN: f64 = 1.0;
    }
    /// Milliseconds.
    pub struct Milli;
    impl Period for Milli {
        const NUM: f64 = 1.0;
        const DEN: f64 = 1000.0;
    }
    /// Microseconds.
    pub struct Micro;
    impl Period for Micro {
        const NUM: f64 = 1.0;
        const DEN: f64 = 1_000_000.0;
    }

    /// The clock template parameter: time points as nanoseconds since the clock's epoch.
    pub trait TokenBucketClock {
        /// The current time in nanoseconds.
        fn now() -> i64;
    }

    /// The monotonic clock (`CLOCK_MONOTONIC`).
    pub struct SteadyClock;
    impl TokenBucketClock for SteadyClock {
        fn now() -> i64 {
            #[repr(C)]
            struct Timespec {
                tv_sec: i64,
                tv_nsec: i64,
            }
            unsafe extern "C" {
                fn clock_gettime(clk: i32, tp: *mut Timespec) -> i32;
            }
            #[cfg(target_os = "linux")]
            const CLOCK_MONOTONIC: i32 = 1;
            #[cfg(not(target_os = "linux"))]
            const CLOCK_MONOTONIC: i32 = 6;
            let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: clock_gettime writes into ts.
            unsafe {
                clock_gettime(CLOCK_MONOTONIC, &mut ts);
            }
            ts.tv_sec * 1_000_000_000 + ts.tv_nsec
        }
    }

    /// A duration measured in units of `P`, as a floating-point count.
    #[derive(Debug)]
    pub struct RateResolution<P: Period> {
        count: f64,
        _p: PhantomData<P>,
    }

    // The period is a tag: copies and comparisons look at the count only (a derive would
    // require the tag types themselves to be comparable).
    impl<P: Period> Clone for RateResolution<P> {
        #[inline]
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<P: Period> Copy for RateResolution<P> {}

    impl<P: Period> PartialEq for RateResolution<P> {
        #[inline]
        fn eq(&self, o: &Self) -> bool {
            self.count == o.count
        }
    }

    impl<P: Period> PartialOrd for RateResolution<P> {
        #[inline]
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
            self.count.partial_cmp(&o.count)
        }
    }

    impl<P: Period> RateResolution<P> {
        /// A duration of `count` units.
        pub const fn new(count: f64) -> Self {
            RateResolution { count, _p: PhantomData }
        }
        /// The number of units.
        pub const fn count(&self) -> f64 {
            self.count
        }
        /// Converts `ns` nanoseconds to units of `P`.
        pub fn from_nanos(ns: f64) -> Self {
            // ns * 1e-9 seconds / (NUM/DEN)
            Self::new(ns * 1e-9 * P::DEN / P::NUM)
        }
    }

    /// A lock-free token bucket shared by several shards (see the module documentation).
    ///
    /// `P` is the rate unit, `R` the rover layout (capped or not) and `C` the clock.
    pub struct SharedTokenBucket<P: Period, R: Rovers, C: TokenBucketClock = SteadyClock> {
        /// Tokens per unit of `P`. Atomic because `update_rate` on one shard runs
        /// concurrently with the reads of the other shards sharing the bucket; relaxed
        /// loads and stores compile to plain moves.
        _replenish_rate: AtomicU64,
        _replenish_limit: u64,
        _replenish_threshold: u64,
        _replenished: AtomicI64,

        /*
         * The token bucket is implemented as a pair of wrapping monotonic
         * counters (called rovers) one chasing the other. Getting a token
         * from the bucket is increasing the tail, replenishing a token back
         * is increasing the head. If increased tail overruns the head then
         * the bucket is empty and we have to wait. The shard that grabs tail
         * earlier will be "woken up" earlier, so they form a queue.
         *
         * The top rover is needed to implement two buckets actually. The
         * tokens are not just replenished by timer. They are replenished by
         * timer from the second bucket. And the second bucket only get a
         * token in it after the request that grabbed it from the first bucket
         * completes and returns it back.
         */
        _rovers: R,
        _p: PhantomData<(P, C)>,
    }

    impl<P: Period, R: Rovers, C: TokenBucketClock> SharedTokenBucket<P, R, C> {
        fn head(&self) -> u64 {
            self._rovers.head().load(Ordering::Relaxed)
        }

        /*
         * Need to make sure that the multiplication in accumulated_in() doesn't
         * overflow. Not to introduce an extra branch there, define that the
         * replenish period is not larger than this delta and limit the rate with
         * the value that can overflow it.
         *
         * The additional /=2 in max_rate math is to make extra sure that the
         * overflow doesn't break wrapping_difference sign tricks.
         */
        /// The longest interval credited by one replenish: one hour.
        pub fn max_delta() -> RateResolution<P> {
            RateResolution::from_nanos(3600.0 * 1e9)
        }

        /// The highest rate that cannot overflow over [`max_delta`](Self::max_delta).
        pub fn max_rate() -> u64 {
            (u64::MAX / 2) / (Self::max_delta().count() as u64)
        }

        /// Whether this bucket caps replenishment.
        pub const IS_CAPPED: CappedRelease = R::CAPPED;

        fn accumulated(rate: u64, delta: RateResolution<P>) -> u64 {
            (rate as f64 * delta.count()).round() as u64
        }

        /// Creates a bucket of `limit` tokens replenished at `rate` (clamped to
        /// [`max_rate`](Self::max_rate)) per unit of `P`, in batches of at least `threshold`
        /// tokens (clamped to `[1, limit]`). With `add_replenish_iffset`, the last replenish
        /// time starts 24 hours in the past, so overflows show up early.
        pub fn new(rate: u64, limit: u64, threshold: u64, add_replenish_iffset: bool) -> Self {
            let hours: i64 = if add_replenish_iffset { 24 } else { 0 };
            SharedTokenBucket {
                _replenish_rate: AtomicU64::new(std::cmp::min(rate, Self::max_rate())),
                _replenish_limit: limit,
                _replenish_threshold: threshold.clamp(1, limit),
                // pretend it was replenished yesterday to spot overflows early
                _replenished: AtomicI64::new(C::now() - hours * 3600 * 1_000_000_000),
                _rovers: R::new(limit),
                _p: PhantomData,
            }
        }

        /// Takes `tokens` and returns the new tail; the caller may proceed once the head
        /// reaches it (see [`deficiency`](Self::deficiency)).
        pub fn grab(&self, tokens: u64) -> u64 {
            fetch_add(self._rovers.tail(), tokens).wrapping_add(tokens)
        }

        /// Returns `tokens` to the second bucket (capped buckets only).
        pub fn release(&self, tokens: u64) {
            self._rovers.release(tokens);
        }

        /// Gives `tokens` back immediately (advances the head).
        pub fn refund(&self, tokens: u64) {
            fetch_add(self._rovers.head(), tokens);
        }

        /// Credits the tokens accumulated since the last replenish, if at least
        /// `threshold`. Only one of the shards racing on the same period wins; the others
        /// return without crediting.
        pub fn replenish(&self, now: i64) {
            let ts = self._replenished.load(Ordering::Relaxed);
            if now <= ts {
                return;
            }
            let delta = now - ts;
            let extra = self.accumulated_in(Duration::from_nanos(delta as u64));
            if extra >= self._replenish_threshold {
                if self
                    ._replenished
                    .compare_exchange(ts, ts + delta, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    return; // another shard replenished
                }
                fetch_add(
                    self._rovers.head(),
                    std::cmp::min(extra, self._rovers.max_extra(self._replenish_limit)),
                );
            }
        }

        /// How many tokens the head is behind `from` (a value returned by
        /// [`grab`](Self::grab)); 0 when they are available.
        pub fn deficiency(&self, from: u64) -> u64 {
            wrapping_difference(&from, &self.head())
        }

        /// Converts `delta` to units of `P`.
        pub fn rate_cast(delta: Duration) -> RateResolution<P> {
            RateResolution::from_nanos(delta.as_nanos() as f64)
        }

        /// The number of tokens accumulated over `delta` (capped at one hour's worth).
        pub fn accumulated_in(&self, delta: Duration) -> u64 {
            let d = Self::rate_cast(delta);
            let m = Self::max_delta();
            let delta_at_rate = if d < m { d } else { m };
            Self::accumulated(self.rate(), delta_at_rate)
        }

        /// The estimated time to accumulate `tokens` at the current rate (the inverse of
        /// [`accumulated_in`](Self::accumulated_in)).
        pub fn duration_for(&self, tokens: u64) -> RateResolution<P> {
            RateResolution::new(tokens as f64 / self.rate() as f64)
        }

        /// The current rate, in tokens per unit of `P`.
        #[inline]
        pub fn rate(&self) -> u64 {
            self._replenish_rate.load(Ordering::Relaxed)
        }
        /// The bucket size in tokens.
        pub fn limit(&self) -> u64 {
            self._replenish_limit
        }
        /// The minimum number of tokens credited by one replenish.
        pub fn threshold(&self) -> u64 {
            self._replenish_threshold
        }
        /// The clock time (nanoseconds) of the last replenish.
        pub fn replenished_ts(&self) -> i64 {
            self._replenished.load(Ordering::SeqCst)
        }

        /// Sets the rate (clamped to [`max_rate`](Self::max_rate)). Takes `&self`, as the
        /// bucket is shared by the shards of an I/O group.
        #[inline]
        pub fn update_rate(&self, rate: u64) {
            self._replenish_rate.store(std::cmp::min(rate, Self::max_rate()), Ordering::Relaxed);
        }
    }
}
