//! The catalog in Group 0 (006 FR-027, 007 contract `catalog.md`): collections with identifiers
//! that are never reused, their options, renames and drops, applied the same way on every replica.

use teseladb_meta::catalog::{CatalogCommand, CatalogError, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_multiraft::StateMachine;

fn ns(db: &str, coll: &str) -> Namespace {
    Namespace { db: db.into(), coll: coll.into() }
}

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: NodeAddrs { name: b"n1".to_vec(), cores: 2, ..NodeAddrs::default() },
        cert_fingerprint: [0; 32],
    };
    sm.apply_command(1, &boot);
    sm
}

fn apply(sm: &mut Group0Sm, index: u64, c: CatalogCommand) -> Group0Result {
    let cmd = Group0Command::Catalog(c);
    // Every command reads back from its encoding, as a replica gets it.
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd.clone()));
    sm.apply_command(index, &cmd)
}

fn create(n: Namespace, uuid: u8, if_absent: bool) -> CatalogCommand {
    CatalogCommand::Create { ns: n, uuid: [uuid; 16], options: vec![5, 0, 0, 0, 0], if_absent }
}

#[test]
fn collections_get_identifiers_that_are_never_reused() {
    let mut sm = cluster();
    let Group0Result::Collection(a) = apply(&mut sm, 2, create(ns("db", "a"), 1, false)) else {
        panic!("create")
    };
    assert_eq!((a.id, a.uuid, a.created_index), (1, [1; 16], 2));
    assert_eq!(
        apply(&mut sm, 3, create(ns("db", "a"), 2, false)),
        err(Group0Error::NamespaceExists)
    );
    // An implicit creation finds the one that exists.
    assert_eq!(
        apply(&mut sm, 4, create(ns("db", "a"), 3, true)),
        Group0Result::Collection(a.clone())
    );
    assert_eq!(apply(&mut sm, 5, CatalogCommand::Drop { ns: ns("db", "a") }), Group0Result::Ok);
    assert!(sm.catalog.dropped.contains(&1));
    let Group0Result::Collection(b) = apply(&mut sm, 6, create(ns("db", "a"), 4, false)) else {
        panic!("create again")
    };
    assert_eq!(b.id, 2);
    assert_eq!(
        apply(&mut sm, 7, CatalogCommand::Drop { ns: ns("db", "zz") }),
        err(Group0Error::NamespaceNotFound)
    );
    assert_eq!(apply(&mut sm, 8, CatalogCommand::Purged { id: 1 }), Group0Result::Ok);
    assert!(sm.catalog.dropped.is_empty());
    assert_eq!(sm.catalog.get(&ns("db", "a")).map(|c| c.id), Some(2));
}

fn err(e: Group0Error) -> Group0Result {
    Group0Result::Err(e)
}

#[test]
fn renames_keep_the_identifier_and_follow_refdb_rules() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "a"), 1, false));
    apply(&mut sm, 3, create(ns("db", "b"), 2, false));
    let rename = |to: Namespace, drop_target| CatalogCommand::Rename {
        from: ns("db", "a"),
        to,
        drop_target,
        uuid: [9; 16],
    };
    assert_eq!(apply(&mut sm, 4, rename(ns("db", "b"), false)), err(Group0Error::NamespaceExists));
    assert_eq!(apply(&mut sm, 5, rename(ns("db", "b"), true)), Group0Result::Ok);
    let moved = sm.catalog.get(&ns("db", "b")).cloned().expect("renamed");
    // Inside one database the UUID stays; the target's collection is dropped.
    assert_eq!((moved.id, moved.uuid), (1, [1; 16]));
    assert!(sm.catalog.dropped.contains(&2));
    assert!(sm.catalog.get(&ns("db", "a")).is_none());
    let to_other = CatalogCommand::Rename {
        from: ns("db", "b"),
        to: ns("other", "c"),
        drop_target: false,
        uuid: [9; 16],
    };
    assert_eq!(apply(&mut sm, 6, to_other.clone()), Group0Result::Ok);
    // To another database it gets the new UUID.
    assert_eq!(sm.catalog.get(&ns("other", "c")).map(|c| (c.id, c.uuid)), Some((1, [9; 16])));
    assert_eq!(apply(&mut sm, 7, to_other), err(Group0Error::NamespaceNotFound));
}

#[test]
fn options_change_and_databases_drop_whole() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "a"), 1, false));
    apply(&mut sm, 3, create(ns("db", "b"), 2, false));
    apply(&mut sm, 4, create(ns("keep", "c"), 3, false));
    let modify = CatalogCommand::Modify { ns: ns("db", "a"), options: vec![1, 2, 3] };
    assert_eq!(apply(&mut sm, 5, modify), Group0Result::Ok);
    assert_eq!(sm.catalog.get(&ns("db", "a")).map(|c| c.options.clone()), Some(vec![1, 2, 3]));
    let missing = CatalogCommand::Modify { ns: ns("db", "zz"), options: vec![] };
    assert_eq!(apply(&mut sm, 6, missing), err(Group0Error::NamespaceNotFound));
    assert_eq!(sm.catalog.databases(), vec!["db".to_owned(), "keep".to_owned()]);
    assert_eq!(sm.catalog.in_db("db").len(), 2);
    assert_eq!(
        apply(&mut sm, 7, CatalogCommand::DropDatabase { db: "db".into() }),
        Group0Result::Ok
    );
    assert_eq!(sm.catalog.databases(), vec!["keep".to_owned()]);
    assert_eq!(sm.catalog.dropped.iter().copied().collect::<Vec<_>>(), vec![1, 2]);
    // Dropping a database that has nothing is not an error, as in RefDb.
    assert_eq!(
        apply(&mut sm, 8, CatalogCommand::DropDatabase { db: "db".into() }),
        Group0Result::Ok
    );
}

#[test]
fn a_database_outlives_its_last_collection_until_drop_database() {
    // D-077: `refrouter` 9.0.4 keeps the database in `config.databases` after its last collection
    // is dropped or renamed away, and lists it until `dropDatabase`.
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "a"), 1, false));
    apply(&mut sm, 3, CatalogCommand::Drop { ns: ns("db", "a") });
    assert_eq!(sm.catalog.databases(), vec!["db".to_owned()]);
    assert!(sm.catalog.in_db("db").is_empty());
    apply(&mut sm, 4, create(ns("src", "c"), 2, false));
    let away = CatalogCommand::Rename {
        from: ns("src", "c"),
        to: ns("dst", "c"),
        drop_target: false,
        uuid: [3; 16],
    };
    assert_eq!(apply(&mut sm, 5, away), Group0Result::Ok);
    let all = ["db", "dst", "src"].map(str::to_owned).to_vec();
    assert_eq!(sm.catalog.databases(), all);
    let by_creation = ["db", "src", "dst"].map(str::to_owned).to_vec();
    assert_eq!(sm.catalog.databases_by_creation(), by_creation);
    // A failed creation leaves no database behind.
    apply(&mut sm, 6, create(ns("dst", "c"), 4, false));
    assert_eq!(sm.catalog.databases(), all);
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).expect("restore");
    assert_eq!(copy.catalog.databases(), all);
    assert_eq!(copy.catalog.databases_by_creation(), by_creation);
    assert_eq!(
        apply(&mut sm, 7, CatalogCommand::DropDatabase { db: "src".into() }),
        Group0Result::Ok
    );
    assert_eq!(
        apply(&mut sm, 8, CatalogCommand::DropDatabase { db: "db".into() }),
        Group0Result::Ok
    );
    assert_eq!(sm.catalog.databases(), vec!["dst".to_owned()]);
}

#[test]
fn databases_that_differ_only_in_case_are_refused() {
    // 13297 `DatabaseDifferCase` of RefDb: a new database may not differ from another only in
    // ASCII case; the same name, or another collection of it, is fine.
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "a"), 1, false));
    let clash = err(Group0Error::DatabaseDifferCase);
    assert_eq!(apply(&mut sm, 3, create(ns("DB", "a"), 2, true)), clash);
    let away = |to: &str| CatalogCommand::Rename {
        from: ns("db", "a"),
        to: ns(to, "a"),
        drop_target: false,
        uuid: [3; 16],
    };
    assert_eq!(apply(&mut sm, 4, away("dB")), clash);
    assert_eq!(apply(&mut sm, 5, CatalogCommand::CreateDatabase { db: "Db".into() }), clash);
    assert!(matches!(
        apply(&mut sm, 6, create(ns("db", "b"), 4, false)),
        Group0Result::Collection(_)
    ));
    assert_eq!(
        apply(&mut sm, 7, CatalogCommand::CreateDatabase { db: "db".into() }),
        Group0Result::Ok
    );
    // A write to a database that has no collection yet records it (D-077).
    assert_eq!(
        apply(&mut sm, 8, CatalogCommand::CreateDatabase { db: "w".into() }),
        Group0Result::Ok
    );
    assert_eq!(sm.catalog.databases(), vec!["db".to_owned(), "w".to_owned()]);
    assert!(sm.catalog.in_db("w").is_empty());
    assert_eq!(apply(&mut sm, 9, create(ns("W", "x"), 5, true)), clash);
    // Once the other database is dropped, the name is free.
    apply(&mut sm, 10, CatalogCommand::DropDatabase { db: "db".into() });
    assert_eq!(apply(&mut sm, 11, away("DB")), err(Group0Error::NamespaceNotFound));
    assert!(matches!(
        apply(&mut sm, 12, create(ns("DB", "a"), 6, false)),
        Group0Result::Collection(_)
    ));
    // A database added again goes last, as a new document of `config.databases`.
    assert_eq!(sm.catalog.databases_by_creation(), vec!["w".to_owned(), "DB".to_owned()]);
}

#[test]
fn the_catalog_travels_in_the_snapshot() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "a"), 1, false));
    apply(&mut sm, 3, create(ns("db", "b"), 2, false));
    apply(&mut sm, 4, CatalogCommand::Drop { ns: ns("db", "b") });
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).expect("restore");
    assert_eq!(copy, sm);
    assert_eq!(copy.catalog.last_collection, 2);
}

#[test]
fn malformed_catalog_entries_do_not_decode() {
    let good = Group0Command::Catalog(create(ns("db", "a"), 1, true)).encode();
    for cut in 2..good.len() {
        assert!(Group0Command::decode(&good[..cut]).is_err(), "cut at {cut}");
    }
    let mut bad = good.clone();
    bad[2] = 99;
    assert!(Group0Command::decode(&bad).is_err());
    let mut not_utf8 =
        Group0Command::Catalog(CatalogCommand::DropDatabase { db: "d".into() }).encode();
    let last = not_utf8.len() - 1;
    not_utf8[last] = 0xff;
    assert!(Group0Command::decode(&not_utf8).is_err());
    let _ = CatalogError::NamespaceExists;
}

/// A snapshot of the previous format (10) still restores, its databases without a mark; the
/// current one keeps the mark of the entry that added each database (008 FR-012).
#[test]
fn a_snapshot_of_the_previous_format_restores_without_marks() {
    use teseladb_meta::group0::SNAPSHOT_VERSION_WITHOUT_DB_MARKS;
    use teseladb_multiraft::ApplyCtx;
    let mut sm = cluster();
    let ns = ns("d", "c");
    let create = Group0Command::Catalog(CatalogCommand::Create {
        ns,
        uuid: [1; 16],
        options: vec![],
        if_absent: false,
    });
    let mark = teseladb_hlc::Hlc { physical: 50, logical: 3 };
    let ctx = ApplyCtx { group: teseladb_types::GroupId(0), index: 2, term: 1, ts: mark };
    sm.apply(&create.encode(), &ctx).expect("applied");
    assert_eq!(sm.catalog.db_marks.get("d"), Some(&mark));
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).expect("restore");
    assert_eq!(copy.catalog.db_marks, sm.catalog.db_marks);
    let mut old = Group0Sm::default();
    old.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_DB_MARKS)).expect("restore");
    assert!(old.catalog.db_marks.is_empty());
    assert_eq!(old.catalog.collections, sm.catalog.collections);
    assert!(old.restore(&sm.encode_snapshot_as(9)).is_err());
}
