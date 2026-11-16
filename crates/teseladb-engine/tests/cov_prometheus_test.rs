#![cfg(target_os = "linux")]
//! Tests of the Prometheus exporter: the protobuf encoding of metric families, and the
//! `/metrics` endpoint served over TCP in the text and protobuf formats, with the name, label
//! and aggregation query parameters, help lines, a global label, histograms (bucketed and
//! native) and summaries.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::internal::estimated_histogram::TimeEstimatedHistogram;
use teseladb::engine::core::metrics::{
    Description, Label, make_counter, make_gauge, make_histogram, make_summary,
    set_metric_family_configs,
};
use teseladb::engine::core::metrics_registration::MetricGroups;
use teseladb::engine::core::metrics_types::{Histogram, HistogramBucket};
use teseladb::engine::core::prometheus::{self, is_accept_protobuf, pm};
use teseladb::engine::core::relabel_config::metrics::MetricFamilyConfig;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::http::httpd::httpd::HttpServerControl;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

// ---------------------------------------------------------------------------------------------
// A minimal protobuf reader.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Field {
    Varint(u64),
    Fixed64(u64),
    Bytes(Vec<u8>),
}

fn read_varint(b: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = b[*pos];
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

/// The fields of one message, in order.
fn decode(b: &[u8]) -> Vec<(u32, Field)> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < b.len() {
        let key = read_varint(b, &mut pos);
        let field = (key >> 3) as u32;
        let value = match key & 7 {
            0 => Field::Varint(read_varint(b, &mut pos)),
            1 => {
                let v = u64::from_le_bytes(b[pos..pos + 8].try_into().unwrap());
                pos += 8;
                Field::Fixed64(v)
            }
            2 => {
                let len = read_varint(b, &mut pos) as usize;
                let v = b[pos..pos + len].to_vec();
                pos += len;
                Field::Bytes(v)
            }
            w => panic!("unexpected wire type {w}"),
        };
        out.push((field, value));
    }
    out
}

fn bytes_of(fields: &[(u32, Field)], n: u32) -> Vec<Vec<u8>> {
    fields
        .iter()
        .filter_map(|(f, v)| match v {
            Field::Bytes(b) if *f == n => Some(b.clone()),
            _ => None,
        })
        .collect()
}

fn varint_of(fields: &[(u32, Field)], n: u32) -> Option<u64> {
    fields.iter().find_map(|(f, v)| match v {
        Field::Varint(x) if *f == n => Some(*x),
        _ => None,
    })
}

fn double_of(fields: &[(u32, Field)], n: u32) -> Option<f64> {
    fields.iter().find_map(|(f, v)| match v {
        Field::Fixed64(x) if *f == n => Some(f64::from_bits(*x)),
        _ => None,
    })
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Splits length-delimited messages.
fn split_delimited(mut b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let mut pos = 0;
        let len = read_varint(b, &mut pos) as usize;
        out.push(b[pos..pos + len].to_vec());
        b = &b[pos + len..];
    }
    out
}

#[test]
fn metric_families_encode_as_delimited_protobuf() {
    let family = pm::MetricFamily {
        name: b"engine_x".to_vec(),
        type_: pm::MetricType::Histogram,
        metric: vec![
            pm::Metric {
                label: vec![pm::LabelPair { name: b"shard".to_vec(), value: b"0".to_vec() }],
                gauge: Some(1.5),
                counter: Some(-0.0),
                summary: Some(pm::Summary {
                    sample_count: 300,
                    sample_sum: 2.0,
                    quantile: vec![pm::Quantile { quantile: 0.5, value: 7.0 }],
                }),
                histogram: Some(pm::Histogram {
                    sample_count: 2,
                    sample_sum: 0.0,
                    bucket: vec![pm::Bucket { cumulative_count: 2, upper_bound: 4.0 }],
                    schema: -3,
                    positive_span: vec![pm::BucketSpan { offset: -1, length: 2 }],
                    positive_delta: vec![1, -1, 0, 1000],
                }),
            },
            pm::Metric::default(),
        ],
    };
    let mut out = vec![0xaa];
    assert!(pm::write_delimited_to(&family, &mut out));
    assert_eq!(out[0], 0xaa, "the message is appended");
    let messages = split_delimited(&out[1..]);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].len(), family.byte_size());
    let mut direct = Vec::new();
    family.write(&mut direct);
    assert_eq!(direct, messages[0]);

    let f = decode(&messages[0]);
    assert_eq!(bytes_of(&f, 1), vec![b"engine_x".to_vec()]);
    assert_eq!(varint_of(&f, 3), Some(pm::MetricType::Histogram as u64));
    let metrics = bytes_of(&f, 4);
    assert_eq!(metrics.len(), 2);
    // An empty metric is an empty message.
    assert!(metrics[1].is_empty());

    let m = decode(&metrics[0]);
    let label = decode(&bytes_of(&m, 1)[0]);
    assert_eq!(bytes_of(&label, 1), vec![b"shard".to_vec()]);
    assert_eq!(bytes_of(&label, 2), vec![b"0".to_vec()]);
    assert_eq!(double_of(&decode(&bytes_of(&m, 2)[0]), 1), Some(1.5));
    // A negative zero is not the default value and is written.
    assert_eq!(
        double_of(&decode(&bytes_of(&m, 3)[0]), 1).map(f64::to_bits),
        Some((-0.0f64).to_bits())
    );

    let summary = decode(&bytes_of(&m, 4)[0]);
    assert_eq!(varint_of(&summary, 1), Some(300));
    assert_eq!(double_of(&summary, 2), Some(2.0));
    let q = decode(&bytes_of(&summary, 3)[0]);
    assert_eq!(double_of(&q, 1), Some(0.5));
    assert_eq!(double_of(&q, 2), Some(7.0));

    let h = decode(&bytes_of(&m, 7)[0]);
    assert_eq!(varint_of(&h, 1), Some(2));
    // A zero sum is the default and is left out.
    assert_eq!(double_of(&h, 2), None);
    let bucket = decode(&bytes_of(&h, 3)[0]);
    assert_eq!(varint_of(&bucket, 1), Some(2));
    assert_eq!(double_of(&bucket, 2), Some(4.0));
    assert_eq!(varint_of(&h, 5).map(unzigzag), Some(-3));
    let span = decode(&bytes_of(&h, 12)[0]);
    assert_eq!(varint_of(&span, 1).map(unzigzag), Some(-1));
    assert_eq!(varint_of(&span, 2), Some(2));
    let packed = &bytes_of(&h, 13)[0];
    let mut pos = 0;
    let mut deltas = Vec::new();
    while pos < packed.len() {
        deltas.push(unzigzag(read_varint(packed, &mut pos)));
    }
    assert_eq!(deltas, vec![1, -1, 0, 1000]);
}

#[test]
fn accept_header_selects_protobuf() {
    assert!(is_accept_protobuf(
        "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily;encoding=delimited"
    ));
    assert!(is_accept_protobuf("text/plain, application/vnd.google.protobuf; q=0.5"));
    assert!(!is_accept_protobuf("text/plain;version=0.0.4"));
    assert!(!is_accept_protobuf(""));
}

// ---------------------------------------------------------------------------------------------
// The endpoint, over TCP.
// ---------------------------------------------------------------------------------------------

/// A response: its status line and headers, and its body with any chunked encoding removed.
struct Response {
    head: String,
    body: Vec<u8>,
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").expect("chunk size line");
        let size =
            usize::from_str_radix(std::str::from_utf8(&b[..eol]).unwrap().trim(), 16).unwrap();
        b = &b[eol + 2..];
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

fn blocking_get(port: u16, target: &str, accept: &str) -> Response {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: localhost\r\nAccept: {accept}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("end of headers");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let rest = &raw[split + 4..];
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        dechunk(rest)
    } else {
        rest.to_vec()
    };
    Response { head, body }
}

/// Sends the request from another OS thread, so that the reactor keeps serving it while this
/// engine thread waits.
fn get(port: u16, target: &str, accept: &str) -> Response {
    let (tx, rx) = mpsc::channel();
    let target = target.to_string();
    let accept = accept.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(blocking_get(port, &target, &accept));
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                sleep(Duration::from_millis(2)).get().unwrap();
            }
            Err(e) => panic!("no response: {e:?}"),
        }
    }
}

fn get_text(port: u16, target: &str) -> String {
    let r = get(port, target, "text/plain");
    assert!(r.head.starts_with("HTTP/1.1 200"), "{}", r.head);
    String::from_utf8(r.body).unwrap()
}

const PROTOBUF: &str =
    "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; encoding=delimited";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Serves the exporter with `ctx` on a free local port while `body` runs; the server is stopped
/// even when `body` fails, and the failure is passed on.
fn with_server(name: &str, ctx: prometheus::Config, body: impl FnOnce(u16)) {
    let mut server = Box::new(HttpServerControl::new());
    server.start(&Sstring::from(name)).get().unwrap();
    prometheus::start(&mut server, ctx).get().unwrap();
    let port = free_port();
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    server.listen(SocketAddress::from_inet_address(&ip, port)).get().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(port)));
    server.stop().get().unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn bucketed_histogram() -> Histogram {
    Histogram {
        sample_count: 3,
        sample_sum: 6.0,
        buckets: vec![
            HistogramBucket { count: 1, upper_bound: 1.0 },
            HistogramBucket { count: 3, upper_bound: 2.0 },
        ],
        native_histogram: None,
    }
}

fn native_histogram() -> Histogram {
    let mut h = TimeEstimatedHistogram::new();
    // Two values in one bucket, then a gap, then one more: two spans.
    h.add_micro(600);
    h.add_micro(600);
    h.add_micro(5000);
    h.to_metrics_histogram()
}

fn quantiles() -> Histogram {
    Histogram {
        sample_count: 10,
        sample_sum: 55.0,
        buckets: vec![
            HistogramBucket { count: 5, upper_bound: 0.5 },
            HistogramBucket { count: 9, upper_bound: 0.99 },
        ],
        native_histogram: None,
    }
}

fn register(groups: &mut MetricGroups) {
    let kind = Label::new("kind");
    let node = Label::new("node");
    groups
        .add_group(
            "covprom",
            vec![
                make_gauge("level", || 1.5, Description::new("a level"), vec![kind.call("a")]),
                make_gauge("level", || 2.5, Description::new("a level"), vec![kind.call("b")]),
                make_counter("events", || 42u64, Description::new("events seen"), vec![]),
                make_histogram("hist_bucketed", bucketed_histogram, Description::new("h"), vec![]),
                make_histogram("hist_native", native_histogram, Description::new("n"), vec![]),
                make_summary("latency", Description::new("quantiles"), quantiles),
                make_gauge("agg", || 1.0, Description::new("aggregated"), vec![node.call("x")]),
                make_gauge("agg", || 2.0, Description::new("aggregated"), vec![node.call("y")]),
            ],
        )
        .unwrap();
    let configs = vec![MetricFamilyConfig {
        name: "covprom_agg".to_string(),
        aggregate_labels: vec!["node".to_string()],
        ..MetricFamilyConfig::default()
    }];
    set_metric_family_configs(&configs);
}

thread_case!(metrics_endpoint_serves_text_and_protobuf, {
    let mut groups = MetricGroups::default();
    register(&mut groups);

    let ctx = prometheus::Config {
        metric_help: "fallback help".into(),
        label: Some(Label::new("instance").call("node1")),
        prefix: "cov".into(),
        allow_protobuf: true,
        ..prometheus::Config::default()
    };
    with_server("covprom", ctx, |port| check_endpoint(port));
    set_metric_family_configs(&[]);
    drop(groups);
});

fn check_endpoint(port: u16) {
    // Text: only the families named by the prefix, with help and type lines.
    let text = get_text(port, "/metrics?__name__=covprom*");
    assert!(text.contains("# HELP cov_covprom_level a level"), "{text}");
    assert!(text.contains("# TYPE cov_covprom_level gauge"));
    assert!(
        text.contains(r#"cov_covprom_level{instance="node1",kind="a",shard="0"} 1.500000"#),
        "{text}"
    );
    assert!(text.contains(r#"cov_covprom_level{instance="node1",kind="b",shard="0"} 2.500000"#));
    assert!(text.contains("# TYPE cov_covprom_events counter"));
    assert!(text.contains(r#"cov_covprom_events{instance="node1",shard="0"} 42"#));
    assert!(
        text.contains(
            r#"cov_covprom_hist_bucketed_bucket{instance="node1",le="1.000000",shard="0"} 1"#
        ),
        "{text}"
    );
    assert!(text.contains(r#"cov_covprom_hist_bucketed_count{instance="node1",shard="0"} 3"#));
    assert!(text.contains("# TYPE cov_covprom_latency summary"));
    assert!(
        text.contains(r#"cov_covprom_latency{instance="node1",quantile="0.500000",shard="0"} 5"#),
        "{text}"
    );
    assert!(text.contains(r#"cov_covprom_latency_count{instance="node1",shard="0"} 10"#));
    // The aggregated family drops the node label and adds the two series up.
    assert!(text.contains(r#"cov_covprom_agg{instance="node1",shard="0"} 3.000000"#), "{text}");
    assert!(!text.contains("node=\"x\""));
    assert!(!text.contains("cov_reactor"), "the name filter keeps other families out");

    // Without aggregation both series are listed; without help no HELP lines; the encoded
    // asterisk is a prefix too; an exact name returns one family.
    let plain = get_text(port, "/metrics?__name__=covprom_agg&__aggregate__=false&__help__=false");
    assert!(
        plain.contains(r#"cov_covprom_agg{instance="node1",node="x",shard="0"} 1.000000"#),
        "{plain}"
    );
    assert!(plain.contains(r#"node="y""#));
    assert!(!plain.contains("# HELP"));
    assert!(!plain.contains("covprom_level"));
    let encoded = get_text(port, "/metrics?__name__=covprom_lev%2A");
    assert!(encoded.contains("cov_covprom_level{"), "{encoded}");
    assert!(!encoded.contains("covprom_events"));

    // A label regex in the query keeps only the matching series.
    let shard0 = get_text(port, "/metrics?__name__=covprom_level&shard=0");
    assert!(shard0.contains("kind=\"a\""));
    let none = get_text(port, "/metrics?__name__=covprom_level&shard=9[0-9]");
    assert!(!none.contains("cov_covprom_level{"), "{none}");
    // An invalid regex fails the request.
    let bad = get(port, "/metrics?shard=%28", "text/plain");
    assert!(!bad.head.starts_with("HTTP/1.1 200"), "{}", bad.head);

    // Protobuf: delimited families with the global label.
    let r = get(port, "/metrics?__name__=covprom*", PROTOBUF);
    assert!(r.head.starts_with("HTTP/1.1 200"), "{}", r.head);
    let families: Vec<Vec<(u32, Field)>> =
        split_delimited(&r.body).iter().map(|m| decode(m)).collect();
    let family = |name: &str| {
        families
            .iter()
            .find(|f| bytes_of(f, 1) == vec![name.as_bytes().to_vec()])
            .unwrap_or_else(|| panic!("no family {name}"))
            .clone()
    };
    let level = family("cov_covprom_level");
    assert_eq!(varint_of(&level, 3), Some(pm::MetricType::Gauge as u64));
    let level_metrics = bytes_of(&level, 4);
    assert_eq!(level_metrics.len(), 2);
    let first = decode(&level_metrics[0]);
    let labels: Vec<(Vec<u8>, Vec<u8>)> = bytes_of(&first, 1)
        .iter()
        .map(|l| {
            let l = decode(l);
            (bytes_of(&l, 1)[0].clone(), bytes_of(&l, 2)[0].clone())
        })
        .collect();
    assert_eq!(labels[0], (b"instance".to_vec(), b"node1".to_vec()));
    assert!(labels.contains(&(b"kind".to_vec(), b"a".to_vec())));
    let gauges: Vec<f64> = level_metrics
        .iter()
        .map(|m| double_of(&decode(&bytes_of(&decode(m), 2)[0]), 1).unwrap())
        .collect();
    assert_eq!(gauges, vec![1.5, 2.5]);

    let events = family("cov_covprom_events");
    // The counter type is 0, the proto3 default, which is left out.
    assert_eq!(varint_of(&events, 3), None);
    assert_eq!(pm::MetricType::Counter as u64, 0);
    let counter = decode(&bytes_of(&decode(&bytes_of(&events, 4)[0]), 3)[0]);
    assert_eq!(double_of(&counter, 1), Some(42.0));

    let bucketed = family("cov_covprom_hist_bucketed");
    assert_eq!(varint_of(&bucketed, 3), Some(pm::MetricType::Histogram as u64));
    let h = decode(&bytes_of(&decode(&bytes_of(&bucketed, 4)[0]), 7)[0]);
    assert_eq!(varint_of(&h, 1), Some(3));
    assert_eq!(double_of(&h, 2), Some(6.0));
    assert_eq!(bytes_of(&h, 3).len(), 2);

    let native = family("cov_covprom_hist_native");
    let h = decode(&bytes_of(&decode(&bytes_of(&native, 4)[0]), 7)[0]);
    assert_eq!(varint_of(&h, 1), Some(3));
    assert_eq!(varint_of(&h, 5).map(unzigzag), Some(2), "schema of four buckets per power of two");
    // Native histograms have spans and deltas instead of buckets.
    assert!(bytes_of(&h, 3).is_empty());
    let spans: Vec<(i64, u64)> = bytes_of(&h, 12)
        .iter()
        .map(|s| {
            let s = decode(s);
            (varint_of(&s, 1).map_or(0, unzigzag), varint_of(&s, 2).unwrap_or(0))
        })
        .collect();
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert!(spans.iter().all(|&(_, len)| len == 1));
    let packed = &bytes_of(&h, 13)[0];
    let mut pos = 0;
    let mut deltas = Vec::new();
    while pos < packed.len() {
        deltas.push(unzigzag(read_varint(packed, &mut pos)));
    }
    // The first non-empty bucket holds 2, the next 1: deltas 2 and -1.
    assert_eq!(deltas, vec![2, -1]);

    let latency = family("cov_covprom_latency");
    assert_eq!(varint_of(&latency, 3), Some(pm::MetricType::Summary as u64));
    let s = decode(&bytes_of(&decode(&bytes_of(&latency, 4)[0]), 4)[0]);
    assert_eq!(varint_of(&s, 1), Some(10));
    let qs = bytes_of(&s, 3);
    assert_eq!(double_of(&decode(&qs[1]), 1), Some(0.99));
    assert_eq!(double_of(&decode(&qs[1]), 2), Some(9.0));

    let agg = family("cov_covprom_agg");
    let agg_metrics = bytes_of(&agg, 4);
    assert_eq!(agg_metrics.len(), 1);
    assert_eq!(double_of(&decode(&bytes_of(&decode(&agg_metrics[0]), 2)[0]), 1), Some(3.0));

    // Families with no matching series are left out of the protobuf output.
    let r = get(port, "/metrics?__name__=covprom_level&shard=7[0-9]", PROTOBUF);
    assert!(r.body.is_empty(), "{} bytes", r.body.len());
}

thread_case!(protobuf_is_served_only_when_allowed, {
    let mut groups = MetricGroups::default();
    groups
        .add_group("covprom2", vec![make_gauge("g", || 7, Description::new("g"), vec![])])
        .unwrap();
    with_server("covprom2", prometheus::Config::default(), |port| {
        let r = get(port, "/metrics?__name__=covprom2_g", PROTOBUF);
        let text = String::from_utf8(r.body).unwrap();
        assert!(text.contains(r#"engine_covprom2_g{shard="0"} 7.000000"#), "{text}");
    });
    drop(groups);
});
