//! The data model behind the metrics layer: metric ids, families, registered series, the
//! per-shard registry and the snapshots the exporters read.
//!
//! Each shard owns its own registry ([`impl_::Impl`]); nothing here is shared between shards.
//! Registration (through [`MetricGroups`](crate::engine::core::metrics::MetricGroups)) adds
//! series to the registry's value map; the exporters (Prometheus, collectd) take a
//! [`impl_::ValuesCopy`] snapshot per shard and format it. Label sets are interned
//! ([`impl_::InternalizedLabelsRef`]) so that the many series sharing a label set share one
//! copy. The functions are implemented in [`metrics`](crate::engine::core::metrics).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::engine::core::metrics::impl_::{DataType, MetricFunction, MetricTypeDef, MetricValue};
use crate::engine::core::metrics::{Description, InstanceIdType, MetricNameType, SkipWhenEmpty};
use crate::engine::core::metrics_registration::GroupNameType;
use crate::engine::core::relabel_config::metrics::{MetricFamilyConfig, RelabelConfig};
use crate::engine::core::shared_ptr::{LwSharedPtr, SharedPtr};
use crate::engine::core::sstring::Sstring;
use crate::engine::util::program_options::{OptionGroup, Value};

/// The implementation parts of the metrics layer.
pub mod impl_ {
    use super::*;

    /// A label set: label name to value, ordered by name.
    pub type LabelsType = BTreeMap<Sstring, Sstring>;
    /// A shared, interned label set.
    pub type InternalizedLabelsRef = LwSharedPtr<LabelsType>;

    /// Hash of a label set: combines the hashes of the label values in name order.
    pub fn hash_labels(s: &LabelsType) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h: u64 = 0;
        for v in s.values() {
            let mut hv = std::hash::DefaultHasher::new();
            v.hash(&mut hv);
            // Classic hash-combine step: mix with the golden-ratio constant and shifts.
            h ^= hv.finish().wrapping_add(0x9e3779b9).wrapping_add(h << 6).wrapping_add(h >> 2);
        }
        h
    }

    /// The identity of one metric series: its group, its name and its label set.
    ///
    /// Metrics are collected in groups that belong to a logical entity; for example, the
    /// measurements of the CPU belong to the group `"cpu"`. The name is the metric's own name,
    /// such as `used_objects` or `used_bytes`. The labels distinguish the series of one
    /// metric; the shard label ([`instance_id`](Self::instance_id)) is the usual one, since
    /// each shard registers its own copy of a metric.
    #[derive(Clone, Default)]
    pub struct MetricId {
        pub(crate) _group: GroupNameType,
        pub(crate) _name: MetricNameType,
        pub(crate) _labels: Option<InternalizedLabelsRef>,
    }

    impl MetricId {
        /// The id of metric `name` of `group` with `labels`.
        pub fn new(
            group: GroupNameType,
            name: MetricNameType,
            labels: InternalizedLabelsRef,
        ) -> Self {
            Self { _group: group, _name: name, _labels: Some(labels) }
        }

        /// The group.
        pub fn group_name(&self) -> &GroupNameType {
            &self._group
        }

        /// Renames the group.
        pub fn set_group_name(&mut self, name: &GroupNameType) {
            self._group = name.clone();
        }

        /// The shard label's value.
        ///
        /// # Panics
        ///
        /// Panics if the series has no shard label.
        pub fn instance_id(&self) -> &InstanceIdType {
            let key: Sstring = crate::engine::core::metrics::shard_label_name();
            match self.labels().get(&key) {
                Some(v) => v,
                // Every registered series carries the shard label; its absence is a bug.
                None => panic!("metric_id::instance_id(): no shard label"),
            }
        }

        /// The metric name.
        pub fn name(&self) -> &MetricNameType {
            &self._name
        }

        /// The labels.
        pub fn labels(&self) -> &LabelsType {
            match &self._labels {
                Some(l) => l,
                None => empty_labels(),
            }
        }

        /// The shared labels.
        pub fn internalized_labels(&self) -> Option<InternalizedLabelsRef> {
            self._labels.clone()
        }

        /// Replaces the labels.
        pub fn update_labels(&mut self, labels: InternalizedLabelsRef) {
            self._labels = Some(labels);
        }

        fn as_tuple(&self) -> (&GroupNameType, &InstanceIdType, &MetricNameType, &LabelsType) {
            (self.group_name(), self.instance_id(), self.name(), self.labels())
        }
    }

    fn empty_labels() -> &'static LabelsType {
        static EMPTY: LabelsType = BTreeMap::new();
        &EMPTY
    }

    impl PartialEq for MetricId {
        fn eq(&self, id2: &Self) -> bool {
            self.as_tuple() == id2.as_tuple()
        }
    }

    impl PartialOrd for MetricId {
        fn partial_cmp(&self, id2: &Self) -> Option<std::cmp::Ordering> {
            Some(self.as_tuple().cmp(&id2.as_tuple()))
        }
    }

    /// Hashes the group name and the shard label.
    impl std::hash::Hash for MetricId {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            use std::hash::Hasher as _;
            let mut a = std::hash::DefaultHasher::new();
            self.group_name().hash(&mut a);
            let mut b = std::hash::DefaultHasher::new();
            self.instance_id().hash(&mut b);
            state.write_u64(a.finish() ^ (b.finish() << 1));
        }
    }

    /// holds metadata information of a metric family
    ///
    /// Holds the information that is shared between all metrics
    /// that belong to the same metric family.
    #[derive(Debug, Clone)]
    pub struct MetricFamilyInfo {
        /// The value type of the family (gauge, counter, histogram, ...).
        pub type_: DataType,
        /// A custom type name that replaces the basic type's name in the exporters, or empty.
        pub inherit_type: MetricTypeDef,
        /// The family's description.
        pub d: Description,
        /// The family name (group and metric name joined).
        pub name: Sstring,
        /// The labels removed when the series are aggregated for export.
        pub aggregate_labels: Vec<String>,
    }

    impl Default for MetricFamilyInfo {
        fn default() -> Self {
            Self {
                type_: DataType::Gauge,
                inherit_type: Sstring::new(),
                d: Description::default(),
                name: Sstring::new(),
                aggregate_labels: Vec::new(),
            }
        }
    }

    /// holds metric metadata
    #[derive(Clone, Default)]
    pub struct MetricInfo {
        /// The series id, with its current (possibly relabelled) labels.
        pub id: MetricId,
        /// The labels the series was registered with, before any relabelling.
        pub original_labels: Option<InternalizedLabelsRef>,
        /// Whether the series is exported.
        pub enabled: bool,
        /// Whether an empty series is left out of the export.
        pub should_skip_when_empty: SkipWhenEmpty,
    }

    /// Shared labels, ordered by their content.
    #[derive(Clone)]
    pub struct InternalizedHolder {
        _labels: InternalizedLabelsRef,
    }

    impl InternalizedHolder {
        /// Holds a new copy of `labels`.
        pub fn new(labels: LabelsType) -> Self {
            Self { _labels: crate::engine::core::shared_ptr::make_lw_shared(labels) }
        }

        /// Holds `labels`.
        pub fn from_ref(labels: InternalizedLabelsRef) -> Self {
            Self { _labels: labels }
        }

        /// The shared labels.
        pub fn labels_ref(&self) -> InternalizedLabelsRef {
            self._labels.clone()
        }

        /// The labels.
        pub fn labels(&self) -> &LabelsType {
            &self._labels
        }

        /// Whether anybody else shares the labels.
        pub fn has_users(&self) -> bool {
            // Getting the count wrong isn't a correctness issue but will just make internalization worse
            self._labels.use_count() > 1
        }
    }

    impl PartialEq for InternalizedHolder {
        fn eq(&self, o: &Self) -> bool {
            self.labels() == o.labels()
        }
    }

    impl Eq for InternalizedHolder {}

    impl PartialOrd for InternalizedHolder {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(o))
        }
    }

    impl Ord for InternalizedHolder {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering {
            self.labels().cmp(o.labels())
        }
    }

    /// Lets a set of holders be searched by a plain label set.
    impl std::borrow::Borrow<LabelsType> for InternalizedHolder {
        fn borrow(&self) -> &LabelsType {
            self.labels()
        }
    }

    /// One registered series.
    pub struct RegisteredMetric {
        pub(crate) _info: MetricInfo,
        pub(crate) _f: MetricFunction,
    }

    impl RegisteredMetric {
        /// Reads the value.
        #[inline]
        pub fn call(&self) -> MetricValue {
            (self._f)()
        }

        /// Whether the series is reported.
        pub fn is_enabled(&self) -> bool {
            self._info.enabled
        }

        /// Enables or disables reporting.
        pub fn set_enabled(&mut self, b: bool) {
            self._info.enabled = b;
        }

        /// Sets whether an unused series is skipped.
        pub fn set_skip_when_empty(&mut self, skip: SkipWhenEmpty) {
            self._info.should_skip_when_empty = skip;
        }

        /// The id.
        pub fn get_id(&self) -> &MetricId {
            &self._info.id
        }

        /// The metadata.
        pub fn info(&self) -> &MetricInfo {
            &self._info
        }

        /// The metadata, mutable.
        pub fn info_mut(&mut self) -> &mut MetricInfo {
            &mut self._info
        }

        /// The value function.
        pub fn get_function(&self) -> &MetricFunction {
            &self._f
        }
    }

    /// A shared handle to a registered series.
    pub type RegisterRef = SharedPtr<RegisteredMetric>;
    /// The series of a family by labels.
    pub type MetricInstances = BTreeMap<InternalizedHolder, Option<RegisterRef>>;
    /// The registrations of a `metric_groups`.
    pub type MetricsRegistration = Vec<RegisterRef>;

    /// The registrations made through one `metric_groups`.
    pub struct MetricGroupsImpl {
        pub(crate) _registration: MetricsRegistration,
        pub(crate) _impl: Option<SharedPtr<Impl>>, // keep impl alive while metrics are registered
    }

    /// The series sharing a name.
    #[derive(Clone, Default)]
    pub struct MetricFamily {
        pub(crate) _instances: MetricInstances,
        pub(crate) _info: MetricFamilyInfo,
    }

    impl MetricFamily {
        /// A family of `instances` described by `info`.
        pub fn new(instances: MetricInstances, info: MetricFamilyInfo) -> Self {
            Self { _instances: instances, _info: info }
        }

        /// The slot of the series with labels `l`, inserted empty when missing.
        pub fn entry(&mut self, l: &InternalizedLabelsRef) -> &mut Option<RegisterRef> {
            self._instances.entry(InternalizedHolder::from_ref(l.clone())).or_insert(None)
        }

        /// `at(l)`
        pub fn at(&self, l: &LabelsType) -> Option<&Option<RegisterRef>> {
            self._instances.get(l)
        }

        /// The family metadata.
        pub fn info(&self) -> &MetricFamilyInfo {
            &self._info
        }

        /// The family metadata, mutable.
        pub fn info_mut(&mut self) -> &mut MetricFamilyInfo {
            &mut self._info
        }

        /// The series with labels `l`.
        pub fn find(&self, l: &LabelsType) -> Option<(&InternalizedHolder, &Option<RegisterRef>)> {
            self._instances.get_key_value(l)
        }

        /// Whether a series with labels `l` exists.
        pub fn contains(&self, l: &LabelsType) -> bool {
            self._instances.contains_key(l)
        }

        /// The series in label order.
        pub fn iter(&self) -> impl Iterator<Item = (&InternalizedHolder, &Option<RegisterRef>)> {
            self._instances.iter()
        }

        /// Whether the family has no series.
        pub fn empty(&self) -> bool {
            self._instances.is_empty()
        }

        /// Removes the series with labels `l`.
        pub fn erase(&mut self, l: &LabelsType) -> Option<Option<RegisterRef>> {
            self._instances.remove(l)
        }

        /// Number of series.
        pub fn size(&self) -> u32 {
            self._instances.len() as u32
        }
    }

    /// The families by name.
    pub type ValueMap = BTreeMap<Sstring, MetricFamily>;

    /// Subset of the per series metadata that is shared via get_values to other shards.
    ///
    /// Allows omitting metadata that is already stored elsewhere or not needed by
    /// the metrics scrap handlers.
    ///
    /// Not copyable to allow for safely sharing internalized data.
    #[derive(Default)]
    pub struct MetricSeriesMetadata {
        // The Prometheus exporter only needs the labels from here, but the collectd exporter
        // needs the group and metric name separately. MetricFamilyInfo only stores the merged and
        // filtered name so we have to duplicate it here.
        _id: MetricId,
        _should_skip_when_empty: SkipWhenEmpty,
    }

    impl MetricSeriesMetadata {
        /// The metadata of series `id`.
        pub fn new(id: MetricId, should_skip_when_empty: SkipWhenEmpty) -> Self {
            Self { _id: id, _should_skip_when_empty: should_skip_when_empty }
        }

        /// The labels.
        pub fn labels(&self) -> &LabelsType {
            self._id.labels()
        }

        /// Whether an unused series is skipped.
        pub fn should_skip_when_empty(&self) -> SkipWhenEmpty {
            self._should_skip_when_empty
        }

        /// The group.
        pub fn group_name(&self) -> GroupNameType {
            self._id.group_name().clone()
        }

        /// The metric name.
        pub fn name(&self) -> GroupNameType {
            self._id.name().clone()
        }
    }

    /// The metadata of the enabled series of one family.
    pub type MetricMetadataFifo = VecDeque<MetricSeriesMetadata>;

    /// holds a metric family metadata
    ///
    /// The meta data of a metric family compose of the
    /// metadata of the family, and a vector of the metadata for
    /// each of the metrics.
    ///
    /// The struct is used for two purposes. First, it allows iterating over all metric families
    /// and all metrics related to them. Second, it only contains enabled metrics,
    /// making disabled metrics more efficient.
    /// The struct is recreated when the registry's value map changes.
    #[derive(Default)]
    pub struct MetricFamilyMetadata {
        /// The family's metadata.
        pub mf: MetricFamilyInfo,
        /// The metadata of each enabled series.
        pub metrics: MetricMetadataFifo,
    }

    impl MetricFamilyMetadata {
        /// The metadata of family `mf` with its enabled series.
        pub fn new(mf: MetricFamilyInfo, metrics: MetricMetadataFifo) -> Self {
            Self { mf, metrics }
        }
    }

    /// The values of the enabled series of one family, in metadata order.
    pub type ValueVector = VecDeque<MetricValue>;
    /// The metadata of every family.
    pub type MetricMetadata = Vec<MetricFamilyMetadata>;
    /// The values of every family.
    pub type MetricValues = VecDeque<ValueVector>;

    /// A snapshot of the metrics of one shard.
    #[derive(Default)]
    pub struct ValuesCopy {
        /// The metadata the values correspond to.
        pub metadata: Option<SharedPtr<MetricMetadata>>,
        /// The values, one vector per family, in metadata order.
        pub values: MetricValues,
    }

    /// The metrics configuration of a shard.
    #[derive(Debug, Clone, Default)]
    pub struct Config {
        /// The host name reported with the metrics.
        pub hostname: Sstring,
    }

    /// The interned label sets of a registry.
    pub type InternalizedSet = BTreeSet<InternalizedHolder>;

    /// The metrics registry of a shard.
    #[derive(Default)]
    pub struct Impl {
        pub(crate) _value_map: ValueMap,
        pub(crate) _config: Config,
        pub(crate) _dirty: bool,
        pub(crate) _metadata: Option<SharedPtr<MetricMetadata>>,
        pub(crate) _labels: BTreeSet<Sstring>,
        pub(crate) _current_metrics: Vec<VecDeque<MetricFunction>>,
        pub(crate) _relabel_configs: Vec<RelabelConfig>,
        pub(crate) _metric_family_configs: Vec<MetricFamilyConfig>,
        pub(crate) _internalized_labels: InternalizedSet,
    }

    impl Impl {
        /// An empty registry (dirty, so the first read builds the metadata).
        pub fn new() -> Self {
            Self { _dirty: true, ..Default::default() }
        }

        /// The families.
        pub fn get_value_map(&mut self) -> &mut ValueMap {
            &mut self._value_map
        }

        /// Stops the registry.
        pub fn stop(&mut self) -> crate::engine::core::future::Future<()> {
            crate::engine::core::future::make_ready_future(())
        }

        /// The configuration.
        pub fn get_config(&self) -> &Config {
            &self._config
        }

        /// Replaces the configuration.
        pub fn set_config(&mut self, c: &Config) {
            self._config = c.clone();
        }

        /// Marks the metadata for rebuilding.
        pub fn dirty(&mut self) {
            self._dirty = true;
        }

        /// Every label key in use.
        pub fn get_labels(&self) -> &BTreeSet<Sstring> {
            &self._labels
        }

        /// The relabel configuration.
        pub fn get_relabel_configs(&self) -> &Vec<RelabelConfig> {
            &self._relabel_configs
        }

        /// The family configuration.
        pub fn get_metric_family_configs(&self) -> &Vec<MetricFamilyConfig> {
            &self._metric_family_configs
        }
    }

    /// `using values_reference = shared_ptr<values_copy>;`
    pub type ValuesReference = SharedPtr<ValuesCopy>;
}

/// result of metric relabeling
///
/// The result of calling set_relabel_configs.
///
/// metrics_relabeled_due_to_collision the number of metrics that caused conflict
/// and were relabeled to avoid name collision.
///
/// Non zero value indicates there were name collisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricRelabelingResult {
    /// Number of series renamed to avoid a name collision.
    pub metrics_relabeled_due_to_collision: usize,
}

/// Metrics configuration options.
pub struct Options {
    /// The option group this struct extends.
    pub group: OptionGroup,
    /// The hostname used by the metrics.
    ///
    /// If not set, the local hostname will be used.
    pub metrics_hostname: Value<String>,
}
