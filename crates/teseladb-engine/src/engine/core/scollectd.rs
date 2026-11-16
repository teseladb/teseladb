//! A collectd exporter: each shard periodically sends its registered values to a collectd
//! server over UDP, in the collectd binary network protocol.
//!
//! A value is identified by the collectd naming schema, the tuple
//! `{plugin, plugin_instance, type, type_instance}`: the plugin is the overall module (for
//! example `"cpu"`), the plugin instance tells instances of the plugin apart (for the CPU, the
//! shard number; empty when there is only one), the type is what is measured (`"usage"`), and
//! the type instance names a part of it (`"idle"`, `"user"`): `cpu/0/usage/idle`. Each type
//! instance can bind any number of values, whose meaning is up to the producer and consumer.
//! The tuple is a unique id: registering the same tuple again replaces the previous values.
//!
//! There is one sender per shard ([`get_impl`]), and values are bound to the shard that
//! registers them. Polling happens at the configured period (once per second by default), and
//! every registered value is sent in UDP packets of at most 1024 bytes to the destination
//! (by default the collectd multicast group). Values can be unregistered at any time, on the
//! shard that registered them; [`Registration`] unregisters them when dropped. The values are
//! also registered with the [`metrics`] layer, so the Prometheus
//! exporter sees them too.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::LazyLock;

use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::loop_::do_until;
use crate::engine::core::metrics::impl_ as mi;
use crate::engine::core::metrics::{self, Description as MetricsDescription};
use crate::engine::core::scollectd_impl::Impl;
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::shared_ptr::{SharedPtr, make_lw_shared, make_shared};
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::{Sstring, to_sstring};
use crate::engine::core::timer::{SteadyClockType, Timer};
use crate::engine::net::packet::Packet;
use crate::engine::net::socket_defs::Ipv4Addr;
use crate::engine::net::socket_defs::SocketAddress;
use crate::engine::util::log::Logger;
use crate::engine::util::program_options::{OptionGroup, Value as OptionValue};
use crate::log_warn;

const _SCOLLECTD_DOC: () = ();

/// The "scollectd" logger.
pub static LOGGER: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("scollectd").expect("logger registered once"));

/// The kind of a value (gauge, counter, ...), shared with the metrics layer.
pub type DataType = mi::DataType;

/// The collectd types of `types.db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KnownType {
    /// `absolute`
    Absolute,
    /// `backends`
    Backends,
    /// `bitrate`
    Bitrate,
    /// `blocked_clients`
    BlockedClients,
    /// `bytes`
    Bytes,
    /// `cache_eviction`
    CacheEviction,
    /// `cache_operation`
    CacheOperation,
    /// `cache_ratio`
    CacheRatio,
    /// `cache_result`
    CacheResult,
    /// `cache_size`
    CacheSize,
    /// `capacity`
    Capacity,
    /// `changes_since_last_save`
    ChangesSinceLastSave,
    /// `charge`
    Charge,
    /// `clock_last_meas`
    ClockLastMeas,
    /// `clock_last_update`
    ClockLastUpdate,
    /// `clock_mode`
    ClockMode,
    /// `clock_reachability`
    ClockReachability,
    /// `clock_skew_ppm`
    ClockSkewPpm,
    /// `clock_state`
    ClockState,
    /// `clock_stratum`
    ClockStratum,
    /// `compression`
    Compression,
    /// `compression_ratio`
    CompressionRatio,
    /// `connections`
    Connections,
    /// `conntrack`
    Conntrack,
    /// `contextswitch`
    Contextswitch,
    /// `count`
    Count,
    /// `counter`
    Counter,
    /// `cpu`
    Cpu,
    /// `cpufreq`
    Cpufreq,
    /// `current`
    Current,
    /// `current_connections`
    CurrentConnections,
    /// `current_sessions`
    CurrentSessions,
    /// `delay`
    Delay,
    /// `derive`
    Derive,
    /// `df`
    Df,
    /// `df_complex`
    DfComplex,
    /// `df_inodes`
    DfInodes,
    /// `disk_io_time`
    DiskIoTime,
    /// `disk_latency`
    DiskLatency,
    /// `disk_merged`
    DiskMerged,
    /// `disk_octets`
    DiskOctets,
    /// `disk_ops`
    DiskOps,
    /// `disk_ops_complex`
    DiskOpsComplex,
    /// `disk_time`
    DiskTime,
    /// `dns_answer`
    DnsAnswer,
    /// `dns_notify`
    DnsNotify,
    /// `dns_octets`
    DnsOctets,
    /// `dns_opcode`
    DnsOpcode,
    /// `dns_qtype`
    DnsQtype,
    /// `dns_qtype_cached`
    DnsQtypeCached,
    /// `dns_query`
    DnsQuery,
    /// `dns_question`
    DnsQuestion,
    /// `dns_rcode`
    DnsRcode,
    /// `dns_reject`
    DnsReject,
    /// `dns_request`
    DnsRequest,
    /// `dns_resolver`
    DnsResolver,
    /// `dns_response`
    DnsResponse,
    /// `dns_transfer`
    DnsTransfer,
    /// `dns_update`
    DnsUpdate,
    /// `dns_zops`
    DnsZops,
    /// `drbd_resource`
    DrbdResource,
    /// `duration`
    Duration,
    /// `email_check`
    EmailCheck,
    /// `email_count`
    EmailCount,
    /// `email_size`
    EmailSize,
    /// `entropy`
    Entropy,
    /// `evicted_keys`
    EvictedKeys,
    /// `expired_keys`
    ExpiredKeys,
    /// `fanspeed`
    Fanspeed,
    /// `file_handles`
    FileHandles,
    /// `file_size`
    FileSize,
    /// `files`
    Files,
    /// `flow`
    Flow,
    /// `fork_rate`
    ForkRate,
    /// `frequency`
    Frequency,
    /// `frequency_error`
    FrequencyError,
    /// `frequency_offset`
    FrequencyOffset,
    /// `fscache_stat`
    FscacheStat,
    /// `gauge`
    Gauge,
    /// `hash_collisions`
    HashCollisions,
    /// `http_request_methods`
    HttpRequestMethods,
    /// `http_requests`
    HttpRequests,
    /// `http_response_codes`
    HttpResponseCodes,
    /// `humidity`
    Humidity,
    /// `if_collisions`
    IfCollisions,
    /// `if_dropped`
    IfDropped,
    /// `if_errors`
    IfErrors,
    /// `if_multicast`
    IfMulticast,
    /// `if_octets`
    IfOctets,
    /// `if_packets`
    IfPackets,
    /// `if_rx_errors`
    IfRxErrors,
    /// `if_rx_octets`
    IfRxOctets,
    /// `if_tx_errors`
    IfTxErrors,
    /// `if_tx_octets`
    IfTxOctets,
    /// `invocations`
    Invocations,
    /// `io_octets`
    IoOctets,
    /// `io_packets`
    IoPackets,
    /// `ipt_bytes`
    IptBytes,
    /// `ipt_packets`
    IptPackets,
    /// `irq`
    Irq,
    /// `latency`
    Latency,
    /// `links`
    Links,
    /// `load`
    Load,
    /// `md_disks`
    MdDisks,
    /// `memory`
    Memory,
    /// `memory_lua`
    MemoryLua,
    /// `memory_throttle_count`
    MemoryThrottleCount,
    /// `multimeter`
    Multimeter,
    /// `mutex_operations`
    MutexOperations,
    /// `objects`
    Objects,
    /// `operations`
    Operations,
    /// `packets`
    Packets,
    /// `pending_operations`
    PendingOperations,
    /// `percent`
    Percent,
    /// `percent_bytes`
    PercentBytes,
    /// `percent_inodes`
    PercentInodes,
    /// `ping`
    Ping,
    /// `ping_droprate`
    PingDroprate,
    /// `ping_stddev`
    PingStddev,
    /// `players`
    Players,
    /// `power`
    Power,
    /// `pressure`
    Pressure,
    /// `protocol_counter`
    ProtocolCounter,
    /// `pubsub`
    Pubsub,
    /// `queue_length`
    QueueLength,
    /// `records`
    Records,
    /// `requests`
    Requests,
    /// `response_code`
    ResponseCode,
    /// `response_time`
    ResponseTime,
    /// `root_delay`
    RootDelay,
    /// `root_dispersion`
    RootDispersion,
    /// `route_etx`
    RouteEtx,
    /// `route_metric`
    RouteMetric,
    /// `routes`
    Routes,
    /// `segments`
    Segments,
    /// `serial_octets`
    SerialOctets,
    /// `signal_noise`
    SignalNoise,
    /// `signal_power`
    SignalPower,
    /// `signal_quality`
    SignalQuality,
    /// `snr`
    Snr,
    /// `spl`
    Spl,
    /// `swap`
    Swap,
    /// `swap_io`
    SwapIo,
    /// `tcp_connections`
    TcpConnections,
    /// `temperature`
    Temperature,
    /// `threads`
    Threads,
    /// `time_dispersion`
    TimeDispersion,
    /// `time_offset`
    TimeOffset,
    /// `time_offset_ntp`
    TimeOffsetNtp,
    /// `time_offset_rms`
    TimeOffsetRms,
    /// `time_ref`
    TimeRef,
    /// `timeleft`
    Timeleft,
    /// `total_bytes`
    TotalBytes,
    /// `total_connections`
    TotalConnections,
    /// `total_objects`
    TotalObjects,
    /// `total_operations`
    TotalOperations,
    /// `total_requests`
    TotalRequests,
    /// `total_sessions`
    TotalSessions,
    /// `total_threads`
    TotalThreads,
    /// `total_time_in_ms`
    TotalTimeInMs,
    /// `total_values`
    TotalValues,
    /// `uptime`
    Uptime,
    /// `users`
    Users,
    /// `vcl`
    Vcl,
    /// `vcpu`
    Vcpu,
    /// `virt_cpu_total`
    VirtCpuTotal,
    /// `virt_vcpu`
    VirtVcpu,
    /// `vmpage_action`
    VmpageAction,
    /// `vmpage_faults`
    VmpageFaults,
    /// `vmpage_io`
    VmpageIo,
    /// `vmpage_number`
    VmpageNumber,
    /// `volatile_changes`
    VolatileChanges,
    /// `voltage`
    Voltage,
    /// `voltage_threshold`
    VoltageThreshold,
    /// `vs_memory`
    VsMemory,
    /// `vs_processes`
    VsProcesses,
    /// `vs_threads`
    VsThreads,
}

/// The collectd type name of `t`.
pub fn type_id_for(t: KnownType) -> TypeId {
    let name = match t {
        KnownType::Absolute => "absolute",
        KnownType::Backends => "backends",
        KnownType::Bitrate => "bitrate",
        KnownType::BlockedClients => "blocked_clients",
        KnownType::Bytes => "bytes",
        KnownType::CacheEviction => "cache_eviction",
        KnownType::CacheOperation => "cache_operation",
        KnownType::CacheRatio => "cache_ratio",
        KnownType::CacheResult => "cache_result",
        KnownType::CacheSize => "cache_size",
        KnownType::Capacity => "capacity",
        KnownType::ChangesSinceLastSave => "changes_since_last_save",
        KnownType::Charge => "charge",
        KnownType::ClockLastMeas => "clock_last_meas",
        KnownType::ClockLastUpdate => "clock_last_update",
        KnownType::ClockMode => "clock_mode",
        KnownType::ClockReachability => "clock_reachability",
        KnownType::ClockSkewPpm => "clock_skew_ppm",
        KnownType::ClockState => "clock_state",
        KnownType::ClockStratum => "clock_stratum",
        KnownType::Compression => "compression",
        KnownType::CompressionRatio => "compression_ratio",
        KnownType::Connections => "connections",
        KnownType::Conntrack => "conntrack",
        KnownType::Contextswitch => "contextswitch",
        KnownType::Count => "count",
        KnownType::Counter => "counter",
        KnownType::Cpu => "cpu",
        KnownType::Cpufreq => "cpufreq",
        KnownType::Current => "current",
        KnownType::CurrentConnections => "current_connections",
        KnownType::CurrentSessions => "current_sessions",
        KnownType::Delay => "delay",
        KnownType::Derive => "derive",
        KnownType::Df => "df",
        KnownType::DfComplex => "df_complex",
        KnownType::DfInodes => "df_inodes",
        KnownType::DiskIoTime => "disk_io_time",
        KnownType::DiskLatency => "disk_latency",
        KnownType::DiskMerged => "disk_merged",
        KnownType::DiskOctets => "disk_octets",
        KnownType::DiskOps => "disk_ops",
        KnownType::DiskOpsComplex => "disk_ops_complex",
        KnownType::DiskTime => "disk_time",
        KnownType::DnsAnswer => "dns_answer",
        KnownType::DnsNotify => "dns_notify",
        KnownType::DnsOctets => "dns_octets",
        KnownType::DnsOpcode => "dns_opcode",
        KnownType::DnsQtype => "dns_qtype",
        KnownType::DnsQtypeCached => "dns_qtype_cached",
        KnownType::DnsQuery => "dns_query",
        KnownType::DnsQuestion => "dns_question",
        KnownType::DnsRcode => "dns_rcode",
        KnownType::DnsReject => "dns_reject",
        KnownType::DnsRequest => "dns_request",
        KnownType::DnsResolver => "dns_resolver",
        KnownType::DnsResponse => "dns_response",
        KnownType::DnsTransfer => "dns_transfer",
        KnownType::DnsUpdate => "dns_update",
        KnownType::DnsZops => "dns_zops",
        KnownType::DrbdResource => "drbd_resource",
        KnownType::Duration => "duration",
        KnownType::EmailCheck => "email_check",
        KnownType::EmailCount => "email_count",
        KnownType::EmailSize => "email_size",
        KnownType::Entropy => "entropy",
        KnownType::EvictedKeys => "evicted_keys",
        KnownType::ExpiredKeys => "expired_keys",
        KnownType::Fanspeed => "fanspeed",
        KnownType::FileHandles => "file_handles",
        KnownType::FileSize => "file_size",
        KnownType::Files => "files",
        KnownType::Flow => "flow",
        KnownType::ForkRate => "fork_rate",
        KnownType::Frequency => "frequency",
        KnownType::FrequencyError => "frequency_error",
        KnownType::FrequencyOffset => "frequency_offset",
        KnownType::FscacheStat => "fscache_stat",
        KnownType::Gauge => "gauge",
        KnownType::HashCollisions => "hash_collisions",
        KnownType::HttpRequestMethods => "http_request_methods",
        KnownType::HttpRequests => "http_requests",
        KnownType::HttpResponseCodes => "http_response_codes",
        KnownType::Humidity => "humidity",
        KnownType::IfCollisions => "if_collisions",
        KnownType::IfDropped => "if_dropped",
        KnownType::IfErrors => "if_errors",
        KnownType::IfMulticast => "if_multicast",
        KnownType::IfOctets => "if_octets",
        KnownType::IfPackets => "if_packets",
        KnownType::IfRxErrors => "if_rx_errors",
        KnownType::IfRxOctets => "if_rx_octets",
        KnownType::IfTxErrors => "if_tx_errors",
        KnownType::IfTxOctets => "if_tx_octets",
        KnownType::Invocations => "invocations",
        KnownType::IoOctets => "io_octets",
        KnownType::IoPackets => "io_packets",
        KnownType::IptBytes => "ipt_bytes",
        KnownType::IptPackets => "ipt_packets",
        KnownType::Irq => "irq",
        KnownType::Latency => "latency",
        KnownType::Links => "links",
        KnownType::Load => "load",
        KnownType::MdDisks => "md_disks",
        KnownType::Memory => "memory",
        KnownType::MemoryLua => "memory_lua",
        KnownType::MemoryThrottleCount => "memory_throttle_count",
        KnownType::Multimeter => "multimeter",
        KnownType::MutexOperations => "mutex_operations",
        KnownType::Objects => "objects",
        KnownType::Operations => "operations",
        KnownType::Packets => "packets",
        KnownType::PendingOperations => "pending_operations",
        KnownType::Percent => "percent",
        KnownType::PercentBytes => "percent_bytes",
        KnownType::PercentInodes => "percent_inodes",
        KnownType::Ping => "ping",
        KnownType::PingDroprate => "ping_droprate",
        KnownType::PingStddev => "ping_stddev",
        KnownType::Players => "players",
        KnownType::Power => "power",
        KnownType::Pressure => "pressure",
        KnownType::ProtocolCounter => "protocol_counter",
        KnownType::Pubsub => "pubsub",
        KnownType::QueueLength => "queue_length",
        KnownType::Records => "records",
        KnownType::Requests => "requests",
        KnownType::ResponseCode => "response_code",
        KnownType::ResponseTime => "response_time",
        KnownType::RootDelay => "root_delay",
        KnownType::RootDispersion => "root_dispersion",
        KnownType::RouteEtx => "route_etx",
        KnownType::RouteMetric => "route_metric",
        KnownType::Routes => "routes",
        KnownType::Segments => "segments",
        KnownType::SerialOctets => "serial_octets",
        KnownType::SignalNoise => "signal_noise",
        KnownType::SignalPower => "signal_power",
        KnownType::SignalQuality => "signal_quality",
        KnownType::Snr => "snr",
        KnownType::Spl => "spl",
        KnownType::Swap => "swap",
        KnownType::SwapIo => "swap_io",
        KnownType::TcpConnections => "tcp_connections",
        KnownType::Temperature => "temperature",
        KnownType::Threads => "threads",
        KnownType::TimeDispersion => "time_dispersion",
        KnownType::TimeOffset => "time_offset",
        KnownType::TimeOffsetNtp => "time_offset_ntp",
        KnownType::TimeOffsetRms => "time_offset_rms",
        KnownType::TimeRef => "time_ref",
        KnownType::Timeleft => "timeleft",
        KnownType::TotalBytes => "total_bytes",
        KnownType::TotalConnections => "total_connections",
        KnownType::TotalObjects => "total_objects",
        KnownType::TotalOperations => "total_operations",
        KnownType::TotalRequests => "total_requests",
        KnownType::TotalSessions => "total_sessions",
        KnownType::TotalThreads => "total_threads",
        KnownType::TotalTimeInMs => "total_time_in_ms",
        KnownType::TotalValues => "total_values",
        KnownType::Uptime => "uptime",
        KnownType::Users => "users",
        KnownType::Vcl => "vcl",
        KnownType::Vcpu => "vcpu",
        KnownType::VirtCpuTotal => "virt_cpu_total",
        KnownType::VirtVcpu => "virt_vcpu",
        KnownType::VmpageAction => "vmpage_action",
        KnownType::VmpageFaults => "vmpage_faults",
        KnownType::VmpageIo => "vmpage_io",
        KnownType::VmpageNumber => "vmpage_number",
        KnownType::VolatileChanges => "volatile_changes",
        KnownType::Voltage => "voltage",
        KnownType::VoltageThreshold => "voltage_threshold",
        KnownType::VsMemory => "vs_memory",
        KnownType::VsProcesses => "vs_processes",
        KnownType::VsThreads => "vs_threads",
    };
    name.into()
}

/// A value with an explicit collectd data type.
#[derive(Debug, Clone)]
pub struct Typed<T> {
    /// The data type.
    pub type_: DataType,
    /// The value (or a callable returning it).
    pub value: T,
}

impl<T> Typed<T> {
    /// A value `v` of data type `t`.
    pub fn new(t: DataType, v: T) -> Self {
        Self { type_: t, value: v }
    }
}

/// Wraps `t` with data type `type_`.
#[inline]
pub fn make_typed<T>(type_: DataType, t: T) -> Typed<T> {
    Typed::new(type_, t)
}

/// The collectd plugin name; the metrics group.
pub type PluginId = metrics::GroupNameType;
/// The collectd plugin instance; the shard id.
pub type PluginInstanceId = metrics::InstanceIdType;
/// The collectd type name.
pub type TypeId = metrics::MetricTypeDef;
/// The collectd type instance; the metric name.
pub type TypeInstance = metrics::MetricNameType;
/// The description of a value.
pub type Description = MetricsDescription;

/// Longest text field of the collectd protocol.
pub const MAX_COLLECTD_FIELD_TEXT_LEN: usize = 63;

thread_local! {
    static NEXT_TRUNCATED_IDX: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The id of a series in collectd terms.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TypeInstanceId {
    _plugin: PluginId,
    _plugin_instance: PluginInstanceId,
    _type: TypeId,
    _type_instance: TypeInstance,
}

impl TypeInstanceId {
    fn truncate(field: &mut Sstring, field_desc: &str) {
        if field.size() > MAX_COLLECTD_FIELD_TEXT_LEN {
            let idx = NEXT_TRUNCATED_IDX.with(|c| {
                c.set(c.get() + 1);
                c.get()
            });
            let suffix = format!("~{}", idx);
            let keep = &field.as_bytes()[..MAX_COLLECTD_FIELD_TEXT_LEN - suffix.len()];
            // The kept bytes are copied as they are, even if that splits a UTF-8 sequence.
            let mut new_field = Sstring::from_bytes(keep);
            new_field.append_bytes(suffix.as_bytes());
            log_warn!(
                LOGGER,
                "Truncating \"{}\" to {} chars: \"{}\" -> \"{}\"",
                field_desc,
                MAX_COLLECTD_FIELD_TEXT_LEN,
                field,
                new_field
            );
            *field = new_field;
        }
    }

    /// The id `p/pi/t/ti`, each field truncated to the protocol limit.
    pub fn new(p: PluginId, pi: PluginInstanceId, t: TypeId, ti: TypeInstance) -> Self {
        let mut id = Self { _plugin: p, _plugin_instance: pi, _type: t, _type_instance: ti };
        Self::truncate(&mut id._plugin, "plugin");
        Self::truncate(&mut id._plugin_instance, "plugin_instance");
        Self::truncate(&mut id._type, "type");
        Self::truncate(&mut id._type_instance, "type_instance");
        id
    }

    /// The id of metric `id` with collectd type `inherit_type`.
    pub fn from_metric_id(id: &mi::MetricId, inherit_type: &TypeId) -> Self {
        Self {
            _plugin: id.group_name().clone(),
            _plugin_instance: id.instance_id().clone(),
            _type: inherit_type.clone(),
            _type_instance: id.name().clone(),
        }
    }

    /// The plugin.
    pub fn plugin(&self) -> &PluginId {
        &self._plugin
    }

    /// The plugin instance.
    pub fn plugin_instance(&self) -> &PluginInstanceId {
        &self._plugin_instance
    }

    /// The type.
    pub fn type_(&self) -> &TypeId {
        &self._type
    }

    /// The type instance.
    pub fn type_instance(&self) -> &TypeInstance {
        &self._type_instance
    }
}

/// `per_cpu_plugin_instance`
pub fn per_cpu_plugin_instance() -> PluginInstanceId {
    "#cpu".into()
}

/// COLLECTD options.
pub struct Options {
    /// The option group this struct extends.
    pub group: OptionGroup,
    /// enable collectd daemon
    pub collectd: OptionValue<bool>,
    /// address to send/broadcast metrics to
    pub collectd_address: OptionValue<String>,
    /// poll period - frequency of sending counter metrics (default: 1000ms, 0 disables)
    pub collectd_poll_period: OptionValue<u32>,
    /// Deprecated option, use metrics-hostname instead
    pub collectd_hostname: OptionValue<String>,
}

/// Anchor for polled registration.
/// Iff the registered type is in some way none-persistent,
/// use this as receiver of the reg and ensure it dies before the
/// added value(s).
///
/// Use:
/// uint64_t v = 0;
/// registration r = add_polled_metric(v);
/// ++r;
/// <scope end, above dies>
#[derive(Default)]
pub struct Registration {
    _id: TypeInstanceId,
    _impl: Option<SharedPtr<mi::Impl>>,
}

impl Registration {
    /// Anchors the registration of `id`.
    pub fn new(id: TypeInstanceId) -> Self {
        Self { _id: id, _impl: Some(mi::get_local_impl()) }
    }

    /// Removes the series now.
    pub fn unregister(&mut self) {
        remove_polled_metric(&self._id);
        self._id = TypeInstanceId::default();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // remove_polled_metric() unregisters the series from this shard's
        // metrics registry, which `_impl` keeps alive for this purpose ("keep impl alive while
        // metrics are registered"). Going through it instead of the thread-locals keeps a
        // registration dropped during thread exit, after them, from panicking in a destructor.
        if let Some(imp) = &self._impl {
            // SAFETY: the registry is this shard's and no other reference to it is live during
            // the call.
            unsafe { imp.get_mut_unchecked() }.remove_registration(&to_metrics_id(&self._id));
        }
        self._id = TypeInstanceId::default();
    }
}

/// Helper type to make generating vectors of registration objects
/// easier, since it constructs from an initializer list of
/// type_instance_id:s, avoiding early conversion to registration objs,
/// which in case of init lists, are copy semantics, not move...
#[derive(Default)]
pub struct Registrations(pub Vec<Registration>);

impl Registrations {
    /// Anchors every id of `l`.
    pub fn from_ids(l: Vec<TypeInstanceId>) -> Self {
        Self(l.into_iter().map(Registration::new).collect())
    }
}

impl std::ops::Deref for Registrations {
    type Target = Vec<Registration>;

    fn deref(&self) -> &Vec<Registration> {
        &self.0
    }
}

impl std::ops::DerefMut for Registrations {
    fn deref_mut(&mut self) -> &mut Vec<Registration> {
        &mut self.0
    }
}

/// A number read by a value: its integral or floating-point result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScNumber {
    /// An integral result, converted to `uint64_t`.
    Int(u64),
    /// A floating-point result.
    Float(f64),
}

/// The primitive number types a value can read, kept exact: a counter
/// beyond 2^53 does not go through `double`.
pub trait ScNumberType: Copy + 'static {
    /// The number as an [`ScNumber`].
    fn to_sc_number(self) -> ScNumber;
}

macro_rules! sc_int_number {
    ($($t:ty),*) => {$(
        impl ScNumberType for $t {
            #[inline]
            fn to_sc_number(self) -> ScNumber {
                // An `as` conversion to u64: a negative value wraps.
                ScNumber::Int(self as u64)
            }
        }
    )*};
}
sc_int_number!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, bool);

impl ScNumberType for f64 {
    #[inline]
    fn to_sc_number(self) -> ScNumber {
        ScNumber::Float(self)
    }
}

impl ScNumberType for f32 {
    #[inline]
    fn to_sc_number(self) -> ScNumber {
        ScNumber::Float(f64::from(self))
    }
}

/// One value of a value list: its data type and how to read it.
///
/// The reader is one boxed callable, made once per registration, so polling allocates
/// nothing for it.
pub struct ScValue {
    _type: DataType,
    _t: Box<dyn Fn() -> ScNumber>,
}

impl ScValue {
    /// A value of data type `type_` read by calling `t`.
    pub fn new<V: ScNumberType>(type_: DataType, t: impl Fn() -> V + 'static) -> Self {
        Self { _type: type_, _t: Box::new(move || t().to_sc_number()) }
    }

    /// Reads the value and returns its 64-bit wire value: for a gauge, the bits of the double
    /// in little-endian byte order, as the protocol requires; for other types, the unsigned
    /// integer.
    pub fn call(&self) -> u64 {
        let v = (self._t)();
        if self._type == DataType::Gauge {
            // A gauge is sent as a double.
            let d = match v {
                ScNumber::Int(i) => i as f64,
                ScNumber::Float(f) => f,
            };
            Self::convert_double(d)
        } else {
            // Other types are sent as an unsigned integer.
            let u = match v {
                ScNumber::Int(i) => i,
                ScNumber::Float(f) => f as u64,
            };
            Self::convert_int(u)
        }
    }

    /// The data type.
    pub fn type_(&self) -> DataType {
        self._type
    }

    fn convert_int(i: u64) -> u64 {
        // ntohq
        u64::from_be(i)
    }

    fn convert_double(t: f64) -> u64 {
        // bpack: the little-endian bytes of the double, read back natively
        u64::from_ne_bytes(t.to_bits().to_le_bytes())
    }
}

/// A list of values bound to one type instance.
pub trait ValueList {
    /// Number of values.
    fn size(&self) -> usize;
    /// Writes the data type of each value.
    fn types(&self, p: &mut [u8]);
    /// Writes each value (8 bytes each, wire order).
    fn values(&self, p: &mut [u8]);
    /// The description.
    fn desc(&self) -> &Description;
    /// Whether the list has no value.
    fn empty(&self) -> bool {
        self.size() == 0
    }
}

/// The values of a type instance, with their description.
pub struct ValuesImpl {
    _description: Description,
    _enabled: bool,
    _values: Vec<ScValue>,
}

impl ValuesImpl {
    /// A list of `values`.
    pub fn new(d: Description, values: Vec<ScValue>) -> Self {
        Self { _description: d, _enabled: true, _values: values }
    }

    /// Whether the list is reported.
    pub fn is_enabled(&self) -> bool {
        self._enabled
    }

    /// Enables or disables reporting.
    pub fn set_enabled(&mut self, b: bool) {
        self._enabled = b;
    }
}

impl ValueList for ValuesImpl {
    fn size(&self) -> usize {
        self._values.len()
    }

    fn types(&self, p: &mut [u8]) {
        for (dst, v) in p.iter_mut().zip(&self._values) {
            *dst = collectd_type_code(v.type_());
        }
    }

    fn values(&self, p: &mut [u8]) {
        for (dst, v) in p.as_chunks_mut::<8>().0.iter_mut().zip(&self._values) {
            dst.copy_from_slice(&v.call().to_ne_bytes());
        }
    }

    fn desc(&self) -> &Description {
        &self._description
    }
}

/// The values `values` of a type instance described by `d`.
pub fn make_type_instance(d: Description, values: Vec<ScValue>) -> ValuesImpl {
    ValuesImpl::new(d, values)
}

/// A callback with no arguments, such as the one [`create_explicit_metric`] returns.
pub type NotifyFunction = Box<dyn Fn()>;

/// Wraps N values of a given type (type_id); used to group types into a plugin_instance_metrics.
pub struct TypedValue {
    _type_id: TypeId,
    _type_instance: TypeInstance,
    _values: SharedPtr<ValuesImpl>,
}

impl TypedValue {
    /// Wraps N values of a given type (type_id).
    /// Used to group types into a plugin_instance_metrics
    pub fn new(tid: &TypeId, ti: &TypeInstance, d: Description, values: Vec<ScValue>) -> Self {
        Self {
            _type_id: tid.clone(),
            _type_instance: ti.clone(),
            _values: make_shared(make_type_instance(d, values)),
        }
    }

    /// The type instance.
    pub fn type_instance(&self) -> &TypeInstance {
        &self._type_instance
    }

    /// The values.
    pub fn values(&self) -> &SharedPtr<ValuesImpl> {
        &self._values
    }

    /// The type.
    pub fn type_(&self) -> &TypeId {
        &self._type_id
    }
}

/// A typed value whose type name comes from the well-known type `t`.
pub fn typed_value_impl(
    t: KnownType,
    ti: &TypeInstance,
    d: Description,
    values: Vec<ScValue>,
) -> TypedValue {
    TypedValue::new(&type_id_for(t), ti, d, values)
}

/// The metrics of one plugin instance.
pub struct PluginInstanceMetrics {
    _plugin_id: PluginId,
    _plugin_instance: PluginInstanceId,
    _registrations: Registrations,
}

impl PluginInstanceMetrics {
    /// Registers `values` under `p/pi`.
    pub fn new(p: &PluginId, pi: &PluginInstanceId, values: &[TypedValue]) -> Self {
        let mut m = Self {
            _plugin_id: p.clone(),
            _plugin_instance: pi.clone(),
            _registrations: Registrations::default(),
        };
        let ids: Vec<TypeInstanceId> = values.iter().map(|v| m.add_impl(v)).collect();
        m._registrations = Registrations::from_ids(ids);
        m
    }

    fn add_impl(&self, v: &TypedValue) -> TypeInstanceId {
        let id = TypeInstanceId::new(
            self._plugin_id.clone(),
            self._plugin_instance.clone(),
            v.type_().clone(),
            v.type_instance().clone(),
        );
        // SAFETY: a synchronous call on this shard; no other reference to the sender is live.
        unsafe { get_impl() }.add_polled(&id, v.values(), true);
        id
    }

    /// Registers one more typed value.
    pub fn add(&mut self, v: &TypedValue) {
        let id = self.add_impl(v);
        self._registrations.push(Registration::new(id));
    }

    /// The ids of the registered values.
    pub fn bound_ids(&self) -> Vec<TypeInstanceId> {
        self._registrations.iter().map(|r| r._id.clone()).collect()
    }
}

/// Simplified wrapper for the common case of per-cpu plugin instances
/// (i.e. distributed objects)
pub fn percpu_plugin_instance_metrics(
    p: &PluginId,
    values: &[TypedValue],
) -> PluginInstanceMetrics {
    PluginInstanceMetrics::new(p, &per_cpu_plugin_instance(), values)
}

/// The metrics-layer id of a collectd id: the plugin as group, the type instance as name, and
/// this shard as the shard label.
pub fn to_metrics_id(id: &TypeInstanceId) -> mi::MetricId {
    let mut labels = mi::LabelsType::new();
    labels.insert(metrics::shard_label_name(), mi::shard());
    let internalized_labels = make_lw_shared(labels);
    mi::MetricId::new(id.plugin().clone(), id.type_instance().clone(), internalized_labels)
}

/// Registers one polled value under `id` with the metrics layer and returns the id.
///
/// Deprecated: register metrics through the metrics layer instead.
///
/// # Errors
///
/// Returns the registration error (for example a double registration).
#[deprecated(note = "Use the metrics layer")]
pub fn add_polled_metric<S: mi::MetricSource>(
    id: &TypeInstanceId,
    d: Description,
    type_: DataType,
    arg: S,
    enabled: bool,
) -> Result<TypeInstanceId, ExceptionPtr> {
    let local = mi::get_local_impl();
    // SAFETY: the registry is this shard's and no other reference to it is live during the call.
    unsafe { local.get_mut_unchecked() }.add_registration(
        &to_metrics_id(id),
        &mi::MetricType { base_type: type_, type_name: id.type_().clone() },
        mi::make_function(arg, type_),
        &d,
        enabled,
        metrics::SkipWhenEmpty::NO,
        &[],
    )?;
    Ok(id.clone())
}

/// Registers one polled value under `id` as a disabled series and returns the id.
///
/// Deprecated: register metrics through the metrics layer instead.
///
/// # Errors
///
/// As [`add_polled_metric`].
#[deprecated(note = "Use the metrics layer")]
#[allow(deprecated)]
pub fn add_disabled_polled_metric<S: mi::MetricSource>(
    id: &TypeInstanceId,
    d: Description,
    type_: DataType,
    arg: S,
) -> Result<TypeInstanceId, ExceptionPtr> {
    add_polled_metric(id, d, type_, arg, false)
}

/// Sends a value list for `id` now (outside polling).
pub fn send_explicit_metric(id: &TypeInstanceId, values: Vec<ScValue>) -> Future<()> {
    send_metric(id, &make_type_instance(Description::default(), values))
}

/// A function that sends the current values of `values` for `id` when called.
pub fn create_explicit_metric(id: &TypeInstanceId, values: Vec<ScValue>) -> NotifyFunction {
    let id = id.clone();
    let list = make_type_instance(Description::default(), values);
    Box::new(move || {
        // The notifier is fire-and-forget: the send's future is discarded.
        let _ = send_metric(&id, &list);
    })
}

// ---------------------------------------------------------------------------------------------
// The sender and the wire protocol.
// ---------------------------------------------------------------------------------------------

const PAYLOAD_SIZE: usize = 1024;

// The part types of the collectd binary protocol, not all of them sent.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum PartType {
    Host = 0x0000,       // The name of the host to associate with subsequent data values
    Time = 0x0001, // Time  Numeric The timestamp to associate with subsequent data values, unix time format (seconds since epoch)
    TimeHr = 0x0008, // Time (high resolution)  Numeric The timestamp to associate with subsequent data values. Time is defined in 2–30 seconds since epoch. New in Version 5.0.
    Plugin = 0x0002, // Plugin String The plugin name to associate with subsequent data values, e.g. "cpu"
    PluginInst = 0x0003, // Plugin instance String  The plugin instance name to associate with subsequent data values, e.g. "1"
    Type = 0x0004, // Type String The type name to associate with subsequent data values, e.g. "cpu"
    TypeInst = 0x0005, // Type instance     String  The type instance name to associate with subsequent data values, e.g. "idle"
    Values = 0x0006,   // Values  other   Data values, see above
    Interval = 0x0007, // Interval Numeric Interval used to set the "step" when creating new RRDs unless rrdtool plugin forces StepSize. Also used to detect values that have timed out.
    IntervalHr = 0x0009, // Interval (high resolution)  Numeric     The interval in which subsequent data values are collected. The interval is given in 2–30 seconds. New in Version 5.0.
    Message = 0x0100,    // Message (notifications) String
    Severity = 0x0101,   // Severity  Numeric
    Signature = 0x0200,  // Signature (HMAC-SHA-256)     other (todo)
    Encryption = 0x0210, // Encryption (AES-256/OFB
}

/// Wire code of a collectd GAUGE value: 8 bytes holding a little-endian double.
const COLLECTD_TYPE_GAUGE: u8 = 1;
/// Wire code of a collectd COUNTER value: 8 bytes holding a big-endian unsigned integer.
const COLLECTD_TYPE_COUNTER: u8 = 0;

/// The collectd type code of a value of data type `t`, matching how its 8 bytes are
/// written: a gauge as a double, every other type as an unsigned integer.
fn collectd_type_code(t: DataType) -> u8 {
    if t == DataType::Gauge {
        COLLECTD_TYPE_GAUGE
    } else {
        COLLECTD_TYPE_COUNTER
    }
}

/// "Time is defined in 2^–30 seconds since epoch. New in Version 5.0."
fn collectd_hres(period: std::time::Duration) -> u64 {
    ((period.as_nanos() << 30) / 1_000_000_000) as u64
}

// yet another writer type, this one to construct collectd network
// protocol data.
pub(crate) struct Cpwriter {
    _buf: [u8; PAYLOAD_SIZE],
    _pos: usize,
    _overflow: bool,
    _cache: HashMap<u16, Sstring>,
}

impl Cpwriter {
    pub(crate) fn new() -> Self {
        Self { _buf: [0; PAYLOAD_SIZE], _pos: 0, _overflow: false, _cache: HashMap::new() }
    }

    fn mark(&self) -> usize {
        self._pos
    }

    fn reset(&mut self, m: usize) {
        self._pos = m;
        self._overflow = false;
    }

    pub(crate) fn size(&self) -> usize {
        self._pos
    }

    pub(crate) fn empty(&self) -> bool {
        self._pos == 0
    }

    fn clear(&mut self) {
        self.reset(0);
        self._cache.clear();
        self._overflow = false;
    }

    pub(crate) fn data(&self) -> &[u8] {
        &self._buf[..self._pos]
    }

    fn check(&mut self, sz: usize) -> bool {
        let av = PAYLOAD_SIZE - self._pos;
        self._overflow |= av < sz;
        !self._overflow
    }

    fn get_type_instance(name: &metrics::MetricNameType, labels: &mi::LabelsType) -> Sstring {
        if labels.is_empty() {
            return name.clone();
        }
        let mut res = name.clone();
        let shard = metrics::shard_label_name();
        for (k, v) in labels {
            if *k != shard {
                res += "-";
                res += v;
            }
        }
        res
    }

    fn ok(&self) -> bool {
        !self._overflow
    }

    fn write_bytes(&mut self, s: &[u8]) -> &mut Self {
        if self.check(s.len()) {
            self._buf[self._pos..self._pos + s.len()].copy_from_slice(s);
            self._pos += s.len();
        }
        self
    }

    fn write_u8(&mut self, t: u8) -> &mut Self {
        self.write_bytes(&[t])
    }

    fn write_u16(&mut self, t: u16) -> &mut Self {
        self.write_bytes(&t.to_be_bytes())
    }

    fn write_u64(&mut self, t: u64) -> &mut Self {
        self.write_bytes(&t.to_be_bytes())
    }

    fn write_le_u64(&mut self, t: u64) -> &mut Self {
        self.write_bytes(&t.to_le_bytes())
    }

    fn write_value(&mut self, v: &mi::MetricValue) {
        match v.type_() {
            DataType::Gauge => {
                let tmpi = v.d().to_bits();
                self.write_le_u64(tmpi);
            }
            DataType::Counter | DataType::RealCounter => {
                // unsigned int 64, big endian (out-of-range values are written as 0)
                self.write_u64(v.ui().unwrap_or(0));
            }
            _ => panic!("assertion failed: 0"),
        }
    }

    fn write_str(&mut self, s: &Sstring) -> &mut Self {
        self.write_bytes(s.as_bytes());
        self.write_u8(0) // include \0
    }

    fn put_str(&mut self, type_: PartType, s: &Sstring) -> &mut Self {
        self.write_u16(type_ as u16);
        self.write_u16((4 + s.size() + 1) as u16); // include \0
        self.write_str(s) // include \0
    }

    fn put_cached(&mut self, type_: PartType, s: &Sstring) -> &mut Self {
        let cached = self._cache.entry(type_ as u16).or_default();
        if cached != s {
            *cached = s.clone();
            self.put_str(type_, s);
        }
        self
    }

    fn put_u64(&mut self, type_: PartType, t: u64) -> &mut Self {
        self.write_u16(type_ as u16);
        self.write_u16((4 + size_of::<u64>()) as u16);
        self.write_u64(t)
    }

    fn put_value_list(&mut self, type_: PartType, v: &dyn ValueList) -> &mut Self {
        let s = v.size();
        let sz = 6 + s + s * size_of::<u64>();
        if self.check(sz) {
            self.write_u16(type_ as u16);
            self.write_u16(sz as u16);
            self.write_u16(s as u16);
            let p = self._pos;
            v.types(&mut self._buf[p..p + s]);
            self._pos += s;
            let p = self._pos;
            v.values(&mut self._buf[p..p + s * size_of::<u64>()]);
            self._pos += s * size_of::<u64>();
        }
        self
    }

    fn put_metric_value(&mut self, type_: PartType, v: &mi::MetricValue) -> &mut Self {
        let sz = 7 + size_of::<u64>();
        if self.check(sz) {
            self.write_u16(type_ as u16);
            self.write_u16(sz as u16);
            self.write_u16(1);
            self.write_u8(collectd_type_code(v.type_()));
            self.write_value(v);
        }
        self
    }

    fn put_header(
        &mut self,
        host: &Sstring,
        group_name: &metrics::GroupNameType,
        name: &metrics::MetricNameType,
        labels: &mi::LabelsType,
        type_: &TypeId,
    ) -> &mut Self {
        let lrts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        self.put_cached(PartType::Host, host);
        self.put_u64(PartType::Time, lrts);
        // Seems hi-res timestamp does not work very well with
        // at the very least my default collectd in fedora (or I did it wrong?)
        // Use lo-res ts for now, it is probably quite sufficient.
        self.put_cached(PartType::Plugin, group_name);
        // Optional
        let instance_id = labels.get(&metrics::shard_label_name()).cloned().unwrap_or_default();
        let pi = if instance_id == per_cpu_plugin_instance() {
            to_sstring(&this_shard_id())
        } else {
            instance_id
        };
        self.put_cached(PartType::PluginInst, &pi);
        self.put_cached(PartType::Type, type_);
        // Optional
        let ti = Self::get_type_instance(name, labels);
        self.put_cached(PartType::TypeInst, &ti);
        self
    }

    pub(crate) fn put_id_values(
        &mut self,
        host: &Sstring,
        period: std::time::Duration,
        id: &TypeInstanceId,
        v: &dyn ValueList,
    ) -> &mut Self {
        let ps = collectd_hres(period);
        let mid = to_metrics_id(id);
        self.put_header(host, mid.group_name(), mid.name(), mid.labels(), id.type_());
        self.put_value_list(PartType::Values, v);
        if ps != 0 {
            self.put_u64(PartType::IntervalHr, ps);
        }
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn put_series(
        &mut self,
        host: &Sstring,
        period: std::time::Duration,
        type_: &TypeId,
        group_name: &metrics::GroupNameType,
        name: &metrics::MetricNameType,
        labels: &mi::LabelsType,
        v: &mi::MetricValue,
    ) -> &mut Self {
        let ps = collectd_hres(period);
        self.put_header(host, group_name, name, labels, type_);
        self.put_metric_value(PartType::Values, v);
        if ps != 0 {
            self.put_u64(PartType::IntervalHr, ps);
        }
        self
    }
}

impl Impl {
    /// Does nothing: polling is implemented on the metrics layer.
    pub fn add_polled(
        &mut self,
        _id: &TypeInstanceId,
        _values: &SharedPtr<ValuesImpl>,
        _enable: bool,
    ) {
        // do nothing
        // add_polled is now implemented on the metrics layer
    }

    /// Removes the series of `id`.
    pub fn remove_polled(&mut self, id: &TypeInstanceId) {
        mi::unregister_metric(&to_metrics_id(id));
    }

    // explicitly send a type_instance value list (outside polling)
    /// Sends `values` for `id` now.
    pub fn send_metric(&mut self, id: &TypeInstanceId, values: &dyn ValueList) -> Future<()> {
        if values.empty() {
            return make_ready_future(());
        }
        let mut out = Cpwriter::new();
        out.put_id_values(&self._host, std::time::Duration::ZERO, id, values);
        self._chan.send(&SocketAddress::from_ipv4_addr(self._addr), Packet::from_data(out.data()))
    }

    /// Sends notification `msg` for `id`.
    pub fn send_notification(&mut self, id: &TypeInstanceId, msg: &Sstring) -> Future<()> {
        let mut out = Cpwriter::new();
        let mid = to_metrics_id(id);
        out.put_header(&self._host, mid.group_name(), mid.name(), mid.labels(), id.type_());
        out.put_str(PartType::Message, msg);
        self._chan.send(&SocketAddress::from_ipv4_addr(self._addr), Packet::from_data(out.data()))
    }

    // initiates actual value polling -> send to target "loop"
    /// Starts sending every `period` to `addr` as `host`.
    ///
    /// # Safety
    /// `self` must not move or be dropped while it is started (until `stop()`): its timer is
    /// armed in place, and the timer, the send loop and the metrics read it through its
    /// address. The thread-local sender of [`get_impl`] satisfies this.
    ///
    /// # Errors
    ///
    /// Returns the registration error of the sender's own metrics.
    pub unsafe fn start(
        &mut self,
        host: &Sstring,
        addr: &Ipv4Addr,
        period: std::time::Duration,
    ) -> Result<(), ExceptionPtr> {
        self._period = period;
        self._addr = *addr;
        self._host = host.clone();
        self._chan = crate::engine::core::reactor::make_unbound_datagram_channel(
            crate::engine::net::socket_defs::AF_INET,
        )?;
        let this: *mut Self = self;
        self.timer().set_callback(move || {
            // SAFETY: by the contract of `start`, the impl stays in place until `stop()`
            // cancels its timer; the callback runs from the reactor, with no other
            // reference to the impl live.
            unsafe { Self::run(this) };
        });

        // dogfood ourselves
        // SAFETY: the metrics are cleared by stop() and, by the contract of `start`, the
        // fields stay in place until then; they are written only by the send loop, never
        // while a metric reads them.
        let bytes = unsafe { metrics::by_ref(&self._bytes) };
        // SAFETY: as for `bytes` above.
        let packets = unsafe { metrics::by_ref(&self._num_packets) };
        // SAFETY: as for `bytes` above.
        let avg = unsafe { metrics::by_ref(&self._avg) };
        // SAFETY: as for `bytes` above.
        let millis = unsafe { metrics::by_ref(&self._millis) };
        self._metrics.add_group(
            "scollectd",
            vec![
                // total_bytes      value:DERIVE:0:U
                metrics::make_counter_fn(
                    "total_bytes_sent",
                    metrics::Description::new("total bytes sent"),
                    bytes,
                ),
                // total_requests      value:DERIVE:0:U
                metrics::make_counter_fn(
                    "total_requests",
                    metrics::Description::new("total requests"),
                    packets,
                ),
                // latency          value:GAUGE:0:U
                metrics::make_gauge_fn("latency", metrics::Description::new("avrage latency"), avg),
                // total_time_in_ms    value:DERIVE:0:U
                metrics::make_counter_fn(
                    "total_time_in_ms",
                    metrics::Description::new("total time in milliseconds"),
                    millis,
                ),
                // total_values     value:DERIVE:0:U
                metrics::make_gauge_fn(
                    "total_values",
                    metrics::Description::new("current number of values reported"),
                    || mi::get_local_impl()._value_map.len() as u64,
                ),
                // records          value:GAUGE:0:U
                metrics::make_gauge_fn(
                    "records",
                    metrics::Description::new("number of records reported"),
                    || mi::get_local_impl()._value_map.len() as u64,
                ),
            ],
        )?;

        // FIXME: future is discarded
        let _ = self.send_notification(
            &TypeInstanceId::new(
                "scollectd".into(),
                per_cpu_plugin_instance(),
                "network".into(),
                TypeInstance::new(),
            ),
            &"daemon started".into(),
        );
        self.arm();
        Ok(())
    }

    /// Stops sending.
    pub fn stop(&mut self) {
        self.timer().cancel();
        self._metrics.clear();
    }

    /// The timer, pinned.
    #[inline]
    fn timer(&mut self) -> std::pin::Pin<&mut Timer<SteadyClockType>> {
        // SAFETY: the timer is only armed by `arm()`, reached from `start()`, whose contract
        // keeps the impl (and so the timer) in place until `stop()` cancels it; an unarmed
        // timer is not linked anywhere, so pinning it does not constrain a later move.
        unsafe { std::pin::Pin::new_unchecked(&mut self._timer) }
    }

    fn arm(&mut self) {
        if self._period != std::time::Duration::ZERO {
            let period = self._period;
            self.timer().arm_in(period);
        }
    }

    /// One sending round: the values of every series, in packets, then the timer re-armed.
    ///
    /// # Safety
    /// `this` is the started sender (in place until `stop()`); the round reaches it only
    /// through short-lived references that end before any other code of the shard runs.
    unsafe fn run(this: *mut Self) {
        struct Context {
            mf: usize,
            md_iterator: usize,
            i: usize,
            type_: TypeId,
            out: Cpwriter,
        }
        // lw_shared_ptr<tuple<...>>: the state of the loop, shared by its two callbacks.
        let ctxt = make_lw_shared(Context {
            mf: 0,
            md_iterator: 0,
            i: 0,
            type_: TypeId::new(),
            out: Cpwriter::new(),
        });
        let vals = mi::get_values();
        // note we're doing this unsynced since we assume
        // all registrations to this instance will be done on the
        // same cpu, and without interuptions (no wait-states)
        // SAFETY: the snapshot is this shard's own (get_values() runs here), read only.
        let Some(copy) = (unsafe { vals.get_unchecked() }) else {
            // SAFETY: by the contract of this function.
            unsafe { (*this).arm() };
            return;
        };
        let metadata = copy.metadata.clone();
        let md_len = metadata.as_ref().map_or(0, |m| m.len());
        if !copy.values.is_empty()
            && let Some(md) = &metadata
        {
            // SAFETY: the context is this shard's and nothing else references it now.
            unsafe { ctxt.get_mut_unchecked() }.type_ = md[0].mf.inherit_type.clone();
        }
        let ctxt_stop = ctxt.clone();
        // SAFETY: a read of the shard's context between the loop's steps.
        let stop_when = move || unsafe { (*ctxt_stop.get()).mf == md_len };
        let vals_ptr: *const ForeignPtrValues = &vals;
        // append as many values as we can fit into a packet (1024 bytes)
        let ctxt_send = ctxt.clone();
        let send_packet = move || -> Future<()> {
            // SAFETY: `vals` is kept alive by the finally below, and only read on this shard.
            let copy = match unsafe { (*vals_ptr).get_unchecked() } {
                Some(c) => c,
                None => return make_ready_future(()),
            };
            let values = &copy.values;
            let Some(metadata) = copy.metadata.as_ref() else {
                return make_ready_future(());
            };
            let start = std::time::Instant::now();
            // SAFETY: the context is this shard's; this step is the only code using it until
            // the send's continuation, which runs after this reference ends.
            let c = unsafe { ctxt_send.get_mut_unchecked() };
            // SAFETY: by the contract of `run`, the sender is in place; this reference ends
            // before the send's continuation, and no other code of the shard runs meanwhile.
            let me = unsafe { &mut *this };
            c.out.clear();
            let mut out_of_space = false;
            while !out_of_space && c.mf < values.len() {
                while c.i < values[c.mf].len() {
                    let v = &values[c.mf][c.i];
                    if v.type_() == DataType::Histogram {
                        c.i += 1;
                        c.md_iterator += 1;
                        continue;
                    }
                    let m = c.out.mark();
                    let md = &metadata[c.mf].metrics[c.md_iterator];
                    c.out.put_series(
                        &me._host,
                        me._period,
                        &c.type_,
                        &md.group_name(),
                        &md.name(),
                        md.labels(),
                        v,
                    );
                    if !c.out.ok() {
                        c.out.reset(m);
                        out_of_space = true;
                        break;
                    }
                    c.i += 1;
                    c.md_iterator += 1;
                }
                if out_of_space {
                    break;
                }
                c.mf += 1;
                if c.mf < values.len() {
                    c.i = 0;
                    c.md_iterator = 0;
                    c.type_ = metadata[c.mf].mf.inherit_type.clone();
                }
            }
            if c.out.empty() {
                return make_ready_future(());
            }
            let pkt = Packet::from_data(c.out.data());
            let out_size = c.out.size();
            me._chan
                .send(&SocketAddress::from_ipv4_addr(me._addr), pkt)
                .then(move |()| {
                    // SAFETY: as above; the continuation runs from the reactor with no other
                    // reference to the sender live.
                    let me = unsafe { &mut *this };
                    let now = std::time::Instant::now();
                    // dogfood stats
                    me._num_packets += 1;
                    me._millis += (now - start).as_millis() as u64;
                    me._bytes += out_size as u64;
                    me._avg = me._millis as f64 / me._num_packets as f64;
                })
                .then_wrapped(|f: Future<()>| {
                    if f.failed() {
                        match f.get_exception().get() {
                            Some(ex) => println!("send failed: {}", ex),
                            None => println!("send failed: - unknown exception"),
                        }
                    }
                })
        };

        // No need to wait for future.
        // The caller has to call impl::stop() to synchronize.
        let _ = do_until(stop_when, send_packet).finally(move || {
            drop(vals);
            // SAFETY: by the contract of `run`.
            unsafe { (*this).arm() };
        });
    }

    /// The ids of every registered series.
    pub fn get_instance_ids(&self) -> Vec<TypeInstanceId> {
        let mut res = Vec::new();
        let local = mi::get_local_impl();
        for family in local._value_map.values() {
            // Need to check for empty value_list, since unreg is two-stage.
            // Not an issue for most uses, but unit testing etc that would like
            // fully deterministic operation here would like us to only return
            // actually active ids
            for (_, reg) in family.iter() {
                if let Some(r) = reg {
                    res.push(TypeInstanceId::from_metric_id(
                        r.get_id(),
                        &family.info().inherit_type,
                    ));
                }
            }
        }
        res
    }

    /// The host name sent.
    pub fn host(&self) -> &Sstring {
        &self._host
    }
}

type ForeignPtrValues = crate::engine::core::sharded::ForeignPtr<mi::ValuesReference>;

/// Registers the values `values` under `id` on this shard, enabled or not.
pub fn add_polled(id: &TypeInstanceId, values: &SharedPtr<ValuesImpl>, enabled: bool) {
    // SAFETY: a synchronous call on this shard; no other reference to the sender is live (its
    // timer and send loop run only from the reactor, not during this call).
    unsafe { get_impl() }.add_polled(id, values, enabled);
}

/// Removes the series of `id`.
pub fn remove_polled_metric(id: &TypeInstanceId) {
    // SAFETY: a synchronous call on this shard; no other reference to the sender is live (its
    // timer and send loop run only from the reactor, not during this call).
    unsafe { get_impl() }.remove_polled(id);
}

/// Sends notification `msg` for `id`.
pub fn send_notification(id: &TypeInstanceId, msg: &Sstring) -> Future<()> {
    // SAFETY: a synchronous call on this shard; no other reference to the sender is live (its
    // timer and send loop run only from the reactor, not during this call).
    unsafe { get_impl() }.send_notification(id, msg)
}

/// Sends `values` for `id` now.
pub fn send_metric(id: &TypeInstanceId, values: &dyn ValueList) -> Future<()> {
    // SAFETY: a synchronous call on this shard; no other reference to the sender is live (its
    // timer and send loop run only from the reactor, not during this call).
    unsafe { get_impl() }.send_metric(id, values)
}

/// Starts the collectd senders on every shard if enabled.
pub fn configure(opts: &Options) -> Result<(), ExceptionPtr> {
    let enable = *opts.collectd.get_value();
    if !enable {
        return Ok(());
    }
    let addr = Ipv4Addr::from_string(opts.collectd_address.get_value())?;
    let period = std::time::Duration::from_millis(*opts.collectd_poll_period.get_value() as u64);

    let host: Sstring = if opts.collectd_hostname.get_value().is_empty() {
        mi::get_local_impl().get_config().hostname.clone()
    } else {
        opts.collectd_hostname.get_value().as_str().into()
    };

    // Now create send loops on each cpu
    for c in 0..Smp::count() {
        let host = host.clone();
        // FIXME: future is discarded
        let _ = Smp::submit_to(c, move || {
            // SAFETY: the thread-local sender of shard `c` never moves and is not otherwise
            // borrowed during the call, as `start` requires.
            unsafe { get_impl().start(&host, &addr, period) }
        });
    }
    Ok(())
}

impl Options {
    /// The collectd options, not yet linked to a group (see [`Options::link`]).
    pub fn new() -> Self {
        Self {
            group: OptionGroup::new("COLLECTD options"),
            collectd: OptionValue::new("collectd", Some(false), "enable collectd daemon"),
            collectd_address: OptionValue::new(
                "collectd-address",
                Some("239.192.74.66:25826".to_string()),
                "address to send/broadcast metrics to",
            ),
            collectd_poll_period: OptionValue::new(
                "collectd-poll-period",
                Some(1000u32),
                "poll period - frequency of sending counter metrics (default: 1000ms, 0 disables)",
            ),
            collectd_hostname: OptionValue::new(
                "collectd-hostname",
                Some(String::new()),
                "Deprecated option, use metrics-hostname instead",
            ),
        }
    }

    /// Links the group under `parent_group` and registers every value with it, in place and
    /// in declaration order. The options must not move after.
    pub fn link(self: Pin<&mut Self>, parent_group: Option<Pin<&mut OptionGroup>>) {
        crate::link_option_group!(
            self,
            parent_group,
            group,
            collectd,
            collectd_address,
            collectd_poll_period,
            collectd_hostname
        );
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

fn get_register(i: &TypeInstanceId) -> Option<mi::RegisterRef> {
    let id = to_metrics_id(i);
    let local = mi::get_local_impl();
    local._value_map.get(&id.full_name()).and_then(|f| f.at(id.labels())).and_then(|r| r.clone())
}

/// A metric value as reported to collectd.
pub type CollectdValue = mi::MetricValue;

/// The current values of `id`.
pub fn get_collectd_value(id: &TypeInstanceId) -> Vec<CollectdValue> {
    let mut vals = Vec::new();
    if let Some(val) = get_register(id) {
        vals.push(val.call());
    }
    vals
}

/// The ids of every registered series.
pub fn get_collectd_ids() -> Vec<TypeInstanceId> {
    // SAFETY: a synchronous call on this shard; no other reference to the sender is live (its
    // timer and send loop run only from the reactor, not during this call).
    unsafe { get_impl() }.get_instance_ids()
}

/// The description of `id`.
pub fn get_collectd_description_str(id: &TypeInstanceId) -> Sstring {
    let mid = to_metrics_id(id);
    let local = mi::get_local_impl();
    local._value_map.get(&mid.full_name()).map(|f| f.info().d.str().clone()).unwrap_or_default()
}

/// Whether `id` is reported.
pub fn is_enabled(id: &TypeInstanceId) -> bool {
    get_register(id).is_some_and(|r| r.is_enabled())
}

/// Enable or disable collectd metrics on local instance
/// - `id`: - the metric to enable or disable
/// - `enable`: - should the collectd metrics be enable or disable
pub fn enable(id: &TypeInstanceId, enable: bool) {
    if let Some(r) = get_register(id) {
        // SAFETY: registrations are this shard's; no reference into this one is live here.
        unsafe { r.get_mut_unchecked() }.set_enabled(enable);
    }
}

/// A copy of this shard's families.
pub fn get_value_map() -> mi::ValueMap {
    mi::get_local_impl()._value_map.clone()
}

thread_local! {
    static SCOLLECTD_IMPL: std::cell::UnsafeCell<Impl> = std::cell::UnsafeCell::new(Impl::new());
}

/// This shard's collectd sender. It lives in a thread-local, so it never moves (its timer may
/// be armed in place); the timer callback and the send loop reach it through this pointer.
#[inline]
pub fn get_impl_ptr() -> *mut Impl {
    SCOLLECTD_IMPL.with(|i| i.get())
}

/// This shard's collectd sender.
///
/// # Safety
/// No other reference to the sender may be used while the returned one is live: every caller
/// gets the same object.
#[inline]
pub unsafe fn get_impl<'a>() -> &'a mut Impl {
    // SAFETY: the impl is thread-local, lives for the thread, and by the caller is not
    // otherwise borrowed.
    unsafe { &mut *get_impl_ptr() }
}
