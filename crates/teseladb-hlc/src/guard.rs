//! The clock rule a node applies to itself (005 FR-006, D-004): it compares its wall clock with its
//! peers' in the Raft heartbeats of the last second ([`PeerClocks`]) and with its own monotonic
//! clock, and takes Group 0's `ClockReport`s. More than [`MAX_CLOCK_OFFSET_MS`] from the median of
//! the voters, a wall clock that jumped against the monotonic one, or a report out of margin, and
//! it stops leading and assigning marks at once.

use std::collections::BTreeMap;

use teseladb_types::{Nanos, NodeId};

/// Largest offset from the median of the other clocks with which a node may lead a range.
pub const MAX_CLOCK_OFFSET_MS: i64 = 125;
/// How long a peer's clock sample counts, and how long a jump keeps a node aside.
pub const PEER_WINDOW: Nanos = Nanos::from_secs(1);

/// Whether a node with that offset may lead ranges.
pub fn may_lead(offset_ms: i64) -> bool {
    offset_ms.abs() <= MAX_CLOCK_OFFSET_MS
}

/// This node's clock against its peers', from the Raft heartbeats of the last second.
#[derive(Clone, Debug, Default)]
pub struct PeerClocks {
    samples: BTreeMap<NodeId, (i64, Nanos)>,
}

impl PeerClocks {
    /// A frame from voter `peer` at `now`: this node's clock minus the peer's, as
    /// [`ClockGuard::observe`] measures it.
    pub fn observe(&mut self, peer: NodeId, offset_ms: i64, now: Nanos) {
        self.samples.insert(peer, (offset_ms, now));
    }

    /// This node's offsets from the clocks heard in the last second, and its own zero, sorted:
    /// the clocks of the voters it knows, itself included (D-004, as in Group 0's rule). Empty if
    /// no peer was heard.
    fn offsets(&self, now: Nanos) -> Vec<i64> {
        let mut recent: Vec<i64> = self
            .samples
            .values()
            .filter(|(_, at)| now.0.saturating_sub(at.0) <= PEER_WINDOW.0)
            .map(|(o, _)| *o)
            .collect();
        if !recent.is_empty() {
            recent.push(0);
            recent.sort_unstable();
        }
        recent
    }

    /// This node's offset from the median clock (the lower one with an even count), if a peer
    /// was heard.
    pub fn median(&self, now: Nanos) -> Option<i64> {
        let v = self.offsets(now);
        v.get(v.len().checked_sub(1)? / 2).copied()
    }

    /// Whether this node may lead and assign marks: within the margin of the median clock. With
    /// an even count it cannot tell which middle clock is right, so it has to be within the margin
    /// of both: a node ahead of the only peer it hears does not take its own clock as the median.
    pub fn in_margin(&self, now: Nanos) -> bool {
        let v = self.offsets(now);
        match v.len() {
            0 => true,
            n if n % 2 == 1 => may_lead(v[n / 2]),
            n => may_lead(v[n / 2 - 1]) && may_lead(v[n / 2]),
        }
    }
}

/// The rule a node applies to itself: its peers' clocks and its own jumps.
#[derive(Clone, Debug, Default)]
pub struct ClockGuard {
    peers: PeerClocks,
    /// Monotonic and wall clocks at the last tick.
    last: Option<(Nanos, u64)>,
    /// When the wall clock last jumped against the monotonic one.
    jumped: Option<Nanos>,
    /// Group 0's latest report: this node's offset from the voters' median, and when it came.
    report: Option<(i64, Nanos)>,
    /// The other clocks' verdict (peers heard and Group 0's report in the last second) at the last
    /// tick that had one; `false` after a start or a jump. See [`ClockGuard::vouched`].
    vouched: bool,
    /// The highest wall clock read while the node was in margin and [`ClockGuard::vouched`]: the
    /// latest time it may have given out in a mark or a read.
    peak_ms: u64,
}

impl ClockGuard {
    /// A tick at monotonic `now` with the wall clock at `wall_ms`: a wall clock that moved more
    /// than [`MAX_CLOCK_OFFSET_MS`] away from the monotonic time elapsed jumped.
    pub fn tick(&mut self, now: Nanos, wall_ms: u64) {
        if let Some((then, wall_then)) = self.last {
            let elapsed = i128::from(now.0.saturating_sub(then.0) / 1_000_000);
            let moved = i128::from(wall_ms) - i128::from(wall_then);
            if (moved - elapsed).abs() > i128::from(MAX_CLOCK_OFFSET_MS) {
                self.jumped = Some(now);
                self.vouched = false;
            }
        }
        self.last = Some((now, wall_ms));
        self.vouched = self.vouched(now);
        if self.vouched && self.in_margin(now) {
            self.peak_ms = self.peak_ms.max(wall_ms);
        }
    }

    /// A frame from voter `peer`, sent when its wall clock read `peer_wall_ms`, arrived when this
    /// node's read `wall_ms` (both as of each core's last tick). The sample is the arrival minus
    /// the sending time: the true offset plus the frame's flight, which nothing discounts, since
    /// one frame carries no round trip. The sample is off by at most that flight plus a tick
    /// on each side; on the networks the margin assumes (D-004) that is far below
    /// [`MAX_CLOCK_OFFSET_MS`]. A clock of 0 is not known yet (a core before its first tick) and says
    /// nothing.
    pub fn observe(&mut self, peer: NodeId, peer_wall_ms: u64, wall_ms: u64, now: Nanos) {
        if peer_wall_ms == 0 || wall_ms == 0 {
            return;
        }
        let offset =
            i64::try_from(i128::from(wall_ms) - i128::from(peer_wall_ms)).unwrap_or(i64::MAX);
        self.peers.observe(peer, offset, now);
    }

    /// Group 0's `ClockReport` at `now`: this node's offset from the median of its voters. It
    /// counts for a window; Group 0 sends one every 500 ms.
    pub fn report(&mut self, offset_ms: i64, now: Nanos) {
        self.report = Some((offset_ms, now));
    }

    /// This node's offset from the median clock of the voters heard in the last second (the lower
    /// median with an even count), if it heard any.
    pub fn offset(&self, now: Nanos) -> Option<i64> {
        self.peers.median(now)
    }

    /// Whether other clocks vouch for this node's: the verdict of the peers heard in the last
    /// second and of a recent report of Group 0, or, if neither, the last such verdict since the
    /// node started and since its wall clock last jumped. After a restart it knows nothing of a
    /// jump from before; once a jump's second is over a node that hears nobody is its own median;
    /// and a node out of margin with the clocks it heard would be in margin again by hearing
    /// nobody. In a group with other voters it assigns no mark while nothing vouches for its
    /// clock, and its peak does not move (005 FR-006). Leading follows [`ClockGuard::in_margin`]
    /// alone, so that a cluster whose nodes all hear nobody still elects a leader.
    pub fn vouched(&self, now: Nanos) -> bool {
        let peers = self.peers.median(now).map(|_| self.peers.in_margin(now));
        let recent = |at: Nanos| now.0.saturating_sub(at.0) <= PEER_WINDOW.0;
        let report = self.report.filter(|(_, at)| recent(*at)).map(|(o, _)| may_lead(o));
        match (peers, report) {
            (None, None) => self.vouched,
            (p, r) => p.unwrap_or(true) && r.unwrap_or(true),
        }
    }

    /// The highest wall clock read while in margin and [`ClockGuard::vouched`] (005 FR-005,
    /// FR-006): marks and reads given out are not above it, give or take the margin. 0 before the
    /// first such tick.
    pub fn peak_ms(&self) -> u64 {
        self.peak_ms
    }

    /// Whether this node may lead and assign marks now.
    pub fn in_margin(&self, now: Nanos) -> bool {
        let recent = |at: Nanos| now.0.saturating_sub(at.0) <= PEER_WINDOW.0;
        let jumped = self.jumped.is_some_and(recent);
        let reported_out = self.report.is_some_and(|(o, at)| recent(at) && !may_lead(o));
        // A wall clock that went back stays aside until it passes the highest time it read while
        // in margin: until then its time is below marks and reads it may have given out.
        let behind = self.last.is_some_and(|(_, wall)| wall < self.peak_ms);
        !jumped && !behind && !reported_out && self.peers.in_margin(now)
    }
}
