//! The dictionaries of field names in the catalog (003 FR-022, T036): a collection's names get
//! numbers that never change, the dictionary follows the collection through a rename and goes
//! with its drop, a collection created again starts without one, the snapshot keeps them from its
//! format 22, and Group 0 refuses the names while a node runs an older format.

use teseladb_meta::catalog::{CatalogCommand, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{
    Group0Error, Group0Result, Group0Sm, SNAPSHOT_VERSION, SNAPSHOT_VERSION_WITHOUT_FIELD_NAMES,
};
use teseladb_meta::node_versions::{FORMAT, key as node_key};
use teseladb_multiraft::StateMachine;
use teseladb_bson::Document;

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
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd.clone()));
    sm.apply_command(index, &cmd)
}

fn create(n: Namespace, uuid: u8) -> CatalogCommand {
    CatalogCommand::Create {
        ns: n,
        uuid: [uuid; 16],
        options: vec![5, 0, 0, 0, 0],
        if_absent: false,
    }
}

fn add(collection: u32, dictionary: u32, names: &[&str]) -> CatalogCommand {
    let names = names.iter().map(|n| n.as_bytes().to_vec()).collect();
    CatalogCommand::AddFieldNames { collection, dictionary, names }
}

/// The numbers of `names` in the dictionary of collection `id`, and its identifier.
fn numbers(sm: &Group0Sm, id: u32, names: &[&str]) -> (Option<u32>, Vec<Option<u16>>) {
    let d = sm.catalog.fields_of(id);
    let got = names.iter().map(|n| d.and_then(|d| d.get(n.as_bytes()))).collect();
    (d.map(|d| d.dictionary()), got)
}

fn id_of(sm: &Group0Sm, n: &Namespace) -> u32 {
    sm.catalog.get(n).expect("collection").id
}

#[test]
fn names_get_numbers_that_never_change() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "c"), 1));
    let id = id_of(&sm, &ns("db", "c"));
    assert_eq!(apply(&mut sm, 3, add(id, 11, &["a", "b"])), Group0Result::Ok);
    // A second proposal, with another identifier, keeps the first one and the old numbers.
    assert_eq!(apply(&mut sm, 4, add(id, 12, &["c", "b", "a"])), Group0Result::Ok);
    assert_eq!(
        numbers(&sm, id, &["a", "b", "c", "d"]),
        (Some(11), vec![Some(0), Some(1), Some(2), None])
    );
    // A collection that does not exist gets none.
    assert_eq!(apply(&mut sm, 5, add(id + 40, 13, &["a"])), Group0Result::Ok);
    assert!(sm.catalog.fields_of(id + 40).is_none());
}

#[test]
fn a_rename_keeps_the_dictionary_and_a_drop_takes_it() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "c"), 1));
    let id = id_of(&sm, &ns("db", "c"));
    apply(&mut sm, 3, add(id, 21, &["x", "y"]));
    let rename = CatalogCommand::Rename {
        from: ns("db", "c"),
        to: ns("other", "d"),
        drop_target: false,
        uuid: [2; 16],
    };
    assert_eq!(apply(&mut sm, 4, rename), Group0Result::Ok);
    assert_eq!(id_of(&sm, &ns("other", "d")), id);
    assert_eq!(numbers(&sm, id, &["x", "y"]), (Some(21), vec![Some(0), Some(1)]));
    assert_eq!(apply(&mut sm, 5, CatalogCommand::Drop { ns: ns("other", "d") }), Group0Result::Ok);
    assert!(sm.catalog.fields_of(id).is_none());
    // Created again under the same name: another identifier and no dictionary.
    apply(&mut sm, 6, create(ns("other", "d"), 3));
    let again = id_of(&sm, &ns("other", "d"));
    assert_ne!(again, id);
    assert!(sm.catalog.fields_of(again).is_none());
    apply(&mut sm, 7, add(again, 22, &["y"]));
    assert_eq!(numbers(&sm, again, &["x", "y"]), (Some(22), vec![None, Some(0)]));
    // `dropDatabase` takes the dictionaries of its collections too.
    apply(&mut sm, 8, CatalogCommand::DropDatabase { db: "other".into() });
    assert!(sm.catalog.fields.is_empty());
}

#[test]
fn the_snapshot_keeps_the_dictionaries_from_format_22() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "c"), 1));
    let id = id_of(&sm, &ns("db", "c"));
    apply(&mut sm, 3, add(id, 31, &["a", "b", "c"]));
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).expect("restore");
    assert_eq!(copy.catalog, sm.catalog);
    assert_eq!(numbers(&copy, id, &["c"]), (Some(31), vec![Some(2)]));
    // The copy goes on numbering where the original would.
    apply(&mut sm, 4, add(id, 31, &["d"]));
    apply(&mut copy, 4, add(id, 31, &["d"]));
    assert_eq!(copy.catalog, sm.catalog);
    // Format 21 has no dictionaries and reads back without them.
    let mut old = Group0Sm::default();
    old.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_FIELD_NAMES)).expect("restore");
    assert!(old.catalog.fields.is_empty());
    assert_eq!(old.catalog.collections, sm.catalog.collections);
}

#[test]
fn names_wait_until_every_node_reads_format_22() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(ns("db", "c"), 1));
    let id = id_of(&sm, &ns("db", "c"));
    let node = *sm.nodes.keys().next().expect("a node");
    let runs = |format: u8| Document::new().with(FORMAT, i64::from(format)).to_bytes().unwrap();
    let record = |sm: &mut Group0Sm, index, format| {
        let cmd = Group0Command::SetSetting { key: node_key(node), value: runs(format) };
        assert_eq!(sm.apply_command(index, &cmd), Group0Result::Ok);
    };
    record(&mut sm, 3, SNAPSHOT_VERSION_WITHOUT_FIELD_NAMES);
    assert_eq!(
        apply(&mut sm, 4, add(id, 41, &["a"])),
        Group0Result::Err(Group0Error::NotEveryNodeRunsIt)
    );
    assert!(sm.catalog.fields.is_empty());
    record(&mut sm, 5, SNAPSHOT_VERSION);
    assert_eq!(apply(&mut sm, 6, add(id, 41, &["a"])), Group0Result::Ok);
    assert_eq!(numbers(&sm, id, &["a"]), (Some(41), vec![Some(0)]));
}
