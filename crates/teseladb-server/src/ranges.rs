//! The replicas of a core's data ranges (007 FR-002, FR-004, FR-005, FR-007): core 0 hands every
//! core Group 0's descriptors ([`Ranges`]); each core creates and opens the replicas Group 0 gives
//! its node on that core ([`Replicas::open`]) and, where it leads one, brings the range's
//! configuration to Group 0's replicas, splits it when it grows past the split size and carries
//! out the splits Group 0 recorded ([`Replicas::lead`]).
//!
//! A replica is opened without the disk ([`RangeSm::try_open`]), in key order, and a range born
//! from a split waits until the local replica of its parent applied the cut, which writes its
//! local keys. A replica that has nothing of the range opens with Group 0's bounds. Every range
//! compacts its log before its first replica change and changes none before: a replica added
//! later receives the range as a snapshot, with its bounds, and never replays the log from its
//! first entry, which neither has the data a split left in the parent's LSM nor the bounds a
//! split since then cut. No replica change goes either while the range's snapshot is older than
//! its last split: that snapshot is never sent (D-067).

use std::collections::{BTreeMap, BTreeSet};

use teseladb_lsm::Lsm;
use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::{Group0Sm, RangeDesc, ReplicaRole};
use teseladb_multiraft::{GroupDesc, MultiRaftCore, OpId, StateMachine};
use teseladb_raft::{Role, Status};
use teseladb_range::host::RangeHost;
use teseladb_range::sm::RangeSm;
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::coresm::CoreSm;
use crate::hosting::Hosting;
use crate::replicas::{Step, next_step};
use std::cell::RefCell;
use std::rc::Rc;

/// Live bytes of a range at which its leader asks Group 0 to split it (FR-005, D-068).
pub const SPLIT_BYTES: u64 = 64 << 20;

/// Bytes on disk, versions included, at which a range splits, as a multiple of the live size
/// (FR-005: 512 MiB against 64 MiB).
pub const SPLIT_DISK_FACTOR: u64 = 8;

/// How often a leader measures its ranges.
pub const SIZE_CHECK_EVERY: Nanos = Nanos::from_secs(1);

/// Pause between two sends of the same Group 0 command for a range (`ReplicasSettled`,
/// `SplitRange`, `CompleteSplit`): Group 0 refuses a repeated one, and a lost one goes again.
const RESEND_EVERY: Nanos = Nanos::from_millis(500);

/// A range this core leads, as its duties need it.
struct Led {
    desc: RangeDesc,
    st: Status,
    /// Its bounds as its state machine applied them.
    bounds: (Vec<u8>, Vec<u8>),
    applied: u64,
    /// Its live bytes.
    live: u64,
}

/// Group 0's ranges as core 0 hands them to the cores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranges {
    /// Every range's descriptor.
    pub descs: Vec<RangeDesc>,
    /// The number each recorded split gives its new range, by the range being split.
    pub splits: BTreeMap<GroupId, GroupId>,
    /// The merges recorded, by the range that absorbs: the range absorbed and the merge.
    pub merges: BTreeMap<GroupId, (GroupId, u64)>,
    /// The last range number Group 0 gave: a lower one it no longer has was merged away.
    pub last_group: u64,
    /// RefDb's UUID of each collection of the catalog, by its identifier (the rollback files
    /// are named after it, 005 FR-010).
    pub collections: BTreeMap<u32, [u8; 16]>,
    /// The core changes recorded (007 FR-011): the target core and the intent, by range.
    pub relocations: BTreeMap<GroupId, (u16, u64)>,
}

impl Ranges {
    /// The ranges of a copy of Group 0.
    pub fn of(sm: &Group0Sm) -> Ranges {
        Ranges {
            descs: sm.ranges.values().cloned().collect(),
            splits: sm.split_ids.clone(),
            merges: sm.merges.clone(),
            last_group: sm.last_group,
            collections: sm.catalog.collections.values().map(|c| (c.id, c.uuid)).collect(),
            relocations: sm.relocations.clone(),
        }
    }
}

/// The replicas of one core.
pub struct Replicas {
    me: NodeId,
    core: u16,
    split_bytes: u64,
    /// Group 0's descriptors of the ranges this node holds on this core.
    descs: BTreeMap<GroupId, RangeDesc>,
    splits: BTreeMap<GroupId, GroupId>,
    merges: BTreeMap<GroupId, (GroupId, u64)>,
    /// Every range Group 0 has, on any node and core, and the last number it gave.
    known: (BTreeSet<GroupId>, u64),
    /// Replicas created and opened.
    open: BTreeSet<GroupId>,
    /// Replicas closed while their data is rebuilt (005 FR-014): not opened again until released.
    held: BTreeSet<GroupId>,
    /// Replicas another one absorbed (FR-009): never opened again.
    retired: BTreeSet<GroupId>,
    /// Configuration changes in flight, by range.
    conf_ops: BTreeMap<GroupId, OpId>,
    /// When each range may send its next Group 0 command.
    resend_at: BTreeMap<GroupId, Nanos>,
    /// Next measure of the ranges' sizes.
    check_at: Nanos,
    /// Which cores of the node hold each range (007 FR-011), and the version of it `descs`
    /// reflects.
    hosting: (Hosting, u64),
    /// Every descriptor of Group 0, which `descs` filters.
    all: Vec<RangeDesc>,
}

impl Replicas {
    /// The replicas of core `core` of node `me`; a range splits at `split_bytes`.
    pub fn new(me: NodeId, core: u16, split_bytes: u64) -> Replicas {
        Replicas {
            me,
            core,
            split_bytes,
            descs: BTreeMap::new(),
            splits: BTreeMap::new(),
            merges: BTreeMap::new(),
            known: (BTreeSet::new(), 0),
            open: BTreeSet::new(),
            held: BTreeSet::new(),
            retired: BTreeSet::new(),
            conf_ops: BTreeMap::new(),
            resend_at: BTreeMap::new(),
            check_at: Nanos::ZERO,
            hosting: (Hosting::default(), 0),
            all: Vec::new(),
        }
    }

    /// The core of these replicas.
    pub fn core(&self) -> u16 {
        self.core
    }

    /// The node's table of which cores hold each range, shared with the other cores.
    pub fn set_hosting(&mut self, hosting: Hosting) {
        self.hosting = (hosting, u64::MAX);
    }

    /// The descriptors of the ranges this node holds on this core: Group 0's core, or a core
    /// change that brought the range here before Group 0 records it.
    fn refresh(&mut self) {
        let version = self.hosting.0.version();
        if version == self.hosting.1 {
            return;
        }
        let (me, core, hosting) = (self.me, self.core, &self.hosting.0);
        self.descs = self
            .all
            .iter()
            .filter(|d| d.core == core || hosting.holds(d.group, core))
            .filter(|d| d.nodes().contains(&me))
            .map(|d| (d.group, d.clone()))
            .collect();
        self.hosting.1 = version;
    }

    /// Group 0's ranges changed.
    pub fn set(&mut self, ranges: &Ranges) {
        self.all.clone_from(&ranges.descs);
        self.hosting.1 = u64::MAX;
        self.refresh();
        self.splits = ranges.splits.clone();
        self.merges = ranges.merges.clone();
        self.known = (ranges.descs.iter().map(|d| d.group).collect(), ranges.last_group);
    }

    /// Whether Group 0 merged range `g` away: it gave its number and no longer has it (a range
    /// leaves Group 0 only by a completed merge). A copy of Group 0 behind the range's creation
    /// does not say so.
    pub fn merged_away(&self, g: GroupId) -> bool {
        let (ids, last) = &self.known;
        g.0 <= *last && !ids.contains(&g)
    }

    /// Group 0's descriptors of the ranges this node holds on this core.
    pub fn descs(&self) -> impl Iterator<Item = &RangeDesc> {
        self.descs.values()
    }

    /// The replicas open on this core.
    pub fn open_groups(&self) -> impl Iterator<Item = GroupId> + '_ {
        self.open.iter().copied()
    }

    /// The merge in which range `g` absorbs another: that range and the merge.
    pub fn merging(&self, g: GroupId) -> Option<(GroupId, u64)> {
        self.merges.get(&g).copied()
    }

    /// The merge in which range `g` is absorbed: the range that absorbs it and the merge.
    pub fn absorbing(&self, g: GroupId) -> Option<(GroupId, u64)> {
        self.merges.iter().find(|(_, (r, _))| *r == g).map(|(l, (_, m))| (*l, *m))
    }

    /// Replica `g` was absorbed by another one of this core: it is not open and never opens
    /// again. `false` if it was not open.
    pub fn retire(&mut self, g: GroupId) -> bool {
        self.retired.insert(g);
        self.open.remove(&g)
    }

    /// Whether this core has range `g` open.
    pub fn is_open(&self, g: GroupId) -> bool {
        self.open.contains(&g)
    }

    /// Replica `g` was closed for a rebuild: it is not open, and is not opened again until
    /// [`Replicas::release`].
    pub fn hold(&mut self, g: GroupId) {
        self.open.remove(&g);
        self.held.insert(g);
    }

    /// The rebuild of replica `g` cleared its data: the next [`Replicas::open`] opens it again.
    pub fn release(&mut self, g: GroupId) {
        self.held.remove(&g);
    }

    /// The replicas of range `g` this node knows of.
    pub fn nodes(&self, g: GroupId) -> Vec<NodeId> {
        self.descs.get(&g).map(RangeDesc::nodes).unwrap_or_default()
    }

    /// Creates and opens, in key order, the replicas Group 0 gives this core that are not open
    /// yet; it stops at the first one that has to wait (a read from the disk, or its parent's
    /// cut).
    pub fn open(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        lsm: &Rc<RefCell<Lsm>>,
    ) {
        self.refresh();
        let mut waiting: Vec<&RangeDesc> = self
            .descs
            .values()
            .filter(|d| !self.open.contains(&d.group) && !self.held.contains(&d.group))
            .filter(|d| !self.retired.contains(&d.group))
            .collect();
        waiting.sort_by(|a, b| a.start.cmp(&b.start));
        let mut opened = Vec::new();
        for d in waiting {
            // Another core of the node holds the range (it moves between cores, 007 FR-011): no
            // replica of it is created here.
            let others = self.hosting.0.others(d.group, self.core);
            if others && core.store().group(d.group).is_none() {
                continue;
            }
            if self.parent_uncut(core, &d.start) {
                break;
            }
            let sm = RangeSm::try_open(d.group, d.start.clone(), d.end.clone(), lsm.clone());
            let Ok(Some(mut sm)) = sm else { break };
            sm.set_core(self.core);
            // A copy imported here starts once no other core holds the range.
            if others && sm.relocated().is_some_and(|(t, _)| t == self.core) {
                continue;
            }
            // A replica opened with nothing of its range gets it as a snapshot: the range's log
            // lacks what a split left in its parent's LSM, and what its compaction dropped
            // (D-067). The first replica of the first range takes its entries as their leader.
            sm.expect_snapshot();
            let (start, end) = (sm.bounds().0.to_vec(), sm.bounds().1.to_vec());
            let born = sm.born();
            let role = |r: ReplicaRole| d.replicas.iter().filter(move |(_, x)| *x == r);
            let desc = GroupDesc {
                group: d.group,
                voters: role(ReplicaRole::Voter).map(|(n, _)| *n).collect(),
                learners: role(ReplicaRole::Learner).map(|(n, _)| *n).collect(),
            };
            if core.create_group(desc, CoreSm::Range(sm)).is_err() {
                break;
            }
            self.hosting.0.add(d.group, self.core);
            match born {
                Some((parent, at_index)) => {
                    host.add_born_range(d.group, start, end, parent, at_index)
                }
                None => host.add_range(d.group, start, end),
            }
            opened.push(d.group);
        }
        self.open.extend(opened);
    }

    /// Whether an open range of this core still holds `start` strictly inside: the range that
    /// starts there was split off it and its local keys arrive with the cut.
    fn parent_uncut(&self, core: &MultiRaftCore<CoreSm>, start: &[u8]) -> bool {
        self.open.iter().filter_map(|g| core.state_machine(*g).and_then(CoreSm::range)).any(|sm| {
            let (s, e) = sm.bounds();
            s < start && start < e
        })
    }

    /// The duties of this core's leaders: each range's configuration brought to Group 0's
    /// replicas, the splits Group 0 recorded carried out and completed, and the ranges that grew
    /// past the split size measured. The Group 0 commands for core 0.
    pub fn lead(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        lsm: &Lsm,
        now: Nanos,
    ) -> Vec<Group0Command> {
        let mut out = Vec::new();
        for done in host.take_splits() {
            let at_index = core.status(done.g).map_or(0, |s| s.applied);
            out.push(Group0Command::CompleteSplit { group: done.g, at: done.at, at_index });
        }
        let measure = now >= self.check_at;
        if measure {
            self.check_at = now + SIZE_CHECK_EVERY;
        }
        let led: Vec<Led> = self
            .descs
            .values()
            .filter(|d| self.open.contains(&d.group))
            .filter_map(|d| {
                let st = core.status(d.group).filter(|s| s.role == Role::Leader)?;
                let sm = core.state_machine(d.group).and_then(CoreSm::range)?;
                let (start, end) = sm.bounds();
                let bounds = (start.to_vec(), end.to_vec());
                let (applied, live) = (sm.applied_index(), sm.live_bytes());
                Some(Led { desc: d.clone(), st, bounds, applied, live })
            })
            .collect();
        for l in led {
            if !self.compacted(core, l.desc.group) {
                continue;
            }
            self.replicas(core, &l, now, &mut out);
            self.split(host, lsm, &l, (measure, now), &mut out);
        }
        out
    }

    /// Whether range `g` may change its replicas: once its log no longer starts at its first
    /// entry, which this asks for, and its snapshot is not older than its last split, which the
    /// core takes after it (D-067): a replica added sooner would wait for it anyway.
    fn compacted(&self, core: &mut MultiRaftCore<CoreSm>, g: GroupId) -> bool {
        let floor = core.state_machine(g).map_or(0, StateMachine::snapshot_floor);
        if let Some(saved) = core.store().group(g).map(|l| l.snapshot_index).filter(|i| *i > 0) {
            return saved >= floor;
        }
        // A range born from a split takes its snapshot once it wrote (its `SplitGenesis`): a
        // snapshot with no write says the replica that receives it needs no data, and the data
        // the split left in the parent's LSM would not go.
        let ready = core
            .state_machine(g)
            .and_then(CoreSm::range)
            .is_some_and(|sm| sm.applied_index() > 0 && (sm.last_write() > 0 || !sm.from_split()));
        if ready {
            core.snapshot_now(g);
        }
        false
    }

    fn may_send(&mut self, g: GroupId, now: Nanos) -> bool {
        if self.resend_at.get(&g).is_some_and(|at| now < *at) {
            return false;
        }
        self.resend_at.insert(g, now + RESEND_EVERY);
        true
    }

    /// The leader brings the range's configuration to Group 0's replicas.
    fn replicas(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        l: &Led,
        now: Nanos,
        out: &mut Vec<Group0Command>,
    ) {
        let (g, st) = (l.desc.group, &l.st);
        if self.conf_ops.contains_key(&g) {
            return;
        }
        let conf = core.conf(g).cloned().unwrap_or_default();
        match next_step(&l.desc, &conf, &st.progress, st.hard_state.commit, self.me) {
            Some(Step::Conf(cc)) => {
                if let Ok(op) = core.propose_conf_change(g, cc, now) {
                    self.conf_ops.insert(g, op);
                }
            }
            Some(Step::Settle { epoch }) if self.may_send(g, now) => {
                out.push(Group0Command::ReplicasSettled { group: g, epoch });
            }
            Some(Step::Settle { .. }) | None => {}
        }
    }

    /// The leader carries out the split Group 0 recorded, completes it once the cut is applied,
    /// or asks for one when the range grew past the split size (FR-005: the cut key comes from the
    /// files' block index and never falls between two versions of a key) or served more than the
    /// load threshold for 10 s (FR-006: the cut key comes from the keys it served).
    fn split(
        &mut self,
        host: &mut RangeHost,
        lsm: &Lsm,
        l: &Led,
        (measure, now): (bool, Nanos),
        out: &mut Vec<Group0Command>,
    ) {
        let (g, desc) = (l.desc.group, &l.desc);
        let (start, end) = (&l.bounds.0, &l.bounds.1);
        if let Some(at) = &desc.pending_split {
            if end == at {
                if self.may_send(g, now) {
                    let at_index = l.applied;
                    out.push(Group0Command::CompleteSplit { group: g, at: at.clone(), at_index });
                }
            } else if let Some(new_range) = self.splits.get(&g) {
                // `false` while a split is in progress: it goes on by itself.
                host.split(g, at, *new_range);
            }
            return;
        }
        // A range whose bounds Group 0 does not have yet (a cut not completed) is not measured.
        if !measure || desc.settling || *end != desc.end {
            return;
        }
        let size = lsm.interval_size(start, end);
        let disk_limit = self.split_bytes.saturating_mul(SPLIT_DISK_FACTOR);
        // By size (FR-005) at the files' middle key, or else by load (FR-006) at the median of the
        // keys the leader served.
        let by_size = l.live >= self.split_bytes || size.bytes >= disk_limit;
        let at = if by_size { size.cut } else { host.load_cut(g, now) };
        // Measured once a second: a request Group 0 has not applied yet goes again then.
        if let Some(at) = at {
            out.push(Group0Command::SplitRange { group: g, at });
        }
    }

    /// The end of a configuration change; `false` if it is not one of this module's.
    pub fn on_done(&mut self, op: OpId) -> bool {
        let Some(g) = self.conf_ops.iter().find(|(_, o)| **o == op).map(|(g, _)| *g) else {
            return false;
        };
        self.conf_ops.remove(&g);
        true
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_lsm::{EntryWrite, Kind, LsmConfig};
    use teseladb_multiraft::{CoreConfig, NodeClock};
    use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};

    const ME: NodeId = NodeId(1);
    const G: GroupId = GroupId(1);

    struct One {
        core: MultiRaftCore<CoreSm>,
        host: RangeHost,
        lsm: Rc<RefCell<Lsm>>,
        disk: MemDisk,
        r: Replicas,
        now: Nanos,
    }

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

    impl One {
        fn new() -> One {
            let mut disk = MemDisk::new();
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
            let core = MultiRaftCore::new(CoreConfig::new(ME, 0), store, 1);
            let lcfg = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
            let lsm = Rc::new(RefCell::new(Lsm::recover(&lcfg, &mut disk).unwrap().0));
            let host = RangeHost::new(lsm.clone(), NodeClock::for_node(ME));
            One { core, host, lsm, disk, r: Replicas::new(ME, 0, 4 << 10), now: Nanos::ZERO }
        }

        /// Runs the core, the LSM and the host for `n` ticks of 10 ms.
        fn run(&mut self, n: u32) {
            for _ in 0..n {
                self.now = self.now + Nanos::from_millis(10);
                let now = self.now;
                self.core.on_tick(now, now.0 / 1_000_000);
                while let Some(op) = self.core.poll_io(now) {
                    let done = self.disk.execute(op);
                    self.core.on_io_done(done, now);
                }
                self.lsm.borrow_mut().run_io(&mut self.disk, now);
                let _ = self.core.drain_actions();
                self.host.follow_state(&self.core);
                self.host.poll(&mut self.core, now, now.0 / 1_000_000);
            }
        }

        fn set(&mut self, descs: Vec<RangeDesc>, splits: &[(u64, u64)]) {
            let splits = splits.iter().map(|(a, b)| (GroupId(*a), GroupId(*b))).collect();
            self.r.set(&Ranges { descs, splits, ..Ranges::default() });
        }

        fn lead(&mut self) -> Vec<Group0Command> {
            let lsm = self.lsm.clone();
            self.r.lead(&mut self.core, &mut self.host, &lsm.borrow(), self.now)
        }
    }

    #[test]
    fn a_leader_compacts_first_then_asks_to_split_what_grew_and_completes_a_cut() {
        let mut o = One::new();
        // A range of another core, and one of another node, are not this core's.
        let mut elsewhere = desc(5, &[3], &[4]);
        elsewhere.core = 1;
        let mut foreign = desc(6, &[3], &[4]);
        foreign.replicas = vec![(NodeId(2), ReplicaRole::Voter)];
        o.set(vec![desc(1, &[1], &[4]), elsewhere, foreign], &[]);
        assert_eq!(o.r.nodes(G), [ME]);
        assert!(o.r.nodes(GroupId(5)).is_empty());
        let lsm = o.lsm.clone();
        o.r.open(&mut o.core, &mut o.host, &lsm);
        assert!(o.r.is_open(G) && !o.r.is_open(GroupId(5)));
        // A range that starts inside an open one waits for its cut.
        o.set(vec![desc(1, &[1], &[4]), desc(2, &[2], &[4])], &[]);
        o.r.open(&mut o.core, &mut o.host, &lsm);
        assert!(!o.r.is_open(GroupId(2)));
        o.set(vec![desc(1, &[1], &[4])], &[]);
        o.run(20);
        // Data past the split size on disk (eight times the live size), written straight to the
        // LSM: the range's live counter does not see it.
        let value = vec![7u8; 300];
        for i in 0..200u32 {
            let k = [&[1u8][..], format!("k{i:04}").as_bytes()].concat();
            let w = [EntryWrite { key: &k, kind: Kind::Put, value: &value }];
            let ts = teseladb_hlc::Hlc { physical: u64::from(i) + 1, logical: 0 };
            lsm.borrow_mut().write_entry(GroupId(90), u64::from(i) + 1, ts, &w).unwrap();
        }
        // Nothing before the log is compacted, which the leader asks for.
        let mut asked = Vec::new();
        for _ in 0..200 {
            asked = o.lead();
            if !asked.is_empty() {
                break;
            }
            o.lsm.borrow_mut().flush();
            o.run(20);
        }
        assert!(o.core.store().group(G).is_some_and(|l| l.snapshot_index > 0));
        assert!(
            matches!(asked.as_slice(), [Group0Command::SplitRange { group: G, .. }]),
            "{asked:?}"
        );
        // Measured once a second, and asked again only after the pause.
        assert!(o.lead().is_empty());
        // Settling, or with other bounds in Group 0, it is not measured.
        o.run(110);
        let mut settling = desc(1, &[1], &[4]);
        settling.settling = true;
        o.set(vec![settling], &[]);
        // The change already matches the configuration: only `ReplicasSettled` goes.
        let settled = o.lead();
        assert!(
            matches!(settled.as_slice(), [Group0Command::ReplicasSettled { .. }]),
            "{settled:?}"
        );
        // Not again before the pause.
        assert!(o.lead().is_empty());
        o.run(110);
        o.set(vec![desc(1, &[1], &[3])], &[]);
        assert!(o.lead().is_empty());
        // A recorded split without its number waits; a cut already applied is completed, once
        // per pause.
        let mut pending = desc(1, &[1], &[4]);
        pending.pending_split = Some(vec![1, 5]);
        o.set(vec![pending.clone()], &[]);
        assert!(o.lead().is_empty());
        pending.pending_split = Some(vec![4]);
        o.set(vec![pending], &[(1, 7)]);
        let done = o.lead();
        assert!(
            matches!(done.as_slice(), [Group0Command::CompleteSplit { group: G, .. }]),
            "{done:?}"
        );
        assert!(o.lead().is_empty());
        assert!(!o.r.on_done(OpId(12345)));
    }

    #[test]
    fn a_range_that_serves_a_concentrated_load_splits_inside_it() {
        use teseladb_range::read::ReadConcern;
        let mut o = One::new();
        // 100 operations a second against a threshold of 50.
        o.host.set_load_split_ops(50);
        o.set(vec![desc(1, &[1], &[4])], &[]);
        let lsm = o.lsm.clone();
        o.r.open(&mut o.core, &mut o.host, &lsm);
        // The log is compacted first (the snapshot waits for a flush).
        for _ in 0..50 {
            if o.core.store().group(G).is_some_and(|l| l.snapshot_index > 0) {
                break;
            }
            o.lead();
            o.lsm.borrow_mut().flush();
            o.run(20);
        }
        // Every read lands on `[3, 0..40)`, a small part of `[1, 4)`.
        let mut asked = Vec::new();
        for i in 0..1_500u32 {
            let key = [3, (i % 40) as u8];
            let now = o.now;
            o.host.get(&mut o.core, G, &key, ReadConcern::Local, now, now.0 / 1_000_000);
            o.run(1);
            asked = o.lead();
            if !asked.is_empty() {
                break;
            }
        }
        let [Group0Command::SplitRange { group: G, at }] = asked.as_slice() else {
            panic!("{asked:?}")
        };
        assert!(at.as_slice() > &[3, 8][..] && at.as_slice() < &[3, 32][..], "{at:?}");
        assert!(o.host.load(G) >= 50);
    }

    #[test]
    fn neighbours_ask_to_merge_only_when_paired_free_small_and_cold() {
        use crate::merges::Merges;
        let mut o = One::new();
        o.set(vec![desc(1, &[1], &[2]), desc(2, &[2], &[4])], &[]);
        let lsm = o.lsm.clone();
        o.r.open(&mut o.core, &mut o.host, &lsm);
        o.run(50);
        let ask = |o: &mut One, m: &mut Merges| {
            let lsm = o.lsm.clone();
            m.step(&mut o.core, &mut o.host, &mut o.r, &lsm, o.now)
        };
        let intent = |out: &[Group0Command]| {
            out.iter().any(|c| matches!(c, Group0Command::MergeRanges { left: G, .. }))
        };
        // Not cold yet.
        let mut m = Merges::new(ME, 16 << 20, Nanos::from_secs(1));
        assert!(!intent(&ask(&mut o, &mut m)));
        o.run(120);
        // Other replicas, a split in progress, another merge, or a right range not open here.
        let mut other = desc(2, &[2], &[4]);
        other.replicas.push((NodeId(2), ReplicaRole::Voter));
        let mut splitting = desc(2, &[2], &[4]);
        splitting.pending_split = Some(vec![3]);
        for right in [other, splitting] {
            o.set(vec![desc(1, &[1], &[2]), right], &[]);
            assert!(!intent(&ask(&mut o, &mut m)));
        }
        o.set(vec![desc(1, &[1], &[2]), desc(2, &[2], &[4])], &[]);
        o.r.merges.insert(GroupId(2), (GroupId(9), 3));
        assert!(!intent(&ask(&mut o, &mut m)));
        o.r.merges.clear();
        o.r.open.remove(&GroupId(2));
        assert!(!intent(&ask(&mut o, &mut m)));
        o.r.open.insert(GroupId(2));
        // Too big (every range holds more than nothing), or with other bounds in Group 0.
        let mut big = Merges::new(ME, 0, Nanos::from_secs(1));
        assert!(!intent(&ask(&mut o, &mut big)));
        o.set(vec![desc(1, &[1], &[3]), desc(2, &[3], &[4])], &[]);
        assert!(!intent(&ask(&mut o, &mut m)));
        // Paired, free, small and cold (range 2 counts again since it was open again): the left
        // range asks, once per pause.
        o.set(vec![desc(1, &[1], &[2]), desc(2, &[2], &[4])], &[]);
        assert!(!intent(&ask(&mut o, &mut m)));
        o.run(120);
        assert!(intent(&ask(&mut o, &mut m)));
        assert!(!intent(&ask(&mut o, &mut m)));
        // A probe this module did not send is not its own.
        assert!(!m.on_done(OpId(777), None));
    }

    #[test]
    fn a_right_replica_left_in_the_log_by_a_restart_is_retired_too() {
        use crate::merges::Merges;
        let mut o = One::new();
        let right = GroupId(2);
        o.set(vec![desc(1, &[1], &[2]), desc(2, &[2], &[4])], &[]);
        let lsm = o.lsm.clone();
        o.r.open(&mut o.core, &mut o.host, &lsm);
        o.run(50);
        // The merge on one node: frozen, durable, absorbed.
        o.r.merges.insert(G, (right, 5));
        let mut m = Merges::new(ME, 16 << 20, Nanos::from_secs(1));
        let absorbed = |o: &One| {
            o.core
                .state_machine(G)
                .and_then(CoreSm::range)
                .is_some_and(|sm| sm.absorbed().is_some())
        };
        for _ in 0..300 {
            if absorbed(&o) {
                break;
            }
            let lsm = o.lsm.clone();
            m.step(&mut o.core, &mut o.host, &mut o.r, &lsm, o.now);
            o.run(1);
        }
        assert!(absorbed(&o));
        // The node stops before retiring the right replica: its log is still on disk.
        assert!(o.core.state_machine(right).is_some());
        o.lsm.borrow_mut().flush();
        o.run(20);
        let One { mut disk, .. } = o;
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let core = MultiRaftCore::new(CoreConfig::new(ME, 0), store, 2);
        let lcfg = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
        let lsm = Rc::new(RefCell::new(Lsm::recover(&lcfg, &mut disk).unwrap().0));
        let host = RangeHost::new(lsm.clone(), NodeClock::for_node(ME));
        let mut o =
            One { core, host, lsm, disk, r: Replicas::new(ME, 0, 4 << 10), now: Nanos::ZERO };
        assert!(o.core.recovered_groups().contains(&right));
        // Group 0 completed the merge: only the left range is opened.
        o.set(vec![desc(1, &[1], &[4])], &[]);
        let mut m = Merges::new(ME, 16 << 20, Nanos::from_secs(1));
        for _ in 0..50 {
            // As every poll of the data core does: the replicas open once their keys are read.
            let lsm = o.lsm.clone();
            o.r.open(&mut o.core, &mut o.host, &lsm);
            m.step(&mut o.core, &mut o.host, &mut o.r, &lsm, o.now);
            o.run(1);
        }
        assert!(o.r.is_open(G));
        assert!(!o.core.recovered_groups().contains(&right));
        assert!(o.core.state_machine(right).is_none() && o.core.store().group(right).is_none());
    }

    #[test]
    fn a_frozen_replica_of_a_range_group_0_merged_away_is_retired_before_its_neighbour_absorbs_it()
    {
        use crate::merges::Merges;
        let mut o = One::new();
        let right = GroupId(2);
        o.set(vec![desc(1, &[1], &[2]), desc(2, &[2], &[4])], &[]);
        let lsm = o.lsm.clone();
        o.r.open(&mut o.core, &mut o.host, &lsm);
        o.run(50);
        // The right replica freezes for the merge; this node's left one has not absorbed it (it
        // will get the merge in a snapshot, whose data cannot land over the right one's keys).
        o.r.merges.insert(G, (right, 5));
        let mut m = Merges::new(ME, 16 << 20, Nanos::from_secs(1));
        let frozen = |o: &One| {
            o.core
                .state_machine(right)
                .and_then(CoreSm::range)
                .is_some_and(|sm| sm.frozen_for() == Some(5))
        };
        for _ in 0..300 {
            if frozen(&o) {
                break;
            }
            let lsm = o.lsm.clone();
            m.step(&mut o.core, &mut o.host, &mut o.r, &lsm, o.now);
            o.run(1);
        }
        assert!(frozen(&o));
        let absorbed = o.core.state_machine(G).and_then(CoreSm::range).and_then(RangeSm::absorbed);
        assert_eq!(absorbed, None);
        // A copy of Group 0 that has not seen the right range yet leaves it alone.
        o.r.set(&Ranges { descs: vec![desc(1, &[1], &[4])], last_group: 1, ..Ranges::default() });
        let lsm = o.lsm.clone();
        m.step(&mut o.core, &mut o.host, &mut o.r, &lsm, o.now);
        assert!(o.core.state_machine(right).is_some());
        // Group 0 completed the merge: the right replica goes.
        o.r.set(&Ranges { descs: vec![desc(1, &[1], &[4])], last_group: 2, ..Ranges::default() });
        for _ in 0..50 {
            let lsm = o.lsm.clone();
            m.step(&mut o.core, &mut o.host, &mut o.r, &lsm, o.now);
            o.run(1);
        }
        assert!(o.core.state_machine(right).is_none() && o.core.store().group(right).is_none());
        assert!(!o.r.is_open(right));
    }

    /// A version of `key` with `size` bytes of value.
    struct PutBytes(Vec<u8>, usize);

    impl teseladb_range::host::Mutation for PutBytes {
        fn eval(
            &self,
            _: &[Option<(u8, &[u8])>],
            _: teseladb_hlc::Hlc,
        ) -> Result<teseladb_range::mutate::Outcome, teseladb_range::host::MutationError> {
            let put = teseladb_range::command::Put {
                key: self.0.clone(),
                value_type: 3,
                value: vec![9; self.1],
            };
            Ok(teseladb_range::mutate::Outcome {
                puts: vec![put],
                deletes: Vec::new(),
                change: Vec::new(),
                result: Vec::new(),
            })
        }
    }

    #[test]
    fn a_range_whose_live_bytes_reach_the_split_size_asks_to_split_below_the_disk_size() {
        use teseladb_range::ack::{W, WriteConcern};
        use teseladb_range::host::WriteReq;
        let mut o = One::new();
        o.set(vec![desc(1, &[1], &[4])], &[]);
        let lsm = o.lsm.clone();
        o.r.open(&mut o.core, &mut o.host, &lsm);
        o.run(30);
        // The log is compacted first (the snapshot waits for a flush).
        for _ in 0..50 {
            assert!(o.lead().is_empty());
            if o.core.store().group(G).is_some_and(|l| l.snapshot_index > 0) {
                break;
            }
            o.lsm.borrow_mut().flush();
            o.run(20);
        }
        // 6 KiB of live versions against a split size of 4 KiB: far below 32 KiB on disk.
        for i in 0..12u8 {
            let key = vec![2, i];
            let wc = WriteConcern { w: W::Nodes(1), j: false };
            let m = Box::new(PutBytes(key.clone(), 500));
            let w = WriteReq { g: G, keys: vec![key], m, dedup: None, wc };
            let now = o.now;
            o.host.mutate(&mut o.core, w, now, now.0 / 1_000_000);
            o.run(5);
        }
        let sm = o.core.state_machine(G).and_then(CoreSm::range).unwrap();
        assert!(sm.live_bytes() >= 4 << 10, "{}", sm.live_bytes());
        assert!(o.lsm.borrow().interval_size(&[1], &[4]).bytes < 32 << 10);
        let mut asked = Vec::new();
        for _ in 0..300 {
            asked = o.lead();
            if !asked.is_empty() {
                break;
            }
            o.run(20);
        }
        assert!(
            matches!(asked.as_slice(), [Group0Command::SplitRange { group: G, .. }]),
            "{asked:?}"
        );
    }
}
