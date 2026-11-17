#![allow(unsafe_code)]
//! The data model of the metrics layer (the original's metrics_api.hh): metric ids compare and
//! order by (group, shard, name, labels) and hash by group and shard; a series without a shard
//! label is a bug; label sets hash by their values in name order; interned label holders order
//! by content and can be looked up by a plain label set; families and the registry accessors.
#![cfg(target_os = "linux")]

use std::hash::{DefaultHasher, Hash, Hasher};

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::metrics::impl_fns::get_value_map;
use teseladb::engine::core::metrics::{Description, SkipWhenEmpty, make_gauge_fn};
use teseladb::engine::core::metrics_api::impl_::{
    Config, Impl, InternalizedHolder, LabelsType, MetricFamily, MetricFamilyInfo, MetricId,
    MetricInstances, MetricSeriesMetadata, hash_labels,
};
use teseladb::engine::core::metrics_registration::MetricGroups;
use teseladb::engine::core::shared_ptr::make_lw_shared;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine_test_case;

fn labels(kv: &[(&str, &str)]) -> LabelsType {
    kv.iter().map(|(k, v)| (Sstring::from(*k), Sstring::from(*v))).collect()
}

fn id(group: &str, name: &str, kv: &[(&str, &str)]) -> MetricId {
    MetricId::new(Sstring::from(group), Sstring::from(name), make_lw_shared(labels(kv)))
}

fn hash_of<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

#[test]
fn metric_ids_compare_by_group_shard_name_and_labels() {
    let a = id("g", "m", &[("shard", "0"), ("x", "1")]);
    assert!(a == id("g", "m", &[("shard", "0"), ("x", "1")]));
    assert!(a != id("g", "m", &[("shard", "0"), ("x", "2")]));
    // The group comes first, then the shard, then the name.
    assert!(id("a", "z", &[("shard", "9")]) < id("b", "a", &[("shard", "0")]));
    assert!(id("g", "z", &[("shard", "0")]) < id("g", "a", &[("shard", "1")]));
    assert!(id("g", "a", &[("shard", "0")]) < id("g", "b", &[("shard", "0")]));
    assert_eq!(a.partial_cmp(&a.clone()), Some(std::cmp::Ordering::Equal));
    assert_eq!(a.instance_id(), "0");
}

#[test]
fn metric_ids_hash_by_group_and_shard_only() {
    let a = id("g", "m1", &[("shard", "0"), ("x", "1")]);
    let b = id("g", "m2", &[("shard", "0"), ("x", "2")]);
    assert_eq!(hash_of(&a), hash_of(&b), "the name and other labels do not enter the hash");
    assert_ne!(hash_of(&a), hash_of(&id("g", "m1", &[("shard", "1")])));
    assert_ne!(hash_of(&a), hash_of(&id("h", "m1", &[("shard", "0")])));
}

#[test]
#[should_panic(expected = "no shard label")]
fn a_series_without_a_shard_label_has_no_instance_id() {
    let _ = id("g", "m", &[("x", "1")]).instance_id();
}

#[test]
fn metric_id_accessors() {
    let mut a = MetricId::default();
    assert!(a.labels().is_empty(), "a default id has the empty label set");
    assert!(a.internalized_labels().is_none());
    a.set_group_name(&Sstring::from("renamed"));
    assert_eq!(a.group_name(), "renamed");
    a.update_labels(make_lw_shared(labels(&[("shard", "3")])));
    assert_eq!(a.instance_id(), "3");
    let md = MetricSeriesMetadata::new(a, SkipWhenEmpty::YES);
    assert_eq!(md.group_name(), "renamed");
    assert!(md.name().empty());
    assert_eq!(md.labels().len(), 1);
    assert_eq!(md.should_skip_when_empty(), SkipWhenEmpty::YES);
}

#[test]
fn label_sets_hash_by_their_values_in_name_order() {
    assert_eq!(hash_labels(&LabelsType::new()), 0);
    let ab = labels(&[("a", "1"), ("b", "2")]);
    let renamed = labels(&[("c", "1"), ("d", "2")]);
    assert_eq!(hash_labels(&ab), hash_labels(&renamed), "only the values are hashed");
    let swapped = labels(&[("a", "2"), ("b", "1")]);
    assert_ne!(hash_labels(&ab), hash_labels(&swapped), "the combination depends on the order");
    assert_ne!(hash_labels(&ab), hash_labels(&labels(&[("a", "1")])));
}

#[test]
fn interned_holders_order_by_content_and_families_find_by_labels() {
    let x = InternalizedHolder::new(labels(&[("k", "a")]));
    let y = InternalizedHolder::from_ref(make_lw_shared(labels(&[("k", "b")])));
    assert!(x < y);
    assert_eq!(x.partial_cmp(&y), Some(std::cmp::Ordering::Less));
    assert!(x == InternalizedHolder::new(labels(&[("k", "a")])), "equal by content");
    assert!(!x.has_users());
    let r = x.labels_ref();
    assert!(x.has_users(), "a second reference shares the labels");
    drop(r);
    let mut instances = MetricInstances::new();
    instances.insert(x.clone(), None);
    instances.insert(y.clone(), None);
    let info = MetricFamilyInfo { name: Sstring::from("fam"), ..Default::default() };
    let mut fam = MetricFamily::new(instances, info);
    assert_eq!(fam.size(), 2);
    assert_eq!(fam.info().name, "fam");
    let (holder, slot) = fam.find(&labels(&[("k", "b")])).expect("found by a plain label set");
    assert!(*holder == y && slot.is_none());
    assert!(fam.find(&labels(&[("k", "c")])).is_none());
    assert!(fam.erase(&labels(&[("k", "a")])).is_some());
    assert_eq!(fam.size(), 1);
}

#[test]
fn registry_accessors() {
    let mut r = Impl::new();
    assert!(r.get_config().hostname.empty());
    r.set_config(&Config { hostname: Sstring::from("host1") });
    assert_eq!(r.get_config().hostname, "host1");
    assert!(r.get_relabel_configs().is_empty());
    assert!(r.get_metric_family_configs().is_empty());
    assert!(r.get_value_map().is_empty());
    assert!(r.stop().get().is_ok());
}

engine_test_case!(a_registered_series_can_be_set_to_skip_when_empty, {
    let mut groups = MetricGroups::default();
    let r = groups
        .add_group("cov2grp", vec![make_gauge_fn("gauge", Description::new("a gauge"), || 1.0)]);
    if let Err(e) = r {
        return teseladb::engine::core::future::make_exception_future::<()>(e);
    }
    let registry = get_value_map();
    // SAFETY: this shard's registry, with no other reference alive across the lookup.
    let values = unsafe { registry.get_mut_unchecked() }.get_value_map();
    let fam = values.get(&Sstring::from("cov2grp_gauge")).expect("registered");
    let (_, slot) = fam.iter().next().expect("one series");
    let m = slot.as_ref().expect("live").clone();
    // SAFETY: no other reference to the series is in use.
    let rm = unsafe { m.get_mut_unchecked() };
    assert_eq!(rm.info().should_skip_when_empty, SkipWhenEmpty::NO);
    rm.set_skip_when_empty(SkipWhenEmpty::YES);
    assert_eq!(rm.info().should_skip_when_empty, SkipWhenEmpty::YES);
    assert_eq!(rm.call().d(), 1.0);
    drop(groups);
    make_ready_future(())
});
