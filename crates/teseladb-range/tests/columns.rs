//! A scan of the range host that takes columns (010 FR-033): its answer holds rows and batches
//! of documents as columns, in key order, and each batch's documents are the rows the scan of
//! the rows gives there, whatever the limit of each part.

mod common;

use std::sync::Arc;

use common::*;
use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_keys::KeyBuilder;
use teseladb_range::push::Piece;

/// Documents written.
const DOCS: i64 = 300;
/// The tenant whose keys fall in the range's `[START, END)`.
const TENANT: u32 = 0x6100_0000;

/// Writes one stored document at its key.
struct PutDoc(Vec<u8>, Vec<u8>);

impl Mutation for PutDoc {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Ok(Outcome {
            puts: vec![Put { key: self.0.clone(), value_type: 1, value: self.1.clone() }],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

fn key(i: i64) -> Vec<u8> {
    KeyBuilder::document(TENANT, 7, RawValue::Int64(i)).as_bytes().to_vec()
}

fn written() -> Cluster {
    let cfg = LsmConfig { dir: "core-00/lsm".into(), block_bytes: 4096, ..LsmConfig::default() };
    let mut c = Cluster::of(1, cfg, 1024);
    let n = c.elect();
    for i in 0..DOCS {
        let d = Document::new()
            .with("n", i)
            .with("x", (i % 7) as f64 / 3.0)
            .with("pad", "p".repeat(50));
        let k = key(i);
        c.mutate_with(n, &[&k], Box::new(PutDoc(k.clone(), d.to_bytes().unwrap())), None);
        c.settle();
    }
    let node = c.nodes.get_mut(&n).unwrap();
    node.host.lsm().borrow_mut().flush();
    c.settle();
    c
}

/// The parts of a scan of the whole range of at most `limit` documents each, with columns of
/// `want` or without: every part's answer.
fn parts(c: &mut Cluster, limit: usize, want: Option<&[&str]>) -> Vec<Reply> {
    let n = c.leader().unwrap();
    let mut from = START.to_vec();
    let mut out = Vec::new();
    loop {
        let (now, wall) = (c.now, c.wall_of(n));
        let node = c.nodes.get_mut(&n).unwrap();
        let r = ScanReq {
            g: G,
            start: from.clone(),
            end: END.to_vec(),
            concern: ReadConcern::Majority,
            limit,
            backward: false,
        };
        let id = match want {
            Some(w) => {
                let w: Arc<[String]> = w.iter().map(|s| (*s).to_owned()).collect();
                node.host.scan_columns(&mut node.core, (r, w), now, wall)
            }
            None => node.host.scan(&mut node.core, r, now, wall),
        };
        c.settle();
        for _ in 0..100 {
            if c.reply(n, id).is_some() {
                break;
            }
            c.ticks(1);
        }
        let reply = c.reply(n, id).expect("an answer").expect("the scan");
        let (count, last) = match &reply {
            Reply::Batch(rows) => (rows.len(), rows.last().map(|(k, _)| k.clone())),
            Reply::Pieces(p) => {
                let docs = p.iter().map(|x| match x {
                    Piece::Row(_) => 1,
                    Piece::Columns(c) => c.docs,
                });
                let last = p.last().map(|x| match x {
                    Piece::Row((k, _)) => k.clone(),
                    Piece::Columns(c) => c.last.clone(),
                });
                (docs.sum(), last)
            }
            other => panic!("{other:?}"),
        };
        out.push(reply);
        match last {
            Some(k) if count >= limit => from = [k.as_slice(), &[0]].concat(),
            _ => return out,
        }
    }
}

#[test]
fn a_scan_with_columns_gives_the_rows_documents_in_batches_and_rows() {
    let mut c = written();
    let rows: Vec<(Vec<u8>, Vec<u8>)> = parts(&mut c, 10_000, None)
        .into_iter()
        .flat_map(|r| match r {
            Reply::Batch(rows) => rows.into_iter().map(|(k, (_, v))| (k, v)).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(rows.len(), DOCS as usize);
    for limit in [10_000, 64, 7] {
        let mut at = 0;
        let mut batched = 0;
        for reply in parts(&mut c, limit, Some(&["n", "x"])) {
            let Reply::Pieces(pieces) = reply else { panic!("pieces") };
            for p in pieces {
                match p {
                    Piece::Row((k, (_, v))) => {
                        assert_eq!((&k, &v), (&rows[at].0, &rows[at].1));
                        at += 1;
                    }
                    Piece::Columns(b) => {
                        let view = b.view();
                        for f in ["n", "x"] {
                            for (j, cell) in view.cells(f).unwrap().enumerate() {
                                let d = RawDoc::from_bytes(&rows[at + j].1).unwrap();
                                assert_eq!(cell.map(Value::from), d.get(f).map(Value::from));
                            }
                        }
                        at += b.docs;
                        batched += b.docs;
                        assert_eq!(b.last, rows[at - 1].0);
                    }
                }
            }
        }
        assert_eq!(at, rows.len(), "limit {limit}");
        assert!(batched > DOCS as usize / 2, "{batched} in batches with limit {limit}");
    }
}
