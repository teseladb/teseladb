//! 014 T017: the catalog changes change streams read (FR-014). Group 0 records each creation,
//! drop, rename and modification of a collection, each drop of a database and each index that
//! becomes ready, is dropped or changes, with the mark of its entry and its position in it; keeps
//! the last `MAX_BYTES` of them and carries them in its snapshot (version 17, which reads 16 with
//! none).

use teseladb_hlc::Hlc;
use teseladb_meta::catalog::{CatalogCommand, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::ddl_log::{DdlChange, DdlLog, MAX_BYTES};
use teseladb_meta::group0::{
    Group0Result, Group0Sm, SNAPSHOT_VERSION, SNAPSHOT_VERSION_WITHOUT_DDL,
};
use teseladb_meta::indexes::{IndexCommand, IndexState};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_types::{Decoder, Encoder, GroupId};

fn mark(index: u64) -> Hlc {
    Hlc { physical: 1_000 * index, logical: 0 }
}

fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied_index() + 1;
    let ctx = ApplyCtx { group: GroupId(0), index, term: 1, ts: mark(index) };
    sm.apply(&cmd.encode(), &ctx).unwrap()
}

fn ns(db: &str, coll: &str) -> Namespace {
    Namespace { db: db.into(), coll: coll.into() }
}

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let first_node = NodeAddrs {
        name: b"n1".to_vec(),
        peer_addr: b"n1:7421".to_vec(),
        http_addr: b"n1:8080".to_vec(),
        native_addr: b"n1:27017".to_vec(),
        cores: 1,
    };
    run(
        &mut sm,
        Group0Command::Bootstrap { cluster_id: [7; 16], first_node, cert_fingerprint: [1; 32] },
    );
    sm
}

fn catalog(sm: &mut Group0Sm, c: CatalogCommand) -> Group0Result {
    run(sm, Group0Command::Catalog(c))
}

fn create(sm: &mut Group0Sm, n: Namespace, uuid: u8) -> u32 {
    let cmd = CatalogCommand::Create {
        ns: n,
        uuid: [uuid; 16],
        options: vec![5, 0, 0, 0, 0],
        if_absent: true,
    };
    match catalog(sm, cmd) {
        Group0Result::Collection(c) => c.id,
        other => panic!("{other:?}"),
    }
}

fn kinds(log: &DdlLog) -> Vec<(u64, u32, &'static str)> {
    log.records
        .iter()
        .map(|r| {
            let k = match &r.change {
                DdlChange::Create { .. } => "create",
                DdlChange::Drop { .. } => "drop",
                DdlChange::Rename { .. } => "rename",
                DdlChange::Modify { .. } => "modify",
                DdlChange::DropDatabase { .. } => "dropDatabase",
                DdlChange::CreateIndex { .. } => "createIndexes",
                DdlChange::DropIndex { .. } => "dropIndexes",
                DdlChange::ModifyIndex { .. } => "modifyIndex",
            };
            (r.mark.physical / 1_000, r.op, k)
        })
        .collect()
}

#[test]
fn group0_records_each_catalog_change_with_its_mark_and_position() {
    let mut sm = cluster();
    let a = create(&mut sm, ns("db", "a"), 1); // index 2
    assert_eq!(create(&mut sm, ns("db", "a"), 9), a, "an implicit creation finds it"); // 3
    match &sm.ddl.records[0].change {
        DdlChange::Create { ns: n, collection } => {
            assert_eq!((n, collection.id, collection.uuid), (&ns("db", "a"), a, [1; 16]));
        }
        other => panic!("{other:?}"),
    }
    let spec = vec![5, 0, 0, 0, 0];
    let idx =
        IndexCommand::Create { collection: a, name: "x_1".into(), spec: spec.clone(), buckets: 0 };
    run(&mut sm, Group0Command::Indexes(idx)); // 4: not ready yet
    let id = *sm.indexes.indexes.keys().next().unwrap();
    let ready = |state| Group0Command::Indexes(IndexCommand::SetState { id, state });
    run(&mut sm, ready(IndexState::Ready)); // 5
    run(&mut sm, ready(IndexState::Ready)); // 6: already ready
    let new_spec = vec![6, 0, 0, 0, 0, 0];
    run(&mut sm, Group0Command::Indexes(IndexCommand::SetSpec { id, spec: new_spec.clone() })); // 7
    run(&mut sm, ready(IndexState::Dropping)); // 8
    run(&mut sm, Group0Command::Indexes(IndexCommand::Drop { id })); // 9: no longer ready
    let modify =
        CatalogCommand::Modify { ns: ns("db", "a"), options: vec![1], description: vec![2] };
    catalog(&mut sm, modify); // 10
    let b = create(&mut sm, ns("db", "b"), 2); // 11
    let rename = CatalogCommand::Rename {
        from: ns("db", "a"),
        to: ns("db", "b"),
        drop_target: true,
        uuid: [0; 16],
    };
    catalog(&mut sm, rename); // 12
    create(&mut sm, ns("db", "c"), 3); // 13
    catalog(&mut sm, CatalogCommand::Drop { ns: ns("db", "c") }); // 14
    catalog(&mut sm, CatalogCommand::Drop { ns: ns("db", "c") }); // 15: refused
    catalog(&mut sm, CatalogCommand::Purged { id: b }); // 16
    catalog(&mut sm, CatalogCommand::DropDatabase { db: "db".into() }); // 17
    catalog(&mut sm, CatalogCommand::DropDatabase { db: "none".into() }); // 18
    catalog(&mut sm, CatalogCommand::CreateDatabase { db: "e".into() }); // 19
    catalog(&mut sm, CatalogCommand::DropDatabase { db: "e".into() }); // 20
    assert_eq!(
        kinds(&sm.ddl),
        vec![
            (2, 0, "create"),
            (5, 0, "createIndexes"),
            (7, 0, "modifyIndex"),
            (8, 0, "dropIndexes"),
            (10, 0, "modify"),
            (11, 0, "create"),
            (12, 0, "rename"),
            (13, 0, "create"),
            (14, 0, "drop"),
            (17, 0, "drop"),
            (17, 1, "dropDatabase"),
            (20, 0, "dropDatabase"),
        ]
    );
    let find = |m: u64| sm.ddl.records.iter().find(|r| r.mark == mark(m)).unwrap().change.clone();
    assert!(
        matches!(find(7), DdlChange::ModifyIndex { before, after, .. } if before == spec && after == new_spec)
    );
    assert!(
        matches!(find(10), DdlChange::Modify { description, collection, .. } if description == vec![2] && collection.options == spec)
    );
    assert!(matches!(find(12), DdlChange::Rename { target: Some(t), .. } if t.id == b));
    assert_eq!(sm.ddl.after(mark(13)).len(), 4);
    assert_eq!(sm.ddl.after(mark(20)), Vec::new());
}

#[test]
fn an_index_change_of_a_dropped_collection_and_a_command_without_mark_record_nothing() {
    let mut sm = cluster();
    let a = create(&mut sm, ns("db", "a"), 1);
    let idx = IndexCommand::Create {
        collection: a,
        name: "x_1".into(),
        spec: vec![5, 0, 0, 0, 0],
        buckets: 0,
    };
    run(&mut sm, Group0Command::Indexes(idx));
    let id = *sm.indexes.indexes.keys().next().unwrap();
    run(&mut sm, Group0Command::Indexes(IndexCommand::SetState { id, state: IndexState::Ready }));
    catalog(&mut sm, CatalogCommand::Drop { ns: ns("db", "a") });
    let before = sm.ddl.records.len();
    run(
        &mut sm,
        Group0Command::Indexes(IndexCommand::SetState { id, state: IndexState::Dropping }),
    );
    assert_eq!(sm.ddl.records.len(), before);
    sm.apply_command(99, &Group0Command::Catalog(CatalogCommand::Drop { ns: ns("db", "zz") }));
    let n = ns("db", "q");
    let cmd = CatalogCommand::Create { ns: n, uuid: [3; 16], options: vec![], if_absent: false };
    sm.apply_command(100, &Group0Command::Catalog(cmd));
    assert_eq!(sm.ddl.records.len(), before, "no mark, no record");
}

#[test]
fn the_log_keeps_its_last_bytes_and_says_up_to_where_it_is_incomplete() {
    let mut log = DdlLog::default();
    let big = vec![0u8; 1024 * 1024];
    let change = |i: u8| DdlChange::Modify {
        ns: ns("db", "a"),
        collection: teseladb_meta::catalog::CollectionEntry {
            id: 1,
            uuid: [i; 16],
            options: Vec::new(),
            created_index: 1,
            tenant: 0,
        },
        description: big.clone(),
    };
    for i in 0..20u8 {
        log.record(mark(u64::from(i) + 1), vec![change(i)]);
    }
    assert!(log.bytes() <= MAX_BYTES);
    assert_eq!(log.floor, mark(5), "the first five went");
    assert_eq!(log.records.front().unwrap().mark, mark(6));
    let mut e = Encoder::new();
    log.encode(&mut e);
    let bytes = e.finish();
    let back = DdlLog::decode(&mut Decoder::new(&bytes)).unwrap();
    assert_eq!((back.bytes(), &back), (log.bytes(), &log));
    let mut bad = bytes.clone();
    // The kind byte of the first record: after the floor (12), the applied mark (1 + 12), the
    // count (1), the mark (12) and the position (4).
    bad[12 + 13 + 1 + 12 + 4] = 99;
    assert!(DdlLog::decode(&mut Decoder::new(&bad)).is_err());
}

#[test]
fn every_change_reads_back_and_the_snapshot_carries_them() {
    let mut sm = cluster();
    let a = create(&mut sm, ns("db", "a"), 1);
    let idx = IndexCommand::Create {
        collection: a,
        name: "x_1".into(),
        spec: vec![5, 0, 0, 0, 0],
        buckets: 0,
    };
    run(&mut sm, Group0Command::Indexes(idx));
    let id = *sm.indexes.indexes.keys().next().unwrap();
    run(&mut sm, Group0Command::Indexes(IndexCommand::SetState { id, state: IndexState::Ready }));
    run(&mut sm, Group0Command::Indexes(IndexCommand::SetSpec { id, spec: vec![5, 0, 0, 0, 0] }));
    run(&mut sm, Group0Command::Indexes(IndexCommand::Drop { id }));
    catalog(
        &mut sm,
        CatalogCommand::Modify { ns: ns("db", "a"), options: vec![], description: vec![] },
    );
    let rename = CatalogCommand::Rename {
        from: ns("db", "a"),
        to: ns("db", "b"),
        drop_target: false,
        uuid: [0; 16],
    };
    catalog(&mut sm, rename);
    catalog(&mut sm, CatalogCommand::DropDatabase { db: "db".into() });
    assert_eq!(sm.ddl.records.len(), 8);
    assert_eq!(SNAPSHOT_VERSION, SNAPSHOT_VERSION_WITHOUT_DDL + 6);
    let mut back = Group0Sm::default();
    back.restore(&sm.snapshot()).unwrap();
    assert_eq!(back.ddl, sm.ddl);
    assert_eq!(back.ddl.applied_mark, Some(mark(sm.applied)));
    let mut old = Group0Sm::default();
    old.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_DDL)).unwrap();
    assert!(old.ddl.records.is_empty());
    assert_eq!(old.catalog, sm.catalog);
}

#[test]
fn a_modify_entry_without_description_reads_back_with_an_empty_one() {
    let cmd =
        CatalogCommand::Modify { ns: ns("db", "a"), options: vec![1, 2], description: vec![3] };
    let mut e = Encoder::new();
    cmd.encode(&mut e);
    let bytes = e.finish();
    assert_eq!(CatalogCommand::decode(&mut Decoder::new(&bytes)).unwrap(), cmd);
    // The format before the description: kind, namespace and options.
    let mut tail = Encoder::new();
    tail.bytes(&[3]);
    let old = &bytes[..bytes.len() - tail.len()];
    let read = CatalogCommand::decode(&mut Decoder::new(old)).unwrap();
    assert_eq!(
        read,
        CatalogCommand::Modify { ns: ns("db", "a"), options: vec![1, 2], description: vec![] }
    );
}

#[test]
fn a_dropped_database_drops_its_views_without_a_drop() {
    let mut sm = cluster();
    create(&mut sm, ns("db", "a"), 1);
    let mut view = vec![];
    let mut e = Encoder::new();
    // { viewOn: "a" } in BSON.
    e.raw(&[19, 0, 0, 0, 2]).raw(b"viewOn\0").raw(&[2, 0, 0, 0]).raw(b"a\0").raw(&[0]);
    view.extend_from_slice(&e.finish());
    let cmd = CatalogCommand::Create {
        ns: ns("db", "v"),
        uuid: [2; 16],
        options: view,
        if_absent: false,
    };
    catalog(&mut sm, cmd);
    catalog(&mut sm, CatalogCommand::DropDatabase { db: "db".into() });
    let last: Vec<_> = kinds(&sm.ddl).into_iter().skip(2).collect();
    assert_eq!(last, vec![(4, 0, "drop"), (4, 1, "dropDatabase")]);
}
