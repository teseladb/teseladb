//! Registration of metrics: definitions, named groups of definitions, and the RAII holders
//! that keep them registered.
//!
//! A component builds [`MetricDefinition`]s (with the `make_counter` / `make_gauge` style
//! constructors of [`metrics`](crate::engine::core::metrics)), groups them under a name, and
//! adds them to a [`MetricGroups`] it owns. The metrics stay registered on the current shard for
//! as long as that holder lives; dropping it, or calling [`MetricGroups::clear`], unregisters
//! them.

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::metrics::impl_::{
    MetricDefinitionImpl, MetricGroupsDef, create_metric_groups,
};
use crate::engine::core::sstring::Sstring;

/// The name of a group of logically related metrics.
pub type GroupNameType = Sstring;

/// One metric of a group, ready to be registered.
pub struct MetricDefinition {
    pub(crate) _impl: Box<MetricDefinitionImpl>,
}

impl MetricDefinition {
    /// Wraps a fully described metric.
    pub fn new(m: MetricDefinitionImpl) -> Self {
        Self { _impl: Box::new(m) }
    }
}

impl From<MetricDefinitionImpl> for MetricDefinition {
    fn from(m: MetricDefinitionImpl) -> Self {
        Self::new(m)
    }
}

/// A named group of metric definitions.
pub struct MetricGroupDefinition {
    /// The group name.
    pub name: GroupNameType,
    /// Its metrics.
    pub metrics: Vec<MetricDefinition>,
}

impl MetricGroupDefinition {
    /// A group `name` of `l`.
    pub fn new(name: &str, l: Vec<MetricDefinition>) -> Self {
        Self { name: name.into(), metrics: l }
    }
}

/// Holds registered metric groups and unregisters them when dropped.
///
/// Groups are added at construction ([`with_groups`](Self::with_groups)) or later with
/// [`add_group`](Self::add_group); see [`metrics`](crate::engine::core::metrics) for the
/// supported metric kinds.
pub struct MetricGroups {
    _impl: Box<dyn MetricGroupsDef>,
}

impl MetricGroups {
    /// No metric registered.
    pub fn new() -> Self {
        Self { _impl: create_metric_groups() }
    }

    /// Creates a holder and registers every group of `mg` in it.
    ///
    /// # Errors
    ///
    /// Returns the first registration error (see [`add_group`](Self::add_group)).
    pub fn with_groups(mg: Vec<MetricGroupDefinition>) -> Result<Self, ExceptionPtr> {
        let mut g = Self::new();
        for i in mg {
            g.add_group(i.name.as_str(), i.metrics)?;
        }
        Ok(g)
    }

    /// Registers the metrics of `l` under the group `name`.
    ///
    /// `l` is any sequence of definitions built with the metric constructors, for example a
    /// counter and a gauge with their descriptions. Metric names must be unique inside the
    /// group. The result is `self`, so calls chain:
    /// `metrics.add_group("g1", a)?.add_group("g2", b)?`.
    ///
    /// # Errors
    ///
    /// Returns an error when a series is registered twice or is registered again with a
    /// different type.
    pub fn add_group<I, D>(&mut self, name: &str, l: I) -> Result<&mut Self, ExceptionPtr>
    where
        I: IntoIterator<Item = D>,
        D: Into<MetricDefinition>,
    {
        let defs: Vec<MetricDefinition> = l.into_iter().map(Into::into).collect();
        self._impl.add_group(name.into(), &defs)?;
        Ok(self)
    }

    /// Unregisters every metric this holder registered.
    pub fn clear(&mut self) {
        self._impl = create_metric_groups();
    }
}

impl Default for MetricGroups {
    fn default() -> Self {
        Self::new()
    }
}

/// A [`MetricGroups`] meant for a single group, registered at construction
/// ([`with_group`](Self::with_group)) or later through `add_group` (by deref).
#[derive(Default)]
pub struct MetricGroup {
    /// The groups holder this extends.
    pub groups: MetricGroups,
}

impl MetricGroup {
    /// An empty group.
    pub fn new() -> Self {
        Self { groups: MetricGroups::new() }
    }

    /// Creates the holder and registers the metrics of `l` under `name`.
    ///
    /// # Errors
    ///
    /// Returns the registration error of [`MetricGroups::add_group`].
    pub fn with_group<I, D>(name: &str, l: I) -> Result<Self, ExceptionPtr>
    where
        I: IntoIterator<Item = D>,
        D: Into<MetricDefinition>,
    {
        let mut g = Self::new();
        g.groups.add_group(name, l)?;
        Ok(g)
    }
}

impl std::ops::Deref for MetricGroup {
    type Target = MetricGroups;

    fn deref(&self) -> &MetricGroups {
        &self.groups
    }
}

impl std::ops::DerefMut for MetricGroup {
    fn deref_mut(&mut self) -> &mut MetricGroups {
        &mut self.groups
    }
}
