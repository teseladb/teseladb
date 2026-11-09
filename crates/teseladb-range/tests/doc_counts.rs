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

fn counts_of(c: &Cluster, n: NodeId, g: GroupId) -> Option<BTreeMap<(u32, u32), u64>> {
    c.nodes[&n].core.state_machine(g).and_then(RangeSm::doc_counts).cloned()
}

/// A split leaves the counts of both halves estimated (each one's documents were counted by no
/// entry of its own) and their leaders recount them (H-004 of the native tests: after a split
/// every count read the documents).
#[test]
fn after_a_split_each_half_recounts_its_documents() {
    const NEW: GroupId = GroupId(9);
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    for node in c.nodes.values_mut() {
        node.host.set_counted_writes(true);
    }
    let keys: Vec<Vec<u8>> = [&b"a"[..], b"b", b"x", b"y", b"z"].iter().map(|pk| doc(pk)).collect();
    for k in &keys {
        write(&mut c, l, k, 1);
    }
    // The documents go by the hash of their key: the cut splits them by that order.
    let at = doc(b"m");
    let left = u64::try_from(keys.iter().filter(|k| **k < at).count()).unwrap();
    let right = u64::try_from(keys.len()).unwrap() - left;
    assert!(left > 0 && right > 0, "documents on both sides of the cut");
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, &at, NEW));
    c.follow();
    c.nodes.get_mut(&l).unwrap().host.take_splits();
    let at_index = c.nodes[&l].core.status(G).unwrap().applied;
    let voters: Vec<NodeId> = c.nodes.keys().copied().collect();
    for node in c.nodes.values_mut() {
        let lsm = node.host.lsm().clone();
        let sm = RangeSm::open(NEW, at.clone(), END.to_vec(), lsm, &mut node.disk).unwrap();
        let desc = GroupDesc { group: NEW, voters: voters.clone(), learners: vec![] };
        node.core.create_group(desc, sm).unwrap();
        node.host.add_born_range(NEW, at.clone(), END.to_vec(), G, at_index);
    }
    // Both leaders recount within a second or two.
    for _ in 0..400 {
        c.ticks_following(1);
    }
    c.settle();
    for n in voters {
        assert_eq!(counts_of(&c, n, G), Some(BTreeMap::from([(COLL, left)])), "left, {n:?}");
        assert_eq!(counts_of(&c, n, NEW), Some(BTreeMap::from([(COLL, right)])), "right, {n:?}");
    }
}

/// The count of a collection read from the view the leader published, without its core (H-004
/// of the native tests, 005 FR-028): exact counts only, and none while a write holds a latch.
#[test]
fn the_leaders_view_gives_the_exact_count_of_a_collection() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    for node in c.nodes.values_mut() {
        node.host.set_counted_writes(true);
    }
    for pk in [&b"a"[..], b"b", b"c"] {
        write(&mut c, l, &doc(pk), 1);
    }
    let count = |c: &Cluster, coll: (u32, u32)| {
        let view = c.nodes[&l].host.views().get(G)?;
        view.count(coll, (c.wall_of(l), || c.now), ReadConcern::Local)
    };
    // The leader publishes its view once its lease can serve reads.
    c.ticks(20);
    assert_eq!(count(&c, COLL), Some(3));
    assert_eq!(count(&c, (COLL.0, COLL.1 + 1)), Some(0), "a collection without documents here");
    // A write in flight holds its latch: the count goes to the core.
    c.mutate_with(l, &[&doc(b"d")], Box::new(SetKey(doc(b"d"), 1)), None);
    assert_eq!(count(&c, COLL), None);
    c.settle();
    assert_eq!(count(&c, COLL), Some(4));
    // Estimated counts are not given.
    for node in c.nodes.values_mut() {
        node.host.set_counted_writes(false);
    }
    write(&mut c, l, &doc(b"e"), 1);
    assert_eq!(count(&c, COLL), None);
}
