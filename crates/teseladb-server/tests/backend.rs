//! The commands' backend against scripted cores (006 FR-014, FR-027): what it retries, what it
//! gives up on and how it reports each answer.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use teseladb_hlc::Hlc;
use teseladb_io::mailbox::{self, Notify};
use teseladb_io::runtime::CoreInput;
use teseladb_meta::catalog::{Catalog, CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::group0::{Group0Error, RangeDesc, ReplicaRole};
use teseladb_meta::indexes::{IndexEntry, IndexState};
use teseladb_multiraft::{Done, NodeClock, OpError, OpId};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::host::{MutationError, RangeError};
use teseladb_range::read::ReadConcern;
use teseladb_server::authz_lease::{DEADLINE_SKEW, LATE_EVALUATION};
use teseladb_server::backend::{NodeHandle, ServerBackend};
use teseladb_server::catalog_ops::{Applied, CatalogAnswer, CatalogOps, encode_applied};
use teseladb_server::coresm::CoreOutput;
use teseladb_server::data::{DataReply, DataReq};
use teseladb_server::data_wire::RemoteReply;
use teseladb_server::data_wire::RemoteReq;
use teseladb_server::logic::NodeMsg;
use teseladb_server::routes::SharedRoutes;
use teseladb_session::retry::StatementKey;
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};
use teseladb_wire::commands::{Backend, WriteOp};

struct Quiet;

/// The wall clock in milliseconds.
fn wall_ms() -> u64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    u64::try_from(since.as_millis()).unwrap()
}

impl Notify for Quiet {
    fn notify(&self) {}
}

type DataScript = Arc<Mutex<VecDeque<Result<DataReply, RangeError>>>>;
type CatalogScript = Arc<Mutex<VecDeque<CatalogAnswer>>>;
type KillScript = Arc<Mutex<VecDeque<bool>>>;

/// Two cores that answer from their scripts (nothing left: no answer).
fn cores(data: DataScript, catalog: CatalogScript, kills: KillScript) -> NodeHandle {
    let mut senders = Vec::new();
    for core in 0..2 {
        let (tx, mut rx) = mailbox::channel::<CoreInput<NodeMsg>>(Arc::new(Quiet));
        senders.push(tx.senders());
        let (data, catalog, kills) = (data.clone(), catalog.clone(), kills.clone());
        thread::spawn(move || {
            let mut inbox = Vec::new();
            for _ in 0..4000 {
                rx.drain_into(&mut inbox);
                for m in inbox.drain(..) {
                    match m {
                        CoreInput::App(NodeMsg::Data(_, reply)) if core == 1 => {
                            if let Some(a) = data.lock().unwrap().pop_front() {
                                let _ = reply.send(a);
                            }
                        }
                        CoreInput::App(NodeMsg::KillSessions(_, reply)) if core == 0 => {
                            if let Some(a) = kills.lock().unwrap().pop_front() {
                                let _ = reply.send(a);
                            }
                        }
                        CoreInput::App(NodeMsg::Catalog(_, reply)) if core == 0 => {
                            if let Some(a) = catalog.lock().unwrap().pop_front() {
                                let _ = reply.send(a);
                            }
                        }
                        _ => {}
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
    }
    let routes = SharedRoutes::default();
    routes.publish(&[range(1, &[1], &[2]), range(2, &[2], &[4])]);
    NodeHandle {
        node: NodeId(3),
        senders,
        clock: NodeClock::for_node(NodeId(3)),
        sessions: Arc::default(),
        routes,
        auth: Arc::default(),
    }
}

/// Range `g` over `[start, end)` on core 1.
fn range(g: u64, start: &[u8], end: &[u8]) -> RangeDesc {
    RangeDesc {
        group: GroupId(g),
        start: start.to_vec(),
        end: end.to_vec(),
        core: 1,
        replicas: vec![(NodeId(3), ReplicaRole::Voter)],
        replica_epoch: 1,
        generation: 1,
        leader_hint: None,
        leader_term: 0,
        pending_split: None,
        settling: false,
    }
}

fn backend(data: Vec<Result<DataReply, RangeError>>, catalog: Vec<CatalogAnswer>) -> ServerBackend {
    let h = cores(
        Arc::new(Mutex::new(data.into())),
        Arc::new(Mutex::new(catalog.into())),
        Arc::new(Mutex::new(vec![true, false].into())),
    );
    ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300))
}

fn op(dedup: bool) -> WriteOp {
    WriteOp {
        keys: vec![vec![1]],
        mutation: vec![],
        dedup: dedup.then_some(StatementKey { lsid: 1, txn_number: 2, stmt_id: 3 }),
        wc: WriteConcern { w: W::Majority, j: true },
    }
}

fn written(result: &[u8]) -> Result<DataReply, RangeError> {
    Ok(DataReply::Remote(RemoteReply::Written {
        ts: Hlc { physical: 1, logical: 0 },
        result: result.to_vec(),
    }))
}

fn ns() -> Namespace {
    Namespace { db: "d".into(), coll: "c".into() }
}

#[test]
fn writes_retry_what_was_not_proposed() {
    let mut b = backend(
        vec![
            Err(RangeError::NotLeader { hint: NodeId(2) }),
            Err(RangeError::Overloaded),
            // Behind a split: the table is looked up again.
            Err(RangeError::OutOfRange),
            written(&[9]),
        ],
        vec![],
    );
    assert_eq!(b.write(op(false)), Ok(vec![9]));
    // A timeout may have been proposed: only a retryable write tries again.
    let mut b = backend(vec![Err(RangeError::Timeout), written(&[8])], vec![]);
    assert_eq!(b.write(op(false)).unwrap_err().code.code, 89);
    let mut b = backend(vec![Err(RangeError::Timeout), written(&[8])], vec![]);
    assert_eq!(b.write(op(true)), Ok(vec![8]));
    let mut b = backend(vec![Err(RangeError::Mutation(MutationError("66\u{0}no".into())))], vec![]);
    assert_eq!(b.write(op(false)).unwrap_err().code.code, 66);
    let mut b = backend(vec![Ok(DataReply::Remote(RemoteReply::Rows(vec![])))], vec![]);
    assert_eq!(b.write(op(false)).unwrap_err().code.code, 1);
    let mut b = backend(vec![Ok(DataReply::Put(Hlc { physical: 1, logical: 0 }))], vec![]);
    assert_eq!(b.write(op(false)).unwrap_err().code.code, 1);
    // No answer at all within the deadline.
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.write(op(true)).unwrap_err().code.code, 89);
}

#[test]
fn scans_return_rows() {
    let rows = vec![(vec![1], vec![2])];
    let mut b = backend(vec![Ok(DataReply::Remote(RemoteReply::Rows(rows.clone())))], vec![]);
    assert_eq!(b.scan(&[1], &[1, 9], 10, ReadConcern::Local), Ok(rows.clone()));
    // A scan over two ranges asks each in key order and stops at the limit.
    let more = vec![(vec![2, 1], vec![3])];
    let both = [rows.clone(), more.clone()].concat();
    let answers = [&rows, &more].map(|r| Ok(DataReply::Remote(RemoteReply::Rows(r.clone()))));
    let mut b = backend(answers.to_vec(), vec![]);
    assert_eq!(b.scan(&[1], &[3], 10, ReadConcern::Local), Ok(both));
    let mut b = backend(vec![Ok(DataReply::Remote(RemoteReply::Rows(rows.clone())))], vec![]);
    assert_eq!(b.scan(&[1], &[3], 1, ReadConcern::Local), Ok(rows));
    let mut b = backend(vec![written(&[1])], vec![]);
    assert!(b.scan(&[1], &[3], 10, ReadConcern::Local).is_err());
    // A key outside every range waits for the table until the deadline.
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.scan(&[5], &[6], 10, ReadConcern::Local).unwrap_err().code.code, 89);
    assert_eq!(b.node(), 3);
    assert_ne!(b.random(), b.random());
    let _ = b.now_ms();
}

#[test]
fn the_catalog_is_read_and_changed_through_core_zero() {
    let entry = CollectionEntry { id: 1, uuid: [2; 16], options: vec![], created_index: 3 };
    let mut catalog = Catalog::default();
    catalog.collections.insert(ns(), entry.clone());
    let mut b = backend(
        vec![],
        vec![CatalogAnswer::Unavailable, CatalogAnswer::Catalog(Arc::new(catalog.clone()))],
    );
    assert_eq!(b.collection(&ns()), Ok(Some(entry.clone())));
    let mut b = backend(vec![], vec![CatalogAnswer::Catalog(Arc::new(catalog))]);
    assert_eq!(b.collections().map(|c| c.len()), Ok(1));
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.collections().unwrap_err().code.code, 10107);
    let index = IndexEntry {
        id: 4,
        collection: 1,
        name: "a_1".into(),
        spec: vec![5, 0, 0, 0, 0],
        buckets: 0,
        state: IndexState::Ready,
    };
    let mut b = backend(
        vec![],
        vec![CatalogAnswer::Unavailable, CatalogAnswer::Indexes(vec![index.clone()], 1)],
    );
    assert_eq!(b.indexes(1), Ok(vec![index]));
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.indexes(1).unwrap_err().code.code, 10107);
    let drop = CatalogCommand::Drop { ns: ns() };
    for (answer, code) in [
        (CatalogAnswer::Refused(Group0Error::NamespaceExists), 48),
        (CatalogAnswer::Refused(Group0Error::NamespaceNotFound), 26),
        (CatalogAnswer::Refused(Group0Error::BadSplit), 1),
    ] {
        let mut b = backend(vec![], vec![answer]);
        assert_eq!(b.catalog_write(drop.clone()).unwrap_err().code.code, code);
    }
    let mut b =
        backend(vec![], vec![CatalogAnswer::Unavailable, CatalogAnswer::Done(Some(entry.clone()))]);
    assert_eq!(b.catalog_write(drop.clone()), Ok(Some(entry)));
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.catalog_write(drop).unwrap_err().code.code, 10107);
}

#[test]
fn forwarded_catalog_answers_wait_for_this_copy() {
    let mut ops = CatalogOps::default();
    let (tx, rx) = std::sync::mpsc::channel();
    ops.local(Ok(OpId(1)), true, tx);
    assert!(ops.owns(OpId(1)));
    let body = encode_applied(Some(&Applied { index: 10, result: Ok(None) }));
    assert_eq!(ops.on_done(OpId(1), Ok(Done::Forwarded(Bytes::from_vec(body))), 0, 5), Some(None));
    assert!(rx.try_recv().is_err());
    ops.poll(9, 0);
    assert!(rx.try_recv().is_err());
    ops.poll(10, 0);
    assert_eq!(rx.try_recv(), Ok(CatalogAnswer::Done(None)));
    // A refused proposal, a local one, another node's and one that is not this module's.
    let (tx, rx) = std::sync::mpsc::channel();
    ops.local(Err(OpError::Overloaded), false, tx);
    assert_eq!(rx.try_recv(), Ok(CatalogAnswer::Unavailable));
    let (tx, rx) = std::sync::mpsc::channel();
    ops.local(Ok(OpId(2)), false, tx);
    let applied = Ok(Done::Applied(CoreOutput::Group0(teseladb_meta::group0::Group0Result::Err(
        Group0Error::NamespaceExists,
    ))));
    assert_eq!(ops.on_done(OpId(2), applied, 4, 4), Some(None));
    assert_eq!(rx.try_recv(), Ok(CatalogAnswer::Refused(Group0Error::NamespaceExists)));
    assert_eq!(ops.remote(None, (NodeId(2), 1)), Some(encode_applied(None)));
    assert_eq!(ops.remote(Some(OpId(3)), (NodeId(2), 1)), None);
    let out = ops.on_done(OpId(3), Err(OpError::Timeout), 4, 4);
    assert!(matches!(out, Some(Some((NodeId(2), 1, _)))));
    assert_eq!(ops.on_done(OpId(9), Err(OpError::Timeout), 4, 4), None);
    let (tx, rx) = std::sync::mpsc::channel();
    ops.local(Ok(OpId(4)), true, tx);
    assert_eq!(ops.on_done(OpId(4), Err(OpError::Timeout), 4, 4), Some(None));
    assert_eq!(rx.try_recv(), Ok(CatalogAnswer::Unavailable));
}

#[test]
fn a_kill_reaches_the_other_nodes_or_reports_that_one_did_not_answer() {
    use teseladb_wire::commands::session::KillScope;
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.kill_everywhere(&KillScope::All), Ok(()));
    let e = b.kill_everywhere(&KillScope::Sessions(vec![1])).unwrap_err();
    assert_eq!(e.code.code, 89);
    // No answer at all.
    assert_eq!(b.kill_everywhere(&KillScope::All).unwrap_err().code.code, 89);
}

#[test]
fn a_command_deadline_cuts_the_waits_short() {
    // No answers: without a deadline each call waits the request deadline (300 ms here).
    let mut b = ServerBackend::new(cores(Arc::default(), Arc::default(), Arc::default()), 7)
        .with_deadline(Duration::from_secs(20));
    let now = b.now_ms();
    b.set_deadline(Some(now + 30));
    let start = std::time::Instant::now();
    assert!(b.collections().is_err());
    let e = b.scan(&[1], &[2], 10, ReadConcern::Local).unwrap_err();
    assert_eq!(e.code.code, 50, "{e:?}");
    assert!(b.cluster_parameters().is_err());
    assert!(b.set_cluster_parameter("p", vec![]).is_err());
    assert!(start.elapsed() < Duration::from_secs(10));
    b.set_deadline(None);
}

#[test]
fn cluster_parameters_are_group_0_settings() {
    use std::collections::BTreeMap;
    use teseladb_server::backend::{PARAMETER_PREFIX, parameters_of};
    let mut settings = BTreeMap::new();
    settings.insert([PARAMETER_PREFIX, b"defaultMaxTimeMS"].concat(), vec![5]);
    settings.insert(b"replicationFactor".to_vec(), vec![3]);
    settings.insert([PARAMETER_PREFIX, &[0xff][..]].concat(), vec![1]);
    let mut b = backend(
        vec![],
        vec![
            CatalogAnswer::Unavailable,
            CatalogAnswer::Settings(settings.clone()),
            CatalogAnswer::Done(None),
        ],
    );
    let expected: BTreeMap<String, Vec<u8>> = [("defaultMaxTimeMS".to_owned(), vec![5])].into();
    assert_eq!(parameters_of(&settings), expected);
    assert_eq!(b.cluster_parameters(), Ok(expected));
    assert_eq!(b.set_cluster_parameter("defaultMaxTimeMS", vec![1]), Ok(()));
}

#[test]
fn users_and_roles_are_read_and_changed_through_core_zero() {
    use teseladb_meta::security::{Principal, SecurityError};
    let doc = vec![5, 0, 0, 0, 0];
    let answers = vec![CatalogAnswer::Unavailable, CatalogAnswer::Principals(vec![doc.clone()])];
    let mut b = backend(vec![], answers);
    assert_eq!(b.principal(Principal::User, "admin", "u"), Ok(Some(doc.clone())));
    let mut b = backend(vec![], vec![CatalogAnswer::Principals(vec![doc.clone(), doc.clone()])]);
    assert_eq!(b.principals(Principal::Role, Some("d")).map(|d| d.len()), Ok(2));
    let mut b = backend(vec![], vec![CatalogAnswer::Principals(vec![])]);
    assert_eq!(b.principal(Principal::Role, "d", "r"), Ok(None));
    // No copy of Group 0 answers before the deadline.
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.principals(Principal::User, None).unwrap_err().code.code, 10107);
    for (answer, want) in [
        (CatalogAnswer::Done(None), Ok(Ok(()))),
        (
            CatalogAnswer::Refused(Group0Error::PrincipalExists(Principal::User)),
            Ok(Err(SecurityError::Exists(Principal::User))),
        ),
        (
            CatalogAnswer::Refused(Group0Error::PrincipalNotFound(Principal::Role)),
            Ok(Err(SecurityError::NotFound(Principal::Role))),
        ),
        (CatalogAnswer::Refused(Group0Error::SecurityConflict), Ok(Err(SecurityError::Conflict))),
    ] {
        let mut b = backend(vec![], vec![answer]);
        assert_eq!(b.security_write(vec![]).map_err(|e| e.code.code), want);
    }
    let mut b = backend(vec![], vec![CatalogAnswer::Refused(Group0Error::BadSplit)]);
    assert_eq!(b.security_write(vec![]).unwrap_err().code.code, 1);
}

#[test]
fn a_scan_into_the_last_range_reads_to_the_end_of_the_scan() {
    let rows = vec![(vec![2, 5], vec![1])];
    let data = vec![Ok(DataReply::Remote(RemoteReply::Rows(rows.clone())))];
    let h = cores(Arc::new(Mutex::new(data.into())), Arc::default(), Arc::default());
    h.routes.publish(&[range(1, &[1], &[2]), range(2, &[2], &[])]);
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300));
    assert_eq!(b.scan(&[2], &[9], 10, ReadConcern::Local), Ok(rows));
}

#[test]
fn reads_of_the_indexes_hold_back_their_changes_until_the_operation_ends() {
    let script: CatalogScript = Arc::default();
    let h = cores(Arc::default(), script.clone(), Arc::default());
    let auth = h.auth.clone();
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300));
    // Core 0 pins each read when it answers it (009 FR-006).
    let read = |at: u64| CatalogAnswer::Indexes(vec![], auth.pins.pin(at));
    script.lock().unwrap().push_back(read(3));
    assert_eq!(b.indexes(1), Ok(vec![]));
    assert_eq!(auth.pins.acked(10, auth.now()), 3);
    b.end_operation();
    assert_eq!(auth.pins.acked(10, auth.now()), 10, "the operation ended");
    // A change of the indexes releases the operation's own reads before it waits.
    script.lock().unwrap().extend([read(4), CatalogAnswer::Done(None)]);
    assert_eq!(b.indexes(1), Ok(vec![]));
    let ready = teseladb_meta::indexes::IndexCommand::SetState { id: 1, state: IndexState::Ready };
    assert_eq!(b.index_write(ready), Ok(()));
    assert_eq!(auth.pins.acked(10, auth.now()), 10);
    // So does a change of users and roles, and a backend that goes away.
    script.lock().unwrap().extend([read(5), CatalogAnswer::Done(None)]);
    assert_eq!(b.indexes(1), Ok(vec![]));
    assert_eq!(b.security_write(vec![]), Ok(Ok(())));
    assert_eq!(auth.pins.acked(10, auth.now()), 10);
    script.lock().unwrap().push_back(read(6));
    assert_eq!(b.indexes(1), Ok(vec![]));
    drop(b);
    assert_eq!(auth.pins.acked(10, auth.now()), 10);
}

/// A core that keeps the data requests it gets and answers them from `answers` (nothing left: no
/// answer).
fn recording_core(
    seen: Arc<Mutex<Vec<RemoteReq>>>,
    catalog: CatalogScript,
    answers: DataScript,
) -> NodeHandle {
    let (tx, mut rx) = mailbox::channel::<CoreInput<NodeMsg>>(Arc::new(Quiet));
    thread::spawn(move || {
        let mut inbox = Vec::new();
        for _ in 0..4000 {
            rx.drain_into(&mut inbox);
            for m in inbox.drain(..) {
                if let CoreInput::App(NodeMsg::Data(DataReq::Remote(_, r), reply)) = m {
                    seen.lock().unwrap().push(r);
                    if let Some(a) = answers.lock().unwrap().pop_front() {
                        let _ = reply.send(a);
                    }
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    });
    let mut h = cores(Arc::default(), catalog, Arc::default());
    h.senders[1] = tx.senders();
    h
}

/// A write that got no answer may still be evaluated by the range's leader until the deadline it
/// carries (009 FR-006): the operation's reads of the indexes stay pinned until then, and the
/// leader refuses the write after it.
#[test]
fn a_write_without_an_answer_keeps_the_reads_of_the_indexes_pinned_past_its_deadline() {
    let seen: Arc<Mutex<Vec<RemoteReq>>> = Arc::default();
    let catalog: CatalogScript = Arc::default();
    let h = recording_core(seen.clone(), catalog.clone(), Arc::default());
    let auth = h.auth.clone();
    let clock = h.clock.clone();
    let wait = Duration::from_millis(300);
    let mut b = ServerBackend::new(h, 7).with_deadline(wait);
    // Core 0 pins the operation's read when it answers it.
    catalog.lock().unwrap().push_back(CatalogAnswer::Indexes(vec![], auth.pins.pin(3)));
    assert_eq!(b.indexes(1), Ok(vec![]));
    let sent = clock.0.peek().physical.max(wall_ms());
    assert!(b.write(op(false)).is_err());
    let after = wall_ms();
    let deadline = match seen.lock().unwrap().first() {
        Some(RemoteReq::Write { deadline: Some(d), .. }) => *d,
        other => panic!("no write with a deadline: {other:?}"),
    };
    // The node's clock when it sent the write, plus the wait (less what went by in between) and
    // the room for a leader's clock ahead of it.
    let skew = u64::try_from(DEADLINE_SKEW.as_millis()).unwrap();
    let sent = sent + skew;
    assert!((sent + 299..=after + skew + 300).contains(&deadline), "{deadline} {sent} {after}");
    b.end_operation();
    assert_eq!(auth.pins.acked(10, auth.now()), 3, "the write may still be evaluated");
    // The wait may not be over yet when the write fails (its reply was dropped): its end, then.
    let late = u64::try_from((wait + LATE_EVALUATION).as_millis()).unwrap();
    let later = auth.now() + Nanos::from_millis(late);
    assert_eq!(auth.pins.acked(10, later), 10, "no leader evaluates it any more");
}

/// A write across ranges reads at a mark the first range's leader gives, under the clock rule,
/// and its other keys at that mark (009 FR-003; D-003, D-069): never at a mark the node's clock
/// last saw, which may lag far behind and make every later write a conflict.
#[test]
fn reads_across_ranges_take_their_mark_from_the_first_leader() {
    let seen: Arc<Mutex<Vec<RemoteReq>>> = Arc::default();
    let ts = Hlc { physical: 1_800_000_000_123, logical: 4 };
    let value = Some((2, vec![7]));
    let read = Ok(DataReply::Remote(RemoteReply::Value { ts, value: value.clone() }));
    let answers: DataScript = Arc::new(Mutex::new(vec![read.clone(), read].into()));
    let h = recording_core(seen.clone(), Arc::default(), answers);
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300));
    let (at, got) = b.read_versions(&[vec![1, 1], vec![2, 1]]).expect("read");
    assert_eq!((at, got), (ts, vec![value.clone(), value]));
    let marks: Vec<Option<Hlc>> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| match r {
            RemoteReq::Read { at, .. } => *at,
            other => panic!("not a read: {other:?}"),
        })
        .collect();
    assert_eq!(marks, vec![None, Some(ts)]);
}

/// Which answers leave a write possibly still to be evaluated (009 FR-006): a timeout or a lost
/// forwarded answer does, a refusal or a read does not.
#[test]
fn only_writes_without_a_trustworthy_answer_hold_the_reads_of_the_indexes() {
    use teseladb_range::intents::IntentOp;
    let held = |answers: Vec<Result<DataReply, RangeError>>, run: &dyn Fn(&mut ServerBackend)| {
        let catalog: CatalogScript = Arc::default();
        let h = cores(Arc::new(Mutex::new(answers.into())), catalog.clone(), Arc::default());
        let auth = h.auth.clone();
        let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300));
        catalog.lock().unwrap().push_back(CatalogAnswer::Indexes(vec![], auth.pins.pin(3)));
        assert_eq!(b.indexes(1), Ok(vec![]));
        run(&mut b);
        b.end_operation();
        auth.pins.acked(10, auth.now()) == 3
    };
    let ts = Hlc { physical: 9, logical: 1 };
    let put = || vec![(vec![1, 1], IntentOp::Put { value_type: 1, value: vec![5] })];
    let write = |b: &mut ServerBackend| assert!(b.write(op(false)).is_err());
    let txn = |b: &mut ServerBackend| assert!(b.transact(ts, put(), None).is_err());
    let read = |b: &mut ServerBackend| assert!(b.read_versions(&[vec![1, 1]]).is_err());
    assert!(held(vec![Err(RangeError::Timeout)], &write));
    assert!(held(vec![Err(RangeError::Unavailable)], &txn));
    assert!(held(vec![], &txn), "no answer at all");
    assert!(!held(vec![Err(RangeError::Unsatisfiable)], &write));
    assert!(!held(vec![Err(RangeError::WriteConflict)], &txn));
    assert!(!held(vec![Err(RangeError::Timeout)], &read));
}

#[test]
fn a_commit_quorum_counts_the_fewest_voters_of_the_ranges() {
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.voters(&[1], &[4]), Ok(1));
    assert_eq!(b.voters(&[3], &[3, 1]), Ok(1));
    assert_eq!(b.voters(&[9], &[10]).unwrap_err().code.code, 10107, "no range there");
}

#[test]
fn writes_across_ranges_read_at_one_mark_and_commit_as_a_transaction() {
    use teseladb_range::intents::IntentOp;
    let ts = Hlc { physical: 9, logical: 1 };
    let value = Some((2, vec![7]));
    let read = Ok(DataReply::Remote(RemoteReply::Value { ts, value: value.clone() }));
    let mut b = backend(vec![read.clone(), read], vec![]);
    let (at, got) = b.read_versions(&[vec![1, 1], vec![2, 1]]).expect("read");
    assert_eq!(got, vec![value.clone(), value]);
    assert_eq!(b.read_versions(&[vec![1, 1]]).unwrap_err().code.code, 89, "no answer: {at:?}");
    // Keys of one range commit in one phase; a conflict is `WriteConflict`.
    let put = || vec![(vec![1, 1], IntentOp::Put { value_type: 1, value: vec![5] })];
    let one = Ok(DataReply::Remote(RemoteReply::Intents { ts }));
    let mut b = backend(vec![one], vec![]);
    assert_eq!(b.transact(ts, put(), None), Ok(ts));
    let mut b = backend(vec![Err(RangeError::WriteConflict)], vec![]);
    assert_eq!(b.transact(ts, put(), None).unwrap_err().code.code, 112);
    // A key no range holds has no route.
    let mut b = backend(vec![], vec![]);
    let nowhere = vec![(vec![9], IntentOp::Delete)];
    assert_eq!(b.transact(ts, nowhere, None).unwrap_err().code.code, 1);
}

#[test]
fn a_transaction_over_two_ranges_commits_and_resolves_its_intents() {
    use teseladb_range::intents::{IntentOp, TxnId, TxnRecord, TxnStatus};
    let ts = |p| Hlc { physical: p, logical: 0 };
    let record = |status, commit| {
        Ok(DataReply::Remote(RemoteReply::Record(TxnRecord {
            txn: TxnId(1),
            lsid: 0,
            txn_number: 0,
            status,
            read_ts: ts(1),
            min_commit_ts: ts(1),
            commit_ts: commit,
            heartbeat_ts: ts(1),
            spans: vec![],
        })))
    };
    let intents = || Ok(DataReply::Remote(RemoteReply::Intents { ts: ts(3) }));
    let answers = vec![
        // The record's range has no leader at first: the request goes again.
        Err(RangeError::NotLeader { hint: teseladb_types::NodeId(0) }),
        record(TxnStatus::Pending, ts(0)),
        intents(),
        intents(),
        record(TxnStatus::Committed, ts(7)),
        intents(),
        intents(),
    ];
    let mut b = backend(answers, vec![]);
    let writes = vec![
        (vec![1, 1], IntentOp::Put { value_type: 1, value: vec![5] }),
        (vec![2, 1], IntentOp::Delete),
    ];
    assert_eq!(b.transact(ts(1), writes, Some(TxnId(1))), Ok(ts(7)));
    // Without answers the transaction gives up at the deadline.
    let mut b = backend(vec![], vec![]);
    let writes = vec![(vec![1, 1], IntentOp::Delete), (vec![2, 1], IntentOp::Delete)];
    assert_eq!(b.transact(ts(1), writes, None).unwrap_err().code.code, 89);
}

/// A read at a mark from the clock that meets a version in its uncertainty window reads again at
/// that version's mark, transparently, and the command learns the mark it read at (008 FR-008,
/// T023); past its restarts it fails with the version's `WriteConflict`.
#[test]
fn a_read_in_its_uncertainty_window_goes_again_above_the_version() {
    use teseladb_txn::causal::ReadMark;
    let data: DataScript = Arc::default();
    let h = cores(data.clone(), Arc::default(), Arc::default());
    let mut b = ServerBackend::new(h, 1);
    let at = Hlc { physical: 1_000, logical: 0 };
    let version = Hlc { physical: 1_100, logical: 2 };
    let window = Some(Hlc { physical: 1_250, logical: u32::MAX });
    b.set_read_mark(Some(ReadMark { at, learned: true, uncertain_to: window }));
    {
        let mut q = data.lock().unwrap();
        q.push_back(Err(RangeError::ReadUncertain { ts: version }));
        q.push_back(Ok(DataReply::Remote(RemoteReply::Rows(vec![(vec![1, 1], vec![9])]))));
    }
    let rows = b.scan(&[1], &[2], 10, ReadConcern::Majority).unwrap();
    assert_eq!(rows, vec![(vec![1, 1], vec![9])]);
    assert_eq!(b.read_mark().map(|m| m.at), Some(version));
    // Always uncertain: it gives up with the error.
    {
        let mut q = data.lock().unwrap();
        for _ in 0..=teseladb_txn::uncertainty::MAX_RESTARTS {
            q.push_back(Err(RangeError::ReadUncertain { ts: version }));
        }
    }
    let e = b.scan(&[1], &[2], 10, ReadConcern::Majority).unwrap_err();
    assert_eq!(e.code.code, 112, "{e:?}");
}
