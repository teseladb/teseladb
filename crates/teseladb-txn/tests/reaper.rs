//! The reaper of finished records (008 FR-019) driven by hand: a removal that cannot reach a
//! range, or whose request fails, gives up and leaves the record to the next scan; an answer that
//! comes after its removal gave up changes nothing.

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply, TxnOp};
use teseladb_range::intents::{TxnId, TxnRecord, TxnStatus};
use teseladb_txn::coordinator::{Request, Route};
use teseladb_txn::reaper::Reaper;
use teseladb_types::GroupId;

const G: GroupId = GroupId(3);
/// A mark long after any activity of the records below.
const LATE: Hlc = Hlc { physical: 10_000_000_000_000, logical: 0 };

/// A routing table with every key in one range, or with none.
struct One(Option<GroupId>);

impl Route for One {
    fn range_of(&self, _key: &[u8]) -> Option<GroupId> {
        self.0
    }
}

const ROUTED: One = One(Some(G));
const LOST: One = One(None);

fn record(txn: u128, spans: Vec<(Vec<u8>, Vec<u8>)>) -> TxnRecord {
    let zero = Hlc { physical: 1, logical: 0 };
    TxnRecord {
        txn: TxnId(txn),
        lsid: 0,
        txn_number: 0,
        status: TxnStatus::Committed,
        read_ts: zero,
        min_commit_ts: zero,
        commit_ts: zero,
        heartbeat_ts: zero,
        spans,
    }
}

/// An interval of one key, as the coordinator writes it.
fn chain(key: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (key.to_vec(), [key, &[0]].concat())
}

/// An interval cut at a long key's first bytes: its keys are searched.
fn prefix() -> (Vec<u8>, Vec<u8>) {
    (b"\x01p".to_vec(), b"\x01q".to_vec())
}

fn requests(r: &mut Reaper) -> Vec<Request> {
    r.take_requests()
}

fn find_and_resolve(reqs: &[Request]) -> (u64, u64) {
    let find = reqs.iter().find(|q| matches!(q.op, TxnOp::FindIntents { .. })).unwrap();
    let resolve = reqs.iter().find(|q| matches!(q.op, TxnOp::ResolveIntents { .. })).unwrap();
    (find.id, resolve.id)
}

#[test]
fn a_removal_without_a_range_gives_up() {
    // Its record's range is unknown: nothing is sent.
    let mut r = Reaper::with_session_ttl(1_000);
    r.scan(&LOST, LATE, [record(1, Vec::new())]);
    assert!(requests(&mut r).is_empty());
    assert!(r.is_idle());
    // Nor is an interval searched in a range it cannot find.
    r.scan(&LOST, LATE, [record(2, vec![prefix()])]);
    assert!(requests(&mut r).is_empty());
    // A recent record of a session asks for its successor, unless it has no range.
    let mut recent = record(3, Vec::new());
    recent.lsid = 9;
    r.scan(&LOST, Hlc { physical: 10, logical: 0 }, [recent.clone()]);
    assert!(requests(&mut r).is_empty());
    // The records were given up: a scan with ranges takes them again.
    r.scan(&ROUTED, LATE, [record(1, Vec::new())]);
    assert_eq!(requests(&mut r).len(), 1);
}

#[test]
fn a_failed_request_of_a_removal_leaves_the_record() {
    let mut r = Reaper::with_session_ttl(1_000);
    // The resolution of its intents fails.
    r.scan(&ROUTED, LATE, [record(1, vec![chain(b"\x01a")])]);
    let reqs = requests(&mut r);
    assert!(matches!(reqs[..], [Request { op: TxnOp::ResolveIntents { .. }, .. }]));
    r.on_reply(&ROUTED, reqs[0].id, Err(RangeError::Timeout));
    assert!(requests(&mut r).is_empty() && r.is_idle());
    // The record answers its end, then forgetting it fails.
    r.scan(&ROUTED, LATE, [record(2, Vec::new())]);
    let end = requests(&mut r);
    r.on_reply(&ROUTED, end[0].id, Ok(Reply::Record(record(2, Vec::new()))));
    let forget = requests(&mut r);
    assert!(matches!(forget[..], [Request { op: TxnOp::ForgetRecord { .. }, .. }]));
    r.on_reply(&ROUTED, forget[0].id, Err(RangeError::Timeout));
    assert_eq!(r.forgotten(), 0);
    // Its end answers once its range is gone.
    r.scan(&ROUTED, LATE, [record(3, Vec::new())]);
    let end = requests(&mut r);
    r.on_reply(&LOST, end[0].id, Ok(Reply::Record(record(3, Vec::new()))));
    assert!(requests(&mut r).is_empty() && r.is_idle());
    // Keys found where no range holds them.
    r.scan(&ROUTED, LATE, [record(4, vec![prefix()])]);
    let find = requests(&mut r);
    let rows = vec![(b"\x01p1".to_vec(), (1, b"x".to_vec()))];
    r.on_reply(&LOST, find[0].id, Ok(Reply::Batch(rows)));
    assert!(requests(&mut r).is_empty() && r.is_idle());
}

#[test]
fn an_answer_after_its_removal_gave_up_changes_nothing() {
    let mut r = Reaper::with_session_ttl(1_000);
    let spans = vec![chain(b"\x01a"), prefix()];
    // The search fails first; the resolution answers after.
    r.scan(&ROUTED, LATE, [record(1, spans.clone())]);
    let (find, resolve) = find_and_resolve(&requests(&mut r));
    r.on_reply(&ROUTED, find, Err(RangeError::Timeout));
    r.on_reply(&ROUTED, resolve, Ok(Reply::Intents { ts: LATE }));
    assert!(requests(&mut r).is_empty() && r.is_idle());
    // The resolution fails first; the search answers after.
    r.scan(&ROUTED, LATE, [record(2, spans)]);
    let (find, resolve) = find_and_resolve(&requests(&mut r));
    r.on_reply(&ROUTED, resolve, Err(RangeError::Timeout));
    r.on_reply(&ROUTED, find, Ok(Reply::Batch(Vec::new())));
    assert!(requests(&mut r).is_empty() && r.is_idle());
    assert_eq!(r.forgotten(), 0);
}
