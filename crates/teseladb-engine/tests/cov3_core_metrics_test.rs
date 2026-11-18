//! The metrics layer and the Prometheus text writer, case by case: metric value conversions,
//! sums and emptiness (the original's metrics_api.hh `metric_value`), every metric
//! constructor, registration errors, relabeling with several source labels and the actions that
//! change nothing, family configs applied after the metadata exists, and the text writer's
//! number formats (printf `%g` and `%f`), label escaping, summaries without sum or count,
//! native histograms ending in a span, the family ranges and the aggregation helper.
#![cfg(target_os = "linux")]
#![allow(deprecated)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::http::httpd::httpd::HttpServerControl;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;

use teseladb::engine::core::future::{ExceptionPtr, RangeError};
use teseladb::engine::core::metrics::impl_::{DataType, MetricNumber, MetricValue, MetricValueU};
use teseladb::engine::core::metrics::impl_fns::get_values;
use teseladb::engine::core::metrics::{
    Description, DoubleRegistration, Histogram, HistogramBucket, Label, NativeHistogramInfo,
    Options, get_metric_family_configs, get_relabel_configs, make_absolute, make_counter,
    make_derive, make_derive_fn, make_derive_labeled, make_gauge, make_gauge_fn, make_histogram,
    make_histogram_labeled, make_total_operations, set_metric_family_configs, set_relabel_configs,
};
use teseladb::engine::core::metrics_registration::MetricGroups;
use teseladb::engine::core::prometheus::{
    self, MetricAggregateByLabels, MetricsFamiliesPerShard, get_range, write_histogram,
    write_summary, write_value_as_string,
};
use teseladb::engine::core::relabel_config::metrics::{
    MetricFamilyConfig, RelabelAction, RelabelConfig, RelabelConfigRegex,
};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::testing::on_internal_error::testing::ScopedNoAbortOnInternalError;
use teseladb::engine_thread_test_case;

fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

fn value_text(v: &MetricValue) -> Result<String, ExceptionPtr> {
    let mut s = Vec::new();
    write_value_as_string(&mut s, v)?;
    Ok(text(&s))
}

fn hist(count: u64, sum: f64, buckets: &[(u64, f64)]) -> Histogram {
    Histogram {
        sample_count: count,
        sample_sum: sum,
        buckets: buckets
            .iter()
            .map(|&(count, upper_bound)| HistogramBucket { count, upper_bound })
            .collect(),
        native_histogram: None,
    }
}

fn labels(kv: &[(&str, &str)]) -> BTreeMap<Sstring, Sstring> {
    kv.iter().map(|(k, v)| (Sstring::from(*k), Sstring::from(*v))).collect()
}

#[test]
fn integer_conversions_reject_values_out_of_range() {
    // As the original's metric_value::ui()/i(): a value that does not fit is a range error,
    // NaN included (it compares false with both bounds).
    assert_eq!(MetricValue::from_double(2.6, DataType::Counter).ui().unwrap(), 3);
    assert_eq!(MetricValue::from_double(-2.6, DataType::Gauge).i().unwrap(), -3);
    for bad in [-1.0, f64::NAN, 1e300] {
        let e = MetricValue::from_double(bad, DataType::Gauge).ui().unwrap_err();
        assert!(e.is::<RangeError>(), "{e:?}");
    }
    for bad in [f64::NAN, 1e300, -1e300] {
        let e = MetricValue::from_double(bad, DataType::Gauge).i().unwrap_err();
        assert!(e.is::<RangeError>());
        assert!(e.get().unwrap().what().contains("cannot convert double value"));
    }
}

#[test]
fn default_values_are_zero_gauges() {
    for v in [MetricValue::new(), MetricValue::default()] {
        assert_eq!(v.type_(), DataType::Gauge);
        assert_eq!(v.d(), 0.0);
        assert!(!v.is_empty(), "a gauge is never empty");
        assert!(v.get_histogram().buckets.is_empty(), "a number has the empty histogram");
        assert_eq!(v.get_histogram().sample_count, 0);
    }
}

#[test]
fn sums_add_numbers_and_histograms_bucket_by_bucket() {
    let a = MetricValue::from_double(1.5, DataType::RealCounter);
    let b = MetricValue::from_double(2.0, DataType::RealCounter);
    assert_eq!(a.add(&b).unwrap().d(), 3.5);

    let h1 = MetricValue::from_histogram(hist(2, 3.0, &[(1, 1.0), (2, 2.0)]), DataType::Histogram);
    let h2 = MetricValue::from_histogram(hist(1, 1.0, &[(1, 1.0), (1, 2.0)]), DataType::Histogram);
    let mut sum = h1.clone();
    sum.add_assign(&h2).unwrap();
    assert_eq!(sum.get_histogram().sample_count, 3);
    assert_eq!(sum.get_histogram().buckets[1].count, 3);
    // As the original's histogram::operator+=: fewer buckets add into the first ones, more
    // buckets are appended, and different bucket bounds do not add.
    let fewer = MetricValue::from_histogram(hist(1, 1.0, &[(1, 1.0)]), DataType::Summary);
    let mut s = MetricValue::from_histogram(hist(1, 1.0, &[(1, 1.0), (1, 2.0)]), DataType::Summary);
    s.add_assign(&fewer).unwrap();
    assert_eq!(s.get_histogram().buckets[0].count, 2);
    assert_eq!(s.get_histogram().buckets[1].count, 1);
    let more = MetricValue::from_histogram(
        hist(1, 1.0, &[(1, 1.0), (1, 2.0), (1, 4.0)]),
        DataType::Summary,
    );
    s.add_assign(&more).unwrap();
    assert_eq!(s.get_histogram().buckets.len(), 3);
    let other = MetricValue::from_histogram(hist(1, 1.0, &[(1, 5.0)]), DataType::Summary);
    assert!(s.add_assign(&other).is_err());

    // A histogram kind holding a number, or a number kind holding a histogram, keeps its value.
    let odd = MetricValue { u: MetricValueU::Double(4.0), _type: DataType::Histogram };
    assert_eq!(odd.add(&h1).unwrap().d(), 4.0);
    let odd2 =
        MetricValue { u: MetricValueU::Histogram(hist(1, 1.0, &[])), _type: DataType::Gauge };
    assert_eq!(odd2.add(&a).unwrap().d(), 0.0);
}

#[test]
fn emptiness_follows_the_kind() {
    let empty_h = MetricValue::from_histogram(hist(0, 0.0, &[]), DataType::Histogram);
    assert!(empty_h.is_empty());
    assert!(MetricValue::from_histogram(hist(0, 0.0, &[]), DataType::Summary).is_empty());
    assert!(!MetricValue::from_histogram(hist(1, 0.0, &[]), DataType::Summary).is_empty());
    assert!(!MetricValue::from_histogram(hist(1, 0.0, &[]), DataType::Histogram).is_empty());
    assert!(MetricValue::from_double(0.0, DataType::Counter).is_empty());
    assert!(MetricValue::from_double(0.0, DataType::RealCounter).is_empty());
    assert!(!MetricValue::from_double(1.0, DataType::RealCounter).is_empty());
    assert!(!MetricValue::from_double(1.0, DataType::Counter).is_empty());
    assert!(!MetricValue::from_double(0.0, DataType::Gauge).is_empty());
}

#[test]
fn booleans_report_as_zero_or_one() {
    assert_eq!(true.to_metric_value(DataType::Gauge).d(), 1.0);
    assert_eq!(false.to_metric_value(DataType::Counter).d(), 0.0);
    const { assert!(<bool as MetricNumber>::IS_INTEGRAL) };
}

#[test]
fn double_registration_displays_its_message() {
    let e = DoubleRegistration::new("twice".to_string());
    assert_eq!(e.to_string(), "twice");
    assert_eq!(format!("{e}"), "twice");
}

#[test]
fn every_counter_constructor_makes_a_counter() {
    let lb = Label::new("l");
    let defs = [
        make_derive("d", || 1u64, Description::new("d"), vec![]),
        make_derive_fn("d", Description::new("d"), || 1u64),
        make_derive_labeled("d", Description::new("d"), vec![lb.call("x")], || 1u64),
        make_absolute("a", || 1u64, Description::new("a"), vec![]),
        make_total_operations("t", || 1u64, Description::new("t"), vec![]),
    ];
    for d in &defs {
        assert_eq!(d.type_.base_type, DataType::Counter, "{}", d.name);
    }
    assert_eq!(defs[2].labels.get(&Sstring::from("l")).unwrap(), "x");
    assert_eq!(defs[4].type_.type_name, "total_operations");
    let real = make_derive("r", || 1.5f64, Description::new("r"), vec![]);
    assert_eq!(real.type_.base_type, DataType::RealCounter);
    let h = make_histogram_labeled("h", Description::new("h"), vec![lb.call("y")], || {
        hist(0, 0.0, &[])
    });
    assert_eq!(h.type_.base_type, DataType::Histogram);
    assert_eq!(h.labels.get(&Sstring::from("l")).unwrap(), "y");
    assert_eq!((h.f)().type_(), DataType::Histogram);
}

#[test]
fn options_default_to_the_host_name() {
    let o = Options::default();
    let host = o.metrics_hostname.get_value().clone();
    assert!(!host.is_empty());
}

// ---------------------------------------------------------------------------------------------
// The registry.
// ---------------------------------------------------------------------------------------------

fn series_labels(family: &str) -> Vec<BTreeMap<Sstring, Sstring>> {
    let values = get_values();
    let v = values.get_local().expect("local values");
    let md = v.metadata.as_ref().expect("metadata");
    md.iter()
        .filter(|f| f.mf.name == family)
        .flat_map(|f| f.metrics.iter().map(|m| m.labels().clone()))
        .collect()
}

engine_thread_test_case!(a_family_keeps_one_type, {
    let mut groups = MetricGroups::default();
    groups
        .add_group("cov3reg", vec![make_gauge("kind", || 1, Description::new("g"), vec![])])
        .unwrap();
    // The same name with other labels but another type is rejected (the original's
    // "registered with different type").
    let lb = Label::new("k");
    let e = groups
        .add_group(
            "cov3reg",
            vec![make_counter("kind", || 1u64, Description::new("c"), vec![lb.call("2")])],
        )
        .err()
        .expect("a counter in a gauge family");
    assert!(e.get().unwrap().what().contains("different type"), "{e:?}");
    // The same series twice is a double registration.
    let e = groups
        .add_group("cov3reg", vec![make_gauge("kind", || 1, Description::new("g"), vec![])])
        .err()
        .expect("twice");
    assert!(e.is::<DoubleRegistration>());
    // Dashes and spaces become underscores in the full name; '+', '(' and ')' are dropped.
    groups
        .add_group("cov3-reg", vec![make_gauge("a b(+)", || 1, Description::new("g"), vec![])])
        .unwrap();
    assert_eq!(series_labels("cov3_reg_a_b").len(), 1);
    drop(groups);
    assert!(series_labels("cov3reg_kind").is_empty());
});

fn regex(s: &str) -> RelabelConfigRegex {
    RelabelConfigRegex::new(s).unwrap()
}

engine_thread_test_case!(relabeling_joins_source_labels_and_ignores_no_op_rules, {
    let mut groups = MetricGroups::default();
    let a = Label::new("a");
    let b = Label::new("b");
    groups
        .add_group(
            "cov3rl",
            vec![
                make_gauge("g", || 1, Description::new("g"), vec![a.call("x"), b.call("y")]),
                make_gauge("g", || 2, Description::new("g"), vec![a.call("z"), b.call("w")]),
            ],
        )
        .unwrap();
    let joined = RelabelConfig {
        source_labels: vec!["a".into(), "b".into(), "__name__".into()],
        separator: "|".into(),
        expr: regex("x\\|y\\|cov3rl_g"),
        target_label: "j".into(),
        replacement: "hit".into(),
        ..RelabelConfig::default()
    };
    // A rule whose label is absent and a replace without target change nothing.
    let drop_absent = RelabelConfig {
        source_labels: vec!["a".into()],
        expr: regex(".*"),
        action: RelabelAction::DropLabel,
        target_label: "absent".into(),
        ..RelabelConfig::default()
    };
    let no_target = RelabelConfig {
        source_labels: vec!["a".into()],
        expr: regex(".*"),
        ..RelabelConfig::default()
    };
    let rules = vec![joined, drop_absent, no_target];
    let r = set_relabel_configs(&rules).get().unwrap();
    assert_eq!(r.metrics_relabeled_due_to_collision, 0);
    assert_eq!(get_relabel_configs().len(), 3);
    let series = series_labels("cov3rl_g");
    assert_eq!(series.len(), 2);
    let hits: Vec<_> = series.iter().filter(|l| l.contains_key(&Sstring::from("j"))).collect();
    assert_eq!(hits.len(), 1, "{series:?}");
    assert_eq!(hits[0].get(&Sstring::from("a")).unwrap(), "x");
    // A new registration gets the rules too.
    groups
        .add_group(
            "cov3rl",
            vec![make_gauge("g", || 3, Description::new("g"), vec![a.call("x"), b.call("v")])],
        )
        .unwrap();
    assert_eq!(series_labels("cov3rl_g").len(), 3);
    set_relabel_configs(&[]).get().unwrap();
    assert!(get_relabel_configs().is_empty());
    assert!(series_labels("cov3rl_g").iter().all(|l| !l.contains_key(&Sstring::from("j"))));
    drop(groups);
});

engine_thread_test_case!(family_configs_reach_metadata_already_built, {
    let mut groups = MetricGroups::default();
    let n = Label::new("n");
    groups
        .add_group(
            "cov3fc",
            vec![
                make_gauge("one", || 1, Description::new("g"), vec![n.call("1")]),
                make_gauge("two", || 1, Description::new("g"), vec![n.call("1")]),
            ],
        )
        .unwrap();
    // Build the metadata first, then change the family configs: by name and by expression.
    let _ = get_values();
    let configs = vec![
        MetricFamilyConfig {
            name: "cov3fc_one".into(),
            aggregate_labels: vec!["n".into()],
            ..MetricFamilyConfig::default()
        },
        MetricFamilyConfig {
            regex_name: regex("cov3fc_t.*"),
            aggregate_labels: vec!["shard".into()],
            ..MetricFamilyConfig::default()
        },
    ];
    set_metric_family_configs(&configs);
    assert_eq!(get_metric_family_configs().len(), 2);
    let values = get_values();
    let md = values.get_local().unwrap().metadata.as_ref().unwrap();
    let agg = |name: &str| {
        md.iter().find(|f| f.mf.name == name).map(|f| f.mf.aggregate_labels.clone()).unwrap()
    };
    assert_eq!(agg("cov3fc_one"), vec!["n".to_string()]);
    assert_eq!(agg("cov3fc_two"), vec!["shard".to_string()]);
    set_metric_family_configs(&[]);
    assert!(get_metric_family_configs().is_empty());
    drop(groups);
});

// ---------------------------------------------------------------------------------------------
// The text writer.
// ---------------------------------------------------------------------------------------------

#[test]
fn values_are_written_with_six_decimals_and_counters_as_integers() {
    assert_eq!(value_text(&MetricValue::from_double(1.25, DataType::Gauge)).unwrap(), "1.250000");
    assert_eq!(value_text(&MetricValue::from_double(f64::NAN, DataType::Gauge)).unwrap(), "nan");
    assert_eq!(value_text(&MetricValue::from_double(-f64::NAN, DataType::Gauge)).unwrap(), "-nan");
    assert_eq!(value_text(&MetricValue::from_double(7.0, DataType::Counter)).unwrap(), "7");
    // A counter that does not fit in an integer is written as NaN.
    assert_eq!(value_text(&MetricValue::from_double(1e300, DataType::Counter)).unwrap(), "NaN");
    // A histogram has no single value.
    let h = MetricValue::from_histogram(hist(1, 1.0, &[]), DataType::Histogram);
    assert_eq!(value_text(&h).unwrap(), "");
}

/// The `_sum` line of a histogram with `sum`: the `%g` format.
fn sum_text(sum: f64) -> String {
    let mut s = Vec::new();
    write_histogram(
        &mut s,
        &prometheus::Config::default(),
        &"h".into(),
        &hist(0, sum, &[]),
        labels(&[]),
    );
    let t = text(&s);
    t.lines().next().unwrap().strip_prefix("h_sum{} ").unwrap().to_string()
}

#[test]
fn sums_use_the_printf_g_format() {
    // printf("%g") of each value.
    let cases: [(f64, &str); 14] = [
        (f64::NAN, "nan"),
        (-f64::NAN, "-nan"),
        (f64::INFINITY, "inf"),
        (f64::NEG_INFINITY, "-inf"),
        (0.0, "0"),
        (-0.0, "-0"),
        (1.5, "1.5"),
        (100000.0, "100000"),
        (123456.0, "123456"),
        (1234567.0, "1.23457e+06"),
        (1e6, "1e+06"),
        (0.0001, "0.0001"),
        (0.00001234, "1.234e-05"),
        (-2.5e-7, "-2.5e-07"),
    ];
    for (v, want) in cases {
        assert_eq!(sum_text(v), want, "{v}");
    }
    assert_eq!(sum_text(1e100), "1e+100");
}

#[test]
fn histogram_lines_escape_labels_and_skip_reserved_ones() {
    let ctx = prometheus::Config {
        label: Some(Label::new("inst").call("a\"b")),
        ..prometheus::Config::default()
    };
    let mut s = Vec::new();
    let l = labels(&[("__hidden", "x"), ("v", "line\nbreak\\")]);
    write_histogram(&mut s, &ctx, &"h".into(), &hist(2, 3.0, &[(1, f64::NAN)]), l);
    let t = text(&s);
    assert!(t.contains(r#"h_sum{inst="a\"b",v="line\nbreak\\"} 3"#), "{t}");
    assert!(!t.contains("__hidden"));
    assert!(t.contains(r#"le="nan""#), "{t}");
    assert!(t.contains(r#"h_bucket{inst="a\"b",le="+Inf",v="line\nbreak\\"} 2"#), "{t}");
}

#[test]
fn summaries_without_sum_or_count_write_only_quantiles() {
    let mut s = Vec::new();
    let ctx = prometheus::Config::default();
    write_summary(&mut s, &ctx, &"q".into(), &hist(0, 0.0, &[(4, 0.5)]), labels(&[]));
    assert_eq!(text(&s), "q{quantile=\"0.500000\"} 4\n");
    let mut s = Vec::new();
    write_summary(&mut s, &ctx, &"q".into(), &hist(3, 1.5, &[]), labels(&[]));
    assert_eq!(text(&s), "q_sum{} 1.5\nq_count{} 3\n");
}

// On the runner's reactor, where the tests take turns: the switch is of the whole process, and
// the runner turns it on at the start of each test.
engine_thread_test_case!(aggregating_mismatched_histograms_reports_and_keeps_the_first, {
    let mut agg = MetricAggregateByLabels::new(vec!["shard".into()]);
    assert!(agg.empty());
    let h2 = MetricValue::from_histogram(hist(1, 1.0, &[(1, 1.0), (1, 2.0)]), DataType::Histogram);
    let h1 = MetricValue::from_histogram(hist(1, 1.0, &[(1, 9.0)]), DataType::Histogram);
    agg.add(&h2, labels(&[("shard", "0"), ("x", "1")]));
    {
        // A mismatch is an internal error, reported without aborting when aborting is off.
        let _no_abort = ScopedNoAbortOnInternalError::new();
        agg.add(&h1, labels(&[("shard", "1"), ("x", "1")]));
    }
    agg.add(&h2, labels(&[("shard", "2"), ("x", "1")]));
    let v = agg.get_values();
    assert_eq!(v.len(), 1);
    let (k, sum) = v.iter().next().unwrap();
    assert_eq!(k, &labels(&[("x", "1")]));
    assert_eq!(sum.get_histogram().sample_count, 2, "the mismatched one is not added");
});

engine_thread_test_case!(family_ranges_by_name_and_prefix, {
    let mut groups = MetricGroups::default();
    let n = Label::new("n");
    groups
        .add_group(
            "cov3rg",
            vec![
                make_gauge("aa", || 1, Description::new("g"), vec![n.call("1")]),
                make_gauge("aa", || 2, Description::new("g"), vec![n.call("2")]),
                make_gauge("ab", || 3, Description::new("g"), vec![]),
                make_gauge_fn("b", Description::new("g"), || 4),
            ],
        )
        .unwrap();
    let mut fams = MetricsFamiliesPerShard::default();
    fams.resize(Smp::count() as usize);
    fams.at_mut(0).assign(get_values());
    assert_eq!(fams.iter().count(), Smp::count() as usize);

    let exact = get_range(&fams, &"cov3rg_aa".into(), false);
    let names: Vec<String> = exact.iter().map(|f| f.name().to_string()).collect();
    assert_eq!(names, vec!["cov3rg_aa"]);
    let first = exact.begin();
    assert_eq!(first.size(), 2);
    assert_eq!(first.get().size(), 2);
    assert_eq!(first.get().name(), "cov3rg_aa");
    assert!(!first.get().end());

    let prefix = get_range(&fams, &"cov3rg_a".into(), true);
    let names: Vec<String> = prefix.iter().map(|f| f.name().to_string()).collect();
    assert_eq!(names, vec!["cov3rg_aa", "cov3rg_ab"]);

    // A name that is not there, between two families or after the last, is an empty range.
    assert_eq!(get_range(&fams, &"cov3rg_a".into(), false).iter().count(), 0);
    assert_eq!(get_range(&fams, &"zzzz".into(), false).iter().count(), 0);

    // upper_bound is the first family after the name.
    let ub = fams.upper_bound(&"cov3rg_aa".into());
    assert_eq!(ub.name(), "cov3rg_ab");
    let past = fams.upper_bound(&"zzzz".into());
    assert!(past.end());
    assert!(past.get().end());
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| past.get().name().size()));
    assert!(r.is_err(), "the end has no name");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        past.get().metadata().name.size()
    }));
    assert!(r.is_err(), "the end has no metadata");
    drop(fams);
    drop(groups);
});

// ---------------------------------------------------------------------------------------------
// The endpoint: aggregated histograms in text, a literal "%2A" suffix, native histograms whose
// last bucket is in a span, in protobuf.
// ---------------------------------------------------------------------------------------------

fn blocking_get(port: u16, target: &str, accept: &str) -> (String, Vec<u8>) {
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
    let mut rest = &raw[split + 4..];
    let mut body = Vec::new();
    if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        loop {
            let eol = rest.windows(2).position(|w| w == b"\r\n").unwrap();
            let n = usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap().trim(), 16)
                .unwrap();
            rest = &rest[eol + 2..];
            if n == 0 {
                break;
            }
            body.extend_from_slice(&rest[..n]);
            rest = &rest[n + 2..];
        }
    } else {
        body = rest.to_vec();
    }
    (head, body)
}

/// Sends the request from another OS thread while this engine thread keeps the reactor going.
fn get(port: u16, target: &str, accept: &str) -> (String, Vec<u8>) {
    let (tx, rx) = mpsc::channel();
    let (target, accept) = (target.to_string(), accept.to_string());
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

fn read_varint(b: &[u8], pos: &mut usize) -> u64 {
    let (mut v, mut shift) = (0u64, 0);
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

/// The length-delimited fields `n` of a message (other wire types skipped).
fn submessages(b: &[u8], n: u64) -> Vec<Vec<u8>> {
    let (mut pos, mut out) = (0, Vec::new());
    while pos < b.len() {
        let key = read_varint(b, &mut pos);
        match key & 7 {
            0 => {
                read_varint(b, &mut pos);
            }
            1 => pos += 8,
            2 => {
                let len = read_varint(b, &mut pos) as usize;
                if key >> 3 == n {
                    out.push(b[pos..pos + len].to_vec());
                }
                pos += len;
            }
            w => panic!("wire type {w}"),
        }
    }
    out
}

fn varint_field(b: &[u8], n: u64) -> Option<u64> {
    let mut pos = 0;
    while pos < b.len() {
        let key = read_varint(b, &mut pos);
        match key & 7 {
            0 => {
                let v = read_varint(b, &mut pos);
                if key >> 3 == n {
                    return Some(v);
                }
            }
            1 => pos += 8,
            2 => pos += read_varint(b, &mut pos) as usize,
            w => panic!("wire type {w}"),
        }
    }
    None
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

const PROTOBUF: &str =
    "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; encoding=delimited";

/// Cumulative counts 0, 2, 2, 3, 5: one span of one bucket at id 1 and one of two buckets at
/// id 3 that is still open at the last bucket.
fn native_ending_in_span() -> Histogram {
    Histogram {
        sample_count: 5,
        sample_sum: 9.0,
        buckets: [0u64, 2, 2, 3, 5]
            .iter()
            .enumerate()
            .map(|(i, &count)| HistogramBucket { count, upper_bound: i as f64 })
            .collect(),
        native_histogram: Some(NativeHistogramInfo { schema: 1, min_id: 0 }),
    }
}

engine_thread_test_case!(endpoint_aggregates_histograms_and_encodes_open_spans, {
    let mut groups = MetricGroups::default();
    let node = Label::new("node");
    groups
        .add_group(
            "cov3ep",
            vec![
                make_histogram(
                    "lat",
                    || hist(1, 2.0, &[(1, 1.0)]),
                    Description::new("l"),
                    vec![node.call("a")],
                ),
                make_histogram(
                    "lat",
                    || hist(2, 4.0, &[(2, 1.0)]),
                    Description::new("l"),
                    vec![node.call("b")],
                ),
                make_histogram("nat", native_ending_in_span, Description::new("n"), vec![]),
                make_gauge("p%2A", || 1, Description::new("g"), vec![]),
            ],
        )
        .unwrap();
    set_metric_family_configs(&[MetricFamilyConfig {
        name: "cov3ep_lat".into(),
        aggregate_labels: vec!["node".into()],
        ..MetricFamilyConfig::default()
    }]);
    let mut server = Box::new(HttpServerControl::new());
    server.start(&Sstring::from("cov3ep")).get().unwrap();
    let ctx = prometheus::Config { allow_protobuf: true, ..prometheus::Config::default() };
    prometheus::start(&mut server, ctx).get().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    server.listen(SocketAddress::from_inet_address(&ip, port)).get().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The two histograms of the aggregated family add up bucket by bucket.
        let (head, body) = get(port, "/metrics?__name__=cov3ep_lat", "text/plain");
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let t = text(&body);
        assert!(t.contains("engine_cov3ep_lat_count{shard=\"0\"} 3"), "{t}");
        assert!(t.contains("engine_cov3ep_lat_bucket{le=\"1.000000\",shard=\"0\"} 3"), "{t}");
        assert!(!t.contains("node="), "{t}");

        // "%252A" reaches the handler as "%2A", the encoded asterisk: a prefix match.
        let (_, body) = get(port, "/metrics?__name__=cov3ep_p%252A", "text/plain");
        let t = text(&body);
        assert!(t.contains("engine_cov3ep_p%2A{shard=\"0\"} 1.000000"), "{t}");

        let (head, body) = get(port, "/metrics?__name__=cov3ep_nat", PROTOBUF);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let mut pos = 0;
        let len = read_varint(&body, &mut pos) as usize;
        let family = &body[pos..pos + len];
        let metric = &submessages(family, 4)[0];
        let h = &submessages(metric, 7)[0];
        assert_eq!(varint_field(h, 1), Some(5));
        let spans: Vec<(i64, u64)> = submessages(h, 12)
            .iter()
            .map(|s| (varint_field(s, 1).map_or(0, unzigzag), varint_field(s, 2).unwrap_or(0)))
            .collect();
        assert_eq!(spans, vec![(1, 1), (1, 2)], "offsets are relative to the previous span");
        let packed = &submessages(h, 13)[0];
        let mut p = 0;
        let mut deltas = Vec::new();
        while p < packed.len() {
            deltas.push(unzigzag(read_varint(packed, &mut p)));
        }
        assert_eq!(deltas, vec![2, -1, 1]);
    }));
    server.stop().get().unwrap();
    set_metric_family_configs(&[]);
    drop(groups);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
});
