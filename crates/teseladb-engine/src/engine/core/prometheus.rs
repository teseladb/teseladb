//! The Prometheus exporter: a `/metrics` HTTP endpoint that serves the metrics of every shard.
//!
//! [`start`] (or [`add_prometheus_routes`]) installs a handler on each shard of the HTTP server.
//! A request gathers a snapshot of the metric families from every shard, merges them by family
//! name and writes them in the Prometheus text exposition format, or, when allowed by
//! [`Config::allow_protobuf`] and asked for in the `Accept` header, in the protobuf exposition
//! format.
//!
//! Query parameters shape the answer: `__name__` selects a family by name (a trailing `*` makes
//! it a prefix), a parameter named after a label keeps the series whose label value matches its
//! regular expression, `__help__=false` leaves out the help lines, and `__aggregate__=false`
//! disables the aggregation of families that have aggregation labels (summed over those labels
//! by default).
//!
//! The protobuf messages (`io.prometheus.client`, proto3) are the plain structs of [`pm`] with a
//! hand-written proto3 wire encoder.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::io::Write as _;
use std::pin::Pin;
use std::rc::Rc;

use crate::engine::core::distributed::Distributed;
use crate::engine::core::do_with::do_with;
use crate::engine::core::future::{
    ExceptionPtr, Future, RangeError, make_exception_future, make_ready_future,
};
use crate::engine::core::iostream::OutputStream;
use crate::engine::core::loop_::{do_for_each, parallel_for_each};
use crate::engine::core::metrics::Histogram;
use crate::engine::core::metrics::LabelInstance;
use crate::engine::core::metrics::impl_ as mi;
use crate::engine::core::on_internal_error::on_internal_error_noexcept;
use crate::engine::core::reactor::engine_logger;
use crate::engine::core::relabel_config::regex::Regex;
use crate::engine::core::sharded::ForeignPtr;
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::thread::{self, Thread};
use crate::engine::http::common::httpd::OperationType;
use crate::engine::http::handlers::{HandlerBase, HandlerBaseData};
use crate::engine::http::httpd::httpd::{HttpServer, HttpServerControl};
use crate::engine::http::reply::{BodyWriter, Reply};
use crate::engine::http::request::Request;
use crate::{log_debug, log_error, log_warn};

/// Holds prometheus related configuration
#[derive(Debug, Clone)]
pub struct Config {
    /// Default help message for the returned metrics
    pub metric_help: Sstring,
    /// Deprecated: use `label` instead.
    pub hostname: Sstring,
    /// A label added to all metrics; better set on the Prometheus server instead.
    pub label: Option<LabelInstance>,
    /// A prefix added to metric names (`"engine"` by default).
    pub prefix: Sstring,
    /// Whether the protobuf format may be served; experimental and off by default.
    pub allow_protobuf: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            metric_help: Sstring::new(),
            hostname: Sstring::new(),
            label: None,
            prefix: "engine".into(),
            allow_protobuf: false,
        }
    }
}

/// The protobuf messages of the Prometheus exposition format (`io.prometheus.client`, proto3)
/// and their wire encoder.
///
/// The encoder writes canonical proto3: fields in field number order, no scalar equal to its
/// default (0, an empty string, `+0.0`), sub-messages that are set even when empty, and repeated
/// scalars packed. Lengths are computed before writing, so a message is written in one pass with
/// no intermediate buffer.
pub mod pm {
    /// `MetricType`
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    #[repr(i32)]
    pub enum MetricType {
        /// `COUNTER`
        #[default]
        Counter = 0,
        /// `GAUGE`
        Gauge = 1,
        /// `SUMMARY`
        Summary = 2,
        /// `UNTYPED`
        Untyped = 3,
        /// `HISTOGRAM`
        Histogram = 4,
        /// `GAUGE_HISTOGRAM`
        GaugeHistogram = 5,
    }

    /// `LabelPair` (`string name = 1; string value = 2;`); the bytes are copied as they are,
    /// without UTF-8 validation.
    #[derive(Debug, Clone, Default)]
    pub struct LabelPair {
        /// `name = 1`
        pub name: Vec<u8>,
        /// `value = 2`
        pub value: Vec<u8>,
    }

    /// `Quantile`
    #[derive(Debug, Clone, Default)]
    pub struct Quantile {
        /// `quantile = 1`
        pub quantile: f64,
        /// `value = 2`
        pub value: f64,
    }

    /// `Summary`
    #[derive(Debug, Clone, Default)]
    pub struct Summary {
        /// `sample_count = 1`
        pub sample_count: u64,
        /// `sample_sum = 2`
        pub sample_sum: f64,
        /// `quantile = 3`
        pub quantile: Vec<Quantile>,
    }

    /// `Bucket`
    #[derive(Debug, Clone, Default)]
    pub struct Bucket {
        /// `cumulative_count = 1`
        pub cumulative_count: u64,
        /// `upper_bound = 2`
        pub upper_bound: f64,
    }

    /// `BucketSpan`
    #[derive(Debug, Clone, Default)]
    pub struct BucketSpan {
        /// `offset = 1` (`sint32`)
        pub offset: i32,
        /// `length = 2` (`uint32`)
        pub length: u32,
    }

    /// `Histogram`, with the fields the exporter sets.
    #[derive(Debug, Clone, Default)]
    pub struct Histogram {
        /// `sample_count = 1`
        pub sample_count: u64,
        /// `sample_sum = 2`
        pub sample_sum: f64,
        /// `bucket = 3`
        pub bucket: Vec<Bucket>,
        /// `schema = 5` (`sint32`)
        pub schema: i32,
        /// `positive_span = 12`
        pub positive_span: Vec<BucketSpan>,
        /// `positive_delta = 13` (`repeated sint64`, packed)
        pub positive_delta: Vec<i64>,
    }

    /// `Metric`
    #[derive(Debug, Clone, Default)]
    pub struct Metric {
        /// `label = 1`
        pub label: Vec<LabelPair>,
        /// `gauge = 2` (`Gauge { double value = 1; }`)
        pub gauge: Option<f64>,
        /// `counter = 3` (`Counter { double value = 1; }`)
        pub counter: Option<f64>,
        /// `summary = 4`
        pub summary: Option<Summary>,
        /// `histogram = 7`
        pub histogram: Option<Histogram>,
    }

    /// `MetricFamily`
    #[derive(Debug, Clone, Default)]
    pub struct MetricFamily {
        /// `name = 1`
        pub name: Vec<u8>,
        /// `type = 3`
        pub type_: MetricType,
        /// `metric = 4`
        pub metric: Vec<Metric>,
    }

    const VARINT: u32 = 0;
    const FIXED64: u32 = 1;
    const LEN: u32 = 2;

    #[inline]
    fn varint_len(v: u64) -> usize {
        (64 - (v | 1).leading_zeros() as usize).div_ceil(7)
    }

    #[inline]
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    #[inline]
    fn zigzag(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    #[inline]
    fn tag_len(field: u32) -> usize {
        varint_len(u64::from(field << 3))
    }

    #[inline]
    fn key(out: &mut Vec<u8>, field: u32, wire: u32) {
        varint(out, u64::from((field << 3) | wire));
    }

    // Each scalar has its length (0 when proto3 leaves it out) and its writer.

    #[inline]
    fn double_len(field: u32, v: f64) -> usize {
        // proto3 leaves out +0.0 only: `-0.0` and NaN have other bits and are written.
        if v.to_bits() == 0 { 0 } else { tag_len(field) + 8 }
    }

    #[inline]
    fn double(out: &mut Vec<u8>, field: u32, v: f64) {
        if v.to_bits() != 0 {
            key(out, field, FIXED64);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }

    #[inline]
    fn uint_len(field: u32, v: u64) -> usize {
        if v == 0 { 0 } else { tag_len(field) + varint_len(v) }
    }

    #[inline]
    fn uint(out: &mut Vec<u8>, field: u32, v: u64) {
        if v != 0 {
            key(out, field, VARINT);
            varint(out, v);
        }
    }

    #[inline]
    fn sint_len(field: u32, v: i64) -> usize {
        if v == 0 { 0 } else { tag_len(field) + varint_len(zigzag(v)) }
    }

    #[inline]
    fn sint(out: &mut Vec<u8>, field: u32, v: i64) {
        if v != 0 {
            key(out, field, VARINT);
            varint(out, zigzag(v));
        }
    }

    #[inline]
    fn bytes_len(field: u32, b: &[u8]) -> usize {
        if b.is_empty() { 0 } else { tag_len(field) + varint_len(b.len() as u64) + b.len() }
    }

    #[inline]
    fn bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
        if !b.is_empty() {
            key(out, field, LEN);
            varint(out, b.len() as u64);
            out.extend_from_slice(b);
        }
    }

    /// The length of a set sub-message field whose body is `len` bytes.
    #[inline]
    fn message_len(field: u32, len: usize) -> usize {
        tag_len(field) + varint_len(len as u64) + len
    }

    #[inline]
    fn message_header(out: &mut Vec<u8>, field: u32, len: usize) {
        key(out, field, LEN);
        varint(out, len as u64);
    }

    impl LabelPair {
        fn byte_size(&self) -> usize {
            bytes_len(1, &self.name) + bytes_len(2, &self.value)
        }

        fn write(&self, out: &mut Vec<u8>) {
            bytes(out, 1, &self.name);
            bytes(out, 2, &self.value);
        }
    }

    impl Quantile {
        fn byte_size(&self) -> usize {
            double_len(1, self.quantile) + double_len(2, self.value)
        }

        fn write(&self, out: &mut Vec<u8>) {
            double(out, 1, self.quantile);
            double(out, 2, self.value);
        }
    }

    impl Summary {
        fn byte_size(&self) -> usize {
            uint_len(1, self.sample_count)
                + double_len(2, self.sample_sum)
                + self.quantile.iter().map(|q| message_len(3, q.byte_size())).sum::<usize>()
        }

        fn write(&self, out: &mut Vec<u8>) {
            uint(out, 1, self.sample_count);
            double(out, 2, self.sample_sum);
            for q in &self.quantile {
                message_header(out, 3, q.byte_size());
                q.write(out);
            }
        }
    }

    impl Bucket {
        fn byte_size(&self) -> usize {
            uint_len(1, self.cumulative_count) + double_len(2, self.upper_bound)
        }

        fn write(&self, out: &mut Vec<u8>) {
            uint(out, 1, self.cumulative_count);
            double(out, 2, self.upper_bound);
        }
    }

    impl BucketSpan {
        fn byte_size(&self) -> usize {
            sint_len(1, i64::from(self.offset)) + uint_len(2, u64::from(self.length))
        }

        fn write(&self, out: &mut Vec<u8>) {
            sint(out, 1, i64::from(self.offset));
            uint(out, 2, u64::from(self.length));
        }
    }

    impl Histogram {
        fn deltas_len(&self) -> usize {
            self.positive_delta.iter().map(|&d| varint_len(zigzag(d))).sum()
        }

        fn byte_size(&self) -> usize {
            let deltas = self.deltas_len();
            uint_len(1, self.sample_count)
                + double_len(2, self.sample_sum)
                + self.bucket.iter().map(|b| message_len(3, b.byte_size())).sum::<usize>()
                + sint_len(5, i64::from(self.schema))
                + self.positive_span.iter().map(|s| message_len(12, s.byte_size())).sum::<usize>()
                + if deltas == 0 { 0 } else { message_len(13, deltas) }
        }

        fn write(&self, out: &mut Vec<u8>) {
            uint(out, 1, self.sample_count);
            double(out, 2, self.sample_sum);
            for b in &self.bucket {
                message_header(out, 3, b.byte_size());
                b.write(out);
            }
            sint(out, 5, i64::from(self.schema));
            for s in &self.positive_span {
                message_header(out, 12, s.byte_size());
                s.write(out);
            }
            let deltas = self.deltas_len();
            if deltas != 0 {
                message_header(out, 13, deltas);
                for &d in &self.positive_delta {
                    varint(out, zigzag(d));
                }
            }
        }
    }

    impl Metric {
        fn byte_size(&self) -> usize {
            self.label.iter().map(|l| message_len(1, l.byte_size())).sum::<usize>()
                + self.gauge.map_or(0, |g| message_len(2, double_len(1, g)))
                + self.counter.map_or(0, |c| message_len(3, double_len(1, c)))
                + self.summary.as_ref().map_or(0, |s| message_len(4, s.byte_size()))
                + self.histogram.as_ref().map_or(0, |h| message_len(7, h.byte_size()))
        }

        fn write(&self, out: &mut Vec<u8>) {
            for l in &self.label {
                message_header(out, 1, l.byte_size());
                l.write(out);
            }
            if let Some(g) = self.gauge {
                message_header(out, 2, double_len(1, g));
                double(out, 1, g);
            }
            if let Some(c) = self.counter {
                message_header(out, 3, double_len(1, c));
                double(out, 1, c);
            }
            if let Some(s) = &self.summary {
                message_header(out, 4, s.byte_size());
                s.write(out);
            }
            if let Some(h) = &self.histogram {
                message_header(out, 7, h.byte_size());
                h.write(out);
            }
        }
    }

    impl MetricFamily {
        /// `ByteSizeLong()`
        pub fn byte_size(&self) -> usize {
            bytes_len(1, &self.name)
                + uint_len(3, self.type_ as i32 as u64)
                + self.metric.iter().map(|m| message_len(4, m.byte_size())).sum::<usize>()
        }

        /// `SerializeToString()` appended to `out`.
        pub fn write(&self, out: &mut Vec<u8>) {
            bytes(out, 1, &self.name);
            uint(out, 3, self.type_ as i32 as u64);
            for m in &self.metric {
                message_header(out, 4, m.byte_size());
                m.write(out);
            }
        }
    }

    /// Appends the varint length of `message` and the message. Writing to memory cannot fail,
    /// so this always returns `true`.
    pub fn write_delimited_to(message: &MetricFamily, raw_output: &mut Vec<u8>) -> bool {
        let size = message.byte_size();
        raw_output.reserve(varint_len(size as u64) + size);
        varint(raw_output, size as u64);
        message.write(raw_output);
        true
    }
}

fn add_label<'a>(mt: &'a mut pm::Metric, id: &mi::LabelsType, ctx: &Config) -> &'a mut pm::Metric {
    mt.label.reserve(id.len() + 1);
    if let Some(label) = &ctx.label {
        mt.label.push(pm::LabelPair {
            name: label.key().as_bytes().to_vec(),
            value: label.value().as_bytes().to_vec(),
        });
    }
    for (name, value) in id {
        mt.label.push(pm::LabelPair {
            name: name.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        });
    }
    mt
}

fn fill_old_type_histogram(h: &Histogram, mh: &mut pm::Histogram) {
    mh.sample_count = h.sample_count;
    mh.sample_sum = h.sample_sum;
    for b in &h.buckets {
        mh.bucket.push(pm::Bucket { cumulative_count: b.count, upper_bound: b.upper_bound });
    }
}

/// Fill a histogram using the Prometheus native histogram representation.
///
/// Prometheus Native histogram (also known as sparse histograms)
/// uses an exponential bucket size with a coefficient equal to 2^(2^-schema).
/// Besides the schema, a native histogram has a list of BucketSpan and a list of deltas.
/// Each entry in the list of deltas represents a nonempty bucket,
/// the bucket value is stored as a delta from the previous nonempty bucket.
/// The bucket-spans list describes the buckets ids.
/// Each back span represents multiple consecutive nonempty buckets.
/// It holds the id of the first bucket in the span of buckets and the length (number of nonempty consecutive buckets).
fn fill_native_type_histogram(h: &Histogram, mh: &mut pm::Histogram) {
    mh.sample_count = h.sample_count;
    mh.sample_sum = h.sample_sum;
    let Some(native) = h.native_histogram else {
        return;
    };
    mh.schema = native.schema;
    let mut last_bucket: f64 = 0.0;
    let mut count: f64 = 0.0;
    let mut length: u32 = 0;
    let mut last_bucket_id: i64 = 0;
    let mut bucket_span: Option<usize> = None;
    for (id, b) in (native.min_id as i64..).zip(&h.buckets) {
        // Metrics histograms are aggregated histograms
        // A non empty bucket is bigger than the previous one
        if count < b.count as f64 {
            // If we are not part of an existing bucket-span, create one
            if bucket_span.is_none() {
                mh.positive_span
                    .push(pm::BucketSpan { offset: (id - last_bucket_id) as i32, length: 0 });
                bucket_span = Some(mh.positive_span.len() - 1);
                length = 0;
            }
            length += 1;
            mh.positive_delta.push((b.count as f64 - count - last_bucket) as i64);
            last_bucket = b.count as f64 - count;
        } else {
            // The current bucket is empty, if there is an existing bucket-span
            // set its length
            if let Some(i) = bucket_span.take() {
                mh.positive_span[i].length = length;
                last_bucket_id = id;
            }
        }
        count = b.count as f64;
    }
    // maybe there is an open bucket span (the last bucket was part of a bucket-span)
    // set its length
    if let Some(i) = bucket_span {
        mh.positive_span[i].length = length;
    }
}

fn fill_metric(mf: &mut pm::MetricFamily, c: &mi::MetricValue, id: &mi::LabelsType, ctx: &Config) {
    let mut m = pm::Metric::default();
    match c.type_() {
        mi::DataType::Gauge => {
            add_label(&mut m, id, ctx).gauge = Some(c.d());
            mf.type_ = pm::MetricType::Gauge;
        }
        mi::DataType::Summary => {
            let h = c.get_histogram();
            let mut mh = pm::Summary {
                sample_count: h.sample_count,
                sample_sum: h.sample_sum,
                quantile: Vec::new(),
            };
            for b in &h.buckets {
                mh.quantile.push(pm::Quantile { value: b.count as f64, quantile: b.upper_bound });
            }
            add_label(&mut m, id, ctx).summary = Some(mh);
            mf.type_ = pm::MetricType::Summary;
        }
        mi::DataType::Histogram => {
            let h = c.get_histogram();
            let mut mh = pm::Histogram {
                sample_count: h.sample_count,
                sample_sum: h.sample_sum,
                ..Default::default()
            };
            if h.native_histogram.is_some() {
                fill_native_type_histogram(h, &mut mh);
            } else {
                fill_old_type_histogram(h, &mut mh);
            }
            add_label(&mut m, id, ctx).histogram = Some(mh);
            mf.type_ = pm::MetricType::Histogram;
        }
        mi::DataType::RealCounter | mi::DataType::Counter => {
            add_label(&mut m, id, ctx).counter = Some(c.d());
            mf.type_ = pm::MetricType::Counter;
        }
    }
    mf.metric.push(m);
}

fn data_type_name(dt: mi::DataType) -> &'static str {
    match dt {
        mi::DataType::Gauge => "gauge",
        mi::DataType::Counter | mi::DataType::RealCounter => "counter",
        mi::DataType::Histogram => "histogram",
        mi::DataType::Summary => "summary",
    }
}

/// Writes `d` in the `%g` format with precision 6 (`nan`, `inf`, shortest of fixed and
/// exponent notation, trailing zeros removed).
fn fmt_g(s: &mut Vec<u8>, d: f64) {
    if d.is_nan() {
        s.extend_from_slice(if d.is_sign_negative() { b"-nan" } else { b"nan" });
        return;
    }
    if d.is_infinite() {
        s.extend_from_slice(if d < 0.0 { b"-inf" } else { b"inf" });
        return;
    }
    if d == 0.0 {
        s.extend_from_slice(if d.is_sign_negative() { b"-0" } else { b"0" });
        return;
    }
    const P: i32 = 6;
    let e = format!("{:.*e}", (P - 1) as usize, d);
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let x: i32 = exp.parse().unwrap_or(0);
    // Writing into a Vec never fails.
    if !(-4..P).contains(&x) {
        let mant = trim_zeros(mant);
        let _ = write!(s, "{}e{}{:02}", mant, if x < 0 { '-' } else { '+' }, x.abs());
    } else {
        let f = format!("{:.*}", (P - 1 - x).max(0) as usize, d);
        s.extend_from_slice(trim_zeros(&f).as_bytes());
    }
}

fn trim_zeros(f: &str) -> &str {
    if f.contains('.') { f.trim_end_matches('0').trim_end_matches('.') } else { f }
}

/// Writes `d` with six decimals (`%f`): as printf, NaN is `nan` (`-nan` with the sign bit)
/// where Rust's `{:.6}` writes `NaN`.
fn fmt_fixed6(s: &mut Vec<u8>, d: f64) {
    if d.is_nan() {
        s.extend_from_slice(if d.is_sign_negative() { b"-nan" } else { b"nan" });
    } else {
        // Writing into a Vec never fails.
        let _ = write!(s, "{:.6}", d);
    }
}

/// `d` with six decimals, as an `Sstring`.
fn to_sstring_f(d: f64) -> Sstring {
    let mut v = Vec::with_capacity(16);
    fmt_fixed6(&mut v, d);
    Sstring::from_bytes(&v)
}

/// Writes a metric value: real values with six decimals, integer counters as integers.
///
/// # Errors
///
/// Returns a range error for a counter that does not fit in an integer.
fn write_metric_value(os: &mut Vec<u8>, v: &mi::MetricValue) -> Result<(), ExceptionPtr> {
    match v.type_() {
        mi::DataType::Gauge | mi::DataType::RealCounter => fmt_fixed6(os, v.d()),
        mi::DataType::Counter => {
            // Writing into a Vec never fails.
            let _ = write!(os, "{}", v.i()?);
        }
        mi::DataType::Histogram | mi::DataType::Summary => {}
    }
    Ok(())
}

/*
 * Sanitizes the prometheus label value as per the line format rules and writes it out to the ostream:
 * > label_value can be any sequence of UTF-8 characters, but the backslash (\), double-quote ("), and
 * > line feed (\n) characters have to be escaped as \\, \", and \n, respectively.
 */
fn escape_and_write_label_value(s: &mut Vec<u8>, label_value: &[u8]) {
    for &c in label_value {
        match c {
            b'\\' => s.extend_from_slice(br"\\"),
            b'"' => s.extend_from_slice(br#"\""#),
            b'\n' => s.extend_from_slice(br"\n"),
            _ => s.push(c),
        }
    }
}

/// `add_name(s, name + suffix, labels, ctx)`: the suffix is written after the name instead of
/// building the concatenated string.
fn add_name(
    s: &mut Vec<u8>,
    name: &[u8],
    suffix: &[u8],
    labels: &BTreeMap<Sstring, Sstring>,
    ctx: &Config,
) {
    s.extend_from_slice(name);
    s.extend_from_slice(suffix);
    s.push(b'{');
    let mut delimiter: &[u8] = b"";
    if let Some(label) = &ctx.label {
        s.extend_from_slice(label.key().as_bytes());
        s.extend_from_slice(b"=\"");
        escape_and_write_label_value(s, label.value().as_bytes());
        s.push(b'"');
        delimiter = b",";
    }

    if !labels.is_empty() {
        for (k, v) in labels {
            if !k.starts_with(b"__") {
                s.extend_from_slice(delimiter);
                s.extend_from_slice(k.as_bytes());
                s.extend_from_slice(b"=\"");
                escape_and_write_label_value(s, v.as_bytes());
                s.push(b'"');
                delimiter = b",";
            }
        }
    }
    s.extend_from_slice(b"} ");
}

/// iterator for metric family
///
/// In prometheus, a single shard collecct all the data from the other
/// shards and report it.
///
/// Each shard returns a value_copy struct that has a vector of vector values (a vector per metric family)
/// and a vector of metadata (and insdie it a vector of metric metadata)
///
/// The metrics are sorted by the metric family name.
///
/// In prometheus, all the metrics that belongs to the same metric family are reported together.
///
/// For efficiency the results from the metrics layer are kept in a vector.
///
/// So we have a vector of shards of a vector of metric families of a vector of values.
///
/// To produce the result, we use the metric_family_iterator that is created by metric_family_range.
///
/// When iterating over the metrics we use two helper structure.
///
/// 1. A map between metric family name and the total number of values (combine on all shards) and
///    pointer to the metric family metadata.
/// 2. A vector of positions to the current metric family for each shard.
///
/// The metric_family_range returns a metric_family_iterator that goes over all the families.
///
/// The iterator returns a metric_family object, that can report the metric_family name, the size (how many
/// metrics in total belongs to the metric family) and a a foreach_metric method.
///
/// The foreach_metric method can be used to perform an action on each of the metric that belongs to
/// that metric family
///
/// Iterating over the metrics is done:
/// - go over each of the shard and each of the entry in the position vector:
///   - if the current family (the metric family that we get from the shard and position) has the current name:
///     - iterate over each of the metrics belong to that metric family:
#[derive(Default)]
pub struct MetricsFamiliesPerShard {
    _data: Vec<ForeignPtr<mi::ValuesReference>>,
}

fn shard_metadata(v: &ForeignPtr<mi::ValuesReference>) -> &[mi::MetricFamilyMetadata] {
    // SAFETY: each shard's values reference is a snapshot that its owner no longer writes
    // while the families are iterated (it is only released on the owner when the
    // `MetricsFamiliesPerShard` is dropped); it is only read here.
    match unsafe { v.get_unchecked() }.and_then(|c| c.metadata.as_ref()) {
        Some(m) => m,
        None => &[],
    }
}

fn shard_values(v: &ForeignPtr<mi::ValuesReference>) -> Option<&mi::MetricValues> {
    // SAFETY: as in shard_metadata.
    unsafe { v.get_unchecked() }.map(|c| &c.values)
}

impl MetricsFamiliesPerShard {
    /// find the last item in a range of metric family based on a comparator function
    fn find_bound(
        &self,
        family_name: &Sstring,
        comp: impl Fn(&Sstring, &mi::MetricFamilyMetadata) -> bool,
    ) -> MetricFamilyIterator<'_> {
        let mut positions = Vec::with_capacity(Smp::count() as usize);
        for shard_info in &self._data {
            let metadata = shard_metadata(shard_info);
            // upper_bound: the first element for which comp(value, element) is true
            let it_b = metadata.partition_point(|e| !comp(family_name, e));
            positions.push(it_b);
        }
        MetricFamilyIterator::with_positions(self, positions)
    }

    /// find the first item following a metric family range.
    /// metric family are sorted, this will return the first item that is outside
    /// of the range
    pub fn upper_bound(&self, family_name: &Sstring) -> MetricFamilyIterator<'_> {
        self.find_bound(family_name, |a, b| a < &b.mf.name)
    }

    /// find the first item in a range of metric family.
    /// metric family are sorted, the first item, is the first to match the
    /// criteria.
    pub fn lower_bound(&self, family_name: &Sstring) -> MetricFamilyIterator<'_> {
        self.find_bound(family_name, |a, b| a <= &b.mf.name)
    }

    // Vector-like access: the following methods make the per-shard families act as a vector
    // of foreign pointers to each shard's snapshot.

    /// The per-shard snapshots.
    pub fn iter(&self) -> std::slice::Iter<'_, ForeignPtr<mi::ValuesReference>> {
        self._data.iter()
    }

    /// Resizes to `new_size` shards.
    pub fn resize(&mut self, new_size: usize) {
        self._data.resize_with(new_size, ForeignPtr::null);
    }

    /// The snapshot of shard `n`.
    pub fn at_mut(&mut self, n: usize) -> &mut ForeignPtr<mi::ValuesReference> {
        &mut self._data[n]
    }
}

/// Gathers every shard's snapshot into `vec`.
///
/// # Safety
///
/// `vec` must stay alive, in place and otherwise unused until the returned future resolves
/// (the continuations write into it through its address).
unsafe fn get_map_value(vec: &mut MetricsFamiliesPerShard) -> Future<()> {
    vec.resize(Smp::count() as usize);
    let vec_ptr = vec as *mut MetricsFamiliesPerShard as usize;
    parallel_for_each(Smp::all_cpus(), move |cpu: u32| {
        Smp::submit_to(cpu, || Ok::<_, ExceptionPtr>(mi::get_values())).then(
            move |res: ForeignPtr<mi::ValuesReference>| {
                // SAFETY: by the contract of this function, `vec` is alive and unused until the
                // gathering resolves; these continuations run on the calling shard, one at a time.
                unsafe {
                    (*(vec_ptr as *mut MetricsFamiliesPerShard)).at_mut(cpu as usize).assign(res)
                };
            },
        )
    })
}

/// a facade class for metric family
#[derive(Clone, Copy)]
pub struct MetricFamily<'a> {
    _name: Option<&'a Sstring>,
    _size: u32,
    _family_info: Option<&'a mi::MetricFamilyInfo>,
}

impl<'a> MetricFamily<'a> {
    /// The family name.
    pub fn name(&self) -> &'a Sstring {
        match self._name {
            Some(n) => n,
            // Asking the end of the range for a name is a caller bug.
            None => panic!("metric_family::name() at the end of the range"),
        }
    }

    /// Number of series in every shard.
    pub fn size(&self) -> u32 {
        self._size
    }

    /// The family metadata.
    pub fn metadata(&self) -> &'a mi::MetricFamilyInfo {
        match self._family_info {
            Some(i) => i,
            None => panic!("metric_family::metadata() at the end of the range"),
        }
    }

    /// Whether this is past the last family.
    pub fn end(&self) -> bool {
        self._name.is_none() || self._family_info.is_none()
    }
}

/// Iterates over the families of every shard in name order.
#[derive(Clone)]
pub struct MetricFamilyIterator<'a> {
    _families: &'a MetricsFamiliesPerShard,
    _positions: Vec<usize>,
    _info: MetricFamily<'a>,
}

impl<'a> MetricFamilyIterator<'a> {
    fn next_family(&mut self) {
        if self._positions.is_empty() {
            return;
        }
        let mut new_name: Option<&'a Sstring> = None;
        let mut new_family_info: Option<&'a mi::MetricFamilyInfo> = None;
        self._info._size = 0;
        for (pos_in_metric_per_shard, metric_family) in
            self._positions.iter_mut().zip(self._families._data.iter())
        {
            let metadata_vec = shard_metadata(metric_family);
            if let Some(cur) = self._info._name
                && *pos_in_metric_per_shard < metadata_vec.len()
                && metadata_vec[*pos_in_metric_per_shard].mf.name <= *cur
            {
                *pos_in_metric_per_shard += 1;
            }
            if *pos_in_metric_per_shard >= metadata_vec.len() {
                // no more metric family in this shard
                continue;
            }
            let metadata = &metadata_vec[*pos_in_metric_per_shard];
            let cmp = match new_name {
                None => std::cmp::Ordering::Less,
                Some(n) => metadata.mf.name.cmp(n),
            };
            if cmp == std::cmp::Ordering::Less {
                new_name = Some(&metadata.mf.name);
                new_family_info = Some(&metadata.mf);
                self._info._size = 0;
            }
            if cmp != std::cmp::Ordering::Greater {
                self._info._size += metadata.metrics.len() as u32;
            }
        }
        self._info._name = new_name;
        self._info._family_info = new_family_info;
    }

    /// An iterator at the first family of every shard (`shards` positions).
    pub fn new(families: &'a MetricsFamiliesPerShard, shards: u32) -> Self {
        Self::with_positions(families, vec![0; shards as usize])
    }

    /// An iterator at `positions`.
    pub fn with_positions(families: &'a MetricsFamiliesPerShard, positions: Vec<usize>) -> Self {
        let mut it = Self {
            _families: families,
            _positions: positions,
            _info: MetricFamily { _name: None, _size: 0, _family_info: None },
        };
        it.next_family();
        it
    }

    /// Moves to the next family.
    pub fn advance(&mut self) {
        self.next_family();
    }

    /// The current family.
    pub fn get(&self) -> MetricFamily<'a> {
        self._info
    }

    /// The current family's name.
    pub fn name(&self) -> &'a Sstring {
        self._info.name()
    }

    /// Number of series of the current family.
    pub fn size(&self) -> u32 {
        self._info._size
    }

    /// The current family's metadata.
    pub fn metadata(&self) -> &'a mi::MetricFamilyInfo {
        self._info.metadata()
    }

    /// Whether the iterator is past the last family.
    pub fn end(&self) -> bool {
        self._positions.is_empty() || self._info.end()
    }

    /// Calls `f` with every series of the current family, shard by shard.
    pub fn foreach_metric(&self, mut f: impl FnMut(&mi::MetricValue, &mi::MetricSeriesMetadata)) {
        // iterating over the shard vector and the position vector
        for (&pos_in_metric_per_shard, metric_family) in
            self._positions.iter().zip(self._families._data.iter())
        {
            let metadata_vec = shard_metadata(metric_family);
            if pos_in_metric_per_shard >= metadata_vec.len() {
                // no more metric family in this shard
                continue;
            }
            let metadata = &metadata_vec[pos_in_metric_per_shard];
            // the the name is different, that means that on this shard, the metric family
            // does not exist, because everything is sorted by metric family name, this is fine.
            if metadata.mf.name == *self.name() {
                let Some(values) =
                    shard_values(metric_family).and_then(|v| v.get(pos_in_metric_per_shard))
                else {
                    continue;
                };
                let metrics_metadata = &metadata.metrics;
                for (value, metric_metadata) in values.iter().zip(metrics_metadata.iter()) {
                    f(value, metric_metadata);
                }
            }
        }
    }
}

impl PartialEq for MetricFamilyIterator<'_> {
    fn eq(&self, o: &Self) -> bool {
        if self.end() {
            return o.end();
        }
        if o.end() {
            return false;
        }
        self.name() == o.name()
    }
}

/// A range of families.
#[derive(Clone)]
pub struct MetricFamilyRange<'a> {
    _begin: MetricFamilyIterator<'a>,
    _end: MetricFamilyIterator<'a>,
}

impl<'a> MetricFamilyRange<'a> {
    /// Every family.
    pub fn all(families: &'a MetricsFamiliesPerShard) -> Self {
        Self {
            _begin: MetricFamilyIterator::new(families, Smp::count()),
            _end: MetricFamilyIterator::new(families, 0),
        }
    }

    /// `[b, e)`
    pub fn new(b: MetricFamilyIterator<'a>, e: MetricFamilyIterator<'a>) -> Self {
        Self { _begin: b, _end: e }
    }

    /// `begin()`
    pub fn begin(&self) -> MetricFamilyIterator<'a> {
        self._begin.clone()
    }

    /// `end()`
    pub fn end(&self) -> MetricFamilyIterator<'a> {
        self._end.clone()
    }

    /// The families of the range, as iterator positions.
    pub fn iter(&self) -> impl Iterator<Item = MetricFamilyIterator<'a>> + 'a {
        let mut cur = self.begin();
        let end = self.end();
        std::iter::from_fn(move || {
            if cur == end {
                return None;
            }
            let r = cur.clone();
            cur.advance();
            Some(r)
        })
    }
}

/// a helper function to get metric family range
/// if metric_family_name is empty will return everything, if not, it will return
/// the range of metric family that match the metric_family_name.
///
/// if prefix is true the match will be based on prefix
pub fn get_range<'a>(
    mf: &'a MetricsFamiliesPerShard,
    metric_family_name: &Sstring,
    prefix: bool,
) -> MetricFamilyRange<'a> {
    if metric_family_name.empty() {
        return MetricFamilyRange::all(mf);
    }
    let mut upper_bount_prefix = metric_family_name.clone();
    let last = upper_bount_prefix.back();
    *last = last.wrapping_add(1);
    if prefix {
        return MetricFamilyRange::new(
            mf.lower_bound(metric_family_name),
            mf.lower_bound(&upper_bount_prefix),
        );
    }
    let lb = mf.lower_bound(metric_family_name);
    if lb.end() || lb.name() != metric_family_name {
        return MetricFamilyRange::new(lb.clone(), lb); // just return an empty range
    }
    let mut up = lb.clone();
    up.advance();
    MetricFamilyRange::new(lb, up)
}

/// Writes a histogram in the text format.
pub fn write_histogram(
    s: &mut Vec<u8>,
    ctx: &Config,
    name: &Sstring,
    h: &Histogram,
    mut labels: BTreeMap<Sstring, Sstring>,
) {
    let name = name.as_bytes();
    // Writing into a Vec never fails.
    add_name(s, name, b"_sum", &labels, ctx);
    fmt_g(s, h.sample_sum);
    s.push(b'\n');
    add_name(s, name, b"_count", &labels, ctx);
    let _ = writeln!(s, "{}", h.sample_count);
    let le: Sstring = "le".into();
    for i in &h.buckets {
        labels.insert(le.clone(), to_sstring_f(i.upper_bound));
        add_name(s, name, b"_bucket", &labels, ctx);
        let _ = writeln!(s, "{}", i.count);
    }
    labels.insert(le, "+Inf".into());
    add_name(s, name, b"_bucket", &labels, ctx);
    let _ = writeln!(s, "{}", h.sample_count);
}

/// Writes a summary in the text format.
pub fn write_summary(
    s: &mut Vec<u8>,
    ctx: &Config,
    name: &Sstring,
    h: &Histogram,
    mut labels: BTreeMap<Sstring, Sstring>,
) {
    let name = name.as_bytes();
    // Writing into a Vec never fails.
    if h.sample_sum != 0.0 {
        add_name(s, name, b"_sum", &labels, ctx);
        fmt_g(s, h.sample_sum);
        s.push(b'\n');
    }
    if h.sample_count != 0 {
        add_name(s, name, b"_count", &labels, ctx);
        let _ = writeln!(s, "{}", h.sample_count);
    }
    let quantile: Sstring = "quantile".into();
    for i in &h.buckets {
        labels.insert(quantile.clone(), to_sstring_f(i.upper_bound));
        add_name(s, name, b"", &labels, ctx);
        let _ = writeln!(s, "{}", i.count);
    }
}

/// The hasher of the aggregation map: a fixed-key SipHash, so that the series come out in the
/// same order on every request (`RandomState` would reorder them on each request).
type AggregateHasher = BuildHasherDefault<DefaultHasher>;

/// The aggregated series by their remaining labels.
pub type AggregatedValues = HashMap<BTreeMap<Sstring, Sstring>, mi::MetricValue, AggregateHasher>;

/// a helper class to aggregate metrics over labels
///
/// This class sum multiple metrics based on a list of labels.
/// It returns one or more metrics each aggregated by the aggregate_by labels.
///
/// To use it, you define what labels it should aggregate by and then pass to
/// it metrics with their labels.
/// For example if a metrics has a 'shard' and 'name' labels and you aggregate by 'shard'
/// it would return a map of metrics each with only the 'name' label
pub struct MetricAggregateByLabels {
    _labels_to_aggregate_by: Vec<String>,
    _values: AggregatedValues,
}

impl MetricAggregateByLabels {
    /// Aggregates by dropping `labels`.
    pub fn new(labels: Vec<String>) -> Self {
        Self { _labels_to_aggregate_by: labels, _values: AggregatedValues::default() }
    }

    /// add a metric
    ///
    /// This method gets a metric and its labels and adds it to the aggregated metric.
    /// For example, if a metric has the labels {'shard':'0', 'name':'myhist'} and we are aggregating
    /// over 'shard'
    /// The metric would be added to the aggregated metric with labels {'name':'myhist'}.
    pub fn add(&mut self, m: &mi::MetricValue, mut labels: BTreeMap<Sstring, Sstring>) {
        for l in &self._labels_to_aggregate_by {
            labels.remove(l.as_bytes());
        }
        match self._values.get_mut(&labels) {
            None => {
                self._values.insert(labels, m.clone());
            }
            Some(v) => {
                // Aggregation must not fail: adding histograms of different bucket counts is an
                // internal error.
                if let Err(e) = v.add_assign(m) {
                    on_internal_error_noexcept(
                        engine_logger(),
                        &format!("prometheus: aggregating mismatching series: {}", e),
                    );
                }
            }
        }
    }

    /// The aggregated series.
    pub fn get_values(&self) -> &AggregatedValues {
        &self._values
    }

    /// Whether nothing was added.
    pub fn empty(&self) -> bool {
        self._values.is_empty()
    }
}

/// Writes the value of a numeric series: `NaN` for one that does not fit (a range error).
///
/// # Errors
///
/// Any other error is logged and returned.
pub fn write_value_as_string(s: &mut Vec<u8>, value: &mi::MetricValue) -> Result<(), ExceptionPtr> {
    match write_metric_value(s, value) {
        Ok(()) => Ok(()),
        Err(e) if e.is::<RangeError>() => {
            log_debug!(
                engine_logger(),
                "prometheus: write_value_as_string: {}: {}",
                String::from_utf8_lossy(s),
                e
            );
            s.extend_from_slice(b"NaN");
            Ok(())
        }
        Err(e) => {
            // print this error as it's ignored later on by `connection::start_response`
            log_error!(
                engine_logger(),
                "prometheus: write_value_as_string: {}: {}",
                String::from_utf8_lossy(s),
                e
            );
            Err(e)
        }
    }
}

/// A predicate over label sets: which series a request keeps.
pub type LabelFilter = Rc<dyn Fn(&mi::LabelsType) -> bool>;

/// Writes `s` to `out` and waits for it, from inside an engine thread.
fn write_and_wait(out: &mut OutputStream, s: &[u8]) -> Result<(), ExceptionPtr> {
    out.write(s).get()?;
    Thread::maybe_yield();
    Ok(())
}

/// Writes the families of `m` in the text exposition format. A failed write ends the output
/// with that error.
///
/// # Safety
///
/// `out` must point to a live output stream used by nothing else, and the families `m`
/// borrows must stay alive and unmodified, until the returned future resolves (the
/// handler keeps them alive with `do_with`; the `'static` of `m` stands for that).
pub unsafe fn write_text_representation(
    out: *mut OutputStream,
    ctx: Config,
    m: MetricFamilyRange<'static>,
    show_help: bool,
    enable_aggregation: bool,
    filter: LabelFilter,
) -> Future<()> {
    thread::async_(move || -> Result<(), ExceptionPtr> {
        // SAFETY: by the contract of this function, the stream is live and unshared until the
        // returned future resolves, which is after this thread ends.
        let out = unsafe { &mut *out };
        let mut found;
        let mut s: Vec<u8> = Vec::new();
        for metric_family in m.iter() {
            let mut name = ctx.prefix.clone();
            name += "_";
            name += metric_family.name();
            found = false;
            let mut aggregated_values =
                MetricAggregateByLabels::new(metric_family.metadata().aggregate_labels.clone());
            let should_aggregate =
                enable_aggregation && !metric_family.metadata().aggregate_labels.is_empty();
            let mut result: Result<(), ExceptionPtr> = Ok(());
            metric_family.foreach_metric(|value, value_info| {
                // After an error the rest of the family is skipped.
                if result.is_err() {
                    return;
                }
                s.clear();
                if (value_info.should_skip_when_empty().0 && value.is_empty())
                    || !filter(value_info.labels())
                {
                    return;
                }
                if !found {
                    let help = metric_family.metadata().d.str();
                    if show_help && !help.empty() {
                        s.extend_from_slice(b"# HELP ");
                        s.extend_from_slice(name.as_bytes());
                        s.push(b' ');
                        s.extend_from_slice(help.as_bytes());
                        s.push(b'\n');
                    }
                    s.extend_from_slice(b"# TYPE ");
                    s.extend_from_slice(name.as_bytes());
                    s.push(b' ');
                    s.extend_from_slice(data_type_name(metric_family.metadata().type_).as_bytes());
                    s.push(b'\n');
                    found = true;
                }
                if should_aggregate {
                    aggregated_values.add(value, value_info.labels().clone());
                } else if value.type_() == mi::DataType::Summary {
                    write_summary(
                        &mut s,
                        &ctx,
                        &name,
                        value.get_histogram(),
                        value_info.labels().clone(),
                    );
                } else if value.type_() == mi::DataType::Histogram {
                    write_histogram(
                        &mut s,
                        &ctx,
                        &name,
                        value.get_histogram(),
                        value_info.labels().clone(),
                    );
                } else {
                    add_name(&mut s, name.as_bytes(), b"", value_info.labels(), &ctx);
                    if let Err(e) = write_value_as_string(&mut s, value) {
                        result = Err(e);
                        return;
                    }
                    s.push(b'\n');
                }
                result = write_and_wait(out, &s);
            });
            result?;
            if !aggregated_values.empty() {
                for (labels, value) in aggregated_values.get_values() {
                    s.clear();
                    if value.type_() == mi::DataType::Histogram {
                        write_histogram(&mut s, &ctx, &name, value.get_histogram(), labels.clone());
                    } else {
                        add_name(&mut s, name.as_bytes(), b"", labels, &ctx);
                        write_value_as_string(&mut s, value)?;
                        s.push(b'\n');
                    }
                    write_and_wait(out, &s)?;
                }
            }
        }
        Ok(())
    })
}

/// Writes the families of `m` in the protobuf exposition format.
///
/// # Safety
/// As for [`write_text_representation`].
pub unsafe fn write_protobuf_representation(
    out: *mut OutputStream,
    ctx: Config,
    m: MetricFamilyRange<'static>,
    enable_aggregation: bool,
    filter: LabelFilter,
) -> Future<()> {
    do_for_each(m.iter(), move |metric_family: MetricFamilyIterator<'static>| -> Future<()> {
        let mut s: Vec<u8> = Vec::new();
        let mut aggregated_values =
            MetricAggregateByLabels::new(metric_family.metadata().aggregate_labels.clone());
        let should_aggregate =
            enable_aggregation && !metric_family.metadata().aggregate_labels.is_empty();
        let name = metric_family.name();
        let mut mtf = pm::MetricFamily::default();
        let mut empty_metric = true;
        let mut full_name = Vec::with_capacity(ctx.prefix.size() + 1 + name.size());
        full_name.extend_from_slice(ctx.prefix.as_bytes());
        full_name.push(b'_');
        full_name.extend_from_slice(name.as_bytes());
        mtf.name = full_name;
        mtf.metric.reserve(metric_family.size() as usize);
        metric_family.foreach_metric(|value, value_info| {
            if (value_info.should_skip_when_empty().0 && value.is_empty())
                || !filter(value_info.labels())
            {
                return;
            }
            if should_aggregate {
                aggregated_values.add(value, value_info.labels().clone());
            } else {
                fill_metric(&mut mtf, value, value_info.labels(), &ctx);
                empty_metric = false;
            }
        });
        for (labels, value) in aggregated_values.get_values() {
            fill_metric(&mut mtf, value, labels, &ctx);
            empty_metric = false;
        }
        if empty_metric {
            return make_ready_future(());
        }
        if !pm::write_delimited_to(&mtf, &mut s) {
            log_warn!(engine_logger(), "Failed to write protobuf metrics");
        }
        // SAFETY: by the contract of this function, the stream is live and unshared until the
        // returned future resolves; `write` copies the bytes before returning, so `s` may be
        // dropped right after.
        unsafe { (*out).write(&s) }
    })
}

/// Whether the `Accept` header asks for the protobuf format.
pub fn is_accept_protobuf(accept: &str) -> bool {
    accept.split(',').any(|i| i.trim().starts_with("application/vnd.google.protobuf;"))
}

/// The `/metrics` handler.
pub struct MetricsHandler {
    _base: HandlerBaseData,
    _ctx: Config,
}

thread_local! {
    /// `metrics_handler::_true_function`
    static TRUE_FUNCTION: LabelFilter = Rc::new(|_: &mi::LabelsType| true);
}

impl MetricsHandler {
    /// A handler with `ctx`.
    pub fn new(ctx: Config) -> Self {
        Self { _base: HandlerBaseData::default(), _ctx: ctx }
    }

    /// tries to trim an asterisk from the end of the string
    /// return true if an asterisk exists.
    fn trim_asterisk(name: &mut Sstring) -> bool {
        if name.size() != 0 && *name.back() == b'*' {
            let n = name.length() - 1;
            name.resize(n);
            return true;
        }
        // Prometheus uses url encoding for the path so '*' is encoded as '%2A'
        if name.ends_with(b"%2A") {
            // This assert is obviously true. It is in here just to
            // silence a bogus gcc warning:
            // https://gcc.gnu.org/bugzilla/show_bug.cgi?id=89337
            assert!(name.length() >= 3);
            let n = name.length() - 3;
            name.resize(n);
            return true;
        }
        false
    }

    /// Return a filter function, based on the request
    ///
    /// A filter function filter what metrics should be included.
    /// It returns true if a metric should be included, or false otherwise.
    /// The filters are created from the request query parameters.
    fn make_filter(req: &Request) -> Result<LabelFilter, ExceptionPtr> {
        let mut matcher: HashMap<Sstring, Regex> = HashMap::new();
        let labels = mi::get_local_impl().get_labels().clone();
        for (k, v) in req.query_parameters.iter() {
            if labels.contains(k) {
                // An invalid expression is a `regex_error`, which fails the request.
                matcher.insert(k.clone(), Regex::new(v.as_str())?);
            }
        }
        if matcher.is_empty() {
            return Ok(TRUE_FUNCTION.with(Rc::clone));
        }
        Ok(Rc::new(move |labels: &mi::LabelsType| {
            for (k, r) in &matcher {
                let l: &[u8] = labels.get(k).map_or(b"", |v| v.as_bytes());
                if !r.is_full_match(l) {
                    return false;
                }
            }
            true
        }))
    }
}

impl HandlerBase for MetricsHandler {
    fn base(&self) -> &HandlerBaseData {
        &self._base
    }

    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self._base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        let is_protobuf_format =
            self._ctx.allow_protobuf && is_accept_protobuf(req.get_header("Accept").as_str());
        let mut metric_family_name = req.get_query_param("__name__");
        let prefix = Self::trim_asterisk(&mut metric_family_name);
        let show_help = req.get_query_param("__help__") != "false";
        let enable_aggregation = req.get_query_param("__aggregate__") != "false";
        let filter = match Self::make_filter(&req) {
            Ok(f) => f,
            Err(e) => return make_exception_future(e),
        };
        let ctx = self._ctx.clone();

        // The writer that takes the stream and closes it.
        rep.write_body_writer(
            if is_protobuf_format { "proto" } else { "txt" },
            BodyWriter::new(move |s: OutputStream| -> Future<()> {
                let ctx = ctx.clone();
                let filter = filter.clone();
                let metric_family_name = metric_family_name.clone();
                // The families of every shard and the stream live in one `do_with` state, which stays at its address until the future returned below resolves. The two
                // members are reached through field pointers taken from one raw pointer, so the
                // shared borrow of the families never overlaps a `&mut` to the whole state.
                type State = (MetricsFamiliesPerShard, OutputStream);
                do_with(
                    (MetricsFamiliesPerShard::default(), s),
                    move |st: &mut State| -> Future<()> {
                        let st: *mut State = st;
                        // SAFETY: do_with keeps the state alive and in place until the chain below
                        // ends; only the gathering uses the families until it resolves.
                        let families_mut = unsafe { &mut *std::ptr::addr_of_mut!((*st).0) };
                        // SAFETY: the families stay alive, in place and unused until the gathering ends.
                        unsafe { get_map_value(families_mut) }
                            .then(move |()| -> Future<()> {
                                // SAFETY: do_with keeps the state alive and in place until the chain ends;
                                // from here on the families are only read, so the range may borrow them for
                                // the rest of the chain (the `'static` stands for that).
                                let families: &'static MetricsFamiliesPerShard =
                                    unsafe { &*std::ptr::addr_of!((*st).0) };
                                let m = get_range(families, &metric_family_name, prefix);
                                // SAFETY: a pointer to the other member; only the writer below uses it.
                                let out: *mut OutputStream =
                                    unsafe { std::ptr::addr_of_mut!((*st).1) };
                                if is_protobuf_format {
                                    // SAFETY: the stream and the families live until this future resolves.
                                    unsafe {
                                        write_protobuf_representation(
                                            out,
                                            ctx,
                                            m,
                                            enable_aggregation,
                                            filter,
                                        )
                                    }
                                } else {
                                    // SAFETY: as above.
                                    unsafe {
                                        write_text_representation(
                                            out,
                                            ctx,
                                            m,
                                            show_help,
                                            enable_aggregation,
                                            filter,
                                        )
                                    }
                                }
                            })
                            .finally(move || -> Future<()> {
                                // SAFETY: the writer has finished; do_with keeps the state until this
                                // future resolves, and nothing else uses the stream.
                                unsafe { (*std::ptr::addr_of_mut!((*st).1)).close() }
                            })
                    },
                )
            }),
        );
        make_ready_future(rep)
    }
}

// Installing the /metrics endpoint, which returns the metrics both in the text format and in
// protobuf, according to the Prometheus specification.

/// Adds the `/metrics` route to `server`.
///
/// # Errors
///
/// Fails with the error of the route table when the route already has a handler.
pub fn add_prometheus_routes(server: &mut HttpServer, ctx: Config) -> Future<()> {
    let handler: Box<dyn HandlerBase> = Box::new(MetricsHandler::new(ctx));
    // A refused handler is dropped by `put`.
    match server._routes.put(OperationType::Get, "/metrics", handler) {
        Ok(_) => make_ready_future(()),
        Err(e) => make_exception_future(e),
    }
}

/// Adds the `/metrics` route to every shard of the sharded `server`.
///
/// # Errors
///
/// As [`add_prometheus_routes`], on any shard.
pub fn add_prometheus_routes_sharded(
    server: Pin<&mut Distributed<HttpServer>>,
    ctx: Config,
) -> Future<()> {
    server.invoke_on_all(move |s: &mut HttpServer| add_prometheus_routes(s, ctx.clone()))
}

/// Starts serving the metrics through `http_server`.
pub fn start(http_server: &mut HttpServerControl, ctx: Config) -> Future<()> {
    // The control owns its sharded server pinned in a box, so it never moves.
    add_prometheus_routes_sharded(http_server.server(), ctx)
}
