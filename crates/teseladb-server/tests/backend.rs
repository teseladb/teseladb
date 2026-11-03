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
type OpsScript = Arc<Mutex<VecDeque<Option<Vec<Vec<u8>>>>>>;

/// Two cores that answer from their scripts (nothing left: no answer).
fn cores(data: DataScript, catalog: CatalogScript, kills: KillScript) -> NodeHandle {
    cores_with_ops(data, catalog, kills, OpsScript::default())
}

/// [`cores`], with core 0 answering `currentOp` of the other nodes from `ops`.
fn cores_with_ops(
    data: DataScript,
    catalog: CatalogScript,
    kills: KillScript,
    ops: OpsScript,
) -> NodeHandle {
    let mut senders = Vec::new();
    for core in 0..2 {
        let (tx, mut rx) = mailbox::channel::<CoreInput<NodeMsg>>(Arc::new(Quiet));
        senders.push(tx.senders());
        let (data, catalog, kills) = (data.clone(), catalog.clone(), kills.clone());
        let ops = ops.clone();
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
                        CoreInput::App(NodeMsg::CurrentOps(reply)) if core == 0 => {
                            if let Some(a) = ops.lock().unwrap().pop_front() {
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
        shutdown: Arc::default(),
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
        moved_ms: 0,
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
    let entry =
        CollectionEntry { id: 1, uuid: [2; 16], options: vec![], created_index: 3, tenant: 0 };
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
    // A span the table never fills waits for it until the deadline, as a write waits for its
    // key's range, and gets the ranges' time-out (NetworkTimeout).
    assert_eq!(b.voters(&[9], &[10]).unwrap_err().code.code, 89, "no range there");
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

fn owner(key: &[u8]) -> teseladb_range::intents::IntentOwner {
    use teseladb_range::intents::{IntentOwner, TxnId};
    IntentOwner {
        txn: TxnId(6),
        record_key: vec![1, 5],
        key: key.to_vec(),
        ts: Hlc { physical: 1, logical: 0 },
    }
}

fn record(status: teseladb_range::intents::TxnStatus) -> Result<DataReply, RangeError> {
    use teseladb_range::intents::{TxnId, TxnRecord};
    let ts = Hlc { physical: 5, logical: 0 };
    Ok(DataReply::Remote(RemoteReply::Record(TxnRecord {
        txn: TxnId(6),
        lsid: 0,
        txn_number: 0,
        status,
        read_ts: ts,
        min_commit_ts: ts,
        commit_ts: ts,
        heartbeat_ts: ts,
        spans: vec![],
    })))
}

/// A write outside a transaction that meets an intent waits a pause and goes again (008 FR-002)
/// whenever the intent is not resolved: the push has no record, the owner is running, the intent's
/// key has no range in the table, or its range refuses the resolution; once the owner is finished
/// and the intent resolved, it goes again at once.
#[test]
fn a_write_waits_for_an_intent_until_it_is_resolved() {
    use teseladb_range::intents::TxnStatus;
    let conflict = |key: &[u8]| Err(RangeError::IntentConflict(owner(key)));
    let rows = Ok(DataReply::Remote(RemoteReply::Rows(vec![])));
    let data = vec![
        conflict(&[1, 1]),
        rows.clone(),
        conflict(&[1, 1]),
        record(TxnStatus::Pending),
        conflict(&[9]),
        record(TxnStatus::Committed),
        conflict(&[1, 1]),
        record(TxnStatus::Aborted),
        Ok(DataReply::Put(Hlc { physical: 1, logical: 0 })),
        conflict(&[1, 1]),
        record(TxnStatus::Committed),
        rows,
        written(&[7]),
    ];
    let h = cores(Arc::new(Mutex::new(data.into())), Arc::default(), Arc::default());
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_secs(5));
    assert_eq!(b.write(op(false)), Ok(vec![7]));
}

/// A read whose push of the intent's owner has no record forgets the push, so that the next reader
/// pushes again, and reads again at its mark (008 FR-003).
#[test]
fn a_read_whose_push_has_no_record_reads_again() {
    use teseladb_txn::push_cache::Lookup;
    let at = Hlc { physical: 1_000, logical: 0 };
    let rows = vec![(vec![1, 1], vec![9])];
    let data: DataScript = Arc::new(Mutex::new(
        vec![
            Err(RangeError::ReadIntent { owner: owner(&[1, 1]), at }),
            Ok(DataReply::Remote(RemoteReply::Rows(vec![]))),
            Ok(DataReply::Remote(RemoteReply::Rows(rows.clone()))),
        ]
        .into(),
    ));
    let h = cores(data, Arc::default(), Arc::default());
    let sessions = h.sessions.clone();
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_secs(5));
    assert_eq!(b.scan(&[1], &[2], 10, ReadConcern::Majority), Ok(rows));
    let mut g = teseladb_wire::commands::lock(&sessions);
    assert!(matches!(g.pushes.lookup(owner(&[1, 1]).txn, at), Lookup::Push(_)));
}

/// A read that meets an owner another reader of the node is pushing waits for that push, and gives
/// up at its deadline if it never settles (008 SC-005).
#[test]
fn a_read_waits_for_another_reader_s_push_until_its_deadline() {
    use teseladb_txn::push_cache::Lookup;
    let at = Hlc { physical: 1_000, logical: 0 };
    let data: DataScript = Arc::new(Mutex::new(
        vec![Err(RangeError::ReadIntent { owner: owner(&[1, 1]), at })].into(),
    ));
    let h = cores(data, Arc::default(), Arc::default());
    let sessions = h.sessions.clone();
    let on_the_way = teseladb_wire::commands::lock(&sessions).pushes.lookup(owner(&[1, 1]).txn, at);
    assert!(matches!(on_the_way, Lookup::Push(_)));
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(100));
    let started = std::time::Instant::now();
    let e = b.scan(&[1], &[2], 10, ReadConcern::Majority).unwrap_err();
    assert_eq!(e.code.code, 89, "{e:?}");
    // The deadline is the operation's, not one that moves on with each round of waiting.
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}

/// A read whose push finds the owner still running has nothing to resolve: it reads again at its
/// mark knowing the owner was pushed above it (008 FR-003).
#[test]
fn a_read_that_pushes_a_running_owner_reads_again_past_it() {
    use teseladb_range::intents::TxnStatus;
    let at = Hlc { physical: 1_000, logical: 0 };
    let rows = vec![(vec![1, 1], vec![9])];
    let data: DataScript = Arc::new(Mutex::new(
        vec![
            Err(RangeError::ReadIntent { owner: owner(&[1, 1]), at }),
            record(TxnStatus::Pending),
            Ok(DataReply::Remote(RemoteReply::Rows(rows.clone()))),
        ]
        .into(),
    ));
    let h = cores(data, Arc::default(), Arc::default());
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_secs(5));
    assert_eq!(b.scan(&[1], &[2], 10, ReadConcern::Majority), Ok(rows));
}

/// A read that meets the intent of a committed owner resolves it and reads again at once, without
/// the pause of a refusal (008 FR-003): a statement of an encrypted insert reads the ESC position
/// the previous one just committed, so a pause there cost every insert 20 ms (017 SC-002).
#[test]
fn a_read_that_learns_its_owner_reads_again_without_a_pause() {
    use teseladb_range::intents::TxnStatus;
    const ROUNDS: usize = 25;
    let at = Hlc { physical: 1_000, logical: 0 };
    let rows = vec![(vec![1, 1], vec![9])];
    let meet = || Err(RangeError::ReadIntent { owner: owner(&[1, 1]), at });
    let resolved = Ok(DataReply::Remote(RemoteReply::Rows(vec![])));
    // The first round pushes the owner and resolves the intent; the others learn it from the
    // node's pushes.
    let mut script = vec![meet(), record(TxnStatus::Committed), resolved];
    script.extend((1..ROUNDS).map(|_| meet()));
    script.push(Ok(DataReply::Remote(RemoteReply::Rows(rows.clone()))));
    let h = cores(Arc::new(Mutex::new(script.into())), Arc::default(), Arc::default());
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_secs(5));
    let started = std::time::Instant::now();
    assert_eq!(b.scan(&[1], &[2], 10, ReadConcern::Majority), Ok(rows));
    // With a pause of 20 ms per round it took at least 500 ms.
    let took = started.elapsed();
    assert!(took < Duration::from_millis(400), "{took:?}");
}

/// What Group 0 refuses, and what it answers late, through the backend (006 FR-014).
#[test]
fn group0_refusals_and_late_answers() {
    use teseladb_meta::indexes::IndexCommand;
    let create = CatalogCommand::CreateDatabase { db: "D".into() };
    let mut b = backend(vec![], vec![CatalogAnswer::Refused(Group0Error::DatabaseDifferCase)]);
    assert_eq!(b.catalog_write(create).unwrap_err().code.code, 13297);
    let mut catalog = Catalog::default();
    catalog.collections.insert(
        ns(),
        CollectionEntry { id: 1, uuid: [2; 16], options: vec![], created_index: 3, tenant: 0 },
    );
    let mut b = backend(vec![], vec![CatalogAnswer::Catalog(Arc::new(catalog))]);
    assert_eq!(b.databases(), Ok(vec![]));
    for (answer, code) in [
        (Group0Error::NamespaceNotFound, 26),
        (Group0Error::IndexNotFound, 27),
        (Group0Error::BadSplit, 1),
    ] {
        let mut b = backend(vec![], vec![CatalogAnswer::Refused(answer)]);
        let e = b.index_write(IndexCommand::Drop { id: 4 }).unwrap_err();
        assert_eq!(e.code.code, code, "{e:?}");
    }
    // The documents of `config` once core 0 has them, or none at the deadline.
    let mut b = backend(vec![], vec![CatalogAnswer::Unavailable, CatalogAnswer::Documents(vec![])]);
    assert_eq!(b.config_documents("shards"), Ok(vec![]));
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.config_documents("shards").unwrap_err().code.code, 10107);
}

/// A transaction's reads through the backend (008 FR-003): a scan over two ranges at one mark up to
/// its limit, an answer of the wrong kind, an intent a transaction's read meets, and the pause
/// between its retries.
#[test]
fn a_transaction_s_reads_across_ranges_and_their_failures() {
    use teseladb_range::push::TxnView;
    let at = Hlc { physical: 1_000, logical: 0 };
    let view = TxnView::default();
    let rows = |r: &[(&[u8], &[u8])]| {
        let r = r.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        Ok(DataReply::Remote(RemoteReply::Rows(r)))
    };
    let mut b = backend(vec![rows(&[(&[1, 1], &[7])]), rows(&[(&[2, 1], &[8])])], vec![]);
    let both = b.txn_scan(&[1], &[3], 10, at, &view, None).unwrap();
    assert_eq!(both, vec![(vec![1, 1], vec![7]), (vec![2, 1], vec![8])]);
    // Within one range, and stopped at the limit before the end.
    let mut b = backend(vec![rows(&[(&[2, 1], &[8])])], vec![]);
    assert_eq!(b.txn_scan(&[2], &[3], 1, at, &view, None).unwrap().len(), 1);
    let mut b = backend(vec![written(&[1])], vec![]);
    assert_eq!(b.txn_scan(&[2], &[3], 1, at, &view, None).unwrap_err().code.code, 1);
    let mut b = backend(vec![rows(&[])], vec![]);
    assert_eq!(b.txn_get(&[1, 1], at, &view, None).unwrap_err().code.code, 1);
    // A transaction's read that meets an intent answers it: the transaction decides.
    let mut b = backend(vec![Err(RangeError::IntentConflict(owner(&[1, 1])))], vec![]);
    assert!(b.txn_get(&[1, 1], at, &view, None).is_err());
    let mut b = backend(vec![], vec![]);
    assert!(b.txn_pause());
    // A write without keys has no range.
    let mut no_keys = op(false);
    no_keys.keys.clear();
    assert_eq!(b.write(no_keys).unwrap_err().code.code, 1);
}

/// The node's services through the backend (013): without them each answers as RefDb does
/// without the option, and with them their failures are refusals, with the reason in the log.
#[test]
fn the_node_s_services_answer_with_or_without_them() {
    use teseladb_server::diag::Diag;
    use teseladb_server::ldap_client::{LdapOpts, LdapServer, SharedLdap};
    use teseladb_wire::commands::LdapLogin;
    let mut b = backend(vec![], vec![]);
    assert!(b.kerberos_keys().is_empty());
    assert_eq!(b.oidc_keys("https://idp", false).unwrap_err().code.code, 96);
    assert_eq!(b.aws_sts_url(), teseladb_auth::aws::DEFAULT_STS_URL);
    let req =
        teseladb_auth::aws::StsRequest { headers: vec![], body: teseladb_auth::aws::STS_BODY };
    assert_eq!(b.aws_sts(&req).unwrap_err().code.code, 96);
    assert_eq!(b.ldap_login("u", b"p"), Ok(LdapLogin::Refused));
    assert_eq!(b.ldap_groups("u"), Ok(None));
    b.invalidate_user_cache();
    assert_eq!(b.rotate_certificates(None), Ok(()));
    // Services on a port nothing listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let opts = LdapOpts {
        servers: Some(closed.to_string()),
        transport: Some("none".into()),
        timeout_ms: Some(500),
        authz_query_template: Some("{USER}?memberOf".into()),
        ..LdapOpts::default()
    };
    let ldap = LdapServer::from_opts(&opts).unwrap().expect("a server");
    let sts = teseladb_server::sts_client::Sts::from_url(Some(&format!("https://{closed}")), false)
        .unwrap();
    let diag = Diag::new(Default::default(), 0, Arc::default(), 1)
        .with_kerberos(Some(Arc::new(vec![])))
        .with_sts(Some(Arc::new(sts)))
        .with_ldap(Some(SharedLdap(Arc::new(ldap))));
    let mut b = backend(vec![], vec![]).with_diag(Arc::new(diag));
    assert!(b.kerberos_keys().is_empty());
    assert_eq!(b.aws_sts_url(), format!("https://{closed}"));
    assert_eq!(b.ldap_login("u", b"p"), Ok(LdapLogin::Refused));
    assert_eq!(b.ldap_groups("u").unwrap_err().code.code, 18);
    b.invalidate_user_cache();
}

/// The last range of the table has no end: a transaction's scan and the count of voters read to the
/// end of what they ask for there; a scan stops at its limit before its end; a primary read
/// selects no replica (008 FR-003, 007 FR-003).
#[test]
fn reads_up_to_the_last_range_and_to_the_limit() {
    use teseladb_range::push::TxnView;
    use teseladb_router::read_pref::{Mode, ReadPreference};
    let at = Hlc { physical: 1_000, logical: 0 };
    let view = TxnView::default();
    let rows = |r: Vec<(Vec<u8>, Vec<u8>)>| Ok(DataReply::Remote(RemoteReply::Rows(r)));
    let data: DataScript = Arc::new(Mutex::new(vec![rows(vec![(vec![2, 1], vec![8])])].into()));
    let h = cores(data, Arc::default(), Arc::default());
    h.routes.publish(&[range(1, &[1], &[2]), range(2, &[2], &[])]);
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300));
    assert_eq!(b.txn_scan(&[2], &[9], 10, at, &view, None).unwrap().len(), 1);
    assert_eq!(b.voters(&[2], &[9]), Ok(1));
    let mut b = backend(vec![rows(vec![(vec![1, 1], vec![7])])], vec![]);
    assert_eq!(b.txn_scan(&[1], &[3], 1, at, &view, None).unwrap().len(), 1);
    let primary = ReadPreference { mode: Mode::Primary, max_staleness_ms: None, tag_sets: vec![] };
    let mut b = backend(vec![rows(vec![])], vec![]);
    b.set_read_preference(Some(primary));
    assert_eq!(b.scan(&[1], &[2], 10, ReadConcern::Local), Ok(vec![]));
}

/// A commit's wait for the clock to reach its mark (008 FR-008): at once for a mark behind, after
/// a pause for one just ahead, and not past the operation's deadline.
#[test]
fn a_commit_waits_for_the_clock_up_to_its_deadline() {
    let mut b = backend(vec![], vec![]);
    assert!(b.txn_wait_clock(wall_ms() - 1));
    assert!(b.txn_wait_clock(wall_ms() + 20));
    assert!(!b.txn_wait_clock(wall_ms() + 60_000));
}

/// A finished owner whose intent's key has no range in the table is left to the next reader; the
/// read goes on knowing the owner (008 FR-003).
#[test]
fn a_read_leaves_an_intent_without_a_range_to_the_next_reader() {
    use teseladb_range::intents::TxnStatus;
    let at = Hlc { physical: 1_000, logical: 0 };
    let data: DataScript = Arc::new(Mutex::new(
        vec![
            Err(RangeError::ReadIntent { owner: owner(&[9]), at }),
            record(TxnStatus::Committed),
            Ok(DataReply::Remote(RemoteReply::Rows(vec![]))),
        ]
        .into(),
    ));
    let h = cores(data, Arc::default(), Arc::default());
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_secs(5));
    assert_eq!(b.scan(&[1], &[2], 10, ReadConcern::Majority), Ok(vec![]));
}

/// A change stream's read of a range: its rows up to `to`, with its oldest pending intent.
fn changes(rows: Vec<(u8, u64)>, to: u64, intent: Option<u64>) -> Result<DataReply, RangeError> {
    let ts = |p: u64| Hlc { physical: p, logical: 0 };
    let rows = rows
        .into_iter()
        .map(|(k, p)| teseladb_range::changes::Versioned {
            key: vec![k],
            ts: ts(p),
            value: Some((1, vec![k])),
        })
        .collect();
    Ok(DataReply::Remote(RemoteReply::Changes { rows, to: ts(to), oldest_intent: intent.map(ts) }))
}

#[test]
fn a_change_read_walks_every_range_and_resolves_at_the_lowest_mark() {
    let ts = |p: u64| Hlc { physical: p, logical: 0 };
    // Range 1 is resolved to 30; range 2 to 50 but holds an intent at 25: the batch stops at 24.
    let data = vec![
        changes(vec![(1, 20), (1, 28)], 30, None),
        changes(vec![(2, 22), (3, 40)], 50, Some(25)),
    ];
    let mut b = backend(data, vec![]);
    let (rows, resolved) = b.changes(&[1], &[4], ts(10)).unwrap();
    assert_eq!(resolved, Hlc { physical: 24, logical: u32::MAX });
    let seen: Vec<(u8, u64)> = rows.iter().map(|r| (r.key[0], r.ts.physical)).collect();
    assert_eq!(seen, [(1, 20), (2, 22)], "nothing above the resolved mark");
    // A range with nothing closed past the start keeps the batch at the start.
    let mut b = backend(vec![changes(vec![], 5, None), changes(vec![], 60, None)], vec![]);
    assert_eq!(b.changes(&[1], &[4], ts(10)).unwrap(), (vec![], ts(10)));
    // An interval inside one range reads only that range.
    let mut b = backend(vec![changes(vec![(2, 12)], 15, None)], vec![]);
    let (rows, resolved) = b.changes(&[2], &[3], ts(10)).unwrap();
    assert_eq!((rows.len(), resolved), (1, ts(15)));
    // A range that answers something else is an internal error.
    let mut b = backend(vec![written(b"x")], vec![]);
    assert_eq!(b.changes(&[1], &[2], ts(10)).unwrap_err().code.code, 1);
}

#[test]
fn a_change_read_moved_by_a_split_or_a_new_leader_reads_each_key_once() {
    // 014 FR-010: a range that split under the read (`OutOfRange`) or changed leader is read
    // again with the table as it is then; every key comes once and the mark is the lowest.
    let ts = |p: u64| Hlc { physical: p, logical: 0 };
    let data = vec![
        Err(RangeError::OutOfRange),
        changes(vec![(1, 20)], 30, None),
        Err(RangeError::NotLeader { hint: NodeId(2) }),
        changes(vec![(2, 22), (3, 26)], 25, None),
    ];
    let mut b = backend(data, vec![]);
    let (rows, resolved) = b.changes(&[1], &[4], ts(10)).unwrap();
    let seen: Vec<(u8, u64)> = rows.iter().map(|r| (r.key[0], r.ts.physical)).collect();
    assert_eq!((seen, resolved), (vec![(1, 20), (2, 22)], ts(25)));
    // 014 FR-018: each range read leaves its lag behind the clock in the metrics for a while.
    let lags = teseladb_server::backend::range_lags(wall_ms());
    assert!(lags.iter().any(|(_, lag)| *lag + 30 >= wall_ms()), "{lags:?}");
    assert!(teseladb_server::backend::range_lags(wall_ms() + 60_000).is_empty());
}

#[test]
fn a_change_stream_reads_the_catalog_changes_and_moves_a_quiet_group_0() {
    // 014 FR-014: the copy's changes and its last mark; a mark far behind the node's clock asks
    // Group 0 for an empty entry and reads again.
    let fresh = Hlc { physical: wall_ms() + 60_000, logical: 0 };
    let old = Hlc { physical: 1, logical: 0 };
    let floor = Hlc { physical: 0, logical: 0 };
    let answer = |at| CatalogAnswer::Ddl(Vec::new(), floor, at);
    let mut b = backend(vec![], vec![CatalogAnswer::Unavailable, answer(fresh)]);
    let batch = b.catalog_changes(floor).unwrap();
    assert_eq!((batch.records.len(), batch.floor, batch.resolved), (0, floor, fresh));
    thread::sleep(Duration::from_millis(60));
    let mut b = backend(vec![], vec![answer(old), CatalogAnswer::Done(None), answer(fresh)]);
    assert_eq!(b.catalog_changes(floor).unwrap().resolved, fresh, "after the empty entry");
    // Another read at once does not ask again: it answers what the copy has.
    let mut b = backend(vec![], vec![answer(old)]);
    assert_eq!(b.catalog_changes(floor).unwrap().resolved, old);
    thread::sleep(Duration::from_millis(60));
    let refused = CatalogAnswer::Refused(Group0Error::NotBootstrapped);
    let mut b = backend(vec![], vec![answer(old), refused]);
    assert_eq!(b.catalog_changes(floor).unwrap_err().code.code, 1);
    let mut b = backend(vec![], vec![]);
    assert_eq!(b.catalog_changes(floor).unwrap_err().code.code, 10107);
}

/// 007 FR-025 and 010 T033: a tenant's number and the Group 0 settings once core 0 has Group 0, or
/// unavailable at the deadline; the other nodes' operations, a bad list or no answer.
#[test]
fn tenants_settings_and_other_operations_wait_for_core_zero() {
    use std::collections::BTreeMap;
    use teseladb_bson::{Document, Value};
    let tenants = CatalogAnswer::Tenants(vec![(4, "acme".into())]);
    let mut b = backend(vec![], vec![CatalogAnswer::Unavailable, tenants.clone(), tenants]);
    assert_eq!(b.tenant_named("acme"), Ok(Some(4)));
    assert_eq!(b.tenant_named("other"), Ok(None));
    assert_eq!(b.tenant_named("acme").unwrap_err().code.code, 10107);
    let settings: BTreeMap<Vec<u8>, Vec<u8>> =
        [(b"zone/a".to_vec(), vec![1]), (b"other".to_vec(), vec![2])].into();
    let answers = vec![CatalogAnswer::Unavailable, CatalogAnswer::Settings(settings)];
    let mut b = backend(vec![], answers);
    let zones = b.group0_settings(b"zone/").unwrap();
    assert_eq!(zones.into_iter().collect::<Vec<_>>(), vec![(b"zone/a".to_vec(), vec![1])]);
    assert_eq!(b.group0_settings(b"zone/").unwrap_err().code.code, 10107);
    let op = Document::new().with("opid", 9);
    let listed = Document::new()
        .with("ops", Value::Array(vec![Value::Document(op.clone()), Value::Int32(1)]));
    let script = vec![
        Some(vec![listed.to_bytes().unwrap(), Document::new().to_bytes().unwrap()]),
        Some(vec![vec![1, 2]]),
        None,
    ];
    let h = cores_with_ops(
        Arc::default(),
        Arc::default(),
        Arc::default(),
        Arc::new(Mutex::new(script.into())),
    );
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300));
    assert_eq!(b.other_operations(), Ok(vec![op]));
    assert_eq!(b.other_operations().unwrap_err().errmsg, "a node answered a bad list");
    for _ in 0..2 {
        let e = b.other_operations().unwrap_err();
        assert_eq!(e.errmsg, "a node of the cluster did not answer the list of its operations");
    }
}

/// 008 FR-014: the reaper reads the records of a range it leads page by page, skipping a row that
/// is not a record; a range this node does not lead is left alone.
#[test]
fn the_reaper_reads_every_page_of_the_records_it_leads() {
    use std::collections::BTreeMap;
    use teseladb_server::data::DataStatus;
    let leader = DataStatus { role: Some(teseladb_raft::Role::Leader), ..DataStatus::default() };
    let page: Vec<(Vec<u8>, Vec<u8>)> = (0..256u16)
        .map(|i| ([2, 0].into_iter().chain(i.to_be_bytes()).collect(), vec![0xff]))
        .collect();
    let data = vec![
        Ok(DataReply::Status(leader.clone())),
        Ok(DataReply::Remote(RemoteReply::Rows(page))),
        Ok(DataReply::Remote(RemoteReply::Rows(vec![(vec![2, 1], vec![1, 2, 3])]))),
        Ok(DataReply::Status(leader.clone())),
        Ok(DataReply::Remote(RemoteReply::Rows(vec![]))),
        Ok(DataReply::Status(DataStatus::default())),
        Ok(DataReply::Status(leader)),
    ];
    let mut b = backend(data, vec![]);
    // Each pass creates the reaper of the range's core once its records are read: the second
    // pass finds its leader's answer next, so the first read both pages.
    let pass = |b: &mut ServerBackend| {
        let mut reapers = BTreeMap::new();
        assert_eq!(b.reap_transactions(&mut reapers), 0);
        reapers.len()
    };
    assert_eq!(pass(&mut b), 1);
    assert_eq!(pass(&mut b), 1);
    // A follower, then a leader whose scan gets no answer: neither is read.
    assert_eq!(pass(&mut b), 0);
    assert_eq!(pass(&mut b), 0);
}

/// The reads of several keys of a transaction go to their ranges together and each key gets its
/// own answer; a read without one (here a leader that changed) goes again on its own (017
/// SC-002).
#[test]
fn reads_of_several_keys_go_together_and_each_gets_its_answer() {
    use teseladb_range::push::TxnView;
    let at = Hlc { physical: 1_000, logical: 0 };
    let view = TxnView::default();
    let rows = |r: Vec<(Vec<u8>, Vec<u8>)>| Ok(DataReply::Remote(RemoteReply::Rows(r)));
    let moved = Err(RangeError::NotLeader { hint: NodeId(0) });
    let keys = vec![vec![1, 1], vec![1, 2], vec![2, 5]];
    let mut b = backend(
        vec![
            rows(vec![(vec![1, 1], vec![7])]),
            rows(vec![]),
            moved,
            rows(vec![(vec![2, 5], vec![9])]),
        ],
        vec![],
    );
    let found = b.txn_points(&keys, at, &view, None).unwrap();
    assert_eq!(found, vec![Some(vec![7]), None, Some(vec![9])]);
    let value =
        |v: Option<(u8, Vec<u8>)>| Ok(DataReply::Remote(RemoteReply::Value { ts: at, value: v }));
    let mut b =
        backend(vec![value(Some((3, vec![1]))), value(None), value(Some((3, vec![2])))], vec![]);
    let found = b.txn_gets(&keys, at, &view, None).unwrap();
    assert_eq!(found, vec![Some((3, vec![1])), None, Some((3, vec![2]))]);
}

/// A commit's answer carries its mark into this node's clock: the next transaction of the node
/// reads at or above what it committed and does not write under it (017 SC-002, 008 FR-008).
#[test]
fn a_committed_mark_moves_the_clock_of_the_node() {
    use teseladb_range::host::TxnOp;
    use teseladb_range::intents::{TxnId, TxnRecord, TxnStatus};
    // A mark of another node in the same millisecond and sequence as the clock takes it.
    let ahead = Hlc { physical: u64::from(u32::MAX) * 100, logical: 9 };
    let committed = Ok(DataReply::Remote(RemoteReply::Record(TxnRecord {
        txn: TxnId(6),
        lsid: 0,
        txn_number: 0,
        status: TxnStatus::Committed,
        read_ts: ahead,
        min_commit_ts: ahead,
        commit_ts: ahead,
        heartbeat_ts: ahead,
        spans: vec![],
    })));
    let mut b = backend(vec![committed], vec![]);
    assert!(b.cluster_mark() < ahead);
    let op = TxnOp::ReadRecord { txn: TxnId(6) };
    assert!(b.txn_send(GroupId(1), op).is_ok());
    assert!(b.cluster_mark() >= ahead, "{:?}", b.cluster_mark());
}

/// The informative commands that read the routing table (`route_entries`: shards, chunks,
/// `getShardVersion`) see the whole cluster: on a node that is starting, whose table does not hold
/// every range yet, the read waits for it as a write waits for its key's range, and only at the
/// deadline answers the ranges' time-out instead of a part of the table.
#[test]
fn the_routing_table_is_read_once_it_holds_the_whole_key_space() {
    // The data space is `[01, 04)`: a node still filling its table holds `[1, 2)` alone.
    let h = cores(Arc::default(), Arc::default(), Arc::default());
    h.routes.publish(&[range(1, &[1], &[2])]);
    let routes = h.routes.clone();
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_secs(5));
    let filler = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        routes.publish(&[range(1, &[1], &[2]), range(2, &[2], &[4])]);
    });
    let entries = b.route_entries().unwrap();
    filler.join().unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
    // A table that never fills: the ranges' time-out (NetworkTimeout) at the deadline.
    let h = cores(Arc::default(), Arc::default(), Arc::default());
    h.routes.publish(&[range(1, &[1], &[2])]);
    let mut b = ServerBackend::new(h, 7).with_deadline(Duration::from_millis(300));
    assert_eq!(b.route_entries().unwrap_err().code.code, 89);
}
