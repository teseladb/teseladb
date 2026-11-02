//! 007 T006: index definitions in the catalog (FR-001), idempotent commands with a request
//! identifier (contract `catalog.md`) and descriptors by key (FR-002), as every replica of Group 0
//! applies them and as they travel in its snapshot.

use teseladb_meta::catalog::{CatalogCommand, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{
    Group0Error, Group0Result, Group0Sm, SNAPSHOT_VERSION_WITHOUT_INDEX_PLACES,
};
use teseladb_meta::indexes::{
    IndexCommand, IndexEntry, IndexState, SearchIndexKind, SearchIndexStatus,
};
use teseladb_meta::request::{REMEMBERED_PER_ORIGIN, RequestId, Requests};
use teseladb_multiraft::StateMachine;
use teseladb_types::{Decoder, Encoder, GroupId, NodeId};

fn ns(db: &str, coll: &str) -> Namespace {
    Namespace { db: db.into(), coll: coll.into() }
}

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: NodeAddrs { name: b"n1".to_vec(), cores: 4, ..NodeAddrs::default() },
        cert_fingerprint: [0; 32],
    };
    sm.apply_command(1, &boot);
    sm
}

/// Applies `cmd` as a replica gets it: from its encoding.
fn apply(sm: &mut Group0Sm, index: u64, cmd: Group0Command) -> Group0Result {
    let bytes = cmd.encode();
    assert_eq!(Group0Command::decode(&bytes), Ok(cmd.clone()));
    sm.apply_command(index, &cmd)
}

fn collection(sm: &mut Group0Sm, index: u64, n: Namespace) -> u32 {
    let create = CatalogCommand::Create {
        ns: n,
        uuid: [index as u8; 16],
        options: vec![],
        if_absent: false,
    };
    match apply(sm, index, Group0Command::Catalog(create)) {
        Group0Result::Collection(c) => c.id,
        other => panic!("create: {other:?}"),
    }
}

fn index(collection: u32, name: &str) -> Group0Command {
    Group0Command::Indexes(IndexCommand::Create {
        collection,
        name: name.into(),
        spec: vec![1, 2, 3],
        buckets: 4,
    })
}

fn restored(sm: &Group0Sm) -> Group0Sm {
    let mut back = Group0Sm::default();
    back.restore(&sm.snapshot()).expect("restore");
    back
}

#[test]
fn index_identifiers_are_never_reused_and_follow_their_collection() {
    let mut sm = cluster();
    let a = collection(&mut sm, 2, ns("db", "a"));
    let Group0Result::Index(first) = apply(&mut sm, 3, index(a, "x_1")) else { panic!("index") };
    assert_eq!(
        first,
        IndexEntry {
            id: 1,
            collection: a,
            name: "x_1".into(),
            spec: vec![1, 2, 3],
            buckets: 4,
            state: IndexState::DeleteOnly
        }
    );
    // An index is created delete-only, and each state survives a snapshot (009 FR-006).
    for state in [IndexState::Building, IndexState::DeleteOnly] {
        sm.indexes.indexes.get_mut(&1).expect("index").state = state;
        assert_eq!(restored(&sm).indexes.indexes[&1].state, state);
    }
    // The same name on the same collection is taken; on another collection it is not.
    assert_eq!(apply(&mut sm, 4, index(a, "x_1")), Group0Result::Err(Group0Error::IndexExists));
    let b = collection(&mut sm, 5, ns("db", "b"));
    let Group0Result::Index(other) = apply(&mut sm, 6, index(b, "x_1")) else { panic!("b") };
    assert_eq!(other.id, 2);
    assert_eq!(
        apply(&mut sm, 7, index(99, "y")),
        Group0Result::Err(Group0Error::NamespaceNotFound)
    );
    let ready = IndexCommand::SetState { id: 1, state: IndexState::Ready };
    assert_eq!(apply(&mut sm, 8, Group0Command::Indexes(ready)), Group0Result::Ok);
    assert_eq!(sm.indexes.indexes[&1].state, IndexState::Ready);
    let missing = IndexCommand::SetState { id: 9, state: IndexState::Dropping };
    assert_eq!(
        apply(&mut sm, 9, Group0Command::Indexes(missing)),
        Group0Result::Err(Group0Error::IndexNotFound)
    );
    assert_eq!(
        apply(&mut sm, 10, Group0Command::Indexes(IndexCommand::Drop { id: 1 })),
        Group0Result::Ok
    );
    assert_eq!(
        apply(&mut sm, 11, Group0Command::Indexes(IndexCommand::Drop { id: 1 })),
        Group0Result::Err(Group0Error::IndexNotFound)
    );
    // A new index after a drop gets a new identifier.
    let Group0Result::Index(again) = apply(&mut sm, 12, index(a, "x_1")) else { panic!("again") };
    assert_eq!(again.id, 3);
    // A rename keeps the collection identifier, so its indexes stay.
    let rename = CatalogCommand::Rename {
        from: ns("db", "a"),
        to: ns("db", "c"),
        drop_target: false,
        uuid: [0; 16],
    };
    assert_eq!(apply(&mut sm, 13, Group0Command::Catalog(rename)), Group0Result::Ok);
    assert_eq!(sm.indexes.of(a).map(|i| i.id).collect::<Vec<_>>(), [3]);
    // Dropping keeps them until the documents are purged.
    apply(&mut sm, 14, Group0Command::Catalog(CatalogCommand::Drop { ns: ns("db", "c") }));
    assert_eq!(sm.indexes.of(a).count(), 1);
    apply(&mut sm, 15, Group0Command::Catalog(CatalogCommand::Purged { id: a }));
    assert_eq!(sm.indexes.of(a).count(), 0);
    assert_eq!(sm.indexes.of(b).count(), 1);
    assert_eq!(restored(&sm), sm);
}

#[test]
fn search_indexes_have_a_name_a_type_a_definition_and_a_status() {
    let mut sm = cluster();
    let a = collection(&mut sm, 2, ns("db", "a"));
    let create = |name: &str, kind| {
        Group0Command::Indexes(IndexCommand::CreateSearch {
            id: [name.len() as u8; 12],
            at_ms: 1_000,
            collection: a,
            name: name.into(),
            kind,
            definition: vec![9],
        })
    };
    assert_eq!(apply(&mut sm, 3, create("default", SearchIndexKind::Search)), Group0Result::Ok);
    assert_eq!(apply(&mut sm, 4, create("vec", SearchIndexKind::VectorSearch)), Group0Result::Ok);
    assert_eq!(
        apply(&mut sm, 5, create("default", SearchIndexKind::Search)),
        Group0Result::Err(Group0Error::IndexExists)
    );
    let missing = Group0Command::Indexes(IndexCommand::CreateSearch {
        id: [0; 12],
        at_ms: 0,
        collection: 77,
        name: "s".into(),
        kind: SearchIndexKind::Search,
        definition: vec![],
    });
    assert_eq!(apply(&mut sm, 6, missing), Group0Result::Err(Group0Error::NamespaceNotFound));
    for (i, status) in [
        SearchIndexStatus::Building,
        SearchIndexStatus::Ready,
        SearchIndexStatus::Failed,
        SearchIndexStatus::Stale,
        SearchIndexStatus::Deleting,
    ]
    .into_iter()
    .enumerate()
    {
        let set = IndexCommand::SetSearchStatus { collection: a, name: "vec".into(), status };
        assert_eq!(apply(&mut sm, 7 + i as u64, Group0Command::Indexes(set)), Group0Result::Ok);
        assert_eq!(sm.indexes.search[&(a, "vec".to_owned())].status, status);
        assert_eq!(restored(&sm), sm);
    }
    let vec = &sm.indexes.search[&(a, "vec".to_owned())];
    assert_eq!((vec.id, vec.version, vec.defined_ms), ([3; 12], 0, 1_000));
    let update = IndexCommand::UpdateSearch {
        at_ms: 2_000,
        collection: a,
        name: "vec".into(),
        definition: vec![1, 1],
    };
    assert_eq!(apply(&mut sm, 20, Group0Command::Indexes(update)), Group0Result::Ok);
    let vec = &sm.indexes.search[&(a, "vec".to_owned())];
    assert_eq!(
        (vec.definition.as_slice(), vec.status, vec.version, vec.defined_ms),
        (&[1u8, 1][..], SearchIndexStatus::Pending, 1, 2_000)
    );
    assert_eq!(restored(&sm), sm);
    let names: Vec<&str> = sm.indexes.search_of(a).map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["default", "vec"]);
    let drop = |name: &str| {
        Group0Command::Indexes(IndexCommand::DropSearch { collection: a, name: name.into() })
    };
    assert_eq!(apply(&mut sm, 21, drop("vec")), Group0Result::Ok);
    assert_eq!(apply(&mut sm, 22, drop("vec")), Group0Result::Err(Group0Error::IndexNotFound));
    let update = IndexCommand::UpdateSearch {
        at_ms: 0,
        collection: a,
        name: "vec".into(),
        definition: vec![],
    };
    assert_eq!(
        apply(&mut sm, 23, Group0Command::Indexes(update)),
        Group0Result::Err(Group0Error::IndexNotFound)
    );
    apply(&mut sm, 24, Group0Command::Catalog(CatalogCommand::Purged { id: a }));
    assert_eq!(sm.indexes.search_of(a).count(), 0);
}

fn idem(origin: u64, id: u64, cmd: Group0Command) -> Group0Command {
    Group0Command::Idempotent { req: RequestId { origin: NodeId(origin), id }, cmd: Box::new(cmd) }
}

#[test]
fn a_retried_request_gets_its_first_result_without_applying_again() {
    let mut sm = cluster();
    let create = Group0Command::Catalog(CatalogCommand::Create {
        ns: ns("db", "a"),
        uuid: [1; 16],
        options: vec![],
        if_absent: false,
    });
    let first = apply(&mut sm, 2, idem(1, 10, create.clone()));
    let Group0Result::Collection(c) = first.clone() else { panic!("create") };
    // The retry of the same request answers the creation, not NamespaceExists.
    assert_eq!(apply(&mut sm, 3, idem(1, 10, create.clone())), first);
    assert_eq!(sm.catalog.last_collection, 1);
    // Another request, or the same number from another node, is applied.
    assert_eq!(
        apply(&mut sm, 4, idem(1, 11, create.clone())),
        Group0Result::Err(Group0Error::NamespaceExists)
    );
    assert_eq!(
        apply(&mut sm, 5, idem(2, 10, create)),
        Group0Result::Err(Group0Error::NamespaceExists)
    );
    // The table travels in the snapshot: a restored replica answers the retry the same way.
    let mut back = restored(&sm);
    assert_eq!(back, sm);
    let retry = idem(1, 10, Group0Command::Catalog(CatalogCommand::Drop { ns: ns("db", "a") }));
    assert_eq!(back.apply_command(6, &retry), Group0Result::Collection(c));
    assert!(back.catalog.get(&ns("db", "a")).is_some());
    let Group0Result::Index(i) = apply(&mut sm, 7, idem(3, 1, index(1, "k"))) else { panic!("i") };
    assert_eq!(
        restored(&sm).requests.get(RequestId { origin: NodeId(3), id: 1 }),
        Some(&Group0Result::Index(i))
    );
}

#[test]
fn each_origin_remembers_its_latest_results() {
    let mut t = Requests::default();
    let req = |id| RequestId { origin: NodeId(4), id };
    for id in 0..=REMEMBERED_PER_ORIGIN as u64 {
        t.record(req(id), Group0Result::Ok);
    }
    assert_eq!(t.get(req(0)), None);
    assert_eq!(t.get(req(1)), Some(&Group0Result::Ok));
    assert_eq!(t.get(req(REMEMBERED_PER_ORIGIN as u64)), Some(&Group0Result::Ok));
    assert_eq!(t.get(RequestId { origin: NodeId(5), id: 1 }), None);
}

#[test]
fn every_result_and_error_travels_in_the_snapshot() {
    let errors = [
        Group0Error::AlreadyBootstrapped,
        Group0Error::NotBootstrapped,
        Group0Error::NameOrAddrTaken,
        Group0Error::NoSuchNode,
        Group0Error::NoSuchGroup,
        Group0Error::BadReplicas,
        Group0Error::BadBounds,
        Group0Error::RangeBusy,
        Group0Error::BadSplit,
        Group0Error::NamespaceExists,
        Group0Error::NamespaceNotFound,
        Group0Error::IndexExists,
        Group0Error::IndexNotFound,
    ];
    let mut results: Vec<Group0Result> = errors.iter().map(|e| Group0Result::Err(*e)).collect();
    results.extend([
        Group0Result::Ok,
        Group0Result::Node(NodeId(3)),
        Group0Result::Range(GroupId(8)),
        Group0Result::Collection(teseladb_meta::catalog::CollectionEntry {
            id: 4,
            uuid: [2; 16],
            options: vec![5, 0, 0, 0, 0],
            created_index: 9,
            tenant: 0,
        }),
        Group0Result::Index(IndexEntry {
            id: 2,
            collection: 4,
            name: "a_1".into(),
            spec: vec![7],
            buckets: 0,
            state: IndexState::DeleteOnly,
        }),
    ]);
    let mut t = Requests::default();
    for (id, r) in results.iter().enumerate() {
        t.record(RequestId { origin: NodeId(1), id: id as u64 }, r.clone());
    }
    let mut e = Encoder::new();
    t.encode(&mut e);
    let bytes = e.finish();
    let back = Requests::decode(&mut Decoder::new(&bytes)).expect("decode");
    assert_eq!(back, t);
    for (id, r) in results.iter().enumerate() {
        assert_eq!(back.get(RequestId { origin: NodeId(1), id: id as u64 }), Some(r));
    }
}

#[test]
fn malformed_entries_do_not_decode() {
    let good = idem(1, 2, index(1, "a")).encode();
    // An idempotent command never wraps another.
    let mut nested = Encoder::new();
    nested.u16(42).u64(1).u64(2).raw(&good);
    assert!(Group0Command::decode(&nested.finish()).is_err());
    // An unknown subtype, state, kind or status.
    for body in
        [vec![7u8], vec![1, 0, 0, 0, 1, 9], vec![3, 0, 0, 0, 1, 0, 9], vec![5, 0, 0, 0, 1, 0, 9]]
    {
        let mut e = Encoder::new();
        e.u16(41).raw(&body);
        assert!(Group0Command::decode(&e.finish()).is_err(), "{body:?}");
    }
    // Snapshots: an unknown result tag and an unknown error code.
    for bytes in [
        vec![1u8, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 9],
        vec![1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 5, 99],
    ] {
        let mut d = Decoder::new(&bytes);
        assert!(Requests::decode(&mut d).is_err(), "{bytes:?}");
    }
    // An index result whose body is not a creation.
    let mut e = Encoder::new();
    e.var(1).u64(1).var(1).u64(0).u8(4).u32(1);
    IndexCommand::Drop { id: 1 }.encode(&mut e);
    assert!(Requests::decode(&mut Decoder::new(&e.finish())).is_err());
    // A snapshot of the previous format is refused.
    let mut snap = cluster().snapshot();
    snap[0] = 2;
    assert!(Group0Sm::default().restore(&snap).is_err());
}

/// A deterministic sequence for the descriptor tests.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

fn key(n: u64) -> Vec<u8> {
    vec![1, (n >> 8) as u8, n as u8]
}

#[test]
fn a_thousand_ranges_are_found_by_key() {
    let mut sm = cluster();
    // One range over the whole space, split 999 times at keys in random order.
    let whole =
        Group0Command::CreateRange { start: vec![1], end: vec![2], replicas: vec![NodeId(1)] };
    assert_eq!(apply(&mut sm, 2, whole), Group0Result::Range(GroupId(1)));
    let mut rng = Lcg(42);
    let mut cuts: Vec<u64> = (1..1000).map(|i| i * 50).collect();
    for i in (1..cuts.len()).rev() {
        cuts.swap(i, (rng.next() % (i as u64 + 1)) as usize);
    }
    let mut at = 3;
    for c in &cuts {
        let k = key(*c);
        let g = sm.range_for(&k).expect("covered").group;
        let intent = apply(&mut sm, at, Group0Command::SplitRange { group: g, at: k.clone() });
        assert!(matches!(intent, Group0Result::Range(_)));
        let done = Group0Command::CompleteSplit { group: g, at: k, at_index: at };
        assert!(matches!(apply(&mut sm, at + 1, done), Group0Result::Range(_)));
        at += 2;
    }
    assert_eq!((sm.ranges.len(), sm.range_index.len()), (1000, 1000));
    let back = restored(&sm);
    assert_eq!(back, sm);
    for _ in 0..5000 {
        let k = key(rng.next() % 50_100);
        let linear = sm.ranges.values().find(|r| r.contains(&k)).map(|r| r.group);
        assert_eq!(sm.range_for(&k).map(|r| r.group), linear, "{k:?}");
        assert_eq!(back.range_for(&k).map(|r| r.group), linear);
    }
    // Keys outside every range.
    assert!(sm.range_for(&[0]).is_none());
    assert!(sm.range_for(&[2]).is_none());
    // The ranges of an interval, in key order and contiguous.
    let over = sm.ranges_over(&key(125), &key(410));
    let starts: Vec<Vec<u8>> = over.iter().map(|r| r.start.clone()).collect();
    assert_eq!(starts, [100, 150, 200, 250, 300, 350, 400].map(key));
    assert_eq!(sm.ranges_over(&key(150), &key(151)).len(), 1);
    assert_eq!(sm.ranges_over(&key(49_940), &[]).len(), 2);
    assert!(sm.ranges_over(&[0], &[0, 1]).is_empty());
}

#[test]
fn the_index_by_key_can_shrink() {
    let mut i = teseladb_meta::descriptors::RangeIndex::default();
    assert!(i.is_empty());
    i.insert(b"b", GroupId(2));
    i.insert(b"a", GroupId(1));
    assert_eq!(i.floor(b"ab"), Some(GroupId(1)));
    i.remove(b"a");
    assert_eq!((i.len(), i.floor(b"ab")), (1, None));
}

#[test]
fn a_split_names_its_new_range_at_the_intent_and_the_name_survives_a_snapshot() {
    let mut sm = cluster();
    let whole =
        Group0Command::CreateRange { start: vec![1], end: vec![2], replicas: vec![NodeId(1)] };
    apply(&mut sm, 2, whole);
    let intent = Group0Command::SplitRange { group: GroupId(1), at: vec![1, 5] };
    assert_eq!(apply(&mut sm, 3, intent), Group0Result::Range(GroupId(2)));
    // A range created meanwhile takes the next number.
    let other =
        Group0Command::CreateRange { start: vec![3], end: vec![4], replicas: vec![NodeId(1)] };
    assert_eq!(apply(&mut sm, 4, other), Group0Result::Range(GroupId(3)));
    let mut back = restored(&sm);
    assert_eq!(back.split_ids.get(&GroupId(1)), Some(&GroupId(2)));
    let done = Group0Command::CompleteSplit { group: GroupId(1), at: vec![1, 5], at_index: 9 };
    assert_eq!(back.apply_command(5, &done), Group0Result::Range(GroupId(2)));
    assert!(back.split_ids.is_empty());
    assert_eq!(back.range_for(&[1, 6]).map(|r| r.group), Some(GroupId(2)));
}

#[test]
fn an_index_specification_is_replaced_in_place() {
    // `collMod` of an index (009 FR-007): its options change, its identifier and state stay.
    let mut sm = cluster();
    let a = collection(&mut sm, 2, ns("db", "a"));
    let Group0Result::Index(i) = apply(&mut sm, 3, index(a, "x_1")) else { panic!("index") };
    let set = IndexCommand::SetSpec { id: i.id, spec: vec![9, 9] };
    assert_eq!(apply(&mut sm, 4, Group0Command::Indexes(set)), Group0Result::Ok);
    let now = &sm.indexes.indexes[&i.id];
    assert_eq!(
        (now.spec.as_slice(), now.state, now.name.as_str()),
        (&[9u8, 9][..], i.state, "x_1")
    );
    let missing = IndexCommand::SetSpec { id: 77, spec: vec![] };
    assert_eq!(
        apply(&mut sm, 5, Group0Command::Indexes(missing)),
        Group0Result::Err(Group0Error::IndexNotFound)
    );
    assert_eq!(restored(&sm), sm);
}

#[test]
fn a_rebuilt_index_takes_the_place_of_the_old_one() {
    // `collMod` to `unique: true` (009 FR-007): the unique index built under another name gets
    // the old one's name and is ready; the old one is dropped under the new one's former name.
    let mut sm = cluster();
    let a = collection(&mut sm, 2, ns("db", "a"));
    let Group0Result::Index(old) = apply(&mut sm, 3, index(a, "x_1")) else { panic!("index") };
    let ready = IndexCommand::SetState { id: old.id, state: IndexState::Ready };
    apply(&mut sm, 4, Group0Command::Indexes(ready));
    let Group0Result::Index(later) = apply(&mut sm, 41, index(a, "z_1")) else { panic!("index") };
    let Group0Result::Index(new) = apply(&mut sm, 5, index(a, "x_1 (unique)")) else {
        panic!("index")
    };
    let swap = IndexCommand::Swap { old: old.id, new: new.id };
    assert_eq!(apply(&mut sm, 6, Group0Command::Indexes(swap.clone())), Group0Result::Ok);
    let (o, n) = (&sm.indexes.indexes[&old.id], &sm.indexes.indexes[&new.id]);
    assert_eq!((o.name.as_str(), o.state), ("x_1 (unique)", IndexState::Dropping));
    assert_eq!((n.name.as_str(), n.state), ("x_1", IndexState::Ready));
    assert_eq!(restored(&sm), sm);
    // It is listed where the old one was, before an index created after the old one, also once
    // the old one is dropped and in a copy taken from the snapshot; a snapshot of the format
    // before the places lists by identifier.
    let order = |sm: &Group0Sm| sm.indexes.of(a).map(|i| i.id).collect::<Vec<_>>();
    assert_eq!(order(&sm), [old.id, new.id, later.id]);
    apply(&mut sm, 52, Group0Command::Indexes(IndexCommand::Drop { id: old.id }));
    assert_eq!(order(&sm), [new.id, later.id]);
    assert_eq!(order(&restored(&sm)), [new.id, later.id]);
    let mut old_format = Group0Sm::default();
    old_format.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_INDEX_PLACES)).unwrap();
    assert_eq!(order(&old_format), [later.id, new.id]);
    assert!(old_format.indexes.places.is_empty());
    let missing = IndexCommand::Swap { old: old.id, new: 77 };
    assert_eq!(
        apply(&mut sm, 7, Group0Command::Indexes(missing)),
        Group0Result::Err(Group0Error::IndexNotFound)
    );
    let itself = IndexCommand::Swap { old: old.id, new: old.id };
    assert_eq!(
        apply(&mut sm, 7, Group0Command::Indexes(itself)),
        Group0Result::Err(Group0Error::IndexNotFound)
    );
    // Indexes of two collections never swap.
    let b = collection(&mut sm, 8, ns("db", "b"));
    let Group0Result::Index(other) = apply(&mut sm, 9, index(b, "y_1")) else { panic!("index") };
    let across = IndexCommand::Swap { old: new.id, new: other.id };
    assert_eq!(
        apply(&mut sm, 10, Group0Command::Indexes(across)),
        Group0Result::Err(Group0Error::IndexNotFound)
    );
    // It goes through the log as it was written.
    let mut e = Encoder::new();
    swap.encode(&mut e);
    let bytes = e.finish();
    assert_eq!(IndexCommand::decode(&mut Decoder::new(&bytes)), Ok(swap));
}
