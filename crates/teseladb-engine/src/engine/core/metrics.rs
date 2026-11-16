//! Metric creation and registration, and the per-shard metrics registry.
//!
//! Components register metrics that the exporters (the Prometheus endpoint and the collectd
//! sender) later read and publish. Every metric is a single value read on demand: either a
//! function that returns it or a reference to a variable ([`by_ref`]). The id of a
//! series is its group, its name and its labels; each shard registers its own copy of a metric,
//! labelled with the shard id ([`shard_label`]).
//!
//! To add metrics to a component, give it a [`MetricGroups`] field and fill it when the
//! component starts, for example with `add_group("cache", [make_gauge("bytes", source,
//! description("used bytes"), vec![])])`, where `source` is a closure returning the value.
//!
//! The metrics stay registered on the shard while the [`MetricGroups`] lives. The registry
//! ([`impl_::Impl`], one per shard) also applies the relabelling rules
//! ([`set_relabel_configs`]) and the family configuration that aggregates series by labels
//! ([`set_metric_family_configs`]).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;

use crate::engine::core::future::{
    ExceptionPtr, Future, RangeError, RuntimeError, make_exception_ptr, make_ready_future,
};
use crate::engine::core::reactor::engine_logger;
use crate::engine::core::relabel_config::metrics::{
    MetricFamilyConfig, RelabelAction, RelabelConfig,
};
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::sharded::ForeignPtr;
use crate::engine::core::shared_ptr::{SharedPtr, make_shared};
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::{Sstring, to_sstring};
use crate::engine::util::program_options::{OptionGroup, Value};
use crate::log_error;

pub use crate::engine::core::metrics_api::{MetricRelabelingResult, Options};
pub use crate::engine::core::metrics_registration::{
    GroupNameType, MetricDefinition, MetricGroup, MetricGroupDefinition, MetricGroups,
};
pub use crate::engine::core::metrics_types::{Histogram, HistogramBucket, NativeHistogramInfo};

const _METRICS_DOC: () = ();

/// A metric registered twice with the same name and labels.
#[derive(Debug, Clone)]
pub struct DoubleRegistration {
    what: String,
}

impl DoubleRegistration {
    /// The error with message `what`.
    pub fn new(what: String) -> Self {
        Self { what }
    }
}

impl fmt::Display for DoubleRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for DoubleRegistration {}

/// A custom type name for a metric (such as `bytes`) that the exporters show instead of the
/// basic type. Used by the metrics layer; create metrics with `make_counter`, `make_gauge` and
/// the other constructors instead.
pub type MetricTypeDef = Sstring;
/// A metric name.
pub type MetricNameType = Sstring;
/// The value that tells instances of a metric apart, typically the shard id.
pub type InstanceIdType = Sstring;

/// Whether a series that was never used is left out of the export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkipWhenEmpty(pub bool);

impl SkipWhenEmpty {
    /// Skip the series while it is empty.
    pub const YES: SkipWhenEmpty = SkipWhenEmpty(true);
    /// Always export the series.
    pub const NO: SkipWhenEmpty = SkipWhenEmpty(false);
}

/// Human-readable description of a metric/group.
///
/// Passed to the metric constructors, for example
/// `make_gauge("metric_name", value, description("what the value means"), vec![])`.
#[derive(Debug, Clone, Default)]
pub struct Description {
    _s: Sstring,
}

impl Description {
    /// A description with text `s`.
    pub fn new(s: &str) -> Self {
        Self { _s: s.into() }
    }

    /// The text.
    pub fn str(&self) -> &Sstring {
        &self._s
    }
}

/// One label of a metric series: a key and a value.
///
/// Labels add information about a metric that you later aggregate by. For example, with
/// several queues on a shard, adding the queue id as a label lets all of them use the same
/// metric name.
///
/// Usually a label instance is not built directly but through a [`Label`] factory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LabelInstance {
    _key: Sstring,
    _value: Sstring,
}

impl LabelInstance {
    /// The label `key` with value `v` formatted with `{}`; any `Display` value works, for
    /// example `LabelInstance::new("smp_queue", 1)` or `LabelInstance::new("my_key", "my_value")`.
    pub fn new<T: fmt::Display>(key: &str, v: T) -> Self {
        Self { _key: key.into(), _value: to_sstring(&v) }
    }

    /// The label key.
    #[inline]
    pub fn key(&self) -> &Sstring {
        &self._key
    }

    /// The label value.
    #[inline]
    pub fn value(&self) -> &Sstring {
        &self._value
    }
}

/// A factory of [`LabelInstance`]s with the same key.
///
/// The same label key is typically used in many places: create the label once, for example
/// `let smp_owner = Label::new("smp_owner");`, and make instances from it when creating the
/// metrics, as in `make_queue_length("send_batch_queue_length", v, d, vec![smp_owner.call(cpu)])`.
#[derive(Debug, Clone)]
pub struct Label {
    key: Sstring,
}

impl Label {
    /// A label factory for the key `key`.
    pub fn new(key: &str) -> Self {
        Self { key: key.into() }
    }

    /// A label instance with this key and `value` formatted with `{}`, for example
    /// `lab.call(1)` or `lab.call("my_value")`.
    pub fn call<T: fmt::Display>(&self, value: T) -> LabelInstance {
        LabelInstance::new(self.key.as_str(), value)
    }

    /// The label key.
    pub fn name(&self) -> &Sstring {
        &self.key
    }
}

/// The implementation parts of the metrics layer, public for the exporters; components should
/// use the constructors at the top of [`metrics`](crate::engine::core::metrics).
pub mod impl_ {
    pub use super::create_metric_groups;
    pub use super::impl_fns::{get_local_impl, get_value_map, get_values, unregister_metric};
    use super::*;
    pub use crate::engine::core::metrics_api::impl_::*;

    /// The kind of a series.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    pub enum DataType {
        /// An integer counter that only grows.
        Counter,
        /// A floating-point counter that only grows.
        RealCounter,
        /// A value that can go up and down.
        Gauge,
        /// A histogram of values by bucket.
        Histogram,
        /// A summary: values by quantile.
        Summary,
    }

    pub use super::MetricTypeDef;

    /// The value of a series: a number or a histogram.
    #[derive(Debug, Clone)]
    pub enum MetricValueU {
        /// A number.
        Double(f64),
        /// A histogram or a summary.
        Histogram(Histogram),
    }

    /// The value a metric function returns: its kind and its number or histogram.
    #[derive(Debug, Clone)]
    pub struct MetricValue {
        /// The number or the histogram.
        pub u: MetricValueU,
        /// The kind of the value.
        pub _type: DataType,
    }

    impl MetricValue {
        /// The kind of the value.
        #[inline]
        pub fn type_(&self) -> DataType {
            self._type
        }

        /// The number (0 for a histogram).
        #[inline]
        pub fn d(&self) -> f64 {
            match &self.u {
                MetricValueU::Double(d) => *d,
                MetricValueU::Histogram(_) => 0.0,
            }
        }

        /// The number as an unsigned integer.
        pub fn ui(&self) -> Result<u64, ExceptionPtr> {
            let d = self.d();
            if d >= 0.0 && d <= i64::MAX as f64 {
                Ok(d.round() as u64)
            } else {
                Err(Self::ulong_conversion_error(d))
            }
        }

        /// The number as a signed integer.
        pub fn i(&self) -> Result<i64, ExceptionPtr> {
            let d = self.d();
            if d >= i64::MIN as f64 && d <= i64::MAX as f64 {
                Ok(d.round() as i64)
            } else {
                Err(Self::ulong_conversion_error(d))
            }
        }

        /// A zero gauge.
        pub fn new() -> Self {
            Self { u: MetricValueU::Double(0.0), _type: DataType::Gauge }
        }

        /// A histogram value.
        pub fn from_histogram(h: Histogram, t: DataType) -> Self {
            Self { u: MetricValueU::Histogram(h), _type: t }
        }

        /// A numeric value of kind `t`.
        pub fn from_double(d: f64, t: DataType) -> Self {
            Self { u: MetricValueU::Double(d), _type: t }
        }

        /// Adds `c` into `self`.
        ///
        /// # Errors
        ///
        /// As [`add`](Self::add).
        pub fn add_assign(&mut self, c: &MetricValue) -> Result<(), ExceptionPtr> {
            *self = self.add(c)?;
            Ok(())
        }

        /// The sum of `self` and `c`: numbers add, histograms add bucket by bucket.
        ///
        /// # Errors
        ///
        /// Returns an error when two histograms have different bucket bounds.
        pub fn add(&self, c: &MetricValue) -> Result<MetricValue, ExceptionPtr> {
            let mut res = self.clone();
            match self._type {
                DataType::Histogram | DataType::Summary => {
                    if let (MetricValueU::Histogram(a), MetricValueU::Histogram(b)) =
                        (&mut res.u, &c.u)
                    {
                        a.add_assign(b)?;
                    }
                }
                _ => {
                    if let MetricValueU::Double(a) = &mut res.u {
                        *a += c.d();
                    }
                }
            }
            Ok(res)
        }

        /// The histogram (an empty one for a number).
        pub fn get_histogram(&self) -> &Histogram {
            static EMPTY: Histogram = Histogram {
                sample_count: 0,
                sample_sum: 0.0,
                buckets: Vec::new(),
                native_histogram: None,
            };
            match &self.u {
                MetricValueU::Histogram(h) => h,
                MetricValueU::Double(_) => &EMPTY,
            }
        }

        /// Whether the metric was never used (meaningful for histograms, summaries and
        /// counters, which only grow).
        ///
        /// Histograms, Summaries and counters are ever growing by nature, so
        /// it is possible to check if they have been used or not.
        pub fn is_empty(&self) -> bool {
            ((self._type == DataType::Histogram || self._type == DataType::Summary)
                && self.get_histogram().sample_count == 0)
                || ((self._type == DataType::Counter || self._type == DataType::RealCounter)
                    && self.d() == 0.0)
        }

        fn ulong_conversion_error(d: f64) -> ExceptionPtr {
            ExceptionPtr::new(RangeError::new(format!(
                "cannot convert double value {} to unsigned long",
                d
            )))
        }
    }

    impl Default for MetricValue {
        fn default() -> Self {
            Self::new()
        }
    }

    /// The function a series calls to read its value; shared between the definition and the
    /// registry.
    pub type MetricFunction = Rc<dyn Fn() -> MetricValue>;

    /// The kind and inherited type name of a series.
    #[derive(Debug, Clone)]
    pub struct MetricType {
        /// The basic kind.
        pub base_type: DataType,
        /// The custom type name shown instead of the basic one, or empty.
        pub type_name: MetricTypeDef,
    }

    /// A metric about to be registered.
    #[derive(Clone)]
    pub struct MetricDefinitionImpl {
        /// The metric name.
        pub name: MetricNameType,
        /// The kind and custom type name.
        pub type_: MetricType,
        /// The function that reads the value.
        pub f: MetricFunction,
        /// The description.
        pub d: Description,
        /// Whether the series is exported.
        pub enabled: bool,
        /// Whether an unused series is left out of the export.
        pub _skip_when_empty: SkipWhenEmpty,
        /// The labels removed when the series are aggregated.
        pub aggregate_labels: Vec<String>,
        /// The label set.
        pub labels: BTreeMap<Sstring, Sstring>,
    }

    impl MetricDefinitionImpl {
        /// A definition; the shard label is added unless given.
        pub fn new(
            name: MetricNameType,
            type_: MetricType,
            f: MetricFunction,
            d: Description,
            _labels: Vec<LabelInstance>,
            _aggregate_labels: Vec<Label>,
        ) -> Self {
            let mut m = Self {
                name,
                type_,
                f,
                d,
                enabled: true,
                _skip_when_empty: SkipWhenEmpty::NO,
                aggregate_labels: Vec::new(),
                labels: BTreeMap::new(),
            };
            for i in _labels {
                m.labels.insert(i.key().clone(), i.value().clone());
            }
            m.labels.entry(shard_label_name()).or_insert_with(shard);
            m.aggregate(&_aggregate_labels);
            m
        }

        /// Sets whether the series is exported.
        pub fn enabled(mut self, _enabled: bool) -> Self {
            self.enabled = _enabled;
            self
        }

        /// Adds the label `label`.
        pub fn label(mut self, label: &LabelInstance) -> Self {
            self.labels.insert(label.key().clone(), label.value().clone());
            self
        }

        /// Sets whether an unused series is skipped.
        pub fn skip(mut self, skip: SkipWhenEmpty) -> Self {
            self._skip_when_empty = skip;
            self
        }

        /// Aggregates the series by `labels`.
        pub fn aggregate(&mut self, _labels: &[Label]) -> &mut Self {
            self.aggregate_labels.reserve(_labels.len());
            self.aggregate_labels.extend(_labels.iter().map(|l| l.name().to_string()));
            self
        }

        /// Sets whether an unused series is skipped.
        pub fn set_skip_when_empty(mut self, skip: bool) -> Self {
            self._skip_when_empty = SkipWhenEmpty(skip);
            self
        }

        /// Overrides the inherited type name.
        pub fn set_type(mut self, type_name: &str) -> Self {
            self.type_.type_name = type_name.into();
            self
        }
    }

    /// The registrations held by a [`MetricGroups`].
    pub trait MetricGroupsDef {
        /// Registers one metric of group `name`.
        ///
        /// # Errors
        ///
        /// Returns an error when the series is already registered.
        fn add_metric(
            &mut self,
            name: GroupNameType,
            md: &MetricDefinition,
        ) -> Result<(), ExceptionPtr>;
        /// Registers the metrics `l` of group `name`, stopping at the first error.
        fn add_group(
            &mut self,
            name: GroupNameType,
            l: &[MetricDefinition],
        ) -> Result<(), ExceptionPtr>;
    }

    /// The shard id as a label value.
    pub fn shard() -> InstanceIdType {
        to_sstring(&this_shard_id())
    }

    /// A value a metric can report.
    pub trait MetricNumber: 'static {
        /// Whether the type is an integer, which makes a counter of it a [`DataType::Counter`]
        /// rather than a [`DataType::RealCounter`].
        const IS_INTEGRAL: bool;
        /// The value as a metric value of kind `dt`.
        fn to_metric_value(&self, dt: DataType) -> MetricValue;
    }

    macro_rules! metric_number {
        ($integral:expr; $($t:ty),*) => {$(
            impl MetricNumber for $t {
                const IS_INTEGRAL: bool = $integral;
                #[inline]
                fn to_metric_value(&self, dt: DataType) -> MetricValue {
                    MetricValue::from_double(*self as f64, dt)
                }
            }
        )*};
    }

    metric_number!(true; u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);
    metric_number!(false; f32, f64);

    impl MetricNumber for bool {
        const IS_INTEGRAL: bool = true;
        #[inline]
        fn to_metric_value(&self, dt: DataType) -> MetricValue {
            MetricValue::from_double(*self as u8 as f64, dt)
        }
    }

    impl MetricNumber for Histogram {
        const IS_INTEGRAL: bool = false;
        #[inline]
        fn to_metric_value(&self, dt: DataType) -> MetricValue {
            MetricValue::from_histogram(self.clone(), dt)
        }
    }

    /// A reference to a value read by a metric: the value must outlive the metric
    /// registration.
    pub struct MetricRef<T: MetricNumber>(*const T);

    impl<T: MetricNumber> MetricRef<T> {
        /// A reference to `*p`.
        ///
        /// # Safety
        ///
        /// `p` must point to a value that stays alive and in place, and is not mutably
        /// borrowed while it is read, for as long as the metric is registered.
        #[inline]
        pub unsafe fn new(p: *const T) -> Self {
            Self(p)
        }
    }

    /// What a metric reads: a callable returning a value, or a `MetricRef`.
    pub trait MetricSource {
        /// The value type.
        type Value: MetricNumber;
        /// The function that reads the value as a metric value of kind `dt`.
        fn make_function(self, dt: DataType) -> MetricFunction;
    }

    impl<F, V> MetricSource for F
    where
        F: Fn() -> V + 'static,
        V: MetricNumber,
    {
        type Value = V;

        fn make_function(self, dt: DataType) -> MetricFunction {
            Rc::new(move || self().to_metric_value(dt))
        }
    }

    impl<T: MetricNumber> MetricSource for MetricRef<T> {
        type Value = T;

        fn make_function(self, dt: DataType) -> MetricFunction {
            let p = self.0 as usize;
            // SAFETY: the referenced value outlives the registration (the contract of
            // `MetricRef::new` / `by_ref`).
            Rc::new(move || unsafe { &*(p as *const T) }.to_metric_value(dt))
        }
    }

    /// The function that reads `val` as a metric value of kind `dt`.
    #[inline]
    pub fn make_function<S: MetricSource>(val: S, dt: DataType) -> MetricFunction {
        val.make_function(dt)
    }

    /// The counter kind for the source's value type: integer or real.
    #[inline]
    pub fn counter_type<S: MetricSource>() -> DataType {
        if <S::Value as MetricNumber>::IS_INTEGRAL {
            DataType::Counter
        } else {
            DataType::RealCounter
        }
    }
}

use impl_::{DataType, MetricDefinitionImpl, MetricRef, MetricSource, MetricType};

/// A [`Description`] with `text`.
#[inline]
pub fn description(text: &str) -> Description {
    Description::new(text)
}

/// A [`Label`] factory for `key`.
#[inline]
pub fn label(key: &str) -> Label {
    Label::new(key)
}

/// The type of a label factory.
pub type LabelType = Label;

/// A metric source reading `*val`.
///
/// # Safety
///
/// `val` must stay alive and in place, and must not be mutably borrowed while the metric
/// reads it, for as long as the metric is registered.
#[inline]
pub unsafe fn by_ref<T: impl_::MetricNumber>(val: &T) -> MetricRef<T> {
    // SAFETY: forwarded to the caller.
    unsafe { MetricRef::new(val) }
}

/// Value for a definition's `enabled` flag that disables the series.
pub const METRIC_DISABLED: bool = false;

/// The name of `shard_label` ("shard").
#[inline]
pub fn shard_label_name() -> Sstring {
    Sstring::from("shard")
}

/// The label factory for the shard id, key `"shard"`.
pub fn shard_label() -> Label {
    Label::new("shard")
}

// The metric kinds are compatible with the collectd metric definitions. Gauges and counters
// cover most uses.

/// Gauge are a general purpose metric.
///
/// They can hold floating point values and can increase or decrease.
pub fn make_gauge<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    MetricDefinitionImpl::new(
        name.into(),
        MetricType { base_type: DataType::Gauge, type_name: "gauge".into() },
        impl_::make_function(val, DataType::Gauge),
        d,
        labels,
        Vec::new(),
    )
}

/// A gauge with no labels, arguments in description-then-value order.
pub fn make_gauge_fn<S: MetricSource>(name: &str, d: Description, val: S) -> MetricDefinitionImpl {
    make_gauge(name, val, d, Vec::new())
}

/// A gauge with labels, arguments in description-then-value order.
pub fn make_gauge_labeled<S: MetricSource>(
    name: &str,
    d: Description,
    labels: Vec<LabelInstance>,
    val: S,
) -> MetricDefinitionImpl {
    make_gauge(name, val, d, labels)
}

/// Derive are used when a rate is more interesting than the value.
///
/// Derive is an integer value that can increase or decrease, typically it is used when looking at the
/// derivation of the value.
///
/// Deprecated in favour of counters.
#[deprecated(note = "Use make_counter()")]
pub fn make_derive<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    make_counter(name, val, d, labels)
}

/// A derive with no labels, arguments in description-then-value order.
#[deprecated(note = "Use make_counter()")]
#[allow(deprecated)]
pub fn make_derive_fn<S: MetricSource>(name: &str, d: Description, val: S) -> MetricDefinitionImpl {
    make_derive(name, val, d, Vec::new())
}

/// A derive with labels, arguments in description-then-value order.
#[deprecated(note = "Use make_counter()")]
#[allow(deprecated)]
pub fn make_derive_labeled<S: MetricSource>(
    name: &str,
    d: Description,
    labels: Vec<LabelInstance>,
    val: S,
) -> MetricDefinitionImpl {
    make_derive(name, val, d, labels)
}

/// Creates a counter metric.
///
/// Counters are used when a rate is more interesting than the value, monitoring systems take
/// derivation from it to display.
///
/// It's an integer or floating point value that can increase or decrease.
pub fn make_counter<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    let type_ = impl_::counter_type::<S>();
    MetricDefinitionImpl::new(
        name.into(),
        MetricType { base_type: type_, type_name: "counter".into() },
        impl_::make_function(val, type_),
        d,
        labels,
        Vec::new(),
    )
}

/// A counter with no labels, arguments in description-then-value order.
pub fn make_counter_fn<S: MetricSource>(
    name: &str,
    d: Description,
    val: S,
) -> MetricDefinitionImpl {
    make_counter(name, val, d, Vec::new())
}

/// A counter with labels, arguments in description-then-value order.
pub fn make_counter_labeled<S: MetricSource>(
    name: &str,
    d: Description,
    labels: Vec<LabelInstance>,
    val: S,
) -> MetricDefinitionImpl {
    make_counter(name, val, d, labels)
}

/// Creates an absolute metric.
///
/// Absolute are used for metric that are being erased after each time they are read.
/// They exist for compatibility and should generally be avoided.
#[deprecated(note = "Use make_counter()")]
pub fn make_absolute<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    make_counter(name, val, d, labels)
}

/// Creates a histogram metric.
///
/// Histograms are a list of buckets with upper bounds and a counter for the number
/// of entries in each bucket.
pub fn make_histogram<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    MetricDefinitionImpl::new(
        name.into(),
        MetricType { base_type: DataType::Histogram, type_name: "histogram".into() },
        impl_::make_function(val, DataType::Histogram),
        d,
        labels,
        Vec::new(),
    )
}

/// A histogram with no labels, arguments in description-then-value order.
pub fn make_histogram_fn<S: MetricSource>(
    name: &str,
    d: Description,
    val: S,
) -> MetricDefinitionImpl {
    make_histogram(name, val, d, Vec::new())
}

/// A histogram with labels, arguments in description-then-value order.
pub fn make_histogram_labeled<S: MetricSource>(
    name: &str,
    d: Description,
    labels: Vec<LabelInstance>,
    val: S,
) -> MetricDefinitionImpl {
    make_histogram(name, val, d, labels)
}

/// Creates a summary metric.
///
/// Summaries are a different kind of histograms. It reports in quantiles.
/// For example, the p99 and p95 latencies.
pub fn make_summary<S: MetricSource>(name: &str, d: Description, val: S) -> MetricDefinitionImpl {
    MetricDefinitionImpl::new(
        name.into(),
        MetricType { base_type: DataType::Summary, type_name: "summary".into() },
        impl_::make_function(val, DataType::Summary),
        d,
        Vec::new(),
        Vec::new(),
    )
}

/// Creates a total-bytes metric.
///
/// total_bytes are used for an ever growing counters, like the total bytes
/// passed on a network.
pub fn make_total_bytes<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    make_counter(name, val, d, labels).set_type("total_bytes")
}

/// Creates a current-bytes metric.
///
/// current_bytes are used to report on current status in bytes.
/// For example the current free memory.
pub fn make_current_bytes<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    make_gauge(name, val, d, labels).set_type("bytes")
}

/// Creates a queue-length metric.
///
/// queue_length are used to report on queue length
pub fn make_queue_length<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    make_gauge(name, val, d, labels).set_type("queue_length")
}

/// Creates a total-operations metric.
///
/// total_operations are used for ever growing operation counter.
pub fn make_total_operations<S: MetricSource>(
    name: &str,
    val: S,
    d: Description,
    labels: Vec<LabelInstance>,
) -> MetricDefinitionImpl {
    make_counter(name, val, d, labels).set_type("total_operations")
}

// ---------------------------------------------------------------------------------------------
// The registry and the configuration functions.
// ---------------------------------------------------------------------------------------------

unsafe extern "C" {
    fn gethostname(name: *mut std::ffi::c_char, len: usize) -> std::ffi::c_int;
}

const PATH_MAX: usize = 4096;

fn get_hostname() -> String {
    let mut hostname = [0u8; PATH_MAX];
    // SAFETY: the buffer has PATH_MAX bytes; it is NUL-terminated below.
    unsafe { gethostname(hostname.as_mut_ptr() as *mut std::ffi::c_char, hostname.len()) };
    hostname[PATH_MAX - 1] = 0;
    let n = hostname.iter().position(|&c| c == 0).unwrap_or(0);
    String::from_utf8_lossy(&hostname[..n]).into_owned()
}

impl Options {
    /// The metrics options, not yet linked to a group (see [`Options::link`]).
    pub fn new() -> Self {
        Self {
            group: OptionGroup::new("Metrics options"),
            metrics_hostname: Value::new(
                "metrics-hostname",
                Some(get_hostname()),
                "set the hostname used by the metrics, if not set, the local hostname will be used",
            ),
        }
    }

    /// Links the group under `parent_group` and registers its value with it, in place. The
    /// options must not move after.
    pub fn link(self: Pin<&mut Self>, parent_group: Option<Pin<&mut OptionGroup>>) {
        crate::link_option_group!(self, parent_group, group, metrics_hostname);
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

/// Applies the metrics configuration (the hostname) on every shard.
pub fn configure(opts: &Options) -> Future<()> {
    let c = impl_::Config { hostname: opts.metrics_hostname.get_value().as_str().into() };
    Smp::invoke_on_all(move || {
        let i = impl_::get_local_impl();
        // SAFETY: the registry is this shard's (thread-local) and no other reference to it is live during the call.
        unsafe { i.get_mut_unchecked() }.set_config(&c);
    })
}

/// Perform relabeling and operation on metrics dynamically.
///
/// The function would return true if the changes were applied with no conflict
/// or false, if there was a conflict in the registration.
///
/// The logic follows the Prometheus `metric_relabel_configs` configuration.
/// The relabel rules are applied one after the other.
/// You can add or change a label, and you can enable or disable a metric;
/// in that case the metrics will not be reported at all.
/// You can turn on and off the skip_when_empty flag.
///
/// Using the Prometheus convention, the metric name is __name__.
/// Names cannot be changed.
///
/// Important notes:
/// - The relabelling always starts from the set of labels the metric was created with.
/// - Calling with an empty set removes the relabel config and returns all metrics to their
///   registration labels.
/// - So that a bad configuration cannot bring the system down, when the rules give two
///   series the same name and labels, an extra label with a unique id is added to one of
///   them (counted in the result).
pub fn set_relabel_configs(relabel_configs: &[RelabelConfig]) -> Future<MetricRelabelingResult> {
    let i = impl_::get_local_impl();
    // SAFETY: the registry is this shard's (thread-local) and no other reference to it is live during the call.
    unsafe { i.get_mut_unchecked() }.set_relabel_configs(relabel_configs)
}

/// The current relabel configuration.
pub fn get_relabel_configs() -> Vec<RelabelConfig> {
    impl_::get_local_impl().get_relabel_configs().clone()
}

/// Replaces the metric family configuration at run time.
///
/// A family configuration applies to all series with the same name and different labels;
/// specifically, it sets the labels the series are aggregated by, selected by metric name.
pub fn set_metric_family_configs(family_config: &[MetricFamilyConfig]) {
    let i = impl_::get_local_impl();
    // SAFETY: the registry is this shard's (thread-local) and no other reference to it is live during the call.
    unsafe { i.get_mut_unchecked() }.set_metric_family_configs(family_config);
}

/// The current metric family configuration.
pub fn get_metric_family_configs() -> Vec<MetricFamilyConfig> {
    impl_::get_local_impl().get_metric_family_configs().clone()
}

/// A random id (four random 32-bit numbers from the OS, joined by `-`) for the label that
/// separates colliding series.
fn get_unique_id() -> String {
    // Four random 32-bit numbers from the OS.
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut buf);
    }
    let rd =
        |i: usize| u32::from_ne_bytes([buf[4 * i], buf[4 * i + 1], buf[4 * i + 2], buf[4 * i + 3]]);
    format!("{}-{}-{}-{}", rd(0), rd(1), rd(2), rd(3))
}

/// Parses a relabel action name: `"skip_when_empty"`, `"report_when_empty"`, `"keep"`,
/// `"drop"` or `"drop_label"`; any other name is [`RelabelAction::Replace`].
pub fn relabel_config_action(action: &str) -> RelabelAction {
    if action == "skip_when_empty" {
        return RelabelAction::SkipWhenEmpty;
    }
    if action == "report_when_empty" {
        return RelabelAction::ReportWhenEmpty;
    }
    if action == "keep" {
        return RelabelAction::Keep;
    }
    if action == "drop" {
        return RelabelAction::Drop;
    }
    if action == "drop_label" {
        return RelabelAction::DropLabel;
    }
    RelabelAction::Replace
}

impl impl_::RegisteredMetric {
    /// A registration of `id` reading `f`.
    pub fn new(
        id: impl_::MetricId,
        f: impl_::MetricFunction,
        enabled: bool,
        skip: SkipWhenEmpty,
    ) -> Self {
        let original_labels = id.internalized_labels();
        Self {
            _info: impl_::MetricInfo { id, original_labels, enabled, should_skip_when_empty: skip },
            _f: f,
        }
    }
}

impl impl_::MetricId {
    /// `group_name` and `name` joined by '_', made safe for exporters.
    pub fn full_name(&self) -> Sstring {
        safe_name(&format!("{}_{}", self._group, self._name))
    }
}

fn safe_name(name: &str) -> Sstring {
    let rep: String = name
        .chars()
        .map(|c| if c == '-' || c == ' ' { '_' } else { c })
        .filter(|c| !matches!(c, '+' | '(' | ')'))
        .collect();
    rep.into()
}

/// A new, empty registration holder bound to the current shard's registry on first use.
pub fn create_metric_groups() -> Box<dyn impl_::MetricGroupsDef> {
    Box::new(impl_::MetricGroupsImpl { _registration: Vec::new(), _impl: None })
}

impl impl_::MetricGroupsDef for impl_::MetricGroupsImpl {
    fn add_metric(
        &mut self,
        name: GroupNameType,
        md: &MetricDefinition,
    ) -> Result<(), ExceptionPtr> {
        // We could just do this in the constructor but some metric groups (like the
        // smp queue ones) are constructed on a different shard than the one where the
        // metrics are added.
        // Hence, the shared pointer's owning-shard check would fail, so it is done only here.
        if self._impl.is_none() {
            self._impl = Some(impl_::get_local_impl());
        }
        let local = impl_::get_local_impl();
        // SAFETY: the registry is this shard's (thread-local) and no other reference to it is live during the call.
        let imp = unsafe { local.get_mut_unchecked() };
        let internalized_labels = imp.internalize_labels(md._impl.labels.clone());
        let id = impl_::MetricId::new(name, md._impl.name.clone(), internalized_labels);
        // A registration error goes up to the caller of add_group().
        let reg = imp.add_registration(
            &id,
            &md._impl.type_,
            md._impl.f.clone(),
            &md._impl.d,
            md._impl.enabled,
            md._impl._skip_when_empty,
            &md._impl.aggregate_labels,
        )?;
        self._registration.push(reg);
        Ok(())
    }

    fn add_group(
        &mut self,
        name: GroupNameType,
        l: &[MetricDefinition],
    ) -> Result<(), ExceptionPtr> {
        for i in l {
            self.add_metric(name.clone(), i)?;
        }
        Ok(())
    }
}

impl Drop for impl_::MetricGroupsImpl {
    fn drop(&mut self) {
        // `unregister_metric(i->info().id)` for each registration, through `_impl`: it is this
        // shard's registry (set by the first add_metric, before any registration), kept alive
        // here for this purpose, so the drop never touches the thread-local, which may already
        // be destroyed when the groups are dropped during thread exit.
        let Some(imp) = &self._impl else { return };
        for i in &self._registration {
            // SAFETY: the registry is this shard's and no other reference to it is live during
            // the call.
            unsafe { imp.get_mut_unchecked() }.remove_registration(&i.info().id);
        }
    }
}

thread_local! {
    // The registry is a thread-local rather than a sharded service because it must exist
    // before its first users (the reactor) register metrics.
    static THE_IMPL: SharedPtr<impl_::Impl> = make_shared(impl_::Impl::new());
}

/// Functions over the current shard's registry.
pub mod impl_fns {
    use super::*;

    /// This shard's registry.
    pub fn get_local_impl() -> SharedPtr<impl_::Impl> {
        THE_IMPL.with(|i| i.clone())
    }

    /// Unregisters series `id` from this shard. Once the thread-local registry is destroyed
    /// (thread exit) there is nothing left to unregister from, so this does nothing then.
    pub fn unregister_metric(id: &impl_::MetricId) {
        let Ok(i) = THE_IMPL.try_with(|i| i.clone()) else { return };
        // SAFETY: the registry is this shard's (thread-local) and no other reference to it is live during the call.
        unsafe { i.get_mut_unchecked() }.remove_registration(id);
    }

    /// This shard's registry, whose `get_value_map()` holds the families.
    pub fn get_value_map() -> SharedPtr<impl_::Impl> {
        get_local_impl()
    }

    /// The values of every enabled series of this shard, with their metadata.
    pub fn get_values() -> ForeignPtr<impl_::ValuesReference> {
        let res_ref = make_shared(impl_::ValuesCopy::default());
        let local = get_local_impl();
        {
            // SAFETY: `res_ref` was just created and is not shared yet.
            let res = unsafe { res_ref.get_mut_unchecked() };
            // The mutable borrow below ends with this statement, before any metric function
            // runs.
            let functions: *const Vec<VecDeque<impl_::MetricFunction>> = {
                // SAFETY: the registry is this shard's and no other reference to it is live
                // here; this borrow ends with the enclosing block, before any metric
                // function runs.
                let imp = unsafe { local.get_mut_unchecked() };
                res.metadata = imp.metadata();
                imp.functions()
            };
            // SAFETY: the vector stays in the registry while the values are read: a metric
            // function only reads the registry (registering from inside a metric function
            // would invalidate this same iteration), so a shared
            // borrow of the functions is sound during the calls.
            for i in unsafe { &*functions }.iter() {
                let mut values = impl_::ValueVector::new();
                for v in i {
                    values.push_back(v());
                }
                res.values.push_back(values);
            }
        }
        ForeignPtr::new(res_ref)
    }
}

impl impl_::Impl {
    /// The shared copy of `labels`.
    pub fn internalize_labels(
        &mut self,
        labels: impl_::LabelsType,
    ) -> impl_::InternalizedLabelsRef {
        if let Some(h) = self._internalized_labels.get(&labels) {
            return h.labels_ref();
        }
        let h = impl_::InternalizedHolder::new(labels);
        let r = h.labels_ref();
        self._internalized_labels.insert(h);
        r
    }

    /// Removes series `id`.
    pub fn remove_registration(&mut self, id: &impl_::MetricId) {
        let name = id.full_name();
        if let Some(family) = self._value_map.get_mut(&name) {
            family.erase(id.labels());
            if family.empty() {
                self._value_map.remove(&name);
            }
            self.dirty();
        }
    }

    fn gc_internalized_labels(&mut self) {
        self._internalized_labels.retain(|h| h.has_users());
    }

    /// Rebuilds the metadata and the value functions if a registration changed.
    pub fn update_metrics_if_needed(&mut self) {
        if self._dirty {
            // Forcing the metadata to an empty initialization
            // Will prevent using corrupted data if an exception is thrown
            self._metadata = Some(make_shared(impl_::MetricMetadata::new()));
            let mut mt: impl_::MetricMetadata = Vec::with_capacity(self._value_map.len());
            self._current_metrics.resize_with(self._value_map.len(), VecDeque::new);
            let mut i = 0usize;
            for mf in self._value_map.values() {
                let mut metrics = impl_::MetricMetadataFifo::new();
                self._current_metrics[i].clear();
                for m in mf._instances.values().flatten() {
                    if m.is_enabled() {
                        metrics.push_back(impl_::MetricSeriesMetadata::new(
                            m.info().id.clone(),
                            m.info().should_skip_when_empty,
                        ));
                        self._current_metrics[i].push_back(m.get_function().clone());
                    }
                }
                if !metrics.is_empty() {
                    // If nothing was added, no need to add the metric_family
                    // and no need to progress
                    mt.push(impl_::MetricFamilyMetadata::new(mf.info().clone(), metrics));
                    i += 1;
                }
            }
            // Maybe we didn't use all the preallocated size
            self._current_metrics.truncate(i);
            self._metadata = Some(make_shared(mt));
            self._dirty = false;
            self.gc_internalized_labels();
        }
    }

    /// The metadata of every enabled series.
    pub fn metadata(&mut self) -> Option<SharedPtr<impl_::MetricMetadata>> {
        self.update_metrics_if_needed();
        self._metadata.clone()
    }

    /// The value functions of every enabled series, by family.
    pub fn functions(&mut self) -> &mut Vec<VecDeque<impl_::MetricFunction>> {
        self.update_metrics_if_needed();
        &mut self._current_metrics
    }

    /// Applies the aggregation of the matching family configs to `mf`.
    pub fn update_aggregate(&self, mf: &mut impl_::MetricFamilyInfo) {
        for fc in &self._metric_family_configs {
            if fc.name == mf.name.as_str() || fc.regex_name.match_(mf.name.as_str()) {
                mf.aggregate_labels = fc.aggregate_labels.clone();
            }
        }
    }

    fn apply_relabeling(&mut self, rc: &RelabelConfig, info: &mut impl_::MetricInfo) -> bool {
        let mut s = String::new();
        let mut first = true;
        for l in &rc.source_labels {
            let key: Sstring = l.as_str().into();
            let val = info.id.labels().get(&key);
            if l != "__name__" && val.is_none() {
                // If not all the labels are found, there is nothing to do.
                return false;
            }
            if first {
                first = false;
            } else {
                s.push_str(&rc.separator);
            }
            if l == "__name__" {
                s.push_str(info.id.full_name().as_str());
            } else if let Some(v) = val {
                s.push_str(v.as_str());
            }
        }
        // The regex search borrows the string, so keep it in a local.
        let tmps = s;
        let Some(m) = rc.expr.regex().search(&tmps) else {
            return false;
        };

        match rc.action {
            RelabelAction::Drop | RelabelAction::Keep => {
                info.enabled = rc.action == RelabelAction::Keep;
                true
            }
            RelabelAction::ReportWhenEmpty | RelabelAction::SkipWhenEmpty => {
                info.should_skip_when_empty = if rc.action == RelabelAction::SkipWhenEmpty {
                    SkipWhenEmpty::YES
                } else {
                    SkipWhenEmpty::NO
                };
                false
            }
            RelabelAction::DropLabel => {
                let key: Sstring = rc.target_label.as_str().into();
                if info.id.labels().contains_key(&key) {
                    let mut new_labels = info.id.labels().clone();
                    new_labels.remove(&key);
                    let r = self.internalize_labels(new_labels);
                    info.id.update_labels(r);
                }
                true
            }
            RelabelAction::Replace => {
                if !rc.target_label.is_empty() {
                    let fmt_s = Sstring::from_bytes(&m.format(&tmps, &rc.replacement));
                    let mut new_labels = info.id.labels().clone();
                    new_labels.insert(rc.target_label.as_str().into(), fmt_s);
                    let r = self.internalize_labels(new_labels);
                    info.id.update_labels(r);
                }
                true
            }
        }
    }

    /// Registers series `id`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_registration(
        &mut self,
        id: &impl_::MetricId,
        type_: &MetricType,
        f: impl_::MetricFunction,
        d: &Description,
        enabled: bool,
        skip: SkipWhenEmpty,
        aggregate_labels: &[String],
    ) -> Result<impl_::RegisterRef, ExceptionPtr> {
        let rm = make_shared(impl_::RegisteredMetric::new(id.clone(), f, enabled, skip));
        let configs = self._relabel_configs.clone();
        for rl in &configs {
            let mut info = rm.info().clone();
            self.apply_relabeling(rl, &mut info);
            // SAFETY: `rm` was just created and only this function holds it.
            *unsafe { rm.get_mut_unchecked() }.info_mut() = info;
        }

        let name = id.full_name();
        let Some(labels_ref) = rm.info().id.internalized_labels() else {
            return Err(make_exception_ptr(RuntimeError::new("metric without labels".to_string())));
        };
        if let Some(metric) = self._value_map.get_mut(&name) {
            if metric.contains(rm.info().id.labels()) {
                return Err(ExceptionPtr::new(DoubleRegistration::new(format!(
                    "registering metrics twice for metrics: {}",
                    name
                ))));
            }
            if metric.info().type_ != type_.base_type {
                return Err(make_exception_ptr(RuntimeError::new(format!(
                    "registering metrics {} registered with different type.",
                    name
                ))));
            }
            *metric.entry(&labels_ref) = Some(rm.clone());
            for k in rm.info().id.labels().keys() {
                self._labels.insert(k.clone());
            }
        } else {
            let mut info = impl_::MetricFamilyInfo {
                type_: type_.base_type,
                inherit_type: type_.type_name.clone(),
                d: d.clone(),
                name: id.full_name(),
                aggregate_labels: aggregate_labels.to_vec(),
            };
            self.update_aggregate(&mut info);
            let family = self._value_map.entry(name).or_default();
            family._info = info;
            *family.entry(&labels_ref) = Some(rm.clone());
        }
        self.dirty();
        Ok(rm)
    }

    /// Replaces the relabel configuration and relabels every series.
    pub fn set_relabel_configs(
        &mut self,
        relabel_configs: &[RelabelConfig],
    ) -> Future<MetricRelabelingResult> {
        self._relabel_configs = relabel_configs.to_vec();
        let mut conflicts = MetricRelabelingResult { metrics_relabeled_due_to_collision: 0 };
        let names: Vec<Sstring> = self._value_map.keys().cloned().collect();
        let configs = self._relabel_configs.clone();
        for fname in names {
            let mut rms: Vec<impl_::RegisterRef> = Vec::new();
            let entries: Vec<(impl_::InternalizedHolder, Option<impl_::RegisterRef>)> =
                match self._value_map.get(&fname) {
                    Some(f) => f._instances.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    None => continue,
                };
            for (key, metric) in entries {
                let Some(metric) = metric else { continue };
                if let Some(orig) = metric.info().original_labels.clone() {
                    // SAFETY: registrations are this shard's; no reference into this one is
                    // live during the statement.
                    unsafe { metric.get_mut_unchecked() }.info_mut().id.update_labels(orig);
                }
                for rl in &configs {
                    let mut info = metric.info().clone();
                    if self.apply_relabeling(rl, &mut info) {
                        self.dirty();
                    }
                    // SAFETY: as above.
                    *unsafe { metric.get_mut_unchecked() }.info_mut() = info;
                }
                if key.labels() != metric.info().id.labels() {
                    // If a metric labels were changed, we should remove it from the map, and place it back again
                    rms.push(metric);
                    if let Some(f) = self._value_map.get_mut(&fname) {
                        f.erase(key.labels());
                    }
                    self.dirty();
                }
            }
            for reg in rms {
                let Some(mut ilb) = reg.info().id.internalized_labels() else { continue };
                let exists =
                    self._value_map.get(&fname).is_some_and(|f| f.contains(reg.info().id.labels()));
                if exists {
                    // Two series cannot have the same name and labels: the configuration is wrong.
                    // At registration that is an error, but a metric reconfiguration at run time
                    // must not bring the server down, and the series cannot be thrown away.
                    // Instead it is added with an extra label, so the user can reconfigure.
                    log_error!(
                        engine_logger(),
                        "Metrics: After relabeling, registering metrics twice for metrics : {}",
                        fname
                    );
                    let id = get_unique_id();
                    let mut new_labels = (*ilb).clone();
                    new_labels.insert("err".into(), id.as_str().into());
                    ilb = self.internalize_labels(new_labels);
                    conflicts.metrics_relabeled_due_to_collision += 1;
                    // SAFETY: as above.
                    unsafe { reg.get_mut_unchecked() }.info_mut().id.update_labels(ilb.clone());
                }
                if let Some(f) = self._value_map.get_mut(&fname) {
                    *f.entry(&ilb) = Some(reg);
                }
            }
        }
        make_ready_future(conflicts)
    }

    /// Replaces the family configuration.
    pub fn set_metric_family_configs(&mut self, family_config: &[MetricFamilyConfig]) {
        self._metric_family_configs = family_config.to_vec();
        for (name, family) in self._value_map.iter_mut() {
            for fc in family_config {
                if fc.name == name.as_str() || fc.regex_name.match_(name.as_str()) {
                    family.info_mut().aggregate_labels = fc.aggregate_labels.clone();
                }
            }
        }
        let Some(md) = &mut self._metadata else {
            // The metadata structure may not have been built yet,
            // or it may rebuild right now, in this case just return
            // and it will be updated the next time, setting the dirty to true
            // is just in case we are somehow in the middle of rebuilding the
            // metadata.
            self._dirty = true;
            return;
        };
        // SAFETY: the metadata is this shard's; exporters hold only shared copies of the
        // pointer and do not read it during this synchronous call.
        for mf_metadata in unsafe { md.get_mut_unchecked() }.iter_mut() {
            for fc in family_config {
                if fc.name == mf_metadata.mf.name.as_str()
                    || fc.regex_name.match_(mf_metadata.mf.name.as_str())
                {
                    mf_metadata.mf.aggregate_labels = fc.aggregate_labels.clone();
                }
            }
        }
    }
}
