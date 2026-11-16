//! State of the per-shard collectd sender: the UDP channel, the send timer, the destination and
//! the sender's own statistics.
//!
//! The sending logic lives in [`scollectd`](crate::engine::core::scollectd); this module holds
//! the [`Impl`] structure and its defaults.

use std::sync::LazyLock;

use crate::engine::core::metrics;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::timer::{SteadyClockType, Timer};
use crate::engine::net::api::DatagramChannel;
use crate::engine::net::socket_defs::Ipv4Addr;

/// Duration type of the polling period (millisecond granularity in practice).
pub type Duration = std::time::Duration;

/// The default collectd multicast destination, 239.192.74.66 port 25826; the address is kept in
/// host byte order, as [`Ipv4Addr`] stores it.
pub static DEFAULT_ADDR: LazyLock<Ipv4Addr> =
    LazyLock::new(|| Ipv4Addr::new(u32::from_be_bytes([239, 192, 74, 66]), 25826));
/// The default polling period.
pub const DEFAULT_PERIOD: Duration = Duration::from_secs(1);

/// The collectd sender of a shard.
pub struct Impl {
    pub(crate) _chan: DatagramChannel,
    pub(crate) _timer: Timer<SteadyClockType>,
    pub(crate) _host: Sstring,
    pub(crate) _addr: Ipv4Addr,
    pub(crate) _period: Duration,
    pub(crate) _num_packets: u64,
    pub(crate) _millis: u64,
    pub(crate) _bytes: u64,
    pub(crate) _avg: f64,
    pub(crate) _metrics: metrics::MetricGroups,
}

/// The values of every registered series, grouped by family.
pub type ValueListMap = metrics::impl_::ValueMap;

impl Impl {
    /// A sender that is not started: host `"localhost"`, [`DEFAULT_ADDR`], [`DEFAULT_PERIOD`]
    /// and zeroed statistics.
    pub fn new() -> Self {
        Self {
            _chan: DatagramChannel::default(),
            _timer: Timer::new(),
            _host: "localhost".into(),
            _addr: *DEFAULT_ADDR,
            _period: DEFAULT_PERIOD,
            _num_packets: 0,
            _millis: 0,
            _bytes: 0,
            _avg: 0.0,
            _metrics: metrics::MetricGroups::new(),
        }
    }
}

impl Default for Impl {
    fn default() -> Self {
        Self::new()
    }
}

/// This shard's sender.
pub use crate::engine::core::scollectd::get_impl;
