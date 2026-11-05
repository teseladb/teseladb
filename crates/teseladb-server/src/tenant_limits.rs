//! The limits of each tenant on this node (007 FR-032, FR-033): its authenticated connections,
//! its commands per second with a bucket of one second, its commands running at once and its
//! bytes written per second with a bucket like the commands', and what `/metrics` and `GET
//! /admin/tenants/<name>/usage` count of it here. Each node applies the whole limits (D-129).
//! The buckets take the time as an argument, so the rules run in tests without a clock.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use teseladb_meta::tenants::TenantLimits;

/// The longest a command waits for its token, its slot or its bytes before it fails (FR-032).
pub const MAX_WAIT: Duration = Duration::from_millis(100);

/// A bucket of `rate` per second that holds one second of them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Bucket {
    /// What it holds; below zero, what was lent to the commands that waited.
    tokens: f64,
    /// When it was last filled, from the limiter's start.
    at: Duration,
    /// Whether it was ever used: a new bucket starts full.
    used: bool,
}

impl Bucket {
    /// Takes `cost` at `now` from a bucket of `rate` per second: the wait before the command may
    /// run, or `None` if it would wait more than [`MAX_WAIT`]. A full bucket lets a command
    /// bigger than a second's worth through and goes below zero.
    pub fn take(&mut self, rate: f64, cost: f64, now: Duration) -> Option<Duration> {
        if !self.used {
            *self = Bucket { tokens: rate, at: now, used: true };
        }
        let elapsed = now.saturating_sub(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate).min(rate);
        self.at = now;
        if self.tokens >= cost || self.tokens >= rate {
            self.tokens -= cost;
            return Some(Duration::ZERO);
        }
        let wait = Duration::from_secs_f64((cost - self.tokens) / rate);
        if wait > MAX_WAIT {
            return None;
        }
        self.tokens -= cost;
        Some(wait)
    }
}

/// What a node counts of a tenant (FR-033).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeUsage {
    /// Its authenticated connections on this node, without its service user's.
    pub connections: u32,
    /// The commands this node served it, without its service user's.
    pub ops_total: u64,
    /// The seconds its commands waited or were refused for its limits.
    pub throttled_seconds: f64,
    /// When one of its users last signed in on this node, in milliseconds since the epoch.
    pub last_auth_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Tenant {
    usage: NodeUsage,
    running: u32,
    ops: Bucket,
    bytes: Bucket,
}

/// Why a command of a tenant does not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Past its commands per second.
    Ops,
    /// Past its commands at once.
    Concurrent,
    /// Past its bytes written per second.
    Bytes,
}

/// The limits of every tenant on this node.
#[derive(Debug, Default)]
pub struct TenantLimiter {
    tenants: Mutex<BTreeMap<u32, Tenant>>,
    /// The start of the buckets' time on this node.
    epoch: std::sync::OnceLock<std::time::Instant>,
}

impl TenantLimiter {
    /// The time of the buckets now: since the first time this node asked.
    pub fn now(&self) -> Duration {
        self.epoch.get_or_init(std::time::Instant::now).elapsed()
    }

    fn with<T>(&self, tenant: u32, f: impl FnOnce(&mut Tenant) -> T) -> T {
        let mut g = self.tenants.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(g.entry(tenant).or_default())
    }

    /// Takes a connection of `tenant` that just signed in at `wall_ms`, if it has fewer than
    /// `max` (0: no limit); `false` closes it.
    pub fn connect(&self, tenant: u32, max: u32, wall_ms: u64) -> bool {
        self.with(tenant, |t| {
            if max > 0 && t.usage.connections >= max {
                return false;
            }
            t.usage.connections += 1;
            t.usage.last_auth_ms = Some(wall_ms);
            true
        })
    }

    /// A connection of `tenant` that [`TenantLimiter::connect`] took closed.
    pub fn disconnect(&self, tenant: u32) {
        self.with(tenant, |t| t.usage.connections = t.usage.connections.saturating_sub(1));
    }

    /// Lets a command of `tenant` that writes `bytes` begin at `now` under `limits`: the wait
    /// before it runs, or why it does not. It counts as running until [`TenantLimiter::end`].
    pub fn begin(
        &self,
        tenant: u32,
        limits: &TenantLimits,
        bytes: u64,
        now: Duration,
    ) -> Result<Duration, Refused> {
        self.with(tenant, |t| {
            // The caller waits for a slot and counts that wait itself ([`TenantLimiter::throttle`]).
            if limits.max_concurrent_ops > 0 && t.running >= limits.max_concurrent_ops {
                return Err(Refused::Concurrent);
            }
            let mut wait = Duration::ZERO;
            if limits.ops_per_second > 0 {
                let rate = f64::from(limits.ops_per_second);
                let Some(w) = t.ops.take(rate, 1.0, now) else {
                    t.usage.throttled_seconds += MAX_WAIT.as_secs_f64();
                    return Err(Refused::Ops);
                };
                wait = wait.max(w);
            }
            if limits.max_write_bytes_per_second > 0 && bytes > 0 {
                let rate = limits.max_write_bytes_per_second as f64;
                let Some(w) = t.bytes.take(rate, bytes as f64, now) else {
                    t.usage.throttled_seconds += MAX_WAIT.as_secs_f64();
                    return Err(Refused::Bytes);
                };
                wait = wait.max(w);
            }
            t.running += 1;
            t.usage.ops_total += 1;
            t.usage.throttled_seconds += wait.as_secs_f64();
            Ok(wait)
        })
    }

    /// Counts `wait` as time `tenant` was held back.
    pub fn throttle(&self, tenant: u32, wait: Duration) {
        self.with(tenant, |t| t.usage.throttled_seconds += wait.as_secs_f64());
    }

    /// A command [`TenantLimiter::begin`] let run ended.
    pub fn end(&self, tenant: u32) {
        self.with(tenant, |t| t.running = t.running.saturating_sub(1));
    }

    /// What this node counted of `tenant`.
    pub fn usage(&self, tenant: u32) -> NodeUsage {
        self.with(tenant, |t| t.usage)
    }

    /// What this node counted of every tenant it saw, by number.
    pub fn all(&self) -> Vec<(u32, NodeUsage)> {
        let g = self.tenants.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.iter().map(|(id, t)| (*id, t.usage)).collect()
    }
}

/// How often a command waiting for a slot among the tenant's commands at once looks again.
const SLOT_POLL: Duration = Duration::from_millis(5);

/// Whether `user` is the service user `limits` name, whose commands no limit counts (FR-036).
pub fn is_service_user(limits: &TenantLimits, user: &teseladb_auth::users::UserName) -> bool {
    limits.service_user.as_ref().is_some_and(|s| s.user == user.user && s.db == user.db)
}

/// The error of a command of a tenant past a limit (FR-032): 462 with the labels of the drivers'
/// `client-backpressure` specification, which they retry with a wait.
pub fn refusal(why: Refused) -> teseladb_wire::error::CommandError {
    let what = match why {
        Refused::Ops => "commands per second",
        Refused::Concurrent => "commands at once",
        Refused::Bytes => "bytes written per second",
    };
    teseladb_wire::error::CommandError::new(
        teseladb_wire::error::INGRESS_REQUEST_RATE_LIMIT_EXCEEDED,
        format!("the tenant is over its limit of {what} on this node"),
    )
    .with_label(SYSTEM_OVERLOADED)
    .with_label(RETRYABLE)
}

/// The labels of [`refusal`].
pub const SYSTEM_OVERLOADED: &str = "SystemOverloadedError";
/// See [`SYSTEM_OVERLOADED`].
pub const RETRYABLE: &str = "RetryableError";

/// Lets a command of `user` of `tenant` that writes `bytes` begin on this node: `None` when no
/// limit counts it, or the milliseconds it waited; the error when it may not run. A slot among
/// the commands at once is waited for up to [`MAX_WAIT`] too.
pub fn begin(
    routes: &crate::routes::SharedRoutes,
    tenant: u32,
    user: &teseladb_auth::users::UserName,
    bytes: u64,
) -> Result<Option<u64>, teseladb_wire::error::CommandError> {
    let states = routes.tenants();
    let Some(state) = states.get(&tenant) else { return Ok(None) };
    if is_service_user(&state.limits, user) {
        return Ok(None);
    }
    let limiter = routes.limiter();
    let started = limiter.now();
    loop {
        let slot_wait = limiter.now().saturating_sub(started);
        match limiter.begin(tenant, &state.limits, bytes, limiter.now()) {
            Ok(wait) => {
                limiter.throttle(tenant, slot_wait);
                std::thread::sleep(wait);
                let waited = limiter.now().saturating_sub(started);
                return Ok(Some(u64::try_from(waited.as_millis()).unwrap_or(u64::MAX)));
            }
            Err(Refused::Concurrent) if slot_wait < MAX_WAIT => std::thread::sleep(SLOT_POLL),
            Err(why) => {
                limiter.throttle(tenant, slot_wait);
                return Err(refusal(why));
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_bucket_lets_a_second_through_then_lends_up_to_the_wait() {
        let mut b = Bucket::default();
        let t0 = Duration::from_secs(10);
        // Full at first: 100 at once.
        for _ in 0..100 {
            assert_eq!(b.take(100.0, 1.0, t0), Some(Duration::ZERO));
        }
        // The next ten wait 10 ms more each, and the eleventh would wait past 100 ms.
        for k in 1..=10_u32 {
            assert_eq!(b.take(100.0, 1.0, t0), Some(10 * k * MS));
        }
        assert_eq!(b.take(100.0, 1.0, t0), None);
        // A second later it is full again, never more.
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(b.take(100.0, 100.0, t1), Some(Duration::ZERO));
        assert_eq!(b.take(100.0, 1.0, t1), Some(10 * MS));
        // A full bucket lets a bigger command through.
        let mut big = Bucket::default();
        assert_eq!(big.take(10.0, 50.0, t0), Some(Duration::ZERO));
        assert_eq!(big.take(10.0, 1.0, t0), None);
    }

    fn limits(ops: u32, concurrent: u32, bytes: u64) -> TenantLimits {
        TenantLimits {
            ops_per_second: ops,
            max_concurrent_ops: concurrent,
            max_write_bytes_per_second: bytes,
            ..TenantLimits::default()
        }
    }

    #[test]
    fn connections_stop_at_the_limit_and_come_back_when_one_closes() {
        let l = TenantLimiter::default();
        assert!(l.connect(1, 2, 5) && l.connect(1, 2, 6));
        assert!(!l.connect(1, 2, 7));
        l.disconnect(1);
        assert!(l.connect(1, 2, 8));
        assert!(l.connect(2, 0, 9), "0 is no limit");
        let u = l.usage(1);
        assert_eq!((u.connections, u.last_auth_ms), (2, Some(8)));
        l.disconnect(3);
        assert_eq!(l.usage(3).connections, 0);
    }

    #[test]
    fn commands_wait_for_their_token_slot_and_bytes_or_fail() {
        let l = TenantLimiter::default();
        let t0 = Duration::from_secs(1);
        // No limit: every command runs at once and counts.
        assert_eq!(l.begin(1, &TenantLimits::default(), 10, t0), Ok(Duration::ZERO));
        l.end(1);
        let two = limits(0, 2, 0);
        assert!(l.begin(2, &two, 0, t0).is_ok() && l.begin(2, &two, 0, t0).is_ok());
        assert_eq!(l.begin(2, &two, 0, t0), Err(Refused::Concurrent));
        l.end(2);
        assert_eq!(l.begin(2, &two, 0, t0), Ok(Duration::ZERO));
        let one = limits(1, 0, 0);
        assert_eq!(l.begin(3, &one, 0, t0), Ok(Duration::ZERO));
        assert_eq!(l.begin(3, &one, 0, t0), Err(Refused::Ops));
        let bytes = limits(0, 0, 1000);
        assert_eq!(l.begin(4, &bytes, 1000, t0), Ok(Duration::ZERO));
        assert_eq!(l.begin(4, &bytes, 50, t0), Ok(50 * MS));
        assert_eq!(l.begin(4, &bytes, 100, t0), Err(Refused::Bytes));
        assert_eq!(l.begin(4, &bytes, 0, t0), Ok(Duration::ZERO), "a read spends no bytes");
        let u = l.usage(4);
        assert_eq!(u.ops_total, 3);
        assert!((u.throttled_seconds - 0.15).abs() < 1e-9, "{u:?}");
        assert_eq!(l.all().len(), 4);
    }
}
