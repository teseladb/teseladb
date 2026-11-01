//! Group 0 never records a split inside a capped collection (015 T007, research R1): the cut
//! moves to the start of the collection's keys, or to their end, or the split is refused.

use teseladb_bson::Document;
use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::{CatalogCommand, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_types::{GroupId, NodeId};

fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied_index() + 1;
    let ctx = ApplyCtx {
        group: GroupId(0),
        index,
        term: 1,
        ts: teseladb_hlc::Hlc { physical: 1_000 + index, logical: 0 },
    };
    sm.apply(&cmd.encode(), &ctx).expect("applied")
}

fn node() -> NodeAddrs {
    NodeAddrs {
        name: b"n1".to_vec(),
        peer_addr: b"n1:7421".to_vec(),
        http_addr: b"n1:8080".to_vec(),
        native_addr: b"n1:27017".to_vec(),
        cores: 2,
    }
}

/// A cluster of one node with one range over every key and collections `capped` and `plain`:
/// their numbers.
fn cluster() -> (Group0Sm, u32, u32) {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: node(),
        cert_fingerprint: [1; 32],
    };
    run(&mut sm, boot);
    let range =
        Group0Command::CreateRange { start: vec![], end: vec![], replicas: vec![NodeId(1)] };
    run(&mut sm, range);
    for (name, options) in [
        ("capped", Document::new().with("capped", true).with("size", 4096)),
        ("plain", Document::new()),
    ] {
        let ns = Namespace { db: "d".into(), coll: name.into() };
        let options = options.to_bytes().expect("options");
        let create =
            CatalogCommand::Create { ns, uuid: [name.len() as u8; 16], options, if_absent: false };
        run(&mut sm, Group0Command::Catalog(create));
    }
    let id = |sm: &Group0Sm, name: &str| {
        sm.catalog.get(&Namespace { db: "d".into(), coll: name.into() }).expect("created").id
    };
    let (c, p) = (id(&sm, "capped"), id(&sm, "plain"));
    (sm, c, p)
}

fn split(sm: &mut Group0Sm, at: Vec<u8>) -> Result<Vec<u8>, Group0Result> {
    match run(sm, Group0Command::SplitRange { group: GroupId(1), at }) {
        Group0Result::Range(_) => {
            Ok(sm.ranges[&GroupId(1)].pending_split.clone().expect("pending"))
        }
        other => Err(other),
    }
}

#[test]
fn a_cut_inside_a_capped_collection_moves_to_its_start() {
    let (mut sm, capped, _) = cluster();
    let inside = KeyBuilder::document_capped(0, capped, 9).as_bytes().to_vec();
    assert_eq!(
        split(&mut sm, inside),
        Ok(KeyBuilder::collection_start(0, capped).as_bytes().to_vec())
    );
}

#[test]
fn a_cut_inside_another_collection_stays() {
    let (mut sm, _, plain) = cluster();
    let inside = KeyBuilder::document_capped(0, plain, 9).as_bytes().to_vec();
    assert_eq!(split(&mut sm, inside.clone()), Ok(inside));
}

#[test]
fn a_range_that_holds_only_a_capped_collection_does_not_split() {
    let (mut sm, capped, _) = cluster();
    let start = KeyBuilder::collection_start(0, capped).as_bytes().to_vec();
    let end = KeyBuilder::collection_end(0, capped).as_bytes().to_vec();
    // Cut the range at the collection's start and end first.
    for at in [start.clone(), end] {
        let pending = split(&mut sm, at.clone()).expect("a split");
        let done = Group0Command::CompleteSplit { group: GroupId(1), at: pending, at_index: 9 };
        assert!(matches!(run(&mut sm, done), Group0Result::Range(_)));
        if at == start {
            // From now on range 1 is the part before the collection: go on with the new range.
            let new = sm.range_for(&at).expect("a range").group;
            sm.ranges.insert(GroupId(1), sm.ranges[&new].clone());
        }
    }
    let inside = KeyBuilder::document_capped(0, capped, 9).as_bytes().to_vec();
    assert_eq!(split(&mut sm, inside), Err(Group0Result::Err(Group0Error::BadSplit)));
}

#[test]
fn a_cut_in_another_tenant_with_the_capped_number_stays() {
    let (mut sm, capped, _) = cluster();
    let inside = KeyBuilder::document_capped(1, capped, 9).as_bytes().to_vec();
    assert_eq!(split(&mut sm, inside.clone()), Ok(inside));
}

/// Splits range 1 at `at` and completes it: the new range's number.
fn cut(sm: &mut Group0Sm, at: Vec<u8>) -> GroupId {
    let left = sm.range_for(&at).expect("a range").group;
    let pending = match run(sm, Group0Command::SplitRange { group: left, at }) {
        Group0Result::Range(_) => sm.ranges[&left].pending_split.clone().expect("pending"),
        other => panic!("{other:?}"),
    };
    let done = Group0Command::CompleteSplit { group: left, at: pending.clone(), at_index: 9 };
    assert!(matches!(run(sm, done), Group0Result::Range(_)));
    sm.range_for(&pending).expect("a range").group
}

#[test]
fn a_range_with_a_capped_collection_never_merges_with_its_neighbours() {
    // 007 FR-009: the capped collection keeps a range of its own.
    let (mut sm, capped, plain) = cluster();
    let own = cut(&mut sm, KeyBuilder::collection_start(0, capped).as_bytes().to_vec());
    let after = cut(&mut sm, KeyBuilder::collection_end(0, capped).as_bytes().to_vec());
    let before = sm.range_for(&[]).expect("the first range").group;
    for (left, right) in [(before, own), (own, after)] {
        let merge = Group0Command::MergeRanges { left, right };
        assert_eq!(run(&mut sm, merge), Group0Result::Err(Group0Error::BadBounds));
    }
    // Two ranges without one merge.
    let (l, r) = if plain < capped {
        (before, cut(&mut sm, KeyBuilder::collection_start(0, plain).as_bytes().to_vec()))
    } else {
        (after, cut(&mut sm, KeyBuilder::collection_end(0, plain).as_bytes().to_vec()))
    };
    assert_eq!(run(&mut sm, Group0Command::MergeRanges { left: l, right: r }), Group0Result::Ok);
}

#[test]
fn a_capped_collection_of_a_tenant_is_cut_out_at_its_own_keys() {
    // 007 FR-025: a tenant's collection has the tenant in its keys.
    let (mut sm, _, _) = cluster();
    run(&mut sm, Group0Command::CreateTenant { name: "acme".into() });
    let ns = Namespace { db: teseladb_meta::tenants::qualified_db(1, "d"), coll: "log".into() };
    let options = Document::new().with("capped", true).with("size", 4096).to_bytes().unwrap();
    let create = CatalogCommand::Create { ns, uuid: [9; 16], options, if_absent: false };
    let Group0Result::Collection(c) = run(&mut sm, Group0Command::Catalog(create)) else {
        panic!("created")
    };
    let inside = KeyBuilder::document_capped(1, c.id, 9).as_bytes().to_vec();
    assert_eq!(
        split(&mut sm, inside),
        Ok(KeyBuilder::collection_start(1, c.id).as_bytes().to_vec())
    );
    // The same number in the default tenant is no capped collection.
    let elsewhere = KeyBuilder::document_capped(0, c.id, 9).as_bytes().to_vec();
    let mut fresh = cluster().0;
    run(&mut fresh, Group0Command::CreateTenant { name: "acme".into() });
    assert_eq!(split(&mut fresh, elsewhere.clone()), Ok(elsewhere));
}
