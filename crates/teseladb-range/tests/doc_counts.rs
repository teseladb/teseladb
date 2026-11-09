//! The live documents of each collection (007 FR-039, H-004): with every node reading the
//! format, a leader's write says how many documents of each collection it adds or removes, and
//! every replica keeps the same exact counts; without it, the counts are estimated.

mod common;

use common::*;

const COLL: (u32, u32) = (0x6100_0000, 4);

/// A document of collection [`COLL`] whose key falls inside the test range.
fn doc(pk: &[u8]) -> Vec<u8> {
    teseladb_keys::space::KeyBuilder::document_of_pk(COLL.0, COLL.1, pk).as_bytes().to_vec()
}

fn write(c: &mut Cluster, n: NodeId, key: &[u8], v: u8) {
    c.mutate_with(n, &[key], Box::new(SetKey(key.to_vec(), v)), None);
    c.settle();
}

fn counts(c: &Cluster, n: NodeId) -> Option<BTreeMap<(u32, u32), u64>> {
    c.nodes[&n].core.state_machine(G).and_then(RangeSm::doc_counts).cloned()
}

#[test]
fn every_replica_counts_the_leaders_counted_writes_alike() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    for node in c.nodes.values_mut() {
        node.host.set_counted_writes(true);
    }
    let (a, b) = (doc(b"a"), doc(b"b"));
    write(&mut c, l, &a, 1);
    write(&mut c, l, &b, 1);
    // A replacement of `a` adds no document.
    write(&mut c, l, &a, 2);
    let want = Some(BTreeMap::from([(COLL, 2)]));
    for n in c.nodes.keys().copied().collect::<Vec<_>>() {
        assert_eq!(counts(&c, n), want, "node {n:?}");
    }
}

#[test]
fn without_every_node_reading_them_the_counts_are_estimated() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    write(&mut c, l, &doc(b"a"), 1);
    for n in c.nodes.keys().copied().collect::<Vec<_>>() {
        assert_eq!(counts(&c, n), None, "node {n:?}");
    }
}

#[test]
fn the_leader_recounts_estimated_counts_and_every_replica_takes_them() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    // Written before every node read the counted format: estimated.
    write(&mut c, l, &doc(b"a"), 1);
    write(&mut c, l, &doc(b"b"), 1);
    for node in c.nodes.values_mut() {
        node.host.set_counted_writes(true);
    }
    // The leader counts within a second and proposes the counts.
    c.ticks(150);
    c.settle();
    let nodes: Vec<NodeId> = c.nodes.keys().copied().collect();
    for n in &nodes {
        assert_eq!(counts(&c, *n), Some(BTreeMap::from([(COLL, 2)])), "node {n:?}");
    }
    // Counted writes go on from the recount.
    write(&mut c, l, &doc(b"c"), 1);
    for n in &nodes {
        assert_eq!(counts(&c, *n), Some(BTreeMap::from([(COLL, 3)])), "node {n:?}");
    }
}
