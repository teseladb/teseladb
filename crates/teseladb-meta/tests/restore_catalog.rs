//! 007 T034: `RestoreCatalog` (kind 50, plan «Copias de seguridad»): a restore puts the catalog
//! and the index definitions of a backup into Group 0 whole, with their collection and index
//! numbers (the keys of the copied data carry them), and only into a catalog without
//! collections or indexes.

use std::sync::Arc;

use teseladb_meta::catalog::{CatalogCommand, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs, kind};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_meta::indexes::IndexCommand;
use teseladb_bson::Document;

fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied + 1;
    sm.applied = index;
    sm.apply_command(index, &cmd)
}

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let node = NodeAddrs {
        name: b"n1".to_vec(),
        peer_addr: b"n1:7421".to_vec(),
        cores: 1,
        ..NodeAddrs::default()
    };
    run(
        &mut sm,
        Group0Command::Bootstrap {
            cluster_id: [4; 16],
            first_node: node,
            cert_fingerprint: [1; 32],
        },
    );
    sm
}

fn create(sm: &mut Group0Sm, db: &str, coll: &str) -> u32 {
    let ns = Namespace { db: db.into(), coll: coll.into() };
    let options = Document::new().to_bytes().unwrap();
    let cmd =
        CatalogCommand::Create { ns, uuid: [coll.len() as u8; 16], options, if_absent: false };
    match run(sm, Group0Command::Catalog(cmd)) {
        Group0Result::Collection(c) => c.id,
        other => panic!("{other:?}"),
    }
}

/// A source cluster with two collections, one of them dropped first so the numbers have a gap,
/// and an index.
fn source() -> Group0Sm {
    let mut sm = cluster();
    create(&mut sm, "app", "gone");
    let ns = Namespace { db: "app".into(), coll: "gone".into() };
    run(&mut sm, Group0Command::Catalog(CatalogCommand::Drop { ns }));
    let id = create(&mut sm, "app", "users");
    create(&mut sm, "other", "c");
    let spec = Document::new().with("key", Document::new().with("a", 1)).with("name", "a_1");
    let index = IndexCommand::Create {
        collection: id,
        name: "a_1".into(),
        spec: spec.to_bytes().unwrap(),
        buckets: 1,
    };
    assert!(matches!(run(&mut sm, Group0Command::Indexes(index)), Group0Result::Index(_)));
    sm
}

fn restore_of(sm: &Group0Sm) -> Group0Command {
    Group0Command::RestoreCatalog {
        catalog: Box::new((*sm.catalog).clone()),
        indexes: Box::new(sm.indexes.clone()),
    }
}

#[test]
fn the_command_round_trips_with_its_kind() {
    let cmd = restore_of(&source());
    assert_eq!(cmd.kind(), kind::RESTORE_CATALOG);
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd));
}

#[test]
fn an_empty_catalog_takes_the_backups_with_its_numbers() {
    let src = source();
    let mut dst = cluster();
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Ok);
    assert_eq!(dst.catalog, src.catalog);
    assert_eq!(dst.indexes, src.indexes);
    // A collection created afterwards takes a number past every restored one.
    let next = create(&mut dst, "app", "later");
    assert!(src.catalog.collections.values().all(|c| c.id < next), "{next}");
    // The restored state travels in the snapshot.
    let mut back = Group0Sm::default();
    teseladb_multiraft::StateMachine::restore(
        &mut back,
        &teseladb_multiraft::StateMachine::snapshot(&dst),
    )
    .unwrap();
    assert_eq!(back.catalog, dst.catalog);
}

#[test]
fn a_catalog_with_a_collection_or_an_index_refuses_it() {
    let src = source();
    let mut dst = cluster();
    create(&mut dst, "app", "mine");
    let before = Arc::clone(&dst.catalog);
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Err(Group0Error::NotEmpty));
    assert_eq!(dst.catalog, before);
    // Applied twice, the second finds the first one's collections.
    let mut dst = cluster();
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Ok);
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Err(Group0Error::NotEmpty));
}
