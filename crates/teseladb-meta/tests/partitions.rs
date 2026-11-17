//! Collections spread over partitions (003 FR-012, D-186): the catalog gives a collection created
//! with `P` partitions `P` consecutive collection numbers, the next collection starts after them,
//! the entry keeps them through a rename, and the collection a number belongs to is found by any
//! of its numbers. A collection created without the option has one.

use teseladb_bson::Document;
use teseladb_keys::partition::Partitions;
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace, PARTITIONS};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Result, Group0Sm};

fn ns(coll: &str) -> Namespace {
    Namespace { db: "db".into(), coll: coll.into() }
}

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: NodeAddrs { name: b"n1".to_vec(), cores: 8, ..NodeAddrs::default() },
        cert_fingerprint: [0; 32],
    };
    sm.apply_command(1, &boot);
    sm
}

fn create(sm: &mut Group0Sm, index: u64, coll: &str, options: &Document) -> CollectionEntry {
    let cmd = Group0Command::Catalog(CatalogCommand::Create {
        ns: ns(coll),
        uuid: [index as u8; 16],
        options: options.to_bytes().unwrap(),
        if_absent: false,
    });
    match sm.apply_command(index, &cmd) {
        Group0Result::Collection(c) => c,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_partitioned_collection_takes_consecutive_numbers_and_keeps_them() {
    let mut sm = cluster();
    let spread = create(&mut sm, 2, "spread", &Document::new().with(PARTITIONS, 8));
    let plain = create(&mut sm, 3, "plain", &Document::new());
    assert_eq!(spread.partitions(), Ok(Partitions::new(spread.id, 8)));
    assert_eq!(plain.id, spread.id + 8, "the next collection starts after the eight numbers");
    assert_eq!(plain.partitions(), Ok(Partitions::single(plain.id)));
    for n in spread.id..spread.id + 8 {
        let found = sm.catalog.by_number(n).map(|(ns, c)| (ns.coll.clone(), c.id));
        assert_eq!(found, Some(("spread".to_owned(), spread.id)), "number {n}");
    }
    assert_eq!(sm.catalog.by_number(plain.id).map(|(_, c)| c.id), Some(plain.id));
    let rename = CatalogCommand::Rename {
        from: ns("spread"),
        to: ns("moved"),
        drop_target: false,
        uuid: [9; 16],
    };
    sm.apply_command(4, &Group0Command::Catalog(rename));
    let moved = sm.catalog.get(&ns("moved")).cloned().expect("renamed");
    assert_eq!(moved.partitions(), Ok(Partitions::new(spread.id, 8)));
    let after = create(&mut sm, 5, "after", &Document::new());
    assert_eq!(after.id, plain.id + 1);
}

#[test]
fn a_partition_count_outside_one_to_sixty_four_is_refused() {
    for bad in [0, 65, -1] {
        let entry = CollectionEntry {
            id: 4,
            uuid: [0; 16],
            options: Document::new().with(PARTITIONS, bad).to_bytes().unwrap(),
            created_index: 1,
            tenant: 1,
        };
        assert!(entry.partitions().is_err(), "{bad} partitions");
    }
}
