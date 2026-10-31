//! Read preferences (007 FR-013): the five modes, `maxStalenessSeconds`, tag sets and `hedge`, with
//! the codes, messages and order of checks of a refrouter 9.0.4 (measured with `find` and
//! `$readPreference`), and the order in which a read tries the replicas of its range.

use teseladb_router::read_pref::{
    Mode, ReadPrefError, ReadPreference, Replica, TagSet, candidates,
};
use teseladb_types::NodeId;

fn tags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect()
}

fn set(pairs: &[(&str, &str)]) -> TagSet {
    Some(tags(pairs))
}

fn new(
    mode: &str,
    staleness: Option<f64>,
    sets: Vec<TagSet>,
) -> Result<ReadPreference, ReadPrefError> {
    ReadPreference::new(mode, staleness, sets, false)
}

fn pref(mode: &str) -> ReadPreference {
    new(mode, None, Vec::new()).unwrap()
}

/// Leader 1 at 5 ms in `east`; followers 2 (1 ms, `east`) and 3 (30 ms, `west`).
fn replicas() -> Vec<Replica> {
    vec![
        Replica { node: NodeId(1), leader: true, latency_ms: 5, tags: tags(&[("dc", "east")]) },
        Replica { node: NodeId(2), leader: false, latency_ms: 1, tags: tags(&[("dc", "east")]) },
        Replica { node: NodeId(3), leader: false, latency_ms: 30, tags: tags(&[("dc", "west")]) },
    ]
}

fn order(p: &ReadPreference) -> Vec<u64> {
    candidates(p, &replicas()).into_iter().map(|n| n.0).collect()
}

#[test]
fn each_mode_tries_the_replicas_in_its_order() {
    assert_eq!(order(&pref("primary")), [1]);
    assert_eq!(order(&pref("primaryPreferred")), [1, 2, 3]);
    assert_eq!(order(&pref("secondary")), [2, 3]);
    assert_eq!(order(&pref("secondaryPreferred")), [2, 3, 1]);
    // The nearest first, the leader among them.
    assert_eq!(order(&pref("nearest")), [2, 1, 3]);
    assert_eq!(pref("nearest").mode, Mode::Nearest);
}

#[test]
fn the_first_tag_set_that_matches_a_replica_chooses() {
    let west = new("secondary", None, vec![set(&[("dc", "west")])]).unwrap();
    assert_eq!(order(&west), [3]);
    // No replica in `north`: the next set, empty, matches any.
    let fallback = new("nearest", None, vec![set(&[("dc", "north")]), set(&[])]).unwrap();
    assert_eq!(order(&fallback), [2, 1, 3]);
    // No set matches: none of those replicas, but the leader still for `secondaryPreferred`.
    let none = new("secondaryPreferred", None, vec![set(&[("dc", "x")])]).unwrap();
    assert_eq!(order(&none), [1]);
    // Every pair of a set has to match; a set RefDb takes but no node's tags can have matches none.
    let both = new("secondary", None, vec![set(&[("dc", "east"), ("rack", "r1")])]).unwrap();
    assert_eq!(order(&both), Vec::<u64>::new());
    let never = new("nearest", None, vec![None, set(&[("dc", "west")])]).unwrap();
    assert_eq!(order(&never), [3]);
}

#[test]
fn bad_preferences_get_the_refrouter_codes_and_messages_in_its_order() {
    let p = new("secondary", Some(90.5), Vec::new()).unwrap();
    assert_eq!(p.max_staleness_ms, Some(90_500));
    // 0 is no limit; a negative value is refused, -1 too.
    assert_eq!(new("secondary", Some(0.0), Vec::new()).unwrap().max_staleness_ms, None);
    assert_eq!(new("primary", Some(0.0), vec![set(&[])]).map(|p| p.mode), Ok(Mode::Primary));
    let refused = |r: Result<ReadPreference, ReadPrefError>| {
        let e = r.unwrap_err();
        (e.code(), e.to_string())
    };
    let bad_value = |m: &str| (2, m.to_owned());
    assert_eq!(
        refused(new("Primary", Some(30.0), Vec::new())),
        bad_value(
            "Enumeration value 'Primary' for field 'readPreference.mode' is not a valid value."
        )
    );
    assert_eq!(
        refused(new("primary", Some(-1.0), Vec::new())),
        bad_value("maxStalenessSeconds must be a non-negative integer")
    );
    assert_eq!(
        refused(new("secondary", Some(1e20), Vec::new())),
        bad_value("maxStalenessSeconds value cannot exceed 9223372036854775807")
    );
    assert_eq!(
        refused(new("primary", Some(89.9), Vec::new())),
        (160, "maxStalenessSeconds value cannot be less than 90".to_owned())
    );
    let hedged = ReadPreference::new("primary", Some(120.0), vec![set(&[("a", "b")])], true);
    assert_eq!(
        refused(hedged),
        (72, "cannot enable hedging for $readPreference mode \"primaryOnly\"".to_owned())
    );
    assert!(ReadPreference::new("primaryPreferred", None, Vec::new(), true).is_ok());
    assert_eq!(
        refused(new("primary", Some(120.0), vec![set(&[("a", "b")])])),
        bad_value("Only empty tags are allowed with primary read preference")
    );
    assert_eq!(
        refused(new("primary", Some(120.0), Vec::new())),
        bad_value("maxStalenessSeconds can not be set for the primary mode")
    );
}

#[test]
fn a_preference_is_described_as_a_refrouter_writes_it() {
    let plain = new("secondary", None, Vec::new()).unwrap();
    assert_eq!(plain.describe(), r#"{ mode: "secondary" }"#);
    let full = new("nearest", Some(90.5), vec![set(&[("dc", "x"), ("r", "1")]), set(&[])]).unwrap();
    assert_eq!(
        full.describe(),
        r#"{ mode: "nearest", tags: [ { dc: "x", r: "1" }, {} ], maxStalenessSeconds: 90 }"#
    );
    assert_eq!(
        new("primaryPreferred", None, vec![None]).unwrap().describe(),
        r#"{ mode: "primaryPreferred", tags: [ {} ] }"#
    );
}
