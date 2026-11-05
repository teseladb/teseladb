//! The 9.0 limiter of new connections (006 FR-036), as a `refnode` 9.0.4 behaves (measured on
//! 05-11-2026). With it on, a connection takes a token from a bucket when its first message
//! arrives; the bucket holds `RatePerSec × BurstCapacitySecs` tokens, starts full and gains
//! `RatePerSec` a second. Without a token the connection waits its turn in a queue of
//! `MaxQueueDepth` places; with the queue full it is closed without a reply. The addresses of the
//! bypass list take no token. Sans-I/O: the caller gives the time and the peer's address.

use core::net::IpAddr;

use teseladb_bson::{Document, Value};

use crate::error::{
    BAD_VALUE, CommandError, EXPECTED_OBJECT, IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, TYPE_MISMATCH,
};

/// Whether the limiter is on.
pub const ENABLED: &str = "ingressConnectionEstablishmentRateLimiterEnabled";
/// The tokens the bucket gains a second.
pub const RATE_PER_SEC: &str = "ingressConnectionEstablishmentRatePerSec";
/// The seconds of tokens the bucket holds.
pub const BURST_CAPACITY_SECS: &str = "ingressConnectionEstablishmentBurstCapacitySecs";
/// The connections that may wait for a token.
pub const MAX_QUEUE_DEPTH: &str = "ingressConnectionEstablishmentMaxQueueDepth";
/// The addresses that take no token.
pub const BYPASS: &str = "ingressConnectionEstablishmentRateLimiterBypass";
/// The five, in name order.
pub const NAMES: [&str; 5] = [BURST_CAPACITY_SECS, MAX_QUEUE_DEPTH, BYPASS, ENABLED, RATE_PER_SEC];

/// The field of the bypass list.
const RANGES: &str = "ranges";
/// The name RefDb's errors give the bypass list's document.
const RANGES_STRUCT: &str = "CIDRRangeListParameters";
const MICROS_PER_SEC: f64 = 1_000_000.0;

/// A new value of one of the parameters, checked.
#[derive(Clone, Debug, PartialEq)]
pub enum Setting {
    /// [`ENABLED`].
    Enabled(bool),
    /// [`RATE_PER_SEC`].
    RatePerSec(i32),
    /// [`BURST_CAPACITY_SECS`].
    BurstSecs(f64),
    /// [`MAX_QUEUE_DEPTH`].
    QueueDepth(i32),
    /// [`BYPASS`], as given.
    Bypass(Vec<String>),
}

// A burst is never NaN ([`parse`] refuses it), so equality is reflexive.
impl Eq for Setting {}

/// What happens to a connection's first message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// It runs now.
    Go,
    /// It runs after `micros` microseconds, its place in the queue.
    Wait {
        /// How long it waits.
        micros: u64,
    },
    /// The connection closes without a reply.
    Reject,
}

/// What `serverStatus` counts of the limiter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngressCounts {
    /// Connections closed for lack of a token.
    pub rejected: i64,
    /// Connections of the bypass list.
    pub exempted: i64,
    /// Connections whose client left while they waited.
    pub interrupted: i64,
    /// Connections waiting now.
    pub queued: i64,
}

/// The limiter of a node: its parameters, its bucket and its counters.
#[derive(Clone, Debug, PartialEq)]
pub struct Limiter {
    enabled: bool,
    rate: i32,
    burst_secs: f64,
    queue_depth: i32,
    bypass: Vec<String>,
    /// Tokens now; below zero while connections wait for the ones they reserved.
    tokens: f64,
    /// When `tokens` was last refilled.
    refilled_us: Option<u64>,
    counts: IngressCounts,
}

impl Default for Limiter {
    fn default() -> Limiter {
        let mut l = Limiter {
            enabled: false,
            rate: i32::MAX,
            burst_secs: f64::MAX,
            queue_depth: 0,
            bypass: Vec::new(),
            tokens: 0.0,
            refilled_us: None,
            counts: IngressCounts::default(),
        };
        l.tokens = l.capacity();
        l
    }
}

impl Limiter {
    /// The tokens the bucket holds when full.
    pub fn capacity(&self) -> f64 {
        f64::from(self.rate) * self.burst_secs
    }

    /// The value `getParameter` shows for `name`, one of [`NAMES`].
    pub fn value(&self, name: &str) -> Value {
        match name {
            ENABLED => Value::Boolean(self.enabled),
            RATE_PER_SEC => Value::Int32(self.rate),
            BURST_CAPACITY_SECS => Value::Double(self.burst_secs),
            MAX_QUEUE_DEPTH => Value::Int32(self.queue_depth),
            _ => {
                let ranges = self.bypass.iter().map(|r| Value::from(r.as_str())).collect();
                Value::Document(Document::new().with(RANGES, Value::Array(ranges)))
            }
        }
    }

    /// Applies `s`; the bucket keeps its tokens, up to the new capacity.
    pub fn set(&mut self, s: Setting) {
        match s {
            Setting::Enabled(on) => self.enabled = on,
            Setting::RatePerSec(n) => self.rate = n,
            Setting::BurstSecs(x) => self.burst_secs = x,
            Setting::QueueDepth(n) => self.queue_depth = n,
            Setting::Bypass(ranges) => self.bypass = ranges,
        }
        self.tokens = self.tokens.min(self.capacity());
    }

    /// What happens to the first message of a connection from `peer` at `now_us`.
    pub fn admit(&mut self, peer: Option<IpAddr>, now_us: u64) -> Admission {
        if !self.enabled {
            return Admission::Go;
        }
        if peer.is_some_and(|ip| self.bypass.iter().any(|r| in_range(ip, r))) {
            self.counts.exempted += 1;
            return Admission::Go;
        }
        self.refill(now_us);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Admission::Go;
        }
        // A bucket that never holds a whole token lets nothing through.
        if self.capacity() < 1.0 || self.counts.queued >= i64::from(self.queue_depth) {
            self.counts.rejected += 1;
            return Admission::Reject;
        }
        self.tokens -= 1.0;
        self.counts.queued += 1;
        let micros = (-self.tokens / f64::from(self.rate) * MICROS_PER_SEC).ceil();
        Admission::Wait { micros: micros as u64 }
    }

    /// A waiting connection's turn came.
    pub fn dequeue(&mut self) {
        self.counts.queued = (self.counts.queued - 1).max(0);
    }

    /// A waiting connection's client left.
    pub fn abandon(&mut self) {
        self.dequeue();
        self.counts.interrupted += 1;
    }

    /// The counters.
    pub fn counts(&self) -> IngressCounts {
        self.counts
    }

    fn refill(&mut self, now_us: u64) {
        let since = self.refilled_us.map_or(0, |at| now_us.saturating_sub(at));
        self.refilled_us = Some(now_us);
        let gained = since as f64 / MICROS_PER_SEC * f64::from(self.rate);
        self.tokens = (self.tokens + gained).min(self.capacity());
    }
}

/// Whether `ip` is in `range`, an address or a CIDR range; a range that does not read holds
/// nothing (RefDb keeps it as given).
fn in_range(ip: IpAddr, range: &str) -> bool {
    let (addr, bits) = match range.split_once('/') {
        Some((a, b)) => (a, b.parse::<u32>().ok()),
        None => (range, None),
    };
    let Ok(net) = addr.parse::<IpAddr>() else { return false };
    let (a, b, width) = match (ip, net) {
        (IpAddr::V4(x), IpAddr::V4(y)) => (u128::from(x.to_bits()), u128::from(y.to_bits()), 32),
        (IpAddr::V6(x), IpAddr::V6(y)) => (x.to_bits(), y.to_bits(), 128),
        _ => return false,
    };
    let bits = match (range.contains('/'), bits) {
        (false, _) => width,
        (true, Some(n)) if n <= width => n,
        (true, _) => return false,
    };
    let shift = width - bits;
    shift == width || (a >> shift) == (b >> shift)
}

/// The new value `v` of parameter `name`, one of [`NAMES`], checked and coerced as a `refnode`
/// 9.0.4 does; `integral` reads an integer as the other parameters do.
pub fn parse(
    name: &str,
    v: &Value,
    integral: impl Fn(&str, &Value) -> Result<i64, CommandError>,
) -> Result<Setting, CommandError> {
    match name {
        ENABLED => Ok(Setting::Enabled(crate::types::bson_true(v))),
        RATE_PER_SEC => int32(name, integral(name, v)?, 1).map(Setting::RatePerSec),
        MAX_QUEUE_DEPTH => int32(name, integral(name, v)?, 0).map(Setting::QueueDepth),
        BURST_CAPACITY_SECS => burst(v).map(Setting::BurstSecs),
        _ => bypass(v).map(Setting::Bypass),
    }
}

/// An int32 parameter's value `n`, at least `least`.
fn int32(name: &str, n: i64, least: i32) -> Result<i32, CommandError> {
    let Ok(n) = i32::try_from(n) else {
        let msg = format!("Failed validating {name}: Out of bounds coercing to integral value");
        return Err(CommandError::new(BAD_VALUE, msg));
    };
    if n < least {
        let msg = format!(
            "Invalid value for parameter {name}: {n} is not greater than or equal to {least}"
        );
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    Ok(n)
}

/// A new [`BURST_CAPACITY_SECS`]: a number above 0 (infinity too).
fn burst(v: &Value) -> Result<f64, CommandError> {
    let x = match v {
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => {
            crate::types::number(v)
        }
        _ => None,
    };
    let Some(x) = x else {
        let msg = format!(
            "Failed validating {BURST_CAPACITY_SECS}: Unable to coerce value to correct type"
        );
        return Err(CommandError::new(BAD_VALUE, msg));
    };
    // NaN is not above 0 either.
    if x > 0.0 {
        return Ok(x);
    }
    let shown = if x.is_nan() { "nan".to_owned() } else { x.to_string() };
    let msg =
        format!("Invalid value for parameter {BURST_CAPACITY_SECS}: {shown} is not greater than 0");
    Err(CommandError::new(BAD_VALUE, msg))
}

/// A new [`BYPASS`]: `{ranges: [<string>, ...]}`, the strings kept as given.
fn bypass(v: &Value) -> Result<Vec<String>, CommandError> {
    let Value::Document(d) = v else {
        let msg = format!("invalid parameter: expected an object ({BYPASS})");
        return Err(CommandError::new(EXPECTED_OBJECT, msg));
    };
    let field = |what: &str| format!("BSON field '{RANGES_STRUCT}.{what}");
    if let Some((k, _)) = d.iter().find(|(k, _)| *k != RANGES) {
        let msg = format!("{}' is an unknown field.", field(k));
        return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg));
    }
    let wrong = |what: &str, got: &Value, want: &str| {
        let got = crate::types::type_name(got);
        let msg = format!("{}' is the wrong type '{got}', expected type '{want}'", field(what));
        CommandError::new(TYPE_MISMATCH, msg)
    };
    let items = match d.get(RANGES) {
        None => {
            let msg = format!("{}' is missing but a required field", field(RANGES));
            return Err(CommandError::new(IDL_FAILED_TO_PARSE, msg));
        }
        Some(Value::Array(items)) => items,
        Some(other) => return Err(wrong(RANGES, other, "array")),
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| match item {
            Value::String(s) => Ok(s.clone()),
            other => Err(wrong(&format!("{RANGES}.{i}"), other, "string")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(rate: i32, burst: f64, depth: i32) -> Limiter {
        let mut l = Limiter::default();
        for s in [
            Setting::RatePerSec(rate),
            Setting::BurstSecs(burst),
            Setting::QueueDepth(depth),
            Setting::Enabled(true),
        ] {
            l.set(s);
        }
        l
    }

    fn ip(text: &str) -> Option<IpAddr> {
        text.parse().ok()
    }

    #[test]
    fn the_bucket_refills_at_its_rate_up_to_its_capacity() {
        let mut l = on(2, 1.0, 0);
        let peer = ip("10.0.0.1");
        assert_eq!(l.admit(peer, 0), Admission::Go);
        assert_eq!(l.admit(peer, 0), Admission::Go);
        assert_eq!(l.admit(peer, 0), Admission::Reject);
        // Half a second gives one token back.
        assert_eq!(l.admit(peer, 500_000), Admission::Go);
        assert_eq!(l.admit(peer, 500_000), Admission::Reject);
        // A long pause fills it, no further.
        assert_eq!(l.admit(peer, 60_000_000), Admission::Go);
        assert_eq!(l.admit(peer, 60_000_000), Admission::Go);
        assert_eq!(l.admit(peer, 60_000_000), Admission::Reject);
        assert_eq!(l.counts().rejected, 3);
    }

    #[test]
    fn a_bucket_under_one_token_rejects_every_connection() {
        // Rate 1 with half a second of burst, measured: everything is closed.
        let mut l = on(1, 0.5, 4);
        for t in [0, 10_000_000, 20_000_000] {
            assert_eq!(l.admit(ip("10.0.0.1"), t), Admission::Reject);
        }
    }

    #[test]
    fn a_smaller_capacity_drops_the_tokens_above_it() {
        // The default bucket is endless until a rate and a burst make it finite.
        let mut l = Limiter::default();
        assert_eq!(l.capacity(), f64::INFINITY);
        l.set(Setting::Enabled(true));
        l.set(Setting::RatePerSec(1));
        l.set(Setting::BurstSecs(1.0));
        assert_eq!(l.admit(ip("10.0.0.1"), 0), Admission::Go);
        assert_eq!(l.admit(ip("10.0.0.1"), 0), Admission::Reject);
    }

    #[test]
    fn the_queue_releases_one_place_per_turn_and_never_goes_below_zero() {
        let mut l = on(1, 1.0, 1);
        assert_eq!(l.admit(ip("10.0.0.1"), 0), Admission::Go);
        assert_eq!(l.admit(ip("10.0.0.1"), 0), Admission::Wait { micros: 1_000_000 });
        assert_eq!(l.admit(ip("10.0.0.1"), 0), Admission::Reject);
        l.dequeue();
        l.dequeue();
        assert_eq!(l.counts().queued, 0);
    }

    #[test]
    fn ranges_hold_their_addresses_of_the_same_family() {
        let a = |s: &str| s.parse::<IpAddr>().expect("an address");
        assert!(in_range(a("10.1.2.3"), "10.0.0.0/8"));
        assert!(!in_range(a("11.1.2.3"), "10.0.0.0/8"));
        assert!(in_range(a("10.1.2.3"), "10.1.2.3"));
        assert!(!in_range(a("10.1.2.4"), "10.1.2.3"));
        assert!(in_range(a("1.2.3.4"), "0.0.0.0/0"));
        assert!(in_range(a("fe80::1"), "fe80::/10"));
        assert!(!in_range(a("10.1.2.3"), "::/0"));
        assert!(!in_range(a("10.1.2.3"), "10.0.0.0/33"));
        assert!(!in_range(a("10.1.2.3"), "10.0.0.0/x"));
        assert!(!in_range(a("10.1.2.3"), "nope"));
    }

    #[test]
    fn every_parameter_shows_its_value() {
        let mut l = Limiter::default();
        l.set(Setting::Bypass(vec!["10.0.0.0/8".to_owned()]));
        let shown: Vec<Value> = NAMES.iter().map(|n| l.value(n)).collect();
        let ranges = Document::new().with(RANGES, Value::Array(vec![Value::from("10.0.0.0/8")]));
        let expected = [
            Value::Double(f64::MAX),
            Value::Int32(0),
            Value::Document(ranges),
            Value::Boolean(false),
            Value::Int32(i32::MAX),
        ];
        assert_eq!(shown, expected);
    }
}
