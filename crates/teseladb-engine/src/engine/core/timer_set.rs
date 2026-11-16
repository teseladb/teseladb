//! A bucketed set of armed timers, cheap to insert into and cancel from.
//!
//! Each reactor keeps one [`TimerSet`] per clock. Timers are kept in 64 intrusive lists keyed
//! by the position of the highest bit in which their deadline differs from the last expiry
//! time, so insertion and removal are O(1) and sorting is deferred until expiry. The design
//! relies on most timers (for example network retransmission timers) being cancelled or
//! rescheduled before they fire. The set never allocates; the timers themselves are the list
//! nodes, so they must stay in place while they are in the set.

use std::marker::PhantomData;
use std::ptr::NonNull;

use crate::engine::core::api::intrusive::{Adapter, List};
use crate::engine::core::scheduling::{
    current_scheduling_group, internal::current_scheduling_group_ptr,
};

/// Internal helpers shared with the reactor.
pub mod internal {
    /// Logs an error raised by a timer callback; defined by the reactor.
    pub use crate::engine::core::reactor::internal::log_timer_callback_exception;
}

/// What a [`TimerSet`] needs from the timers it holds.
pub trait TimerSetEntry {
    /// The time point type of the timer's clock.
    type TimePoint: Copy + Ord;
    /// Returns the deadline of the timer. It must not change while the timer is in a set.
    fn get_timeout(&self) -> Self::TimePoint;
    /// Converts a time point to clock ticks since the epoch.
    fn timestamp(tp: Self::TimePoint) -> i64;
    /// Converts clock ticks since the epoch back to a time point.
    fn from_timestamp(ts: i64) -> Self::TimePoint;
    /// Returns the current time of the timer's clock.
    fn now() -> Self::TimePoint;
    /// Whether the timer has been moved to an expired list and not yet fired.
    fn expired(&self) -> bool;
    /// Sets the expired flag of the timer.
    ///
    /// # Safety
    /// `this` must point to a live timer, with no reference to it live.
    unsafe fn set_expired(this: *mut Self, v: bool);
    /// Runs the expiry step of [`TimerSet::complete`] for this timer: clears its queued flag, and if
    /// armed, disarms it, re-adds it if periodic, sets the scheduling group and calls
    /// the callback, which may destroy the timer.
    ///
    /// # Safety
    /// As [`TimerSetEntry::set_expired`]; the timer must not be in a list.
    unsafe fn fire(this: *mut Self);
    /// Cancels the timer, unlinking it from its set; returns whether it was armed.
    ///
    /// # Safety
    /// As [`TimerSetEntry::set_expired`].
    unsafe fn cancel(this: *mut Self) -> bool;
    /// Cancels a timer that its set, being dropped, has already unlinked: clears its armed and
    /// queued flags without reaching the set again through the reactor.
    ///
    /// # Safety
    /// As [`TimerSetEntry::set_expired`]; the timer must have been unlinked from the set
    /// that is being destroyed.
    unsafe fn cancel_unlinked(this: *mut Self);
}

type TimestampT = i64;
const MAX_TIMESTAMP: TimestampT = TimestampT::MAX;
const TIMESTAMP_BITS: usize = 63; // value bits of a signed 64-bit timestamp
// The last bucket is reserved for active timers with timeout <= _last.
const N_BUCKETS: usize = TIMESTAMP_BITS + 1;

/// Number of leading zero bits among the value bits of a non-negative signed timestamp: the sign
/// bit is not counted, as `bitsets::count_leading_zeros<long>` does (`__builtin_clzl(x) - 1`), so
/// the highest result, for 1, is 62 and every timestamp past the last expiry has a bucket below
/// the reserved one.
#[inline]
fn count_leading_zeros(x: TimestampT) -> usize {
    (x as u64).leading_zeros() as usize - 1
}

/// The bucket of a timer due at `timestamp` in a set whose last expiry was at `last`.
#[inline]
fn bucket_of(timestamp: TimestampT, last: TimestampT) -> usize {
    if timestamp <= last {
        return N_BUCKETS - 1;
    }
    let index = count_leading_zeros(timestamp ^ last);
    assert!(index < N_BUCKETS - 1);
    index
}

/// A data structure designed for holding and expiring timers. It's
/// optimized for timer non-delivery by deferring sorting cost until
/// expiry time. The optimization is based on the observation that in
/// many workloads timers are cancelled or rescheduled before they
/// expire. That's especially the case for TCP timers.
///
/// The timers implement [`TimerSetEntry`], whose `get_timeout` gives each timer's deadline.
/// `A` is the intrusive-list adapter that links timers through their embedded hook.
pub struct TimerSet<A: Adapter>
where
    A::Value: TimerSetEntry,
{
    buckets: [List<A>; N_BUCKETS],
    last: TimestampT,
    next: TimestampT,
    /// Bit `i` is set when bucket `i` is non-empty.
    non_empty_buckets: u64,
    _marker: PhantomData<A>,
}

/// An intrusive list of timers, used for the expired timers handed back by [`TimerSet::expire`].
pub type TimerList<A> = List<A>;

impl<A: Adapter> Default for TimerSet<A>
where
    A::Value: TimerSetEntry,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Adapter> TimerSet<A>
where
    A::Value: TimerSetEntry,
{
    #[inline]
    fn get_timestamp(tp: <A::Value as TimerSetEntry>::TimePoint) -> TimestampT {
        <A::Value as TimerSetEntry>::timestamp(tp)
    }

    #[inline]
    fn get_timer_timestamp(timer: &A::Value) -> TimestampT {
        Self::get_timestamp(timer.get_timeout())
    }

    #[inline]
    fn get_index(&self, timestamp: TimestampT) -> usize {
        bucket_of(timestamp, self.last)
    }

    #[inline]
    fn get_last_non_empty_bucket(&self) -> usize {
        63 - self.non_empty_buckets.leading_zeros() as usize
    }

    #[inline]
    fn set_bucket(&mut self, i: usize, v: bool) {
        if v {
            self.non_empty_buckets |= 1 << i;
        } else {
            self.non_empty_buckets &= !(1 << i);
        }
    }

    /// Iterates the indices of the non-empty buckets from `offset` upwards.
    #[inline]
    fn set_buckets_from(&self, offset: usize) -> impl Iterator<Item = usize> {
        let mut bits = if offset >= 64 { 0 } else { self.non_empty_buckets & (!0u64 << offset) };
        std::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            Some(i)
        })
    }

    /// Creates an empty set. Does not allocate.
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| List::new()),
            last: 0,
            next: MAX_TIMESTAMP,
            non_empty_buckets: 0,
            _marker: PhantomData,
        }
    }

    /// Adds `timer` to the active set, where it stays until [`TimerSet::expire`] expires it
    /// or [`TimerSet::remove`] removes it.
    ///
    /// The timer's timeout is its expiry, and must not change while the timer is in the
    /// active set.
    ///
    /// Returns `true` if and only if the timer's timeout is earlier than
    /// [`TimerSet::get_next_timeout`]; the caller should then reschedule
    /// [`TimerSet::expire`] for that timeout, so that timers expire in time.
    ///
    /// # Safety
    /// `timer` must not be in the active set or in an expired list. It must be a live timer that stays in place while it is in the set, and the
    /// set must not move while it holds timers (its buckets are intrusive lists).
    pub unsafe fn insert(&mut self, timer: NonNull<A::Value>) -> bool {
        // SAFETY: guaranteed by the caller.
        let timestamp = Self::get_timer_timestamp(unsafe { timer.as_ref() });
        let index = self.get_index(timestamp);
        // SAFETY: as above.
        unsafe { self.buckets[index].push_back(timer.as_ptr()) };
        self.set_bucket(index, true);
        if timestamp < self.next {
            self.next = timestamp;
            return true;
        }
        false
    }

    /// Removes `timer` from the active set; afterwards the set holds no reference to it.
    ///
    /// # Safety
    /// `timer` must be in the active set, not in an expired list.
    pub unsafe fn remove(&mut self, timer: NonNull<A::Value>) {
        // SAFETY: guaranteed by the caller.
        let index = self.get_index(Self::get_timer_timestamp(unsafe { timer.as_ref() }));
        // SAFETY: the timer is linked in this bucket.
        unsafe { (*A::hook(timer.as_ptr())).unlink() };
        if self.buckets[index].is_empty() {
            self.set_bucket(index, false);
        }
    }

    /// Removes `timer` from the active set or, if it has expired, from its expired list
    /// (and clears its expired flag).
    ///
    /// # Safety
    /// `timer` must be in the active set or in `expired`.
    pub unsafe fn remove_or_expired(
        &mut self,
        timer: NonNull<A::Value>,
        _expired: &mut TimerList<A>,
    ) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if timer.as_ref().expired() {
                (*A::hook(timer.as_ptr())).unlink();
                <A::Value as TimerSetEntry>::set_expired(timer.as_ptr(), false);
            } else {
                self.remove(timer);
            }
        }
    }

    /// Expires the active timers whose timeout is at or before `now`, moving them to
    /// `exp`.
    ///
    /// Successive calls must pass time points that never decrease;
    /// [`TimerSet::get_next_timeout`] gives the next one to use.
    ///
    /// The expired timers are appended to `exp` rather than returned: a list holding nodes
    /// cannot be moved, since its nodes point at its sentinel.
    ///
    /// # Safety
    /// `exp` must not move while it holds timers.
    pub unsafe fn expire(
        &mut self,
        now: <A::Value as TimerSetEntry>::TimePoint,
        exp: &mut TimerList<A>,
    ) {
        let timestamp = Self::get_timestamp(now);
        if timestamp < self.last {
            std::process::abort();
        }
        let index = self.get_index(timestamp);
        let mut bits =
            self.non_empty_buckets & if index + 1 >= 64 { 0 } else { !0u64 << (index + 1) };
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            // SAFETY: guaranteed by the caller.
            unsafe { exp.splice_back(&mut self.buckets[i]) };
            self.set_bucket(i, false);
        }
        self.last = timestamp;
        self.next = MAX_TIMESTAMP;
        // The timers of the bucket are moved to a local list, which stays on this frame
        // until it is drained.
        let mut list = TimerList::<A>::new();
        // SAFETY: `list` does not move while it holds timers.
        unsafe { list.splice_back(&mut self.buckets[index]) };
        while let Some(timer) = list.pop_front() {
            // SAFETY: timers in the set are live.
            if unsafe { timer.as_ref().get_timeout() } <= now {
                // SAFETY: the timer was just unlinked and stays in place; exp as above.
                unsafe { exp.push_back(timer.as_ptr()) };
            } else {
                // SAFETY: as above; the set does not move while it holds timers.
                unsafe { self.insert(timer) };
            }
        }
        self.set_bucket(index, !self.buckets[index].is_empty());
        if self.next == MAX_TIMESTAMP && self.non_empty_buckets != 0 {
            let last = self.get_last_non_empty_bucket();
            for timer in self.buckets[last].iter() {
                // SAFETY: timers in the set are live.
                self.next = self.next.min(Self::get_timer_timestamp(unsafe { timer.as_ref() }));
            }
        }
    }

    /// Expires the timers due now into `expired_timers`, fires each of them, restores the
    /// current scheduling group and finally calls `enable_fn`.
    ///
    /// The callbacks re-enter the reactor: they arm and cancel timers of this set and of
    /// `expired_timers`. So the set and the list are passed as raw pointers, and no
    /// reference to either is live while a callback runs.
    ///
    /// # Safety
    /// `this` and `expired_timers` must point to a live set and list that do not move,
    /// with no reference to either live during the call.
    pub unsafe fn complete<F: FnOnce()>(
        this: *mut Self,
        expired_timers: *mut TimerList<A>,
        enable_fn: F,
    ) {
        // SAFETY: guaranteed by the caller; every reference below ends before a callback
        // runs.
        unsafe {
            let now = (*this).now();
            (*expired_timers).clear();
            (*this).expire(now, &mut *expired_timers);
            for t in (*expired_timers).iter() {
                // Expired timers are live.
                <A::Value as TimerSetEntry>::set_expired(t.as_ptr(), true);
            }
            let prev_sg = current_scheduling_group();
            while let Some(t) = (*expired_timers).pop_front() {
                // The timer is live; fire() may re-queue it, drop its callback or destroy
                // it.
                <A::Value as TimerSetEntry>::fire(t.as_ptr());
            }
            // complete_timers() can be called from the context of run_tasks()
            // as well so we need to restore the previous scheduling group (set by run_tasks()).
            // The pointer refers to the thread-local current group.
            *current_scheduling_group_ptr() = prev_sg;
        }
        enable_fn();
    }

    /// Returns the time point at which [`TimerSet::expire`] should be called next so that
    /// timers expire in time.
    ///
    /// Successive values never decrease.
    #[inline]
    pub fn get_next_timeout(&self) -> <A::Value as TimerSetEntry>::TimePoint {
        <A::Value as TimerSetEntry>::from_timestamp(self.last.max(self.next))
    }

    /// Removes every timer from the active set.
    pub fn clear(&mut self) {
        let mut bits = self.non_empty_buckets;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            self.buckets[i].clear();
        }
    }

    /// Returns the number of timers in the active set. O(number of timers).
    pub fn size(&self) -> usize {
        self.set_buckets_from(0).map(|i| self.buckets[i].size()).sum()
    }

    /// Returns whether the active set holds no timer.
    #[inline]
    pub fn empty(&self) -> bool {
        self.non_empty_buckets == 0
    }

    /// Returns the current time of the timers' clock.
    #[inline]
    pub fn now(&self) -> <A::Value as TimerSetEntry>::TimePoint {
        <A::Value as TimerSetEntry>::now()
    }
}

impl<A: Adapter> Drop for TimerSet<A>
where
    A::Value: TimerSetEntry,
{
    /// Cancels every timer still in the set.
    ///
    /// A plain cancel would go back through the reactor into this same set to unlink the
    /// timer. The destructor already holds `&mut self`, so it unlinks each timer itself and
    /// then cancels it without reaching the set again: the timer ends unlinked, neither armed
    /// nor queued, and no second `&mut` to the set is created.
    fn drop(&mut self) {
        for i in 0..self.buckets.len() {
            while let Some(t) = self.buckets[i].pop_front() {
                // SAFETY: timers in the set are live; it was just unlinked from this set,
                // and cancel_unlinked touches the timer only.
                unsafe { <A::Value as TimerSetEntry>::cancel_unlinked(t.as_ptr()) };
            }
        }
        self.non_empty_buckets = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{N_BUCKETS, bucket_of, count_leading_zeros};

    /// The values of `bitsets::count_leading_zeros<long>` (`__builtin_clzl(x) - 1`).
    #[test]
    fn leading_zeros_count_the_value_bits_of_a_signed_timestamp() {
        assert_eq!(count_leading_zeros(1), 62);
        assert_eq!(count_leading_zeros(2), 61);
        assert_eq!(count_leading_zeros(i64::MAX), 0);
    }

    /// A timer due one tick after the last expiry, with that expiry at an even tick, goes in a
    /// bucket of its own (the original puts it in bucket 62); the last bucket stays for the
    /// timers already due.
    #[test]
    fn a_timer_one_tick_past_an_even_last_expiry_has_a_bucket() {
        assert_eq!(bucket_of(1001, 1000), 62);
        assert_eq!(bucket_of(1002, 1001), 61);
        assert_eq!(bucket_of(1000, 1000), N_BUCKETS - 1);
        assert_eq!(bucket_of(999, 1000), N_BUCKETS - 1);
        // Every timestamp past the last expiry, near it or far from it.
        for last in [0_i64, 1, 2, 1000, 1 << 40, i64::MAX - 1] {
            for delta in [1_i64, 2, 3, 41, 42, 1 << 20] {
                let Some(t) = last.checked_add(delta) else { continue };
                assert!(bucket_of(t, last) < N_BUCKETS - 1, "last {last} timer {t}");
            }
        }
    }
}
