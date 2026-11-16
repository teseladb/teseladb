#![cfg(target_os = "linux")]
//! Two collectd protocol details of the exporter.
//!
//! A text field longer than the protocol limit is cut and gets a `~<n>` suffix; the result
//! must fit the limit for every `n`, also when `n` is a power of ten (1, 10, 100...).
//!
//! The type byte of each value tells the collectd server how to read its 8 bytes: COUNTER
//! (0) and DERIVE (2) are big-endian integers, GAUGE (1) is a little-endian double. The
//! exporter sends gauges as doubles and every other type as an unsigned integer, so a gauge
//! must be tagged 1 and an integer value 0.

use teseladb::engine::core::metrics::impl_::DataType;
use teseladb::engine::core::scollectd::{
    Description, MAX_COLLECTD_FIELD_TEXT_LEN, ScValue, TypeInstanceId, ValueList,
    make_type_instance, per_cpu_plugin_instance,
};

/// The collectd wire code of a COUNTER value.
const COLLECTD_COUNTER: u8 = 0;
/// The collectd wire code of a GAUGE value.
const COLLECTD_GAUGE: u8 = 1;

#[test]
fn truncated_fields_fit_the_limit_for_every_suffix() {
    // Each test runs on a thread of its own, so this thread's truncation counter starts at
    // zero: the suffixes go from ~1 to ~12.
    let long = "p".repeat(2 * MAX_COLLECTD_FIELD_TEXT_LEN);
    for n in 1..=12u32 {
        let id = TypeInstanceId::new(
            long.as_str().into(),
            per_cpu_plugin_instance(),
            "t".into(),
            "i".into(),
        );
        let plugin = id.plugin();
        let suffix = format!("~{n}");
        assert!(plugin.as_str().ends_with(&suffix), "{plugin}");
        assert_eq!(plugin.size(), MAX_COLLECTD_FIELD_TEXT_LEN, "suffix {suffix}: {plugin}");
    }
}

#[test]
fn value_lists_tag_gauges_and_integers_with_the_collectd_codes() {
    let list = make_type_instance(
        Description::new("three values"),
        vec![
            ScValue::new(DataType::Counter, || 9u64),
            ScValue::new(DataType::Gauge, || 0.5f64),
            ScValue::new(DataType::RealCounter, || 3u64),
        ],
    );
    let mut types = [0xffu8; 3];
    list.types(&mut types);
    assert_eq!(types, [COLLECTD_COUNTER, COLLECTD_GAUGE, COLLECTD_COUNTER]);
    // The bytes match the tags: big-endian integers and a little-endian double.
    let mut values = [0u8; 24];
    list.values(&mut values);
    assert_eq!(&values[..8], &9u64.to_be_bytes());
    assert_eq!(&values[8..16], &0.5f64.to_le_bytes());
    assert_eq!(&values[16..], &3u64.to_be_bytes());
}
