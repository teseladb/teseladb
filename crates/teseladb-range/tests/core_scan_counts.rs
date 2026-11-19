//! The process counts what the scans of its cores take (012 FR-015): the entries of their
//! sources, the items the storage gives them, the documents they give their readers and the
//! versions that only serve the history, which they pass over. A document written with a
//! description keeps a change meta next to it (014, D-114), and a scan of documents goes
//! through both: the counts tell how much of a scan's work is documents (one-node load p59r:
//! the parts of aggregations on the cores were 41.5 % of the node's CPU, and no count said what
//! they read). The scan's cursors pass over the metas before the merge (004 FR-046): they are
//! counted, and are no entries nor items of the scan. Alone in its program: the counts are the
//! process's.

mod txn_node;

use teseladb_hlc::Hlc;
use teseladb_range::HISTORY_VALUE;
use teseladb_range::ack::WriteConcern;
use teseladb_range::command::Put;
use teseladb_range::host::{Mutation, MutationError, Reply, WriteReq};
use teseladb_range::mutate::Outcome;
use teseladb_range::push::{CoreScans, ScanState, TxnView, core_scan_totals};
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
