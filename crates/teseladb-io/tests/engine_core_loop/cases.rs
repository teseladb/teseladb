//! The cases of `engine_core_loop`, run in order in one engine.

use std::io::Write;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::engine::{CoreInput, EngineConfig, NodeEngine};
use teseladb_io::handshake;
use teseladb_io::peer::{Hello, encode_frame};
use teseladb_raftlog::{Disk, IoDone, IoOp, MemDisk};
use teseladb_types::{Bytes, NodeId};

use crate::toy::*;

type Case = fn(&NodeEngine);

pub fn run() {
    let engine = NodeEngine::start(&EngineConfig { shards: 2, ..EngineConfig::default() }).unwrap();
    let cases: &[(&str, Case)] = &[
        ("two_nodes_play_ping_pong_and_tick", two_nodes_play_ping_pong_and_tick),
        ("disk_operations_complete_in_order", disk_operations_complete_in_order),
        ("a_refused_dial_is_retried_with_the_new_address", a_refused_dial_is_retried),
        ("a_reply_goes_to_the_address_the_peer_announced_last", a_reply_goes_to_the_announced),
        ("a_stale_announced_address_gives_way", a_stale_announced_address_gives_way),
        ("every_tick_records_how_late_it_ran", every_tick_records_how_late_it_ran),
        ("a_peer_that_goes_away_is_reported_unreachable", a_peer_that_goes_away),
        ("an_unknown_address_drops_the_frames", an_unknown_address_drops_the_frames),
        ("garbage_after_the_handshake_closes_the_connection", garbage_after_the_handshake),
        ("a_dial_to_another_identity_fails", a_dial_to_another_identity_fails),
        ("a_slow_reader_gets_everything_once_it_reads", a_slow_reader_gets_everything),
        ("an_oversized_frame_is_dropped_and_the_link_keeps_working", an_oversized_frame_is_dropped),
        ("garbage_after_the_peers_hello_closes_the_new_link", garbage_after_the_peers_hello),
        ("the_core_stops_when_its_disk_dies", the_core_stops_when_its_disk_dies),
        ("the_acceptor_closes_failed_handshakes", the_acceptor_closes_failed_handshakes),
        ("a_stop_waits_for_the_writes_the_logic_still_holds", a_stop_waits_for_the_writes),
        ("a_stop_does_not_wait_for_a_dead_disk", a_stop_does_not_wait_for_a_dead_disk),
        ("a_core_at_rest_ticks_ten_times_less", a_core_at_rest_ticks_ten_times_less),
        ("a_core_whose_logic_is_busy_never_rests", a_core_whose_logic_is_busy_never_rests),
        ("an_overloaded_core_refuses_new_requests", an_overloaded_core_refuses_new_requests),
        ("a_core_that_keeps_up_refuses_nothing", a_core_that_keeps_up_refuses_nothing),
        ("document_operations_go_ahead_of_long_scans", document_operations_go_ahead_of_scans),
        (
            "a_long_turn_gives_the_shard_back_between_messages",
            a_long_turn_gives_the_shard_back_between_messages,
        ),
        ("background_requests_waiting_at_a_stop_are_answered", background_at_a_stop_answered),
        ("a_core_reaches_another_core_from_its_shard", a_core_reaches_another_core),
        ("the_logic_is_told_the_runtime_sends_no_files", the_logic_is_told_no_file_sends),
        ("a_core_is_built_on_a_deep_stack", a_core_is_built_on_a_deep_stack),
        ("a_core_s_first_turn_runs_on_a_deep_stack", a_core_s_first_turn_runs_on_a_deep_stack),
    ];
    // `CASES=a,b` runs only those.
    let only = std::env::var("CASES").ok();
    let wanted = |name: &str| only.as_deref().is_none_or(|o| o.split(',').any(|w| w == name));
    for (name, case) in cases.iter().filter(|(n, _)| wanted(n)) {
        case(&engine);
        println!("{name}: ok");
    }
    let sender = {
        let n = start(&engine, 0, 90, Toy::default());
        let tx = n.tx.clone();
        drop(stop(&engine, n));
        tx
    };
    engine.stop();
    // The engine is gone: nothing reaches its shards any more.
    assert!(sender.is_closed());
    assert!(sender.send(CoreInput::App(Cmd::Request(1))).is_err());
    assert!(sender.send_waiting(CoreInput::Stop).is_err());
    println!("a_sender_of_a_stopped_engine_says_so: ok");
}

fn two_nodes_play_ping_pong_and_tick(e: &NodeEngine) {
    let (a_ticks, b_seen, b_ticks) = (Arc::default(), Arc::default(), Arc::default());
    let a = start(e, 0, 1, Toy { shared_ticks: Arc::clone(&a_ticks), ..Toy::default() });
    let toy_b = Toy {
        shared_seen: Arc::clone(&b_seen),
        shared_ticks: Arc::clone(&b_ticks),
        ..Toy::default()
    };
    let b = start(e, 1, 2, toy_b);
    send(&a, Cmd::Addr(NodeId(2), b.addr.to_string()));
    send(&b, Cmd::Addr(NodeId(1), a.addr.to_string()));
    send(&b, Cmd::Ping(NodeId(1), 0));
    send(&a, Cmd::Ping(NodeId(2), 1_000));
    // Until the exchange ends and both cores ticked, however loaded the machine is.
    wait(|| load(&b_seen) >= 1_000 && load(&a_ticks) > 10 && load(&b_ticks) > 10);
    let (ta, _, _) = stop(e, a);
    let (tb, _, _) = stop(e, b);
    // A stops answering at its limit, so the last request B saw is the limit.
    assert!(tb.last_seen >= 1_000, "B saw {}", tb.last_seen);
    assert!(ta.ticks > 10 && tb.ticks > 10, "ticks {} {}", ta.ticks, tb.ticks);
    // Every turn that brought a frame read the wall clock first, many times between two ticks
    // (005 FR-032).
    assert!(ta.clocks > ta.ticks && tb.clocks > tb.ticks, "clocks {} {}", ta.clocks, tb.clocks);
}

fn disk_operations_complete_in_order(e: &NodeEngine) {
    let a = start(e, 0, 3, Toy::default());
    send(&a, Cmd::Disk);
    thread::sleep(Duration::from_millis(100));
    let (t, disk, gone) = stop(e, a);
    assert!(!gone);
    assert_eq!(t.done.len(), 3);
    assert_eq!(t.done[2], IoDone::Read { token: 9, data: Ok(Bytes::from_vec(b"abc".to_vec())) });
    assert_eq!(disk.unwrap().read_all(&PathBuf::from("x")).unwrap(), b"abc");
}

fn a_refused_dial_is_retried(e: &NodeEngine) {
    let refused = Arc::default();
    let heard = Arc::default();
    let a = start(e, 0, 4, Toy { shared_unreachable: Arc::clone(&refused), ..Toy::default() });
    let b = start(e, 1, 5, Toy { shared_seen: Arc::clone(&heard), ..Toy::default() });
    // A closed port refuses the first dials.
    send(&a, Cmd::Addr(NodeId(5), free_port().to_string()));
    send(&a, Cmd::Ping(NodeId(5), 3));
    wait(|| load(&refused) > 0);
    send(&a, Cmd::Addr(NodeId(5), b.addr.to_string()));
    send(&b, Cmd::Addr(NodeId(4), a.addr.to_string()));
    // The frame was dropped with the failed dial: new pings go out after the backoff.
    let deadline = Instant::now() + Duration::from_secs(20);
    while load(&heard) == 0 {
        assert!(Instant::now() < deadline, "B never heard from A");
        send(&a, Cmd::Ping(NodeId(5), 3));
        thread::sleep(Duration::from_millis(200));
    }
    let (ta, _, _) = stop(e, a);
    let (tb, _, _) = stop(e, b);
    assert!(ta.unreachable.contains(&NodeId(5)));
    assert!(tb.last_seen >= 1, "B never heard from A");
}

fn a_reply_goes_to_the_announced(e: &NodeEngine) {
    let a = start(e, 0, 10, Toy::default());
    let b = start(e, 1, 11, Toy::default());
    // A's logic still has B at an address nobody listens on (B came back on another port).
    send(&a, Cmd::Addr(NodeId(11), free_port().to_string()));
    send(&b, Cmd::Addr(NodeId(10), a.addr.to_string()));
    // B dials A; A answers by dialing B at the address of B's `Hello`.
    send(&b, Cmd::Ping(NodeId(10), 1));
    thread::sleep(Duration::from_millis(500));
    let (ta, _, _) = stop(e, a);
    let (tb, _, _) = stop(e, b);
    assert_eq!(tb.last_seen, 2, "A's answer never reached B");
    assert!(ta.unreachable.is_empty(), "A dialed the old address");
}

fn a_stale_announced_address_gives_way(e: &NodeEngine) {
    let a = start(e, 0, 12, Toy::default());
    // B dials A from a port it then leaves: A learns an address that dies.
    let b = start(e, 1, 13, Toy::default());
    send(&b, Cmd::Addr(NodeId(12), a.addr.to_string()));
    send(&b, Cmd::Ping(NodeId(12), 1));
    thread::sleep(Duration::from_millis(200));
    drop(stop(e, b));
    // B comes back elsewhere and does not dial; A's logic has the new address.
    let b = start(e, 1, 13, Toy::default());
    send(&a, Cmd::Addr(NodeId(13), b.addr.to_string()));
    for _ in 0..30 {
        send(&a, Cmd::Ping(NodeId(13), 1));
        thread::sleep(Duration::from_millis(100));
    }
    let (tb, _, _) = stop(e, b);
    drop(stop(e, a));
    assert!(tb.last_seen >= 1, "A kept dialing the address B left");
}

/// Each tick records how late it ran past the time it was due (012 FR-015, `core_tick_late`).
fn every_tick_records_how_late_it_ran(e: &NodeEngine) {
    let ticks = teseladb_stages::count(teseladb_stages::Stage::CoreTickLate);
    let a = start(e, 0, 9, Toy::default());
    thread::sleep(Duration::from_millis(100));
    drop(stop(e, a));
    // Ticks of 5 ms for 100 ms: more than a few.
    assert!(teseladb_stages::count(teseladb_stages::Stage::CoreTickLate) >= ticks + 5);
}

fn a_peer_that_goes_away(e: &NodeEngine) {
    let a = start(e, 0, 6, Toy::default());
    let b = start(e, 1, 7, Toy::default());
    send(&a, Cmd::Addr(NodeId(7), b.addr.to_string()));
    send(&a, Cmd::Ping(NodeId(7), 1));
    thread::sleep(Duration::from_millis(200));
    let failed = teseladb_io::counters::peer_links_failed();
    drop(stop(e, b));
    // The closed connection is noticed when A next reads or writes on it.
    for _ in 0..20 {
        send(&a, Cmd::Ping(NodeId(7), 1));
        thread::sleep(Duration::from_millis(20));
    }
    let (ta, _, _) = stop(e, a);
    assert!(ta.unreachable.contains(&NodeId(7)));
    // The metric counts the link that went down (012 FR-015).
    assert!(teseladb_io::counters::peer_links_failed() > failed);
}

fn an_unknown_address_drops_the_frames(e: &NodeEngine) {
    let a = start(e, 0, 8, Toy::default());
    send(&a, Cmd::Ping(NodeId(99), 1));
    thread::sleep(Duration::from_millis(50));
    let (ta, _, _) = stop(e, a);
    assert!(ta.unreachable.is_empty());
}

fn garbage_after_the_handshake(e: &NodeEngine) {
    let a = start(e, 1, 9, Toy::default());
    let mut s = TcpStream::connect(a.addr).unwrap();
    let got = handshake::dial(&mut s, &hello(10, "x")).unwrap();
    assert_eq!(got.hello.node_id, NodeId(9));
    // A good frame first, then one with an unknown kind, then another good one.
    let mut good = Vec::new();
    encode_frame(&ack(5), false, &mut good).unwrap();
    s.write_all(&good).unwrap();
    s.write_all(&[4, 0, 0, 0, 0xEE, 0, 0, 0]).unwrap();
    let mut after = Vec::new();
    encode_frame(&ack(6), false, &mut after).unwrap();
    let _ = s.write_all(&after);
    thread::sleep(Duration::from_millis(100));
    let (ta, _, _) = stop(e, a);
    assert_eq!(ta.last_seen, 5, "a frame after an invalid one was delivered");
}

fn a_dial_to_another_identity_fails(e: &NodeEngine) {
    let a = start(e, 0, 30, Toy::default());
    // Node 31's address answers as node 32.
    let (addr, peer) = fake_peer(32, b"");
    send(&a, Cmd::Addr(NodeId(31), addr));
    send(&a, Cmd::Bulk(NodeId(31), 1, 10));
    let _s = peer.join().unwrap();
    thread::sleep(Duration::from_millis(200));
    let (ta, _, _) = stop(e, a);
    assert!(ta.unreachable.contains(&NodeId(31)));
}

fn a_slow_reader_gets_everything(e: &NodeEngine) {
    let a = start(e, 0, 20, Toy { deadline: true, ..Toy::default() });
    let (addr, peer) = fake_peer(21, b"");
    send(&a, Cmd::Addr(NodeId(21), addr));
    // 24 MiB with nobody reading: the socket buffers fill and the rest waits for writability.
    send(&a, Cmd::Bulk(NodeId(21), 24, 1 << 20));
    let mut s = peer.join().unwrap();
    thread::sleep(Duration::from_millis(200));
    // What went out before the connection was up was queued (up to 8 MiB); the rest of the burst
    // was dropped with it, and what comes after goes on the connection.
    let more = 6;
    send(&a, Cmd::Bulk(NodeId(21), more, 1 << 20));
    let got = count_goodbyes(&mut s, 8 + more - 1);
    assert!(got >= 8 + more - 1);
    let (ta, _, _) = stop(e, a);
    assert!(ta.unreachable.is_empty());
}

fn an_oversized_frame_is_dropped(e: &NodeEngine) {
    let a = start(e, 0, 22, Toy::default());
    let (addr, peer) = fake_peer(23, b"");
    send(&a, Cmd::Addr(NodeId(23), addr));
    send(&a, Cmd::Bulk(NodeId(23), 1, 17 << 20));
    send(&a, Cmd::Bulk(NodeId(23), 2, 10));
    let mut s = peer.join().unwrap();
    assert_eq!(count_goodbyes(&mut s, 2), 2);
    drop(stop(e, a));
}

fn garbage_after_the_peers_hello(e: &NodeEngine) {
    let a = start(e, 0, 24, Toy::default());
    let (addr, peer) = fake_peer(25, &[4, 0, 0, 0, 0xEE, 0, 0, 0]);
    send(&a, Cmd::Addr(NodeId(25), addr));
    send(&a, Cmd::Bulk(NodeId(25), 1, 10));
    let _s = peer.join().unwrap();
    thread::sleep(Duration::from_millis(200));
    let (ta, _, _) = stop(e, a);
    assert!(ta.unreachable.contains(&NodeId(25)));
}

struct PanicDisk;

impl Disk for PanicDisk {
    fn list(&mut self, _dir: &std::path::Path) -> Result<Vec<PathBuf>, teseladb_raftlog::IoErrno> {
        Ok(Vec::new())
    }
    fn read_all(&mut self, _path: &std::path::Path) -> Result<Vec<u8>, teseladb_raftlog::IoErrno> {
        Ok(Vec::new())
    }
    fn execute(&mut self, _op: IoOp) -> IoDone {
        panic!("disk failure in the test");
    }
}

fn start_panicking(e: &NodeEngine, id: u64) -> Node<PanicDisk> {
    start_with(e, 0, id, Toy::default(), PanicDisk, Duration::from_millis(10), None)
}

fn the_core_stops_when_its_disk_dies(e: &NodeEngine) {
    let a = start_panicking(e, 26);
    // The first operation kills the disk thread; a later turn finds it gone and stops the core.
    for _ in 0..50 {
        if a.tx.send(CoreInput::App(Cmd::Disk)).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let (toy, disk, gone) = stop(e, a);
    assert!(gone);
    assert!(disk.is_none());
    assert!(toy.done.is_empty());
}

fn the_acceptor_closes_failed_handshakes(e: &NodeEngine) {
    let a = start(e, 1, 27, Toy::default());
    // Garbage instead of a Hello, and a peer that closes without saying anything.
    let mut s = TcpStream::connect(a.addr).unwrap();
    s.write_all(&[4, 0, 0, 0, 0xEE, 0, 0, 0]).unwrap();
    let s2 = TcpStream::connect(a.addr).unwrap();
    s2.shutdown(std::net::Shutdown::Write).unwrap();
    // A peer of another cluster.
    let mut s3 = TcpStream::connect(a.addr).unwrap();
    let other = Hello { cluster_id: [1; 16], ..hello(28, "x") };
    assert!(handshake::dial(&mut s3, &other).is_err());
    thread::sleep(Duration::from_millis(100));
    let (ta, _, _) = stop(e, a);
    assert_eq!(ta.last_seen, 0);
}

fn a_stop_waits_for_the_writes(e: &NodeEngine) {
    let a = start_with(e, 0, 42, Toy::default(), MemDisk::new(), Duration::from_millis(10), None);
    // Both reach the same turn: the creation goes to the disk, the write waits for it, and the
    // stop comes before its result.
    send(&a, Cmd::OneByOne);
    let (toy, disk, _) = stop(e, a);
    assert_eq!(toy.done.len(), 2, "{:?}", toy.done);
    assert!(toy.io.is_empty() && toy.in_flight == 0);
    let mut disk = disk.expect("the disk thread lives");
    assert_eq!(disk.read_all(std::path::Path::new("x")).unwrap(), b"abc");
}

fn a_stop_does_not_wait_for_a_dead_disk(e: &NodeEngine) {
    let a = start_panicking(e, 43);
    // The creation kills the disk thread: its result never comes, and the stop ends anyway.
    send(&a, Cmd::OneByOne);
    let begun = Instant::now();
    let (toy, disk, _) = stop(e, a);
    assert!(disk.is_none());
    assert!(toy.done.is_empty());
    assert!(begun.elapsed() < Duration::from_secs(5), "{:?}", begun.elapsed());
}

fn a_core_at_rest_ticks_ten_times_less(e: &NodeEngine) {
    let (ticks, wakes) = (Arc::default(), Arc::default());
    let toy = Toy {
        rests: true,
        shared_ticks: Arc::clone(&ticks),
        shared_wakes: Arc::clone(&wakes),
        ..Toy::default()
    };
    let a = start_with(e, 1, 60, toy, MemDisk::new(), Duration::from_millis(10), None);
    // A second at 10 ms before it rests, then 100 ms: about 100 + 10 in 2 s, against 200.
    thread::sleep(Duration::from_secs(2));
    let rested = load(&ticks);
    assert!(rested <= 140, "{rested} ticks in 2 s");
    // Work ends the rest: the turn that takes it wakes the logic first, with a fresh wall clock.
    send(&a, Cmd::Addr(NodeId(61), "x".into()));
    wait(|| load(&wakes) == 1);
    // And the ticks go back to every 10 ms: about 20 in 200 ms, against 2 at rest.
    thread::sleep(Duration::from_millis(200));
    let (t, _, _) = stop(e, a);
    assert_eq!(t.wakes, 1, "the clock part of a tick before the work");
    assert!(t.ticks >= rested + 8, "{} ticks", t.ticks - rested);
}

fn a_core_whose_logic_is_busy_never_rests(e: &NodeEngine) {
    let a = start_with(e, 0, 62, Toy::default(), MemDisk::new(), Duration::from_millis(10), None);
    thread::sleep(Duration::from_millis(1_500));
    let (t, _, _) = stop(e, a);
    // Every 10 ms all along: about 150.
    assert!(t.ticks >= 120, "{} ticks", t.ticks);
}

fn an_overloaded_core_refuses_new_requests(e: &NodeEngine) {
    // 012 FR-029: twenty stalls of 10 ms queued at once keep each message waiting more than 5 ms
    // for well over 100 ms; the requests behind them are refused, and once the queue is empty a
    // new request is served.
    let a = start(e, 0, 20, Toy::default());
    for _ in 0..20 {
        send(&a, Cmd::Stall(Duration::from_millis(10)));
    }
    for id in 1..=5 {
        send(&a, Cmd::Request(id));
    }
    // A stall alone waits for nothing: once it has run, the core admits again.
    thread::sleep(Duration::from_millis(400));
    send(&a, Cmd::Stall(Duration::ZERO));
    thread::sleep(Duration::from_millis(50));
    send(&a, Cmd::Request(6));
    thread::sleep(Duration::from_millis(50));
    let (t, _, _) = stop(e, a);
    assert_eq!(t.refused, vec![1, 2, 3, 4, 5]);
    assert_eq!(t.served, vec![6]);
}

fn a_core_that_keeps_up_refuses_nothing(e: &NodeEngine) {
    let a = start(e, 0, 21, Toy::default());
    for id in 0..200 {
        send(&a, Cmd::Request(id));
        if id % 20 == 0 {
            thread::sleep(Duration::from_millis(2));
        }
    }
    thread::sleep(Duration::from_millis(100));
    let (t, _, _) = stop(e, a);
    assert!(t.refused.is_empty(), "{:?}", t.refused);
    assert_eq!(t.served.len(), 200);
}

fn document_operations_go_ahead_of_scans(e: &NodeEngine) {
    // 012 FR-030: scans and requests queued behind a busy stretch come out with the requests
    // first; the scans run once nothing else waits, and all of them are answered.
    let waits = teseladb_stages::count(teseladb_stages::Stage::BackgroundWait);
    let a = start(e, 0, 22, Toy::default());
    send(&a, Cmd::Stall(Duration::from_millis(30)));
    thread::sleep(Duration::from_millis(5));
    for id in [101, 102] {
        send(&a, Cmd::Scan(id));
    }
    for id in [1, 2] {
        send(&a, Cmd::Request(id));
    }
    thread::sleep(Duration::from_millis(100));
    // Alone, a scan runs at once.
    send(&a, Cmd::Scan(103));
    thread::sleep(Duration::from_millis(50));
    let (t, _, _) = stop(e, a);
    assert_eq!(t.served, vec![1, 2, 101, 102, 103]);
    // Each scan's wait in the background queue is timed (012 FR-015).
    assert!(teseladb_stages::count(teseladb_stages::Stage::BackgroundWait) >= waits + 3);
}

fn a_long_turn_gives_the_shard_back_between_messages(e: &NodeEngine) {
    // Ten messages of 5 ms each arrive at once: the turn takes them one task quota at a time, so
    // a task of the core's shard queued meanwhile runs after one or two of them, not after the 50
    // ms of all ten; and every message is taken, in order.
    let a = start(e, 0, 24, Toy::default());
    for _ in 0..10 {
        send(&a, Cmd::Stall(Duration::from_millis(5)));
    }
    send(&a, Cmd::Request(1));
    send(&a, Cmd::Request(2));
    thread::sleep(Duration::from_millis(2));
    let asked = Instant::now();
    e.run_on(0, || ()).unwrap();
    let waited = asked.elapsed();
    let (t, _, _) = stop(e, a);
    assert_eq!(t.served, vec![1, 2]);
    assert!(waited < Duration::from_millis(25), "the shard's task waited {waited:?}");
}

fn background_at_a_stop_answered(e: &NodeEngine) {
    let a = start(e, 0, 23, Toy::default());
    send(&a, Cmd::Stall(Duration::from_millis(20)));
    send(&a, Cmd::Scan(7));
    let (t, _, _) = stop(e, a);
    assert_eq!(t.served, vec![7]);
}

fn a_core_reaches_another_core(e: &NodeEngine) {
    // A message from core A's shard to core B's goes through the engine's queues between shards
    // (smp), one from A's shard to itself runs on A.
    let a = start(e, 0, 70, Toy::default());
    let b = start(e, 1, 71, Toy::default());
    send(&a, Cmd::Relay(b.tx.clone(), Box::new(Cmd::Request(5))));
    send(&a, Cmd::Relay(a.tx.clone(), Box::new(Cmd::Request(6))));
    thread::sleep(Duration::from_millis(100));
    let (ta, _, _) = stop(e, a);
    let (tb, _, _) = stop(e, b);
    assert_eq!(ta.served, vec![6]);
    assert_eq!(tb.served, vec![5]);
}

/// A core's `build`, the recovery of its log and LSM, runs on a deep stack of its own, as a
/// client's command does: on the engine thread's 128 KiB, taken from the shard's memory, a deep
/// recovery wrote past the stack into the blocks beside it, and the node crashed later in an
/// allocation (the native one-node load and the server's relocation test).
fn a_core_is_built_on_a_deep_stack(e: &NodeEngine) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let room = Arc::new(AtomicUsize::new(0));
    let seen = room.clone();
    let h = hello(73, &free_port().to_string());
    let core = start_core_built(e, 0, &h, Duration::from_millis(5), None, move || {
        seen.store(teseladb_sys::deep_stack::room_here().unwrap_or(0), Ordering::SeqCst);
        (Toy::default(), MemDisk::new())
    });
    assert!(core.sender().send(CoreInput::Stop).is_ok());
    drop(core.join().unwrap());
    let room = room.load(Ordering::SeqCst);
    assert!(room >= 1 << 20, "{room} bytes of stack for the core's recovery");
}

/// The driver's first turn runs where the core was built: a coroutine runs until it first waits
/// on the stack that made it, so the first turn ran on the engine thread's 128 KiB even with the
/// recovery on a deep stack, and in a build with stack guards the node faulted there (the LSM's
/// first schedule, from the logic's `poll_io`).
fn a_core_s_first_turn_runs_on_a_deep_stack(e: &NodeEngine) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let room = Arc::new(AtomicUsize::new(usize::MAX));
    let seen = room.clone();
    let h = hello(74, &free_port().to_string());
    let core = start_core_built(e, 0, &h, Duration::from_millis(5), None, move || {
        (Toy { first_poll_room: Some(seen), ..Toy::default() }, MemDisk::new())
    });
    assert!(core.sender().send(CoreInput::Stop).is_ok());
    drop(core.join().unwrap());
    let room = room.load(Ordering::SeqCst);
    assert!(room != usize::MAX, "the logic was polled");
    assert!(room >= 1 << 20, "{room} bytes of stack for the core's first turn");
}

fn the_logic_is_told_no_file_sends(e: &NodeEngine) {
    let a = start(e, 0, 72, Toy::default());
    let (t, _, _) = stop(e, a);
    assert_eq!(t.file_sends, Some(false));
}
