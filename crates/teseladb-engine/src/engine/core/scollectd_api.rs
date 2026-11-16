//! Public query interface of the collectd exporter: read the current values, ids and
//! descriptions of the exported series and enable or disable individual series.
//!
//! The items are implemented in [`scollectd`](crate::engine::core::scollectd) and re-exported
//! here.

/// A metric value as reported to collectd.
pub use crate::engine::core::scollectd::CollectdValue;

/// The current values of a series; the ids of every series; a series' description; whether a
/// series is reported; enabling or disabling one; and a copy of the families.
pub use crate::engine::core::scollectd::{
    enable, get_collectd_description_str, get_collectd_ids, get_collectd_value, get_value_map,
    is_enabled,
};
