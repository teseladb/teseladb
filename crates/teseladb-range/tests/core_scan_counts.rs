//! The process counts what the scans of its cores take (012 FR-015): the entries of their
//! sources, the items the storage gives them, the documents they give their readers and the
//! versions that only serve the history, which they pass over. A document written with a
//! description keeps a change meta next to it (014, D-114), and a scan of documents goes
//! through both: the counts tell how much of a scan's work is documents (one-node load p59r:
//! the parts of aggregations on the cores were 41.5 % of the node's CPU, and no count said what
//! they read). The scan's cursors pass over the metas before the merge (004 FR-046): they are
//! counted, and are no entries nor items of the scan. A step of a scan is measured in what it
//! takes from its sources, the versions passed over among them (023 FR-044). Alone in its
//! program: the counts are the process's.

mod txn_node;

use teseladb_hlc::Hlc;
use teseladb_range::HISTORY_VALUE;
use teseladb_range::ack::WriteConcern;
use teseladb_range::command::Put;
use teseladb_range::host::{Mutation, MutationError, Reply, WriteReq};
use teseladb_range::mutate::Outcome;
use teseladb_range::push::{
    CoreScans, SCAN_STEP_DOCUMENTS, SCAN_STEP_ENTRIES, ScanState, TxnView, core_scan_totals,
};
use teseladb_range::read::ReadConcern;
use teseladb_range::view::{MATCH_BATCH, Part};
use txn_node::*;

/// Writes a document and, next to it, the meta of the write.
struct WithMeta(Vec<u8>);

fn meta_of(doc: &[u8]) -> Vec<u8> {
    [doc, &[0, 0, 0, 0, 0]].concat()
}

impl Mutation for WithMeta {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let doc = Put { key: self.0.clone(), value_type: DOC, value: b"v".to_vec() };
        let meta = Put { key: meta_of(&self.0), value_type: HISTORY_VALUE, value: b"m".to_vec() };
        Ok(Outcome { puts: vec![doc, meta], deletes: vec![], change: vec![], result: vec![] })
    }
}

fn write_with_meta(n: &mut Node, key: &[u8], wc: WriteConcern) {
    let (now, wall) = (n.now, n.wall());
    let keys = vec![key.to_vec(), meta_of(key)];
    let w = WriteReq { g: G, keys, m: Box::new(WithMeta(key.to_vec())), dedup: None, wc };
    let id = n.host.mutate(&mut n.core, w, now, wall);
    assert!(matches!(n.answer(id), Ok(Reply::Mutated { .. })));
}

#[test]
fn the_scans_of_the_cores_count_the_documents_and_the_metas_they_go_through() {
    let mut n = Node::new();
    n.write(A, b"alone");
    write_with_meta(&mut n, B, MAJORITY);
    write_with_meta(&mut n, b"\x01c", MAJORITY);
    // Written again: the scan passes over the older version of each.
    write_with_meta(&mut n, b"\x01c", MAJORITY);
    let before = core_scan_totals();
    let lsm = n.host.lsm();
    let (view, mark) = (TxnView::default(), far());
    let mut scan = ScanState::new(lsm, (START, END), (mark, None), &view, false);
    let rows = scan.step(lsm, &view, 100).expect("in memory").expect("read");
    let keys: Vec<&[u8]> = rows.iter().map(|(k, _)| k.as_slice()).collect();
    assert_eq!(keys, [A, B, b"\x01c"], "the documents, without their metas");
    let after = core_scan_totals();
    let took = CoreScans {
        entries: after.entries - before.entries,
        items: after.items - before.items,
        documents: after.documents - before.documents,
        metas: after.metas - before.metas,
    };
    // The three versions of the two metas are passed over; the older version of the document
    // written twice is an entry of the merge.
    assert_eq!((took.documents, took.metas, took.items), (3, 3, 3), "{took:?}");
    assert_eq!(took.entries, 4, "{took:?}");
}

/// Steps `scan` to its end: its documents and how many times it stopped at its step.
fn to_the_end(n: &Node, scan: &mut ScanState) -> (usize, usize) {
    let (lsm, view) = (n.host.lsm(), TxnView::default());
    let mut stops = 0;
    loop {
        match scan.step(lsm, &view, 10_000) {
            Some(rows) => return (rows.expect("read").len(), stops),
            None => {
                assert!(scan.paused(), "in memory a step only stops at its entries");
                stops += 1;
            }
        }
    }
}

/// A step of a scan is its share of its core's turn, and what holds the turn is what the step
/// takes from its sources, not what it gives (023 FR-044): the versions under each document and
/// the ones its cursors pass over (004 FR-046) count. Measured in documents alone, a step over
/// documents rewritten many times took several times the entries of one over documents written
/// once, and the messages to the core waited a fifth longer (one-node load p63b).
#[test]
fn a_step_of_a_scan_stops_at_the_entries_it_takes_from_its_sources() {
    let mut n = Node::new();
    let docs = SCAN_STEP_DOCUMENTS - 8;
    let key = |i: usize| [b"\x01d".as_slice(), &u16::try_from(i).unwrap().to_be_bytes()].concat();
    let span: (&[u8], &[u8]) = (b"\x01d", b"\x01e");
    let (view, mark) = (TxnView::default(), far());
    // Written once: fewer documents than a step gives, and an entry and a meta for each.
    for i in 0..docs {
        write_with_meta(&mut n, &key(i), MAJORITY);
    }
    assert!(2 * u64::try_from(docs).unwrap() < SCAN_STEP_ENTRIES);
    let mut scan = ScanState::new(n.host.lsm(), span, (mark, None), &view, false);
    assert_eq!(to_the_end(&n, &mut scan), (docs, 0), "one step reads them all");
    // Rewritten: the same documents, with more versions under them than a step takes.
    let writes = 1 + 2 * usize::try_from(SCAN_STEP_ENTRIES).unwrap() / docs;
    for _ in 1..writes {
        for i in 0..docs {
            write_with_meta(&mut n, &key(i), MAJORITY);
        }
    }
    let mark = far();
    let mut scan = ScanState::new(n.host.lsm(), span, (mark, None), &view, false);
    let (read, stops) = to_the_end(&n, &mut scan);
    assert_eq!(read, docs, "every document");
    assert!(stops >= 1, "the step stops at the entries it took, with documents left to give");
}

/// A part read from the range's view lets its shard run its other work once every
/// [`MATCH_BATCH`] entries its scan takes from its sources (023 FR-044). Counted in the documents
/// it gave, a part over documents rewritten with their metas read several times the entries
/// between two pauses, and the device log's writer waited longer for its shard (one-node load
/// p63b to p64b: a sync waited 0.15 to 0.18 ms in its queue, 0.11 to 0.13 before).
#[test]
fn a_part_read_from_the_view_pauses_at_the_entries_it_takes() {
    let mut n = Node::new();
    let docs = MATCH_BATCH - 4;
    let key = |i: usize| [b"\x01f".as_slice(), &u16::try_from(i).unwrap().to_be_bytes()].concat();
    // Fewer documents than a pause, each written four times with its meta.
    for _ in 0..4 {
        for i in 0..docs {
            write_with_meta(&mut n, &key(i), MAJORITY);
        }
    }
    n.settle();
    let view = n.host.views().get(G).expect("the view");
    let begun = view.begin();
    let span: (&[u8], &[u8]) = (b"\x01f", b"\x01g");
    let part = Part { docs: 1_000, backward: false, blocks: 64 };
    let (mut cache, now) = (teseladb_lsm::ViewCache::default(), n.now);
    let mut pauses = 0;
    let read = view
        .scan_begun_pausing(
            &mut cache,
            span,
            (part, &begun),
            || now,
            ReadConcern::Local,
            |_| false,
            || pauses += 1,
        )
        .expect("served outside");
    assert_eq!(read.rows.len(), docs, "every document");
    // Two entries of each document go through the merge and four versions of its meta are
    // passed over: six times its documents, several pauses' worth.
    assert!(pauses >= 4, "{pauses} pauses for {docs} documents rewritten");
}
