//! The merges of a core's ranges (007 FR-009, FR-022). A range and its right neighbour merge when
//! both hold less than [`MERGE_BYTES`] of live data and no write reached either for
//! [`MERGE_COLD`], with the same replicas on the same core (Group 0 checks it). The steps:
//!
//! 1. The left range's leader records the intent (`MergeRanges`); Group 0 names the merge by the
//!    index of that entry and refuses splits and replica changes of both ranges until it ends.
//! 2. The right range's leader freezes it (`MergeFreeze`). Each node flushes the LSM until the
//!    frozen replica's data is on its disk.
//! 3. The left range's leader asks every node of the ranges whether its replica of the right
//!    range is frozen for the merge with its data on disk ([`Request::MergeProbe`]); once every
//!    node, itself included, says yes, it proposes `Merge` with the right range's frozen state,
//!    read from its own node. The data does not move: both replicas of a node share its LSM. If
//!    that does not happen within [`MERGE_GIVE_UP`], it gives the merge up in its log instead.
//! 4. The left range's log decides: once it applied `Merge` its leader completes the merge in
//!    Group 0 (`CompleteMerge`), and once it gave it up it aborts it (`AbortMerge`), after which
//!    the right range's leader thaws it.
//! 5. On every node, once the left replica applied `Merge`, the right replica goes: its group and
//!    log are removed, it is never opened again, and its local keys are cleared from the LSM.
//!
//! Every Group 0 command and every proposal goes again after a pause if nothing changed: Group 0
//! refuses a repeated one and the ranges' logs apply a repeated one as a no-op.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use teseladb_lsm::Lsm;
use teseladb_meta::bootstrap::{DATA_END, DATA_START};
use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::RangeDesc;
use teseladb_multiraft::{GroupDesc, MultiRaftCore, OpId};
use teseladb_raft::Role;
use teseladb_range::host::{MergeSource, RangeHost};
use teseladb_range::sm::RangeSm;
use teseladb_range::snapshot::local_interval;
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

use crate::coresm::CoreSm;
use crate::forward::Request;
use crate::ranges::Replicas;

/// Live bytes below which two neighbours merge (FR-009).
pub const MERGE_BYTES: u64 = 16 << 20;

/// How long neither range may take a write before they merge (FR-009).
pub const MERGE_COLD: Nanos = Nanos::from_secs(600);

/// How long the left range's leader waits for every replica of the right range to be frozen with
/// its data on disk before it gives the merge up.
pub const MERGE_GIVE_UP: Nanos = Nanos::from_secs(30);

/// Pause between two sends of the same command, proposal or probe.
const RESEND_EVERY: Nanos = Nanos::from_millis(500);

/// The answer of a probe: the replica is frozen for the merge with its data on disk.
const READY: u8 = 1;

/// What a left range's leader knows of the merge it carries out.
struct Pending {
    merge: u64,
    since: Nanos,
    ready: BTreeSet<NodeId>,
}

/// The kinds of things that go again after a pause, by range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Act {
    Intent,
    Freeze,
    Thaw,
    Absorb,
    GiveUp,
    End,
    Flush,
}

/// The merges of one core.
pub struct Merges {
    me: NodeId,
    merge_bytes: u64,
    cold_for: Nanos,
    /// The last mark of a write each range applied, and since when.
    writes: BTreeMap<GroupId, (teseladb_hlc::Hlc, Nanos)>,
    /// Probes in flight: the left range and the node asked.
    probes: BTreeMap<OpId, (GroupId, NodeId)>,
    /// The merge each left range this core leads carries out.
    pending: BTreeMap<GroupId, Pending>,
    /// When each thing may go again.
    resend_at: BTreeMap<(GroupId, Act), Nanos>,
    /// When each left range may probe each node again.
    probe_at: BTreeMap<(GroupId, NodeId), Nanos>,
    /// `config.settings` turned automatic merges off.
    off: bool,
}

/// The state machine of range `g` on this core.
fn range_sm(core: &MultiRaftCore<CoreSm>, g: GroupId) -> Option<&RangeSm> {
    core.state_machine(g).and_then(CoreSm::range)
}

/// Whether this node's replica of `right` is frozen for `merge` with its data on disk.
pub fn ready(core: &MultiRaftCore<CoreSm>, lsm: &Lsm, right: GroupId, merge: u64) -> bool {
    let awaiting = core.awaited_snapshot(right).is_some();
    replica_ready(range_sm(core, right), lsm.flushed_index(right), awaiting, merge)
}

/// Whether replica `sm`, whose LSM flushed up to `flushed`, is frozen for `merge` with its data
/// on disk. A replica that waits for a snapshot's data is not: its intervals are being cleared
/// and filled again, and the left range would absorb what the clearing removes.
fn replica_ready(sm: Option<&RangeSm>, flushed: u64, awaiting: bool, merge: u64) -> bool {
    !awaiting && sm.is_some_and(|sm| sm.frozen_for() == Some(merge) && flushed >= sm.last_write())
}

/// The answer to a probe.
pub fn probe_answer(core: &MultiRaftCore<CoreSm>, lsm: &Lsm, right: GroupId, merge: u64) -> Bytes {
    Bytes::from_vec(vec![u8::from(ready(core, lsm, right, merge))])
}

impl Merges {
    /// The merges of a core of node `me`: neighbours merge below `merge_bytes` of live data each
    /// once no write reached either for `cold_for`.
    pub fn new(me: NodeId, merge_bytes: u64, cold_for: Nanos) -> Merges {
        Merges {
            me,
            merge_bytes,
            cold_for,
            writes: BTreeMap::new(),
            probes: BTreeMap::new(),
            pending: BTreeMap::new(),
            resend_at: BTreeMap::new(),
            probe_at: BTreeMap::new(),
            off: false,
        }
    }

    /// Turns the merges this core's leaders ask for off or on (`config.settings`, 007 FR-017);
    /// the merges Group 0 already recorded go on.
    pub fn set_off(&mut self, off: bool) {
        self.off = off;
    }

    fn may(&mut self, g: GroupId, act: Act, now: Nanos) -> bool {
        if self.resend_at.get(&(g, act)).is_some_and(|at| now < *at) {
            return false;
        }
        self.resend_at.insert((g, act), now + RESEND_EVERY);
        true
    }

    /// Moves the merges: the replicas absorbed elsewhere retired, the frozen ones flushed, and the
    /// duties of this core's leaders. The Group 0 commands for core 0.
    pub fn step(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        replicas: &mut Replicas,
        lsm: &RefCell<Lsm>,
        now: Nanos,
    ) -> Vec<Group0Command> {
        self.retire(core, host, replicas, lsm);
        self.track(core, replicas, now);
        self.flush(core, replicas, lsm, now);
        let led: Vec<RangeDesc> = replicas
            .descs()
            .filter(|d| replicas.is_open(d.group))
            .filter(|d| core.status(d.group).is_some_and(|s| s.role == Role::Leader))
            .cloned()
            .collect();
        self.pending.retain(|g, _| led.iter().any(|d| d.group == *g));
        let mut out = Vec::new();
        for d in &led {
            self.right_side(core, host, replicas, d, now);
            self.left_side(core, host, replicas, (d, &lsm.borrow()), now, &mut out);
        }
        out
    }

    /// The right range's leader freezes it for the merge Group 0 recorded, and thaws it once the
    /// merge it is frozen for is no longer recorded (aborted).
    fn right_side(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        replicas: &Replicas,
        d: &RangeDesc,
        now: Nanos,
    ) {
        let g = d.group;
        let frozen = range_sm(core, g).and_then(RangeSm::frozen_for);
        match (replicas.absorbing(g), frozen) {
            (Some((_, merge)), f) if f != Some(merge) && self.may(g, Act::Freeze, now) => {
                host.freeze(g, merge);
            }
            (None, Some(_)) if self.may(g, Act::Thaw, now) => {
                host.thaw(core, g, now);
            }
            _ => {}
        }
    }

    /// The left range's leader carries out the merge Group 0 recorded, or asks for one.
    fn left_side(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        replicas: &Replicas,
        (d, lsm): (&RangeDesc, &Lsm),
        now: Nanos,
        out: &mut Vec<Group0Command>,
    ) {
        let g = d.group;
        let (absorbed, abandoned) =
            range_sm(core, g).map_or((None, 0), |sm| (sm.absorbed(), sm.abandoned()));
        let Some((right, merge)) = replicas.merging(g) else {
            self.intent(core, replicas, d, now, out);
            return;
        };
        if absorbed == Some((right, merge)) {
            if self.may(g, Act::End, now) {
                out.push(Group0Command::CompleteMerge { left: g, right });
            }
            return;
        }
        if abandoned >= merge {
            if self.may(g, Act::End, now) {
                out.push(Group0Command::AbortMerge { left: g, right });
            }
            return;
        }
        let p =
            self.pending.entry(g).or_insert(Pending { merge, since: now, ready: BTreeSet::new() });
        if p.merge != merge {
            *p = Pending { merge, since: now, ready: BTreeSet::new() };
        }
        if ready(core, lsm, right, merge) {
            p.ready.insert(self.me);
        }
        let waiting: Vec<NodeId> = d.nodes().into_iter().filter(|n| !p.ready.contains(n)).collect();
        let since = p.since;
        // Every node is ready, this one too: its replica of the right range gives the state.
        let source = range_sm(core, right).filter(|_| waiting.is_empty()).map(|r| MergeSource {
            right,
            merge,
            end: r.bounds().1.to_vec(),
            gc: r.gc_threshold(),
            mark: r.last_mark(),
            live: r.live_bytes(),
            fences: r.fences().to_vec(),
        });
        if let Some(source) = source {
            if self.may(g, Act::Absorb, now) {
                host.absorb(g, source);
            }
        } else if now.saturating_since(since) >= MERGE_GIVE_UP {
            if self.may(g, Act::GiveUp, now) {
                host.give_up(core, g, (right, merge), now);
            }
        } else {
            for n in waiting.into_iter().filter(|n| *n != self.me) {
                let asked = self.probes.values().any(|(l, to)| *l == g && *to == n);
                if asked || self.probe_at.get(&(g, n)).is_some_and(|at| now < *at) {
                    continue;
                }
                self.probe_at.insert((g, n), now + RESEND_EVERY);
                let body = Request::MergeProbe { range: right, merge }.encode();
                let op = core.forward(n, Bytes::from_vec(body), now);
                self.probes.insert(op, (g, n));
            }
        }
    }

    /// The left range's leader asks Group 0 to merge it with its right neighbour once both are
    /// small and cold, on the same replicas, with nothing else in progress.
    fn intent(
        &mut self,
        core: &MultiRaftCore<CoreSm>,
        replicas: &Replicas,
        d: &RangeDesc,
        now: Nanos,
        out: &mut Vec<Group0Command>,
    ) {
        if self.off {
            return;
        }
        let Some(right) = replicas.descs().find(|r| !d.end.is_empty() && r.start == d.end) else {
            return;
        };
        let free = |r: &RangeDesc| !r.settling && r.pending_split.is_none();
        let paired = right.replicas == d.replicas && free(d) && free(right);
        let in_merge =
            replicas.absorbing(right.group).is_some() || replicas.merging(right.group).is_some();
        if !paired || in_merge || !replicas.is_open(right.group) {
            return;
        }
        // Small, with the bounds Group 0 has (no cut or merge waiting for Group 0).
        let small = |r: &RangeDesc| {
            range_sm(core, r.group)
                .is_some_and(|sm| sm.live_bytes() < self.merge_bytes && sm.bounds().1 == r.end)
        };
        let cold = |g: GroupId| {
            self.writes
                .get(&g)
                .is_some_and(|(_, since)| now.saturating_since(*since) >= self.cold_for)
        };
        let (l, r) = (d.group, right.group);
        if small(d) && small(right) && cold(l) && cold(r) && self.may(l, Act::Intent, now) {
            out.push(Group0Command::MergeRanges { left: l, right: r });
        }
    }

    /// Follows the last write of each open range.
    fn track(&mut self, core: &MultiRaftCore<CoreSm>, replicas: &Replicas, now: Nanos) {
        let open: Vec<GroupId> =
            replicas.descs().map(|d| d.group).filter(|g| replicas.is_open(*g)).collect();
        self.writes.retain(|g, _| open.contains(g));
        let marks: Vec<(GroupId, teseladb_hlc::Hlc)> = open
            .into_iter()
            .filter_map(|g| range_sm(core, g).map(|sm| (g, sm.last_mark())))
            .collect();
        for (g, mark) in marks {
            let seen = self.writes.entry(g).or_insert((mark, now));
            if seen.0 != mark {
                *seen = (mark, now);
            }
        }
    }

    /// Flushes the LSM while a frozen replica's data is not all on disk.
    fn flush(
        &mut self,
        core: &MultiRaftCore<CoreSm>,
        replicas: &Replicas,
        lsm: &RefCell<Lsm>,
        now: Nanos,
    ) {
        let frozen: Vec<GroupId> = replicas
            .open_groups()
            .filter(|g| {
                range_sm(core, *g).is_some_and(|sm| {
                    sm.frozen_for().is_some() && lsm.borrow().flushed_index(*g) < sm.last_write()
                })
            })
            .collect();
        for g in frozen {
            if self.may(g, Act::Flush, now) {
                lsm.borrow_mut().flush();
            }
        }
    }

    /// Removes the replicas of this core that another of its replicas absorbed: their group and
    /// log go, they are never opened again and their local keys are cleared. A replica whose data
    /// is not on disk yet waits for the flush; one recovered from the log after a restart and not
    /// opened gets its state machine only to be removed.
    fn retire(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        replicas: &mut Replicas,
        lsm: &RefCell<Lsm>,
    ) {
        let absorbed: Vec<(GroupId, Vec<u8>, Vec<u8>)> = core
            .group_ids()
            .into_iter()
            .filter_map(|g| range_sm(core, g))
            .filter_map(|sm| {
                let (start, end) = sm.bounds();
                sm.absorbed().map(|(right, _)| (right, start.to_vec(), end.to_vec()))
            })
            .collect();
        let recovered = core.recovered_groups();
        // A replica frozen for a merge Group 0 completed goes too, before this node's left
        // replica applies it: the left one may get it in a snapshot, whose data cannot land
        // while the right one still holds its keys.
        let mut absorbed = absorbed;
        // Only ranges: Group 0 and the meta groups on this core are never merged away, and
        // reading their state from disk on every poll would cost a read of the LSM each time.
        let gone: Vec<GroupId> = core
            .group_ids()
            .into_iter()
            .filter(|g| range_sm(core, *g).is_some())
            .chain(recovered.iter().copied())
            .filter(|g| replicas.merged_away(*g) && !absorbed.iter().any(|(r, _, _)| r == g))
            .collect();
        for g in gone {
            let frozen = match range_sm(core, g) {
                Some(sm) => sm.frozen_for().is_some(),
                None => {
                    RangeSm::try_open(g, DATA_START.to_vec(), DATA_END.to_vec(), host.lsm().clone())
                        .ok()
                        .flatten()
                        .is_some_and(|sm| sm.frozen_for().is_some())
                }
            };
            if frozen {
                absorbed.push((g, DATA_START.to_vec(), DATA_END.to_vec()));
            }
        }
        for (right, start, end) in absorbed {
            if recovered.contains(&right) {
                // Its bounds do not matter: it is removed at once.
                let sm = RangeSm::try_open(right, start, end, host.lsm().clone());
                if let Ok(Some(sm)) = sm {
                    let desc = GroupDesc { group: right, voters: Vec::new(), learners: Vec::new() };
                    let _ = core.create_group(desc, CoreSm::Range(sm));
                }
            }
            let durable = range_sm(core, right)
                .is_none_or(|sm| lsm.borrow().flushed_index(right) >= sm.last_write());
            if !durable {
                continue;
            }
            let removed = core.remove_group(right).is_ok();
            let was_open = replicas.retire(right);
            if removed || was_open {
                host.remove_range(right);
                lsm.borrow_mut().clear_range(right, vec![local_interval(right)]);
            }
        }
    }

    /// The end of a probe; `false` if it is not this module's.
    pub fn on_done(&mut self, op: OpId, answer: Option<&[u8]>) -> bool {
        let Some((g, node)) = self.probes.remove(&op) else { return false };
        if answer == Some(&[READY][..])
            && let Some(p) = self.pending.get_mut(&g)
        {
            p.ready.insert(node);
        }
        true
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use teseladb_hlc::Hlc;
    use teseladb_lsm::{Lsm, LsmConfig};
    use teseladb_multiraft::{ApplyCtx, StateMachine};
    use teseladb_raftlog::MemDisk;
    use teseladb_range::command::Command;
    use teseladb_range::sm::RangeSm;
    use teseladb_types::GroupId;

    use super::replica_ready;

    const R: GroupId = GroupId(5);
    const MERGE: u64 = 7;

    #[test]
    fn a_replica_that_waits_for_a_snapshot_is_not_ready_to_be_absorbed() {
        let mut disk = MemDisk::new();
        let cfg = LsmConfig { dir: "lsm".into(), ..LsmConfig::default() };
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg, &mut disk).unwrap().0));
        let mut sm = RangeSm::open(R, vec![1], vec![2], lsm, &mut disk).unwrap();
        let ts = Hlc { physical: 10, logical: 0 };
        let ctx = ApplyCtx { group: R, index: 1, term: 1, ts };
        sm.apply(&Command::MergeFreeze { merge: MERGE }.encode(), &ctx).unwrap();
        let written = sm.last_write();
        assert!(replica_ready(Some(&sm), written, false, MERGE));
        assert!(!replica_ready(Some(&sm), written, true, MERGE), "its data is being replaced");
        assert!(!replica_ready(Some(&sm), written - 1, false, MERGE), "not on disk yet");
        assert!(!replica_ready(Some(&sm), written, false, MERGE + 1), "another merge");
        assert!(!replica_ready(None, written, false, MERGE), "no replica");
    }

    mod intents {
        use std::cell::RefCell;
        use std::rc::Rc;

        use teseladb_hlc::Hlc;
        use teseladb_lsm::{Lsm, LsmConfig};
        use teseladb_meta::command::Group0Command;
        use teseladb_meta::group0::{RangeDesc, ReplicaRole};
        use teseladb_multiraft::{
            ApplyCtx, CoreConfig, GroupDesc, MultiRaftCore, NodeClock, OpId, StateMachine,
        };
        use teseladb_raftlog::{CoreStore, MemDisk, StorageConfig};
        use teseladb_range::command::{Absorbed, Command};
        use teseladb_range::host::RangeHost;
        use teseladb_range::sm::RangeSm;
        use teseladb_types::{GroupId, Nanos, NodeId};

        use super::super::{Merges, READY};
        use crate::coresm::CoreSm;
        use crate::ranges::{Ranges, Replicas};

        const ME: NodeId = NodeId(1);

        fn desc(g: u64, start: &[u8], end: &[u8]) -> RangeDesc {
            RangeDesc {
                group: GroupId(g),
                start: start.to_vec(),
                end: end.to_vec(),
                core: 0,
                replicas: vec![(ME, ReplicaRole::Voter)],
                replica_epoch: 1,
                generation: 1,
                leader_hint: None,
                leader_term: 0,
                pending_split: None,
                settling: false,
            }
        }

        fn core() -> MultiRaftCore<CoreSm> {
            let mut disk = MemDisk::new();
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
            MultiRaftCore::new(CoreConfig::new(ME, 0), store, 1)
        }

        #[test]
        fn a_range_with_no_end_or_a_busy_neighbour_asks_for_no_merge() {
            let core = core();
            let mut busy = desc(2, b"m", b"");
            busy.pending_split = Some(b"q".to_vec());
            let descs = vec![desc(1, b"a", b"m"), busy];
            let mut r = Replicas::new(ME, 0, 1 << 30);
            r.set(&Ranges { descs: descs.clone(), ..Ranges::default() });
            assert_eq!(r.descs().count(), 2, "both ranges are this core's");
            let mut m = Merges::new(ME, 1 << 20, Nanos::ZERO);
            let mut out = Vec::new();
            // The last range has no right neighbour; the first one's neighbour is splitting.
            m.intent(&core, &r, &descs[1], Nanos::from_secs(9), &mut out);
            m.intent(&core, &r, &descs[0], Nanos::from_secs(9), &mut out);
            assert!(out.is_empty());
        }

        /// A core whose two ranges `[01, 02)` and `[02, 04)` are open, with their host, replicas and
        /// LSM; `edit` changes the ranges before they are opened.
        fn opened(
            edit: impl FnOnce(&mut Ranges),
        ) -> (MultiRaftCore<CoreSm>, RangeHost, Replicas, Rc<RefCell<Lsm>>, Ranges) {
            let mut core = core();
            let mut disk = MemDisk::new();
            let cfg = LsmConfig { dir: "lsm".into(), ..LsmConfig::default() };
            let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg, &mut disk).unwrap().0));
            let mut host = RangeHost::new(lsm.clone(), NodeClock::for_node(ME));
            let mut ranges = Ranges {
                descs: vec![desc(1, &[1], &[2]), desc(2, &[2], &[4])],
                ..Ranges::default()
            };
            edit(&mut ranges);
            let mut r = Replicas::new(ME, 0, 1 << 30);
            r.set(&ranges);
            r.open(&mut core, &mut host, &lsm);
            assert!(r.is_open(GroupId(1)) && r.is_open(GroupId(2)), "both ranges open");
            (core, host, r, lsm, ranges)
        }

        #[test]
        fn neighbours_settling_absorbed_or_written_lately_ask_for_no_merge() {
            let now = Nanos::from_secs(9);
            // The left range settles its replicas.
            let (core, _, r, _, ranges) = opened(|g| g.descs[0].settling = true);
            let mut m = Merges::new(ME, 1 << 20, Nanos::from_secs(5));
            let mut out = Vec::new();
            m.intent(&core, &r, &ranges.descs[0], now, &mut out);
            // The right range is being absorbed by another one.
            let (core, _, r, _, ranges) = opened(|g| {
                g.merges.insert(GroupId(7), (GroupId(2), 3));
            });
            m.intent(&core, &r, &ranges.descs[0], now, &mut out);
            // Both small, but written less than `cold_for` ago.
            let (core, _, r, _, ranges) = opened(|_| {});
            m.track(&core, &r, now);
            m.intent(&core, &r, &ranges.descs[0], now, &mut out);
            assert!(out.is_empty(), "{out:?}");
            // Once cold, the merge is asked for, unless `config.settings` turned merges off.
            let cold = now + Nanos::from_secs(5);
            m.set_off(true);
            m.intent(&core, &r, &ranges.descs[0], cold, &mut out);
            assert!(out.is_empty());
            m.set_off(false);
            m.intent(&core, &r, &ranges.descs[0], cold, &mut out);
            assert_eq!(
                out,
                vec![Group0Command::MergeRanges { left: GroupId(1), right: GroupId(2) }]
            );
        }

        #[test]
        fn a_probe_answered_long_ago_is_sent_again() {
            let (mut core, mut host, mut r, lsm, mut ranges) = opened(|g| {
                g.descs[0].replicas.push((NodeId(2), ReplicaRole::Voter));
            });
            ranges.merges.insert(GroupId(1), (GroupId(2), 3));
            r.set(&ranges);
            let mut m = Merges::new(ME, 1 << 20, Nanos::ZERO);
            let mut out = Vec::new();
            let d = ranges.descs[0].clone();
            let now = Nanos::from_secs(1);
            m.left_side(&mut core, &mut host, &r, (&d, &lsm.borrow()), now, &mut out);
            let (op, _) = m.probes.iter().next().map(|(o, v)| (*o, *v)).expect("a probe");
            assert!(m.on_done(op, None));
            // Answered, but not before its time: none; past it, again.
            m.left_side(&mut core, &mut host, &r, (&d, &lsm.borrow()), now, &mut out);
            assert!(m.probes.is_empty());
            let later = now + Nanos::from_secs(1);
            m.left_side(&mut core, &mut host, &r, (&d, &lsm.borrow()), later, &mut out);
            assert_eq!(m.probes.len(), 1);
            assert!(out.is_empty());
        }

        #[test]
        fn an_absorbed_replica_open_here_without_its_group_is_retired() {
            let (mut core, mut host, mut r, lsm, _) = opened(|_| {});
            // The left range absorbed the right one, whose group is already gone.
            core.remove_group(GroupId(2)).unwrap();
            core.remove_group(GroupId(1)).unwrap();
            let mut disk = MemDisk::new();
            let mut left =
                RangeSm::open(GroupId(1), vec![1], vec![2], lsm.clone(), &mut disk).unwrap();
            let zero = Hlc { physical: 0, logical: 0 };
            let absorb = Absorbed {
                end: vec![4],
                gc: zero,
                mark: zero,
                live: 0,
                records: vec![],
                fences: vec![],
            };
            let merge = Command::Merge { right: GroupId(2), merge: 3, absorb: Some(absorb) };
            let ctx = ApplyCtx { group: GroupId(1), index: 1, term: 1, ts: zero };
            left.apply(&merge.encode(), &ctx).unwrap();
            assert!(left.absorbed().is_some());
            let g = GroupDesc { group: GroupId(1), voters: vec![ME], learners: Vec::new() };
            core.create_group(g, CoreSm::Range(left)).unwrap();
            let mut m = Merges::new(ME, 1 << 20, Nanos::ZERO);
            m.retire(&mut core, &mut host, &mut r, &lsm);
            assert!(!r.is_open(GroupId(2)), "retired");
        }

        #[test]
        fn a_probe_answer_for_a_merge_no_longer_pending_changes_nothing() {
            let mut m = Merges::new(ME, 1 << 20, Nanos::ZERO);
            m.probes.insert(OpId(4), (GroupId(1), NodeId(2)));
            assert!(m.on_done(OpId(4), Some(&[READY][..])));
            assert!(m.pending.is_empty() && m.probes.is_empty());
            assert!(!m.on_done(OpId(4), Some(&[READY][..])), "answered already");
        }
    }
}
