//! The split of a core's disk bandwidth between the log's class and the data's (012 FR-028), with
//! tokens: while the log waits more than [`LOG_WAIT_LIMIT`], the data's operations start only on
//! the tokens the log's operations earn them, [`DATA_SHARE_PERCENT`] of the bytes started by both;
//! otherwise the data takes whatever bandwidth the log leaves free.
//!
//! The log's wait counts from the submission of one of its operations until the kernel completes
//! it, and the age of the oldest one not started yet: the device's queue, where the data's bytes
//! hold the log's back, is part of it. The log presses only while it has operations queued or
//! running; an idle log never holds the data.
//!
//! The data may start an operation as long as its balance of tokens is positive, and the balance
//! then goes down by the operation's cost: a large operation is never held for ever, and over a
//! run of them the data's share stays at most one operation above its [`DATA_SHARE_PERCENT`].

use std::time::{Duration, Instant};

/// The log's wait past which the data's share is capped.
pub const LOG_WAIT_LIMIT: Duration = Duration::from_millis(1);
/// The data's largest share of the bytes started while the log waits past [`LOG_WAIT_LIMIT`].
pub const DATA_SHARE_PERCENT: u64 = 30;
/// The cost of an operation that moves no bytes of its own (a `Sync`, a create, a rename): what
/// the device spends on it is like one block write.
pub const FIXED_COST: u64 = 4096;

/// The tokens of a core's executor.
#[derive(Debug, Clone)]
pub struct BandwidthSplit {
    /// The wait of the log's last completed operation.
    last_log_wait: Duration,
    /// When the oldest operation of the log still queued was submitted.
    oldest_log_queued: Option<Instant>,
    /// Whether the log has operations queued or running.
    log_busy: bool,
    /// The data's tokens, in bytes: the log's starts earn them and the data's spend them.
    data_tokens: i64,
    /// Whether the data was capped on the last check: a new pressing period starts from no tokens.
    capped: bool,
}

impl Default for BandwidthSplit {
    fn default() -> BandwidthSplit {
        BandwidthSplit::new()
    }
}

impl BandwidthSplit {
    /// A split with the log idle.
    pub fn new() -> BandwidthSplit {
        BandwidthSplit {
            last_log_wait: Duration::ZERO,
            oldest_log_queued: None,
            log_busy: false,
            data_tokens: 0,
            capped: false,
        }
    }

    /// What the executor's log class looks like now: whether it has operations queued or running,
    /// and when the oldest one still queued was submitted.
    pub fn set_log_state(&mut self, busy: bool, oldest_queued: Option<Instant>) {
        self.log_busy = busy;
        self.oldest_log_queued = oldest_queued;
        if !busy {
            // A quiet log's last wait says nothing about the next one.
            self.last_log_wait = Duration::ZERO;
        }
    }

    /// One of the log's operations completed `wait` after its submission.
    pub fn log_completed(&mut self, wait: Duration) {
        self.last_log_wait = wait;
    }

    /// Whether the log waits past [`LOG_WAIT_LIMIT`] at `now`, so the data's share is capped.
    pub fn log_pressed(&self, now: Instant) -> bool {
        if !self.log_busy {
            return false;
        }
        let queued =
            self.oldest_log_queued.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        self.last_log_wait.max(queued) > LOG_WAIT_LIMIT
    }

    /// Whether a data operation may start at `now`, and if so it is counted.
    pub fn try_start_data(&mut self, cost: u64, now: Instant) -> bool {
        if !self.log_pressed(now) {
            self.capped = false;
            return true;
        }
        if !self.capped {
            self.capped = true;
            self.data_tokens = 0;
        }
        if self.data_tokens <= 0 {
            return false;
        }
        self.data_tokens -= i64::try_from(cost).unwrap_or(i64::MAX);
        true
    }

    /// A log operation of `cost` started: while the log presses, it earns the data its share.
    pub fn log_started(&mut self, cost: u64, now: Instant) {
        if !self.log_pressed(now) {
            return;
        }
        if !self.capped {
            self.capped = true;
            self.data_tokens = 0;
        }
        // data / (log + data) <= p  <=>  data <= log * p / (100 - p).
        let earned = u128::from(cost) * u128::from(DATA_SHARE_PERCENT)
            / u128::from(100 - DATA_SHARE_PERCENT);
        let earned = i64::try_from(earned).unwrap_or(i64::MAX);
        // Bounded: a long pressing period does not bank tokens for a burst afterwards.
        self.data_tokens = self.data_tokens.saturating_add(earned).min(MAX_BANKED_TOKENS);
    }

    /// The data's balance of tokens (for tests and statistics).
    pub fn data_tokens(&self) -> i64 {
        self.data_tokens
    }
}

/// The most tokens the data may hold: four of the reserve's buffers' worth.
const MAX_BANKED_TOKENS: i64 = 4 * (crate::buffers::BUFFER_BYTES as i64);

/// The cost of an operation that moves `bytes` bytes (0 for one that moves none).
pub fn cost_of(bytes: u64) -> u64 {
    bytes.max(FIXED_COST)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_idle_or_quick_log_leaves_the_data_all_the_bandwidth() {
        let now = Instant::now();
        let mut s = BandwidthSplit::new();
        assert!(!s.log_pressed(now));
        for _ in 0..100 {
            assert!(s.try_start_data(1 << 20, now));
        }
        // A busy log whose operations finish within the limit does not press.
        s.set_log_state(true, Some(now));
        s.log_completed(Duration::from_micros(900));
        assert!(!s.log_pressed(now));
        assert!(s.try_start_data(1 << 20, now));
        // A slow last wait of a log that went quiet does not press either.
        s.log_completed(ms(5));
        s.set_log_state(false, None);
        assert!(!s.log_pressed(now));
        assert!(s.try_start_data(1 << 20, now));
    }

    #[test]
    fn a_log_waiting_past_the_limit_caps_the_data_at_its_share() {
        let now = Instant::now();
        let mut s = BandwidthSplit::new();
        s.set_log_state(true, None);
        s.log_completed(ms(2));
        assert!(s.log_pressed(now));
        // No tokens yet: the data waits.
        assert!(!s.try_start_data(4096, now));
        let (mut log, mut data) = (0u64, 0u64);
        for _ in 0..10_000 {
            s.log_started(4096, now);
            log += 4096;
            while s.try_start_data(4096, now) {
                data += 4096;
            }
        }
        let share = data * 100 / (log + data);
        assert!(share <= DATA_SHARE_PERCENT, "data took {share}%");
        assert!(share >= DATA_SHARE_PERCENT - 1, "data took only {share}%");
    }

    #[test]
    fn the_age_of_a_queued_log_operation_presses_too() {
        let then = Instant::now();
        let now = then + ms(3);
        let mut s = BandwidthSplit::new();
        s.set_log_state(true, Some(then));
        assert!(s.log_pressed(now));
        assert!(!s.log_pressed(then));
        assert!(!s.try_start_data(4096, now));
    }

    #[test]
    fn a_large_data_operation_goes_on_credit_and_is_paid_back() {
        let now = Instant::now();
        let mut s = BandwidthSplit::new();
        s.set_log_state(true, None);
        s.log_completed(ms(2));
        s.log_started(4096, now);
        assert!(s.try_start_data(1 << 20, now), "a positive balance starts it");
        assert!(s.data_tokens() < 0);
        assert!(!s.try_start_data(4096, now), "then the debt is paid first");
        // Once the log is quick again the data is free, and the next pressing starts afresh.
        s.log_completed(ms(0));
        assert!(s.try_start_data(4096, now));
        s.log_completed(ms(2));
        s.log_started(4096, now);
        assert!(s.try_start_data(4096, now));
    }

    #[test]
    fn tokens_do_not_pile_up_for_a_burst() {
        let now = Instant::now();
        let mut s = BandwidthSplit::new();
        s.set_log_state(true, None);
        s.log_completed(ms(2));
        for _ in 0..100_000 {
            s.log_started(1 << 20, now);
        }
        assert_eq!(s.data_tokens(), MAX_BANKED_TOKENS);
        // The log's starts while it does not press earn nothing.
        let mut quiet = BandwidthSplit::new();
        quiet.log_started(1 << 20, now);
        assert_eq!(quiet.data_tokens(), 0);
    }

    #[test]
    fn operations_without_bytes_cost_one_block() {
        assert_eq!(cost_of(0), FIXED_COST);
        assert_eq!(cost_of(1), FIXED_COST);
        assert_eq!(cost_of(1 << 20), 1 << 20);
        assert_eq!(BandwidthSplit::default().data_tokens(), 0);
    }
}
