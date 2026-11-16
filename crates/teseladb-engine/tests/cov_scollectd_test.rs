//! Tests of the collectd exporter: the names of the known types, the truncation of long ids,
//! the wire encoding of values, the query interface (ids, values, descriptions, enabling), and
//! the packets the per-shard senders send to a UDP socket: the start notification, explicit
//! values and the polled series.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]
#![allow(deprecated)]

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use teseladb::engine::core::metrics::impl_::DataType;
use teseladb::engine::core::scollectd::send_notification;
use teseladb::engine::core::scollectd::{
    Description, KnownType, MAX_COLLECTD_FIELD_TEXT_LEN, Options, Registration, ScValue,
    TypeInstanceId, ValueList, add_disabled_polled_metric, add_polled_metric, configure,
    create_explicit_metric, enable, get_collectd_description_str, get_collectd_ids,
    get_collectd_value, get_impl, is_enabled, make_type_instance, make_typed,
    per_cpu_plugin_instance, percpu_plugin_instance_metrics, remove_polled_metric,
    send_explicit_metric, type_id_for, typed_value_impl,
};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine_thread_test_case;

fn snake_case(variant: &str) -> String {
    let mut out = String::new();
    for (i, c) in variant.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

macro_rules! check_known_types {
    ($($v:ident),* $(,)?) => {
        $(
            assert_eq!(
                type_id_for(KnownType::$v).as_str(),
                snake_case(stringify!($v)),
                "type name of {}",
                stringify!($v)
            );
        )*
    };
}

#[test]
fn known_type_names_follow_the_variant_names() {
    check_known_types!(
        Absolute,
        Backends,
        Bitrate,
        BlockedClients,
        Bytes,
        CacheEviction,
        CacheOperation,
        CacheRatio,
        CacheResult,
        CacheSize,
        Capacity,
        ChangesSinceLastSave,
        Charge,
        ClockLastMeas,
        ClockLastUpdate,
        ClockMode,
        ClockReachability,
        ClockSkewPpm,
        ClockState,
        ClockStratum,
        Compression,
        CompressionRatio,
        Connections,
        Conntrack,
        Contextswitch,
        Count,
        Counter,
        Cpu,
        Cpufreq,
        Current,
        CurrentConnections,
        CurrentSessions,
        Delay,
        Derive,
        Df,
        DfComplex,
        DfInodes,
        DiskIoTime,
        DiskLatency,
        DiskMerged,
        DiskOctets,
        DiskOps,
        DiskOpsComplex,
        DiskTime,
        DnsAnswer,
        DnsNotify,
        DnsOctets,
        DnsOpcode,
        DnsQtype,
        DnsQtypeCached,
        DnsQuery,
        DnsQuestion,
        DnsRcode,
        DnsReject,
        DnsRequest,
        DnsResolver,
        DnsResponse,
        DnsTransfer,
        DnsUpdate,
        DnsZops,
        DrbdResource,
        Duration,
        EmailCheck,
        EmailCount,
        EmailSize,
        Entropy,
        EvictedKeys,
        ExpiredKeys,
        Fanspeed,
        FileHandles,
        FileSize,
        Files,
        Flow,
        ForkRate,
        Frequency,
        FrequencyError,
        FrequencyOffset,
        FscacheStat,
        Gauge,
        HashCollisions,
        HttpRequestMethods,
        HttpRequests,
        HttpResponseCodes,
        Humidity,
        IfCollisions,
        IfDropped,
        IfErrors,
        IfMulticast,
        IfOctets,
        IfPackets,
        IfRxErrors,
        IfRxOctets,
        IfTxErrors,
        IfTxOctets,
        Invocations,
        IoOctets,
        IoPackets,
        IptBytes,
        IptPackets,
        Irq,
        Latency,
        Links,
        Load,
        MdDisks,
        Memory,
        MemoryLua,
        MemoryThrottleCount,
        Multimeter,
        MutexOperations,
        Objects,
        Operations,
        Packets,
        PendingOperations,
        Percent,
        PercentBytes,
        PercentInodes,
        Ping,
        PingDroprate,
        PingStddev,
        Players,
        Power,
        Pressure,
        ProtocolCounter,
        Pubsub,
        QueueLength,
        Records,
        Requests,
        ResponseCode,
        ResponseTime,
        RootDelay,
        RootDispersion,
        RouteEtx,
        RouteMetric,
        Routes,
        Segments,
        SerialOctets,
        SignalNoise,
        SignalPower,
        SignalQuality,
        Snr,
        Spl,
        Swap,
        SwapIo,
        TcpConnections,
        Temperature,
        Threads,
        TimeDispersion,
        TimeOffset,
        TimeOffsetNtp,
        TimeOffsetRms,
        TimeRef,
        Timeleft,
        TotalBytes,
        TotalConnections,
        TotalObjects,
        TotalOperations,
        TotalRequests,
        TotalSessions,
        TotalThreads,
        TotalTimeInMs,
        TotalValues,
        Uptime,
        Users,
        Vcl,
        Vcpu,
        VirtCpuTotal,
        VirtVcpu,
        VmpageAction,
        VmpageFaults,
        VmpageIo,
        VmpageNumber,
        VolatileChanges,
        Voltage,
        VoltageThreshold,
        VsMemory,
        VsProcesses,
        VsThreads,
    );
}

fn id(plugin: &str, type_: &str, type_instance: &str) -> TypeInstanceId {
    TypeInstanceId::new(
        plugin.into(),
        per_cpu_plugin_instance(),
        type_.into(),
        type_instance.into(),
    )
}

#[test]
fn type_instance_id_keeps_short_fields_and_truncates_long_ones() {
    let short = id("plugin", "gauge", "value");
    assert_eq!(short.plugin().as_str(), "plugin");
    assert_eq!(short.plugin_instance().as_str(), "#cpu");
    assert_eq!(short.type_().as_str(), "gauge");
    assert_eq!(short.type_instance().as_str(), "value");

    // A field of exactly the maximum length is kept as it is.
    let exact = "x".repeat(MAX_COLLECTD_FIELD_TEXT_LEN);
    assert_eq!(id(&exact, "t", "i").plugin().as_str(), exact);

    // Longer fields are cut and get a "~<n>" suffix with a per-thread counter; the first
    // truncation of this thread uses 1, the next ones count up.
    let long = "a".repeat(100);
    let first = id(&long, "t", "i");
    assert!(first.plugin().as_str().starts_with("aaaa"));
    assert!(first.plugin().as_str().ends_with("~1"), "{}", first.plugin());
    let second = id("p", &long, "i");
    assert!(second.type_().as_str().ends_with("~2"), "{}", second.type_());
    assert_eq!(second.type_().size(), MAX_COLLECTD_FIELD_TEXT_LEN);
    let third = id("p", "t", &long);
    assert!(third.type_instance().as_str().ends_with("~3"));
    assert_eq!(third.type_instance().size(), MAX_COLLECTD_FIELD_TEXT_LEN);
}

#[test]
fn sc_values_are_encoded_for_the_wire() {
    // A gauge is sent as the little-endian bytes of a double.
    let gauge = ScValue::new(DataType::Gauge, || 2.5f64);
    assert_eq!(gauge.type_(), DataType::Gauge);
    assert_eq!(gauge.call().to_ne_bytes(), 2.5f64.to_le_bytes());
    // An integer gauge is converted to a double first.
    let int_gauge = ScValue::new(DataType::Gauge, || 3u32);
    assert_eq!(int_gauge.call().to_ne_bytes(), 3.0f64.to_le_bytes());
    // Counters are sent as big-endian unsigned integers; floats are truncated.
    let counter = ScValue::new(DataType::Counter, || 0x0102u64);
    assert_eq!(counter.call().to_ne_bytes(), 0x0102u64.to_be_bytes());
    let float_counter = ScValue::new(DataType::Counter, || 7.9f32);
    assert_eq!(float_counter.call().to_ne_bytes(), 7u64.to_be_bytes());
    let negative = ScValue::new(DataType::Counter, || -1i8);
    assert_eq!(negative.call(), u64::MAX);

    let typed = make_typed(DataType::Counter, 5u8);
    assert_eq!(typed.type_, DataType::Counter);
    assert_eq!(typed.value, 5);
}

#[test]
fn values_impl_lays_out_types_and_values() {
    let mut list = make_type_instance(
        Description::new("two values"),
        vec![ScValue::new(DataType::Counter, || 9u64), ScValue::new(DataType::Gauge, || 0.5f64)],
    );
    assert_eq!(list.size(), 2);
    assert!(!list.empty());
    assert_eq!(list.desc().str().as_str(), "two values");
    let mut types = [0xffu8; 2];
    list.types(&mut types);
    // The collectd codes: COUNTER (0) for the integer, GAUGE (1) for the double.
    assert_eq!(types, [0, 1]);
    let mut values = [0u8; 16];
    list.values(&mut values);
    assert_eq!(&values[..8], &9u64.to_be_bytes());
    assert_eq!(&values[8..], &0.5f64.to_le_bytes());

    assert!(list.is_enabled());
    list.set_enabled(false);
    assert!(!list.is_enabled());

    let empty = make_type_instance(Description::default(), Vec::new());
    assert!(empty.empty());
}

#[test]
fn typed_values_carry_their_type_and_instance() {
    let v = typed_value_impl(
        KnownType::Bytes,
        &"rx".into(),
        Description::new("received"),
        vec![ScValue::new(DataType::Counter, || 1u64)],
    );
    assert_eq!(v.type_().as_str(), "bytes");
    assert_eq!(v.type_instance().as_str(), "rx");
    assert_eq!(v.values().size(), 1);
}

engine_thread_test_case!(polled_metrics_are_queried_enabled_and_removed, {
    let gauge_id = id("covquery", "gauge", "level");
    add_polled_metric(&gauge_id, Description::new("the level"), DataType::Gauge, || 4.25f64, true)
        .unwrap();
    let counter_id = id("covquery", "total_operations", "ops");
    add_disabled_polled_metric(&counter_id, Description::new("ops"), DataType::Counter, || 12u64)
        .unwrap();

    // The ids read back name the shard as the plugin instance and the family's type.
    let listed = |plugin: &str, type_instance: &str| {
        get_collectd_ids()
            .into_iter()
            .find(|i| i.plugin().as_str() == plugin && i.type_instance().as_str() == type_instance)
    };
    let gauge_back = listed("covquery", "level").expect("the gauge is listed");
    assert_eq!(gauge_back.plugin_instance().as_str(), "0");
    assert_eq!(gauge_back.type_().as_str(), "gauge");
    assert_eq!(listed("covquery", "ops").unwrap().type_().as_str(), "total_operations");
    let values = get_collectd_value(&gauge_id);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].d(), 4.25);
    assert_eq!(get_collectd_value(&counter_id)[0].ui().unwrap(), 12);
    assert_eq!(get_collectd_description_str(&gauge_id).as_str(), "the level");
    assert!(get_collectd_description_str(&id("covquery", "gauge", "missing")).is_empty());
    assert!(get_collectd_value(&id("covquery", "gauge", "missing")).is_empty());

    assert!(is_enabled(&gauge_id));
    assert!(!is_enabled(&counter_id));
    enable(&counter_id, true);
    assert!(is_enabled(&counter_id));
    enable(&gauge_id, false);
    assert!(!is_enabled(&gauge_id));
    assert!(!is_enabled(&id("covquery", "gauge", "missing")));

    remove_polled_metric(&gauge_id);
    assert!(listed("covquery", "level").is_none());
    // A registration removes its series when dropped, or when unregistered explicitly.
    {
        let _reg = Registration::new(counter_id.clone());
    }
    assert!(listed("covquery", "ops").is_none());
    let reg_id = id("covquery", "gauge", "registered");
    add_polled_metric(&reg_id, Description::default(), DataType::Gauge, || 1.0f64, true).unwrap();
    let mut reg = Registration::new(reg_id.clone());
    assert!(listed("covquery", "registered").is_some());
    reg.unregister();
    assert!(listed("covquery", "registered").is_none());
});

engine_thread_test_case!(plugin_instance_metrics_bind_their_ids, {
    let values = [typed_value_impl(
        KnownType::Objects,
        &"live".into(),
        Description::new("live objects"),
        vec![ScValue::new(DataType::Gauge, || 3u32)],
    )];
    let mut metrics = percpu_plugin_instance_metrics(&"covplugin".into(), &values);
    let extra = typed_value_impl(
        KnownType::Objects,
        &"dead".into(),
        Description::default(),
        vec![ScValue::new(DataType::Gauge, || 0u32)],
    );
    metrics.add(&extra);
    let bound = metrics.bound_ids();
    assert_eq!(bound.len(), 2);
    assert_eq!(bound[0], id("covplugin", "objects", "live"));
    assert_eq!(bound[1], id("covplugin", "objects", "dead"));
});

/// One part of a collectd packet.
#[derive(Debug, Clone, PartialEq)]
enum Part {
    Str(u16, String),
    Number(u16, u64),
    Values(Vec<(u8, [u8; 8])>),
}

const HOST: u16 = 0x0000;
const TIME: u16 = 0x0001;
const PLUGIN: u16 = 0x0002;
const TYPE: u16 = 0x0004;
const TYPE_INSTANCE: u16 = 0x0005;
const VALUES: u16 = 0x0006;
const INTERVAL_HR: u16 = 0x0009;
const MESSAGE: u16 = 0x0100;

/// Splits a packet into its parts; panics on a malformed one.
fn parse_packet(mut p: &[u8]) -> Vec<Part> {
    let mut parts = Vec::new();
    while !p.is_empty() {
        let type_ = u16::from_be_bytes([p[0], p[1]]);
        let len = usize::from(u16::from_be_bytes([p[2], p[3]]));
        assert!(len >= 4 && len <= p.len(), "bad part length {len}");
        let body = &p[4..len];
        let part = match type_ {
            TIME | INTERVAL_HR => Part::Number(type_, u64::from_be_bytes(body.try_into().unwrap())),
            VALUES => {
                let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
                assert_eq!(body.len(), 2 + n * 9);
                let types = &body[2..2 + n];
                let vals = &body[2 + n..];
                Part::Values(
                    (0..n)
                        .map(|i| (types[i], vals[i * 8..i * 8 + 8].try_into().unwrap()))
                        .collect(),
                )
            }
            _ => {
                assert_eq!(body.last(), Some(&0), "string part without its terminator");
                Part::Str(type_, String::from_utf8(body[..body.len() - 1].to_vec()).unwrap())
            }
        };
        parts.push(part);
        p = &p[len..];
    }
    parts
}

/// Receives packets until `found` accepts one of them, or panics after a few seconds. The
/// socket is non-blocking: the reactor keeps running (and sending) between the attempts.
fn wait_for_packet(sock: &UdpSocket, mut found: impl FnMut(&[Part]) -> bool) -> Vec<Part> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        match sock.recv(&mut buf) {
            Ok(n) => {
                assert!(n <= 1024, "packet of {n} bytes");
                let parts = parse_packet(&buf[..n]);
                if found(&parts) {
                    return parts;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                sleep(Duration::from_millis(5)).get().unwrap();
            }
            Err(e) => panic!("recv: {e}"),
        }
    }
    panic!("no matching collectd packet");
}

/// The senders' poll period, in milliseconds. A shard's first round must not start while its
/// start notification is still being sent on the same channel (see `stop_senders`), so the
/// period leaves the notification ample time even on a loaded machine.
const POLL_PERIOD_MS: u64 = 200;

/// How long the socket must stay silent for the senders to count as stopped: five poll
/// periods.
const QUIET: Duration = Duration::from_millis(5 * POLL_PERIOD_MS);

/// Stops the sender of every shard and waits until none of them sends any more.
///
/// A datagram channel takes one send at a time: a second send issued before the first
/// completes shares its message header and its poll completion (the original aborts or
/// hangs the same way). A sender's round already running when `stop()` cancels its timer
/// re-arms the timer when it ends, so `stop()` is repeated until the socket stays quiet;
/// after that no round runs, and the test's own sends on the channel cannot overlap one.
fn stop_senders(sock: &UdpSocket) {
    let mut buf = [0u8; 2048];
    for _ in 0..20 {
        Smp::invoke_on_all(|| {
            // SAFETY: a synchronous call on that shard with no other reference to its sender.
            unsafe { get_impl() }.stop();
        })
        .get()
        .unwrap();
        sleep(QUIET).get().unwrap();
        while sock.recv(&mut buf).is_ok() {}
        sleep(QUIET).get().unwrap();
        if sock.recv(&mut buf).is_err() {
            return;
        }
    }
    panic!("a stopped sender still sends");
}

fn has_str(parts: &[Part], type_: u16, s: &str) -> bool {
    parts.contains(&Part::Str(type_, s.to_string()))
}

engine_thread_test_case!(senders_send_notifications_explicit_values_and_series, {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let port = sock.local_addr().unwrap().port();

    let series_id = id("covsend", "gauge", "temperature");
    add_polled_metric(&series_id, Description::new("t"), DataType::Gauge, || 21.5f64, true)
        .unwrap();

    let mut opts = Options::new();
    opts.collectd.set_value(true);
    opts.collectd_address.set_value(format!("127.0.0.1:{port}"));
    opts.collectd_poll_period.set_value(POLL_PERIOD_MS as u32);
    opts.collectd_hostname.set_value("covhost".to_string());
    configure(&opts).unwrap();
    // SAFETY: a synchronous call on this shard with no other reference to the sender.
    assert_eq!(unsafe { get_impl() }.host().as_str(), "covhost");

    // Every shard announces itself when it starts.
    let started = wait_for_packet(&sock, |p| has_str(p, MESSAGE, "daemon started"));
    assert!(has_str(&started, HOST, "covhost"));
    assert!(has_str(&started, PLUGIN, "scollectd"));
    assert!(has_str(&started, TYPE, "network"));

    // The polled series arrives with the gauge as a little-endian double.
    let series = wait_for_packet(&sock, |p| {
        has_str(p, PLUGIN, "covsend") && has_str(p, TYPE_INSTANCE, "temperature")
    });
    assert!(has_str(&series, HOST, "covhost"));
    assert!(series.iter().any(|p| matches!(p, Part::Number(TIME, t) if *t > 0)));
    // The interval is the poll period in units of 2^-30 seconds.
    let period = (POLL_PERIOD_MS << 30) / 1000;
    assert!(series.contains(&Part::Number(INTERVAL_HR, period)));
    assert!(series.iter().any(|p| matches!(p, Part::Values(v)
        if v.iter().any(|(_, bytes)| *bytes == 21.5f64.to_le_bytes()))));

    // The explicit sends below share the shard's channel with the polling rounds, so the
    // rounds are stopped first (see `stop_senders`).
    stop_senders(&sock);
    remove_polled_metric(&series_id);

    // An explicit value list is sent at once, with its own types.
    let explicit_id = id("covexplicit", "derive", "events");
    send_explicit_metric(
        &explicit_id,
        vec![ScValue::new(DataType::Counter, || 77u64), ScValue::new(DataType::Gauge, || 0.25f64)],
    )
    .get()
    .unwrap();
    let explicit = wait_for_packet(&sock, |p| has_str(p, PLUGIN, "covexplicit"));
    // The collectd codes: COUNTER (0) and GAUGE (1).
    let expected = Part::Values(vec![(0, 77u64.to_be_bytes()), (1, 0.25f64.to_le_bytes())]);
    assert!(explicit.contains(&expected), "{explicit:?}");
    assert!(has_str(&explicit, TYPE_INSTANCE, "events"));
    // A sender with no values sends nothing and completes at once.
    send_explicit_metric(&id("covexplicit", "derive", "none"), Vec::new()).get().unwrap();

    let notify = create_explicit_metric(
        &id("covnotify", "derive", "ticks"),
        vec![ScValue::new(DataType::Counter, || 5u64)],
    );
    notify();
    let notified = wait_for_packet(&sock, |p| has_str(p, PLUGIN, "covnotify"));
    assert!(notified.contains(&Part::Values(vec![(0, 5u64.to_be_bytes())])));

    let msg = send_notification(&id("covmsg", "derive", "x"), &Sstring::from("hello"));
    msg.get().unwrap();
    wait_for_packet(&sock, |p| has_str(p, PLUGIN, "covmsg") && has_str(p, MESSAGE, "hello"));

    // The stopped senders send nothing more after a few periods.
    let mut buf = [0u8; 2048];
    sleep(QUIET).get().unwrap();
    assert!(sock.recv(&mut buf).is_err(), "a stopped sender still sends");
});

engine_thread_test_case!(disabled_or_bad_options_start_nothing, {
    let opts = Options::default();
    assert!(!*opts.collectd.get_value());
    assert_eq!(opts.collectd_address.get_value(), "239.192.74.66:25826");
    assert_eq!(*opts.collectd_poll_period.get_value(), 1000);
    configure(&opts).unwrap();

    let mut bad = Options::new();
    bad.collectd.set_value(true);
    bad.collectd_address.set_value("not an address".to_string());
    assert!(configure(&bad).is_err());
});
