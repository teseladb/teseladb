//! The limits of the HTTP port without I/O: how long a connection may take to send
//! its fields or stay idle, and the requests each credential may have running at once and start
//! each second (`httpOptions.rateLimits`), refused with `429` and `Retry-After`.

use std::collections::BTreeMap;

/// A connection that has not sent a request's fields whole after this long is closed.
pub const HEADER_TIMEOUT_MS: u64 = 10_000;
/// A connection with nothing to do for this long is closed.
pub const IDLE_TIMEOUT_MS: u64 = 120_000;
/// The window of the per-second limit.
const SECOND_MS: u64 = 1_000;

/// The limits of each credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimits {
    /// Requests running at once.
    pub concurrent: u64,
    /// Requests started in a second.
    pub per_second: u64,
}

/// Why a request may not start now, with the seconds after which it may (`Retry-After`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limited {
    /// Seconds to wait.
    pub retry_after_secs: u64,
}

#[derive(Clone, Debug, Default)]
struct Usage {
    running: u64,
    window_start_ms: u64,
    in_window: u64,
}

/// The requests of each credential (a user, an API key, or the address of a caller without
/// one), shared by the connections of a node.
#[derive(Clone, Debug, Default)]
pub struct RateLimiter {
    usage: BTreeMap<String, Usage>,
}

impl RateLimiter {
    /// A request of `credential` starts at `now_ms` under `limits`, or is refused. A request
    /// that starts must be [`RateLimiter::finish`]ed.
    ///
    /// # Errors
    ///
    /// [`Limited`], with the seconds after which it may start, when `credential` has as many requests
    /// running as it may or has started as many as it may in the current second.
    pub fn start(
        &mut self,
        credential: &str,
        now_ms: u64,
        limits: RateLimits,
    ) -> Result<(), Limited> {
        let u = self.usage.entry(credential.to_owned()).or_default();
        if now_ms.saturating_sub(u.window_start_ms) >= SECOND_MS {
            u.window_start_ms = now_ms;
            u.in_window = 0;
        }
        if u.running >= limits.concurrent {
            return Err(Limited { retry_after_secs: 1 });
        }
        if u.in_window >= limits.per_second {
            let left_ms = SECOND_MS - now_ms.saturating_sub(u.window_start_ms);
            return Err(Limited { retry_after_secs: left_ms.div_ceil(SECOND_MS).max(1) });
        }
        u.running += 1;
        u.in_window += 1;
        Ok(())
    }

    /// A request of `credential` ended; a credential with nothing running and no recent
    /// request is forgotten.
    pub fn finish(&mut self, credential: &str) {
        if let Some(u) = self.usage.get_mut(credential) {
            u.running = u.running.saturating_sub(1);
        }
    }

    /// Forgets the credentials idle at `now_ms`, so that the map holds only the active ones.
    pub fn sweep(&mut self, now_ms: u64) {
        self.usage
            .retain(|_, u| u.running > 0 || now_ms.saturating_sub(u.window_start_ms) < SECOND_MS);
    }

    /// Requests of `credential` running.
    #[must_use]
    pub fn running(&self, credential: &str) -> u64 {
        self.usage.get(credential).map_or(0, |u| u.running)
    }
}

/// When a connection is closed for want of activity: `HEADER_TIMEOUT_MS` after the first byte of
/// fields that are not whole yet, `IDLE_TIMEOUT_MS` after the last activity otherwise.
#[must_use]
pub fn deadline_ms(last_activity_ms: u64, partial_since_ms: Option<u64>) -> u64 {
    let idle = last_activity_ms.saturating_add(IDLE_TIMEOUT_MS);
    match partial_since_ms {
        Some(t) => idle.min(t.saturating_add(HEADER_TIMEOUT_MS)),
        None => idle,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    const LIMITS: RateLimits = RateLimits { concurrent: 2, per_second: 3 };

    #[test]
    fn concurrent_requests_past_the_limit_wait_a_second() {
        let mut r = RateLimiter::default();
        assert_eq!(r.start("u", 0, LIMITS), Ok(()));
        assert_eq!(r.start("u", 0, LIMITS), Ok(()));
        assert_eq!(r.start("u", 0, LIMITS), Err(Limited { retry_after_secs: 1 }));
        // Another credential has its own count.
        assert_eq!(r.start("k", 0, LIMITS), Ok(()));
        r.finish("u");
        assert_eq!(r.running("u"), 1);
        assert_eq!(r.start("u", 10, LIMITS), Ok(()));
        // Three started in this second: the fourth waits for the next.
        r.finish("u");
        r.finish("u");
        assert_eq!(r.start("u", 400, LIMITS), Err(Limited { retry_after_secs: 1 }));
        assert_eq!(r.start("u", 1_000, LIMITS), Ok(()));
        r.finish("u");
        r.finish("nobody");
        assert_eq!(r.running("nobody"), 0);
    }

    #[test]
    fn the_wait_covers_the_rest_of_the_second() {
        let limits = RateLimits { concurrent: 10, per_second: 1 };
        let mut r = RateLimiter::default();
        assert_eq!(r.start("u", 0, limits), Ok(()));
        assert_eq!(r.start("u", 999, limits), Err(Limited { retry_after_secs: 1 }));
    }

    #[test]
    fn idle_credentials_are_forgotten() {
        let mut r = RateLimiter::default();
        r.start("a", 0, LIMITS).expect("start succeeds");
        r.start("b", 0, LIMITS).expect("start succeeds");
        r.finish("b");
        r.sweep(500);
        assert_eq!(r.usage.len(), 2);
        r.sweep(2_000);
        assert_eq!(r.usage.keys().collect::<Vec<_>>(), ["a"]);
    }

    #[test]
    fn deadlines_follow_the_fields_and_the_idle_time() {
        assert_eq!(deadline_ms(1_000, None), 121_000);
        assert_eq!(deadline_ms(1_000, Some(500)), 10_500);
        assert_eq!(deadline_ms(1_000, Some(200_000)), 121_000);
    }
}
