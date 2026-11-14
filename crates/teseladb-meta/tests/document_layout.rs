//! The key layout of a collection's documents in its catalog entry (003 FR-012, D-153): a
//! collection created with the `_id` order keeps it on every replica, through a rename, a change
//! of options that carries it and a snapshot; one created before, without the option, keeps the
//! hash bucket of its `_id`, so its documents are not moved.

use teseladb_bson::Document;
use teseladb_meta::catalog::{
    CatalogCommand, CollectionEntry, DocumentLayout, Namespace, ORDERED_IDS,
};
use teseladb_meta::collection_types::KindError;
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Result, Group0Sm};
use teseladb_multiraft::StateMachine;

fn ns(coll: &str) -> Namespace {
    Namespace { db: "db".into(), coll: coll.into() }
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

fn entry(sm: &mut Group0Sm, index: u64, coll: &str, options: &Document) -> CollectionEntry {
    let cmd = Group0Command::Catalog(CatalogCommand::Create {
        ns: ns(coll),
        uuid: [index as u8; 16],
        options: options.to_bytes().unwrap(),
        if_absent: false,
    });
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd.clone()));
    match sm.apply_command(index, &cmd) {
        Group0Result::Collection(c) => c,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_collection_created_in_id_order_keeps_it_and_an_older_one_its_buckets() {
    let mut sm = cluster();
    let ordered = entry(&mut sm, 2, "new", &Document::new().with(ORDERED_IDS, true));
    let older = entry(&mut sm, 3, "old", &Document::new().with("validator", Document::new()));
    assert_eq!(ordered.layout(), Ok(DocumentLayout::IdOrder));
    assert_eq!(older.layout(), Ok(DocumentLayout::Buckets));
    // A rename keeps the entry, and with it the layout of the documents already written.
    let rename = CatalogCommand::Rename {
        from: ns("new"),
        to: ns("renamed"),
        drop_target: false,
        uuid: [9; 16],
    };
    sm.apply_command(4, &Group0Command::Catalog(rename));
    let renamed = sm.catalog.get(&ns("renamed")).cloned().expect("renamed");
    assert_eq!((renamed.id, renamed.layout()), (ordered.id, Ok(DocumentLayout::IdOrder)));
    // Every replica restores the same layouts from a snapshot.
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).expect("restore");
    let layouts = |sm: &Group0Sm| {
        sm.catalog.collections.values().map(|c| (c.id, c.layout())).collect::<Vec<_>>()
    };
    assert_eq!(layouts(&copy), layouts(&sm));
}

#[test]
fn only_a_true_option_gives_the_id_order() {
    let layout = |options: Document| {
        let c = CollectionEntry {
            id: 1,
            uuid: [0; 16],
            options: options.to_bytes().unwrap(),
            created_index: 1,
            tenant: 0,
        };
        c.layout()
    };
    assert_eq!(layout(Document::new()), Ok(DocumentLayout::Buckets));
    assert_eq!(layout(Document::new().with(ORDERED_IDS, false)), Ok(DocumentLayout::Buckets));
    assert_eq!(layout(Document::new().with(ORDERED_IDS, 1)), Ok(DocumentLayout::Buckets));
    assert_eq!(layout(Document::new().with(ORDERED_IDS, true)), Ok(DocumentLayout::IdOrder));
    let broken =
        CollectionEntry { id: 1, uuid: [0; 16], options: vec![1, 2], created_index: 1, tenant: 0 };
    assert_eq!(broken.layout(), Err(KindError("options that are not BSON")));
}
