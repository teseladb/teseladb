//! The authorization fence (013 T020, FR-012): the tables behave as `spec/tla/AuthzFence.tla`
//! models them, and random schedules of requests, answers, applies, revocations and crashes
//! never let a node authorize with a revoked state once the revocation answered or a lease after
//! it.

use teseladb_auth::fence::{
    AUTH_LEASE_MS, AuthLease, DRIFT_MARGIN_MS, FenceTable, HeldAsk, HeldAsks,
};
use teseladb_auth::lease_driver::{FenceMsg, Group0Host, Group0View, LeaseDriver};

#[test]
fn a_node_holds_a_lease_from_when_it_asked() {
    let mut lease = AuthLease::new();
    assert!(!lease.valid(0));
    let seq = lease.ask(100);
    lease.granted(seq, 5, 5);
    let until = 100 + AUTH_LEASE_MS - DRIFT_MARGIN_MS;
    assert_eq!(lease.until_ms(), until);
    assert!(lease.valid(until - 1));
    assert!(!lease.valid(until));
    // A copy behind the leader gets nothing; a late or unknown answer neither.
    let seq = lease.ask(until);
    lease.granted(seq, 7, 6);
    assert!(!lease.valid(until));
    lease.granted(seq, 7, 7);
    assert!(!lease.valid(until), "the request was already answered");
    let old = lease.ask(until + 1);
    let new = lease.ask(until + 2);
    lease.granted(old, 7, 7);
    assert!(!lease.valid(until + 3));
    lease.granted(new, 7, 7);
    assert!(lease.valid(until + 3));
    // A shorter grant never shortens the lease.
    let back = lease.ask(0);
    lease.granted(back, 7, 7);
    assert!(lease.valid(until + 3));
    lease.drop_lease();
    assert!(!lease.valid(until + 3));
    lease.granted(back, 7, 7);
    assert!(!lease.valid(until + 3));
}

#[test]
fn a_revocation_waits_for_every_node_or_its_lease() {
    let mut t = FenceTable::new();
    assert!(t.released(1, 0), "no node, nothing to wait for");
    assert_eq!(t.request(1, 3, 4, 1_000), 4);
    t.request(2, 4, 4, 1_000);
    assert!(t.released(3, 1_000));
    assert!(!t.released(4, 1_000));
    assert_eq!(t.fenced(4, 1_000), 3);
    assert_eq!(t.fenced(2, 1_000), 2);
    assert_eq!(t.fenced(4, 1_000 + AUTH_LEASE_MS), 4);
    assert_eq!(t.deadline(4), 1_000 + AUTH_LEASE_MS);
    assert!(t.released(4, 1_000 + AUTH_LEASE_MS));
    t.request(1, 4, 4, 1_500);
    assert!(t.released(4, 1_500));
    assert_eq!(t.deadline(4), 0);
    // An acknowledgement never goes back, a lease never shortens.
    t.request(1, 2, 4, 1_200);
    assert!(t.released(4, 1_500));
    assert!(!t.released(5, 1_500 + AUTH_LEASE_MS - 1));
    t.remove(1);
    t.remove(2);
    assert!(t.released(5, 0));
}

#[test]
fn a_new_leader_waits_one_lease_for_what_it_does_not_know() {
    let t = FenceTable::take_over(&[1, 2], 10_000);
    assert!(!t.released(1, 10_000));
    assert!(t.released(1, 10_000 + AUTH_LEASE_MS));
}

/// A small deterministic generator for the schedules.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

const NODES: u64 = 3;

/// A node: its copy of Group 0, its lease, whether it runs, and the request in flight with the
/// time the leader will see it.
struct Node {
    applied: u64,
    lease: AuthLease,
    up: bool,
    flight: Option<(u64, u64, u64)>,
    answer: Option<(u64, u64)>,
}

#[test]
fn random_schedules_keep_the_fence() {
    for seed in 1..400u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut table = FenceTable::new();
        let mut nodes: Vec<Node> = (0..NODES)
            .map(|_| Node {
                applied: 0,
                lease: AuthLease::new(),
                up: true,
                flight: None,
                answer: None,
            })
            .collect();
        let mut commit = 0u64;
        let mut revocations: Vec<(u64, u64, bool)> = Vec::new();
        let mut now = 0u64;
        for _ in 0..600 {
            now += r.next(200);
            let n = r.next(NODES) as usize;
            match r.next(7) {
                0 => {
                    commit += 1;
                    revocations.push((commit, now, false));
                }
                1 if nodes[n].up && nodes[n].applied < commit => nodes[n].applied += 1,
                2 if nodes[n].up && nodes[n].flight.is_none() => {
                    let seq = nodes[n].lease.ask(now);
                    let applied = nodes[n].applied;
                    nodes[n].flight = Some((seq, applied, now + r.next(300)));
                }
                3 => {
                    if let Some((seq, applied, at)) = nodes[n].flight
                        && at <= now
                    {
                        nodes[n].flight = None;
                        let c = table.request(n as u64, applied, commit, now);
                        nodes[n].answer = Some((seq, c));
                    }
                }
                4 => {
                    if let Some((seq, c)) = nodes[n].answer.take()
                        && nodes[n].up
                    {
                        let applied = nodes[n].applied;
                        nodes[n].lease.granted(seq, c, applied);
                    }
                }
                5 => {
                    nodes[n].up = !nodes[n].up;
                    if !nodes[n].up {
                        nodes[n].lease.drop_lease();
                    }
                }
                _ => {
                    let fenced = table.fenced(commit, now);
                    for rev in revocations.iter_mut() {
                        let released = table.released(rev.0, now);
                        assert_eq!(released, rev.0 <= fenced, "seed {seed}");
                        rev.2 |= released;
                    }
                }
            }
            for (i, rev) in revocations.iter().enumerate() {
                let fenced = rev.2 || now >= rev.1 + AUTH_LEASE_MS;
                for node in &nodes {
                    if fenced && node.up && node.lease.valid(now) {
                        assert!(node.applied >= rev.0, "seed {seed}: revocation {i} not fenced");
                    }
                }
            }
        }
    }
}

fn ask(node: u64, at_ns: u64) -> HeldAsk {
    HeldAsk { node, seq: at_ns, applied: 3, at_ns }
}

/// The leader answers a request only once a majority acknowledged a round it sent no earlier
/// than the request arrived (D-073).
#[test]
fn a_request_waits_for_a_round_sent_after_it() {
    let mut held = HeldAsks::new();
    assert!(!held.round_due());
    held.hold(ask(1, 100));
    held.hold(ask(2, 150));
    assert!(held.round_due(), "nothing was sent for them");
    assert!(held.release(None).is_empty(), "no round acknowledged in the term");
    assert!(held.release(Some(99)).is_empty(), "a round older than both requests");
    held.round_sent(150);
    assert!(!held.round_due());
    assert_eq!(held.release(Some(120)), vec![ask(1, 100)]);
    // A later request of the same node replaces the one held.
    held.hold(ask(2, 160));
    assert!(held.round_due(), "it arrived after the round");
    held.round_sent(160);
    assert!(held.release(Some(150)).is_empty());
    assert_eq!(held.release(Some(160)), vec![ask(2, 160)]);
    assert!(held.release(Some(u64::MAX)).is_empty(), "each request answers once");
    // A leader that stops leading drops what it held.
    held.hold(ask(3, 200));
    held.clear();
    assert!(!held.round_due());
    assert!(held.release(Some(u64::MAX)).is_empty());
}

/// A replica of Group 0 the test drives by hand.
#[derive(Default)]
struct FakeHost {
    view: Option<Group0View>,
    heard: Option<u64>,
    rounds: Vec<u64>,
}

impl Group0Host for FakeHost {
    fn view(&self) -> Option<Group0View> {
        self.view
    }

    fn known_nodes(&self) -> Vec<u64> {
        vec![1, 2]
    }

    fn quorum_heard_at_ns(&self) -> Option<u64> {
        self.heard
    }

    fn send_confirm_round(&mut self, now_ns: u64) {
        self.rounds.push(now_ns);
    }
}

const MS: u64 = 1_000_000;
const ASK_EVERY: u64 = 500 * MS;

fn same(applied: u64) -> u64 {
    applied
}

/// A follower asks the leader it knows and takes the grant; a node without a replica, or that
/// knows no leader, asks nothing.
#[test]
fn a_follower_asks_its_leader() {
    let mut d = LeaseDriver::new(ASK_EVERY);
    let mut host = FakeHost::default();
    assert!(d.tick(&mut host, 2, 0, &same).is_empty(), "no replica");
    host.view = Some(Group0View { term: 1, leads: false, leader: 0, commit: 4, applied: 4 });
    assert!(d.tick(&mut host, 2, MS, &same).is_empty(), "no leader known");
    host.view = Some(Group0View { term: 1, leads: false, leader: 1, commit: 4, applied: 4 });
    let out = d.tick(&mut host, 2, ASK_EVERY + MS, &|a| a - 1);
    let [FenceMsg::Ask { to: 1, node: 2, seq, applied: 3 }] = out[..] else { panic!("{out:?}") };
    assert!(d.tick(&mut host, 2, ASK_EVERY + 2 * MS, &same).is_empty(), "one request a period");
    d.on_ask(3, 1, 4, ASK_EVERY + 2 * MS);
    assert!(!d.leads(), "a follower holds no request");
    d.on_grant(seq, 4, 4, 4);
    assert_eq!(d.fenced(), 4);
    assert!(d.holds(ASK_EVERY + 2 * MS));
    assert_eq!(d.until_ms(), 501 + AUTH_LEASE_MS - DRIFT_MARGIN_MS);
    d.restart();
    assert!(!d.holds(ASK_EVERY + 2 * MS));
    assert_eq!(d.fenced(), 0);
}

/// The leader grants nothing, to itself or to others, until a majority acknowledges a round sent
/// after each request, and it takes over only once its copy holds every committed entry (D-073).
#[test]
fn the_leader_grants_only_on_a_round_a_majority_acknowledged() {
    let mut d = LeaseDriver::new(ASK_EVERY);
    let mut host = FakeHost {
        view: Some(Group0View { term: 1, leads: true, leader: 1, commit: 5, applied: 4 }),
        ..FakeHost::default()
    };
    assert!(d.tick(&mut host, 1, 0, &same).is_empty());
    assert!(!d.leads(), "its copy is behind the commit index");
    host.view = Some(Group0View { term: 1, leads: true, leader: 1, commit: 5, applied: 5 });
    assert!(d.tick(&mut host, 1, ASK_EVERY, &same).is_empty());
    assert!(d.leads());
    assert_eq!(host.rounds, vec![ASK_EVERY], "a round for its own request");
    assert!(!d.holds(ASK_EVERY), "nothing acknowledged yet");
    d.on_ask(2, 9, 5, ASK_EVERY + MS);
    // A round acknowledged from before the requests answers none of them.
    host.heard = Some(ASK_EVERY - MS);
    assert!(d.tick(&mut host, 1, ASK_EVERY + 2 * MS, &same).is_empty());
    assert_eq!(host.rounds, vec![ASK_EVERY, ASK_EVERY + 2 * MS], "a round for the other");
    assert!(!d.holds(ASK_EVERY + 2 * MS));
    host.heard = Some(ASK_EVERY + 2 * MS);
    let out = d.tick(&mut host, 1, ASK_EVERY + 3 * MS, &same);
    assert_eq!(out, vec![FenceMsg::Grant { to: 2, seq: 9, commit: 5, fenced: 5 }]);
    assert!(d.holds(ASK_EVERY + 3 * MS));
    // It stops leading: what it holds goes, and it asks the new leader.
    d.on_ask(2, 10, 5, ASK_EVERY + 4 * MS);
    host.view = Some(Group0View { term: 1, leads: false, leader: 3, commit: 5, applied: 5 });
    host.heard = Some(u64::MAX);
    let out = d.tick(&mut host, 1, 2 * ASK_EVERY, &same);
    assert!(matches!(out[..], [FenceMsg::Ask { to: 3, node: 1, .. }]), "{out:?}");
    assert!(!d.leads());
}

/// A replica that loses its term and wins a later one between two ticks takes over afresh: the
/// requests it held go, and every node it knows counts as holding a lease of the term between.
#[test]
fn a_leader_of_a_new_term_takes_over_afresh() {
    let mut d = LeaseDriver::new(ASK_EVERY);
    let mut host = FakeHost {
        view: Some(Group0View { term: 1, leads: true, leader: 1, commit: 5, applied: 5 }),
        heard: Some(0),
        ..FakeHost::default()
    };
    assert!(d.tick(&mut host, 1, 0, &same).is_empty());
    d.on_ask(2, 9, 5, MS);
    // Node 2 acknowledged nothing of term 2: index 6 waits for its lease to run out.
    host.view = Some(Group0View { term: 2, leads: true, leader: 1, commit: 6, applied: 6 });
    host.heard = Some(u64::MAX);
    let out = d.tick(&mut host, 1, 2 * MS, &same);
    assert!(out.is_empty(), "the request of term 1 is not answered: {out:?}");
    assert!(d.fenced() < 6);
    let late = (2 + AUTH_LEASE_MS) * MS;
    d.tick(&mut host, 1, late, &same);
    assert_eq!(d.fenced(), 6, "the leases of the term between ran out");
}
