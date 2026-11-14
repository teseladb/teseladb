//! Local identifiers of documents (009 FR-023): with every node reading the format, the leader
//! numbers the new documents of each write, the document and its index entries alike, and every
//! replica keeps the same numbers and the same next number, also after a change of leader;
//! without it, the documents keep none.

mod common;

use common::*;
use teseladb_keys::local_id::{
    INDEX_ENTRY_VALUE, NO_LOCAL_ID, document_id, entry_id, provisional, with_document_id,
    with_entry_id,
};
use teseladb_range::sm::RangeState;

const COLL: (u32, u32) = (0x6100_0000, 4);

/// A document of collection [`COLL`] whose key falls inside the test range.
fn doc(pk: &[u8]) -> Vec<u8> {
    teseladb_keys::space::KeyBuilder::document_of_pk(COLL.0, COLL.1, pk).as_bytes().to_vec()
}

/// An entry of index 3 of [`COLL`] for `pk`.
fn entry(pk: &[u8]) -> Vec<u8> {
    let mut k = teseladb_keys::KeyBuilder::index_prefix(COLL.0, COLL.1, 3).as_bytes().to_vec();
    k.extend_from_slice(pk);
    k
}

/// Inserts its documents, each with one index entry, both with the provisional number of the
/// document's place in the write.
struct Insert(Vec<Vec<u8>>);

impl Mutation for Insert {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let mut puts = Vec::new();
        for (slot, pk) in (0u32..).zip(&self.0) {
            let value = with_document_id(vec![5, 0, 0, 0, 0], provisional(slot));
            puts.push(Put { key: doc(pk), value_type: 1, value });
            let value = with_entry_id(provisional(slot), &[]);
            puts.push(Put { key: entry(pk), value_type: INDEX_ENTRY_VALUE, value });
        }
        Ok(Outcome { puts, deletes: vec![], change: vec![], result: vec![] })
    }
}

fn insert(c: &mut Cluster, n: NodeId, pks: &[&[u8]]) {
    let keys: Vec<Vec<u8>> = pks.iter().map(|p| doc(p)).collect();
    let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
    let pks = pks.iter().map(|p| p.to_vec()).collect();
    c.mutate_with(n, &refs, Box::new(Insert(pks)), None);
    c.settle();
}

/// The value node `n` holds at `key`.
fn stored(c: &mut Cluster, n: NodeId, key: &[u8]) -> Vec<u8> {
    let r = c.get(n, key, ReadConcern::Local);
    c.settle();
    match c.reply(n, r) {
        Some(Ok(Reply::Value { value: Some((_, v)), .. })) => v,
        other => panic!("not a value: {other:?}"),
    }
}

fn next(c: &Cluster, n: NodeId) -> u32 {
    c.nodes[&n].core.state_machine(G).map_or(0, RangeState::next_local_id)
}

fn numbered(c: &mut Cluster, n: NodeId, pk: &[u8]) -> (Option<u32>, Option<u32>) {
    let d = stored(c, n, &doc(pk));
    let e = stored(c, n, &entry(pk));
    (document_id(&d), entry_id(&e))
}

#[test]
fn the_leader_numbers_new_documents_and_every_replica_keeps_the_numbers() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    for node in c.nodes.values_mut() {
        node.host.set_local_ids(true);
    }
    insert(&mut c, l, &[b"a", b"b"]);
    insert(&mut c, l, &[b"c"]);
    let nodes: Vec<NodeId> = c.nodes.keys().copied().collect();
    for n in &nodes {
        assert_eq!(next(&c, *n), 3, "node {n:?}");
    }
    let want = [(b"a", 0), (b"b", 1), (b"c", 2)];
    for (pk, id) in want {
        assert_eq!(numbered(&mut c, l, pk), (Some(id), Some(id)));
    }
    // A new leader holds the same numbers and goes on past every one given.
    c.isolate(l);
    c.ticks(300);
    c.settle();
    let l2 = c.leader().filter(|n| *n != l).expect("a new leader");
    for (pk, id) in want {
        assert_eq!(numbered(&mut c, l2, pk), (Some(id), Some(id)));
    }
    insert(&mut c, l2, &[b"d"]);
    assert_eq!(numbered(&mut c, l2, b"d"), (Some(3), Some(3)));
    assert_eq!(next(&c, l2), 4);
}

#[test]
fn without_every_node_reading_the_format_documents_keep_no_number() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    insert(&mut c, l, &[b"a"]);
    let d = stored(&mut c, l, &doc(b"a"));
    assert_eq!(document_id(&d), None);
    assert_eq!(&d[d.len() - 5..d.len() - 1], NO_LOCAL_ID.to_le_bytes());
    assert_eq!(numbered(&mut c, l, b"a"), (None, None));
    assert_eq!(next(&c, l), 0);
}
