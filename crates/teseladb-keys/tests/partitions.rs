//! Collections spread over partitions (003 FR-012, D-186): a document and its non-unique index
//! entries share the partition's collection number, a unique index other than `_id` lives whole in
//! partition 0, and the collection's keys are one interval over its numbers.

use teseladb_bson::{Document, RawDoc};
use teseladb_keys::partition::{MAX_PARTITIONS, Partitions};
use teseladb_keys::{Direction, FieldSpec, IndexKeySpec, KeyBuilder, KeyParts, PrimaryKey};

const TENANT: u32 = 1;

fn spec() -> IndexKeySpec {
    IndexKeySpec { fields: vec![FieldSpec { direction: Direction::Ascending }], buckets: 0 }
}

fn collection_number(key: &[u8]) -> u32 {
    match KeyParts::of(key).expect("a data key") {
        KeyParts::Data { collection, .. } => collection,
        other => panic!("not a data key: {other:?}"),
    }
}

#[test]
fn a_document_and_its_index_entries_share_the_partition_number() {
    let parts = Partitions::new(40, 8);
    for i in 0..200i32 {
        let bytes = Document::new().with("_id", i).with("v", i * 3).to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let id = doc.get("_id").unwrap();
        let pk = PrimaryKey::id(id);
        let number = parts.number_of(&pk);
        assert!(parts.contains(number), "{number} outside 40..48");
        let key = parts.document(TENANT, &pk);
        assert_eq!(collection_number(key.as_bytes()), number);
        assert_eq!(key, KeyBuilder::document_clustered(TENANT, number, id), "in _id order");
        let v = doc.get("v").unwrap();
        let (entry, _) = KeyBuilder::index(TENANT, number, 2, &spec(), &[v], &pk, None);
        assert_eq!(collection_number(entry.as_bytes()), number);
    }
}

#[test]
fn every_partition_gets_documents() {
    let parts = Partitions::new(7, 4);
    let mut seen = [0usize; 4];
    for i in 0..400i32 {
        let bytes = Document::new().with("_id", i).to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let pk = PrimaryKey::id(doc.get("_id").unwrap());
        seen[(parts.number_of(&pk) - 7) as usize] += 1;
    }
    assert!(seen.iter().all(|n| *n > 50), "{seen:?}");
}

#[test]
fn a_unique_index_lives_in_partition_zero() {
    let parts = Partitions::new(40, 8);
    assert_eq!(parts.unique_number(), 40);
}

#[test]
fn the_collection_is_one_interval_over_its_numbers() {
    let parts = Partitions::new(40, 8);
    assert_eq!(parts.start(TENANT), KeyBuilder::collection_start(TENANT, 40));
    assert_eq!(parts.end(TENANT), KeyBuilder::collection_end(TENANT, 47));
    assert_eq!(parts.numbers().collect::<Vec<_>>(), (40..48).collect::<Vec<_>>());
    assert!(parts.contains(47) && !parts.contains(48) && !parts.contains(39));
    assert_eq!(parts.partition_of_number(45), Some(5));
    assert_eq!(parts.partition_of_number(48), None);
}

#[test]
fn one_partition_is_the_collection_itself() {
    let parts = Partitions::single(9);
    let bytes = Document::new().with("_id", 5).to_bytes().unwrap();
    let doc = RawDoc::from_bytes(&bytes).unwrap();
    let pk = PrimaryKey::id(doc.get("_id").unwrap());
    assert_eq!(parts.number_of(&pk), 9);
    assert_eq!(parts.numbers().collect::<Vec<_>>(), vec![9]);
}

#[test]
#[should_panic(expected = "partitions")]
fn more_than_the_most_partitions_is_refused() {
    let _ = Partitions::new(1, MAX_PARTITIONS + 1);
}
