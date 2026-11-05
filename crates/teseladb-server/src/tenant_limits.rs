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
    /// The bytes this node let the tenant write since the last read of its data (FR-031).
    written: u64,
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

    /// Lets `tenant` write `bytes` more on this node if what it wrote since the last read of its
    /// data stays within `share`; `false` refuses the write.
    pub fn write_within(&self, tenant: u32, bytes: u64, share: u64) -> bool {
        self.with(tenant, |t| {
            if t.written.saturating_add(bytes) > share {
                return false;
            }
            t.written += bytes;
            true
        })
    }

    /// The tenants' data was read again: what each wrote starts from zero.
    pub fn reset_written(&self) {
        let mut g = self.tenants.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for t in g.values_mut() {
            t.written = 0;
        }
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

/// How often each node reads again the data each tenant uses (FR-031).
pub const USAGE_EVERY: Duration = Duration::from_secs(5);

/// The first key of the data of `tenant`: documents and index entries (007 FR-012).
pub fn tenant_start(tenant: u32) -> Vec<u8> {
    let mut k = vec![teseladb_keys::space::DATA];
    k.extend_from_slice(&tenant.to_be_bytes());
    k
}

/// The first key past the data of `tenant`.
pub fn tenant_end(tenant: u32) -> Vec<u8> {
    match tenant.checked_add(1) {
        Some(next) => tenant_start(next),
        None => vec![teseladb_keys::space::DATA + 1],
    }
}

/// The data `tenant` uses in the cluster (FR-031): the live bytes of each range inside its key
/// interval, as its state machine keeps them, and the stored bytes of the part of a range that
/// holds other data too.
pub fn storage_of<B: teseladb_wire::commands::Backend + ?Sized>(
    b: &mut B,
    tenant: u32,
) -> Result<u64, teseladb_wire::error::CommandError> {
    let start = tenant_start(tenant);
    let end = tenant_end(tenant);
    let mut total = 0u64;
    for e in b.route_entries()? {
        // An empty end is no end.
        let range_end = if e.end.is_empty() { None } else { Some(e.end.as_slice()) };
        if range_end.is_some_and(|re| re <= start.as_slice())
            || e.start.as_slice() >= end.as_slice()
        {
            continue;
        }
        let contained = e.start >= start && range_end.is_some_and(|re| re <= end.as_slice());
        total += if contained {
            b.range_live_bytes(&e)?
        } else {
            let from = e.start.clone().max(start.clone());
            let to = range_end.map_or(end.clone(), |re| re.min(end.as_slice()).to_vec());
            b.stored_bytes(&from, &to)?
        };
    }
    Ok(total)
}

/// Reads again the data each tenant uses and gives every tenant a new margin on this node. A
/// tenant whose read fails (no leader for a range) keeps its last value.
pub fn read_usage(backend: &mut crate::backend::ServerBackend) {
    let tenants: Vec<u32> = backend.routes().tenants().keys().copied().collect();
    if tenants.is_empty() {
        return;
    }
    let mut usage = (*backend.routes().usage()).clone();
    for t in tenants {
        if let Ok(bytes) = storage_of(backend, t) {
            usage.insert(t, bytes);
        }
        teseladb_wire::commands::Backend::end_operation(backend);
    }
    backend.routes().publish_usage(std::sync::Arc::new(usage));
    backend.routes().limiter().reset_written();
}

/// The thread of each node that reads the tenants' data every [`USAGE_EVERY`] until `stop`: on its
/// own, so that no other periodic task of the node holds it back.
pub fn spawn_reader(
    mut backend: crate::backend::ServerBackend,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("tenant-usage".into()).spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            read_usage(&mut backend);
            std::thread::park_timeout(USAGE_EVERY);
        }
    })
}

/// A tenant's name as the value of a Prometheus label: backslash, quote and line break escaped.
pub fn label(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// The series of each tenant but the default one on this node (FR-033): the data it uses in the
/// cluster, and this node's connections, commands served, seconds held back and last sign-in.
pub fn metrics(routes: &crate::routes::SharedRoutes, out: &mut String) {
    use std::fmt::Write as _;
    let usage = routes.usage();
    let rows: Vec<(String, u64, NodeUsage)> = routes
        .tenants()
        .keys()
        .filter_map(|id| {
            let name = routes.tenant_name(*id)?;
            Some((label(&name), usage.get(id).copied().unwrap_or(0), routes.limiter().usage(*id)))
        })
        .collect();
    let series: [(&str, &str, &str); 5] = [
        (
            "teseladb_tenant_storage_bytes",
            "gauge",
            "Bytes of data and indexes the tenant uses in the cluster.",
        ),
        (
            "teseladb_tenant_connections",
            "gauge",
            "Authenticated connections of the tenant on this node, without its service user's.",
        ),
        (
            "teseladb_tenant_ops_total",
            "counter",
            "Commands this node served the tenant, without its service user's.",
        ),
        (
            "teseladb_tenant_throttled_seconds_total",
            "counter",
            "Seconds the tenant's commands waited or were refused for its limits on this node.",
        ),
        (
            "teseladb_tenant_last_auth_timestamp_seconds",
            "gauge",
            "When a user of the tenant last signed in on this node.",
        ),
    ];
    for (i, (name, kind, help)) in series.iter().enumerate() {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (tenant, storage, u) in &rows {
            let value = match i {
                0 => storage.to_string(),
                1 => u.connections.to_string(),
                2 => u.ops_total.to_string(),
                3 => format!("{:.3}", u.throttled_seconds),
                _ => match u.last_auth_ms {
                    Some(ms) => format!("{:.3}", ms as f64 / 1000.0),
                    None => continue,
                },
            };
            let _ = writeln!(out, "{name}{{tenant=\"{tenant}\"}} {value}");
        }
    }
}

/// `GET /admin/tenants/<name>/usage` of tenant `id` (FR-033): the data it uses in the cluster and
/// what this node counted of it.
pub fn usage_doc(routes: &crate::routes::SharedRoutes, id: u32) -> teseladb_bson::Document {
    let u = routes.limiter().usage(id);
    let storage = routes.usage().get(&id).copied().unwrap_or(0);
    let last = u.last_auth_ms.map_or(teseladb_bson::Value::Null, |ms| {
        teseladb_bson::Value::DateTime(i64::try_from(ms).unwrap_or(i64::MAX))
    });
    teseladb_bson::Document::new()
        .with("storageBytes", i64::try_from(storage).unwrap_or(i64::MAX))
        .with("connections", i64::from(u.connections))
        .with("opsTotal", i64::try_from(u.ops_total).unwrap_or(i64::MAX))
        .with("throttledSeconds", u.throttled_seconds)
        .with("lastAuth", last)
        .with("ok", 1.0)
}

/// The error of a write of tenant `name` past its storage quota (FR-031).
pub fn over_quota(name: &str, used: u64, limit: u64) -> teseladb_wire::error::CommandError {
    teseladb_wire::error::CommandError::new(
        teseladb_wire::error::OUT_OF_DISK_SPACE,
        format!("tenant {name} is over its storage quota: {used} of {limit} bytes"),
    )
}

/// Whether a write of `bytes` of `tenant` fits in its storage quota on this node: the data it
/// uses below the limit, and what this node let it write since the last read within its part of
/// the margin, `(limit - used) / nodes` (FR-031).
fn check_quota(
    routes: &crate::routes::SharedRoutes,
    tenant: u32,
    limit: u64,
    bytes: u64,
) -> Result<(), teseladb_wire::error::CommandError> {
    let used = routes.usage().get(&tenant).copied().unwrap_or(0);
    let name = || routes.tenant_name(tenant).unwrap_or_else(|| tenant.to_string());
    if used >= limit {
        return Err(over_quota(&name(), used, limit));
    }
    let share = (limit - used) / routes.nodes() as u64;
    if !routes.limiter().write_within(tenant, bytes, share) {
        return Err(over_quota(&name(), used, limit));
    }
    Ok(())
}

/// Lets a command of `user` of `tenant` that writes `bytes` begin on this node: `None` when no
/// limit counts it, or the milliseconds it waited; the error when it may not run. A command that
/// makes the tenant's data grow (`grows`) fits in its storage quota first. A slot among the
/// commands at once is waited for up to [`MAX_WAIT`] too.
pub fn begin(
    routes: &crate::routes::SharedRoutes,
    tenant: u32,
    user: &teseladb_auth::users::UserName,
    bytes: u64,
    grows: bool,
) -> Result<Option<u64>, teseladb_wire::error::CommandError> {
    let states = routes.tenants();
    let Some(state) = states.get(&tenant) else { return Ok(None) };
    if grows && state.limits.storage_bytes > 0 {
        check_quota(routes, tenant, state.limits.storage_bytes, bytes)?;
    }
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
