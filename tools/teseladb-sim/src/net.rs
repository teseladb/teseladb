//! Simulated network: long-tail latencies, losses, partitions (also asymmetric)
//! and connection cuts. Each connection `(from, to, core)` delivers in order, like TCP; a cut drops
//! what was in flight. Reordering and duplicates happen as with real TCP, between connections
//! (002 research R3b): a reordered message skips its connection's order, and a duplicate arrives
//! again later as over a new connection.

use std::collections::BTreeMap;

use teseladb_types::{Nanos, Rng};

/// Network parameters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetParams {
    /// Probability of losing a message.
    pub loss: f64,
    /// Minimum latency.
    pub min_latency: Nanos,
    /// Usual maximum latency.
    pub max_latency: Nanos,
    /// Probability of a long tail (latency ×10).
    pub tail: f64,
    /// Probability that a message skips its connection's order.
    pub reorder: f64,
    /// Probability that a message arrives a second time, later.
    pub duplicate: f64,
}

impl Default for NetParams {
    fn default() -> NetParams {
        NetParams {
            loss: 0.0,
            min_latency: Nanos::from_micros(20),
            max_latency: Nanos::from_micros(500),
            tail: 0.01,
            reorder: 0.0,
            duplicate: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Conn {
    epoch: u64,
    last_at: Nanos,
}

/// Connection epoch of a duplicate: it travels as over a new connection, so a cut of the one it
/// was sent on does not drop it (a partition still does).
pub const ANY_CONNECTION: u64 = u64::MAX;

/// Message accepted by the network: when it arrives and with which connection epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InFlight {
    /// Delivery instant.
    pub at: Nanos,
    /// Epoch of the connection when it was sent (if it changes, the message is dropped).
    pub epoch: u64,
    /// Arrival of a second copy, with epoch [`ANY_CONNECTION`].
    pub copy: Option<Nanos>,
}

/// Whether an event of probability `p` happens; draws nothing when it cannot, so that a network
/// without it keeps the random stream it had.
fn chance(rng: &mut Rng, p: f64) -> bool {
    p > 0.0 && rng.chance(p)
}

/// Network of `n` nodes.
#[derive(Clone, Debug)]
pub struct SimNetwork {
    n: usize,
    reach: Vec<bool>,
    /// Current parameters.
    pub params: NetParams,
    conns: BTreeMap<(u32, u32, u16), Conn>,
}

impl SimNetwork {
    /// Network without partitions.
    pub fn new(n: usize, params: NetParams) -> SimNetwork {
        SimNetwork { n, reach: vec![true; n * n], params, conns: BTreeMap::new() }
    }

    /// Tells whether `from` reaches `to`.
    pub fn reachable(&self, from: u32, to: u32) -> bool {
        self.reach[from as usize * self.n + to as usize]
    }

    /// Sets whether `from` reaches `to` (one direction: partitions may be asymmetric).
    pub fn set_reach(&mut self, from: u32, to: u32, ok: bool) {
        self.reach[from as usize * self.n + to as usize] = ok;
    }

    /// Partition: each node only reaches those of its group (those in no group end up
    /// isolated).
    pub fn partition(&mut self, groups: &[Vec<u32>]) {
        let group_of = |x: u32| groups.iter().position(|g| g.contains(&x));
        for a in 0..self.n as u32 {
            for b in 0..self.n as u32 {
                let same = a == b || (group_of(a).is_some() && group_of(a) == group_of(b));
                self.set_reach(a, b, same);
            }
        }
    }

    /// Removes partitions and losses.
    pub fn heal(&mut self) {
        self.reach.fill(true);
        self.params.loss = 0.0;
    }

    /// Cuts the connections between `a` and `b` in both directions: what was in flight is lost.
    pub fn cut(&mut self, a: u32, b: u32) {
        for ((x, y, _), c) in self.conns.iter_mut() {
            if (*x == a && *y == b) || (*x == b && *y == a) {
                c.epoch += 1;
            }
        }
    }

    fn latency(&self, rng: &mut Rng) -> Nanos {
        let lo = self.params.min_latency.0;
        let hi = self.params.max_latency.0.max(lo);
        // Skewed downwards (approximating a log-normal) with an occasional long tail.
        let u = rng.next_f64();
        let mut l = lo + ((hi - lo) as f64 * u * u) as u64;
        if rng.chance(self.params.tail) {
            l *= 10;
        }
        Nanos(l)
    }

    /// Sends a message: `None` if it does not arrive (unreachable or lost).
    pub fn send(
        &mut self,
        from: u32,
        to: u32,
        core: u16,
        now: Nanos,
        rng: &mut Rng,
    ) -> Option<InFlight> {
        if !self.reachable(from, to) || rng.chance(self.params.loss) {
            return None;
        }
        let lat = self.latency(rng);
        let reorder = chance(rng, self.params.reorder);
        let copy = chance(rng, self.params.duplicate).then(|| now + lat + self.latency(rng));
        let conn = self.conns.entry((from, to, core)).or_default();
        let at = if reorder { now + lat } else { (now + lat).max(conn.last_at) };
        if !reorder {
            conn.last_at = at;
        }
        Some(InFlight { at, epoch: conn.epoch, copy })
    }

    /// On arrival: delivered if it is still reachable and the connection was not cut meanwhile.
    pub fn accept(&self, from: u32, to: u32, core: u16, epoch: u64) -> bool {
        self.reachable(from, to)
            && (epoch == ANY_CONNECTION
                || self.conns.get(&(from, to, core)).is_some_and(|c| c.epoch == epoch))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn fifo_latency_loss_partitions_and_cuts() {
        let mut net = SimNetwork::new(3, NetParams::default());
        let mut rng = Rng::new(1);
        // In order per connection and within the limits (except the long tail).
        let mut last = Nanos::ZERO;
        for i in 0..1000 {
            let m = net.send(0, 1, 0, Nanos(i), &mut rng).unwrap();
            assert!(m.at >= last);
            assert!(m.at.0 >= i + 20_000);
            assert!(m.at.0 <= i.max(last.0) + 5_000_000);
            last = m.at;
            assert!(net.accept(0, 1, 0, m.epoch));
        }
        // Cut: what was in flight is dropped; new messages go through.
        let m = net.send(1, 0, 0, Nanos::ZERO, &mut rng).unwrap();
        net.cut(0, 1);
        assert!(!net.accept(1, 0, 0, m.epoch));
        let m2 = net.send(1, 0, 0, Nanos::ZERO, &mut rng).unwrap();
        assert!(net.accept(1, 0, 0, m2.epoch));
        assert!(!net.accept(2, 0, 0, 0), "unknown connection");
        // Partition {0,1} | {2}: asymmetric afterwards.
        net.partition(&[vec![0, 1], vec![2]]);
        assert!(net.send(0, 2, 0, Nanos::ZERO, &mut rng).is_none());
        assert!(net.send(0, 1, 0, Nanos::ZERO, &mut rng).is_some());
        net.set_reach(2, 0, true);
        assert!(net.reachable(2, 0) && !net.reachable(0, 2));
        net.partition(&[vec![0]]);
        assert!(!net.reachable(1, 2) && net.reachable(1, 1));
        // Losses.
        net.heal();
        net.params.loss = 1.0;
        assert!(net.send(0, 1, 0, Nanos::ZERO, &mut rng).is_none());
        net.heal();
        assert!(net.send(0, 1, 0, Nanos::ZERO, &mut rng).is_some());
    }
}
