//! The authorization fence (013 T020, FR-012): the tables behave as `spec/tla/AuthzFence.tla`
//! models them, and random schedules of requests, answers, applies, revocations and crashes
//! never let a node authorize with a revoked state once the revocation answered or a lease after
//! it.

use teseladb_auth::fence::{AUTH_LEASE_MS, AuthLease, DRIFT_MARGIN_MS, FenceTable};

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
