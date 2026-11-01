//! Range descriptors by key (007 FR-002): Group 0 keeps its ranges by group and, next to them,
//! an index by first key, so the range of a key is the one with the greatest first key at or
//! below it (ranges never overlap) and the ranges of an interval come out in key order. The index
//! is derived from the descriptors: it is rebuilt on restore and never travels in the snapshot.
//!
//! The orders on descriptors (creation, splits, merges, replica, leader and core changes) live in
//! [`DescTable`], which Group 0 and the Meta2 ranges ([`crate::meta2`]) both apply, so a Meta2 range
//! applies them with the same epoch and generation conditions Group 0 applied (007 FR-014).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use teseladb_types::{GroupId, NodeId};

use crate::changelog::{Change, ChangeKind, Changelog};
use crate::group0::{Group0Error, Group0Sm, RangeDesc, ReplicaRole};

/// First key → range, for the ranges of Group 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeIndex {
    by_start: BTreeMap<Vec<u8>, GroupId>,
}

impl RangeIndex {
    /// Indexes range `g` by its first key.
    pub fn insert(&mut self, start: &[u8], g: GroupId) {
        self.by_start.insert(start.to_vec(), g);
    }

    /// Forgets the range whose first key is `start`.
    pub fn remove(&mut self, start: &[u8]) {
        self.by_start.remove(start);
    }

    /// The range with the greatest first key at or below `key`, if any.
    pub fn floor(&self, key: &[u8]) -> Option<GroupId> {
        self.by_start
            .range::<[u8], _>((Bound::Unbounded, Bound::Included(key)))
            .next_back()
            .map(|(_, g)| *g)
    }

    /// Ranges whose first key is in `[from, to)` (`to` empty: no end), in key order.
    pub fn starting_in<'a>(
        &'a self,
        from: &[u8],
        to: &'a [u8],
    ) -> impl Iterator<Item = GroupId> + 'a {
        self.by_start
            .range::<[u8], _>((Bound::Included(from), Bound::Unbounded))
            .take_while(move |(s, _)| to.is_empty() || s.as_slice() < to)
            .map(|(_, g)| *g)
    }

    /// Number of ranges indexed.
    pub fn len(&self) -> usize {
        self.by_start.len()
    }

    /// Whether no range is indexed.
    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }
}

/// What an order on descriptors checks about the cluster's nodes.
pub(crate) trait NodeFacts {
    /// Whether node `n` is known and active.
    fn active(&self, n: NodeId) -> bool;
    /// The cores of node `n`, if it is known (in any state).
    fn cores(&self, n: NodeId) -> Option<u16>;
}

/// Where the numbers of new ranges come from.
pub(crate) enum Ids<'a> {
    /// Group 0's counter, below [`BLOCK_BASE`]: the numbers above are the Meta2 ranges' blocks.
    Group0(&'a mut u64),
    /// The block of a Meta2 range: its group in the high 32 bits and a counter in the low ones.
    Block {
        /// The Meta2 range's group.
        owner: GroupId,
        /// Last number given in the block.
        last: &'a mut u64,
    },
}

/// First number of the Meta2 ranges' blocks: Group 0 gives numbers below it.
pub const BLOCK_BASE: u64 = 1 << 32;

impl Ids<'_> {
    /// The next number, if any is left.
    fn next(&mut self) -> Option<GroupId> {
        match self {
            Ids::Group0(last) => {
                let n = (**last + 1 < BLOCK_BASE).then_some(**last + 1)?;
                **last = n;
                Some(GroupId(n))
            }
            Ids::Block { owner, last } => {
                let n = (**last + 1 < BLOCK_BASE).then_some(**last + 1)?;
                **last = n;
                Some(GroupId((owner.0 << 32) | n))
            }
        }
    }
}

/// The descriptors a holder (Group 0 or a Meta2 range) keeps, with its changes in progress.
pub(crate) struct DescTable<'a> {
    /// Descriptors by group.
    pub ranges: &'a mut BTreeMap<GroupId, RangeDesc>,
    /// The index by first key.
    pub index: &'a mut RangeIndex,
    /// The number each recorded split gives its new range, by the range being split.
    pub split_ids: &'a mut BTreeMap<GroupId, GroupId>,
    /// The merges in progress, by the range that absorbs.
    pub merges: &'a mut BTreeMap<GroupId, (GroupId, u64)>,
    /// The core changes in progress, by range.
    pub relocations: &'a mut BTreeMap<GroupId, (u16, u64)>,
    /// Numbers of new ranges.
    pub ids: Ids<'a>,
    /// The history of the changes completed here.
    pub log: &'a mut Changelog,
    /// The mark of the entry being applied, in milliseconds.
    pub now_ms: u64,
}

/// The change `kind` of range `r` as it is now, at `time_ms`.
fn change_of(r: &RangeDesc, time_ms: u64, kind: ChangeKind) -> Change {
    Change {
        seq: 0,
        time_ms,
        group: r.group,
        start: r.start.clone(),
        end: r.end.clone(),
        leader: r.leader_hint,
        kind,
    }
}

/// The merge that absorbs range `g` among `merges`: the range that absorbs it and the intent.
pub(crate) fn merging_in(
    merges: &BTreeMap<GroupId, (GroupId, u64)>,
    g: GroupId,
) -> Option<(GroupId, u64)> {
    merges.iter().find(|(_, (r, _))| *r == g).map(|(l, (_, at))| (*l, *at))
}

/// The core for a new range on `replicas` (FR-004): the one below `cores` with the fewest ranges
/// of `ranges` summed over those nodes, Group 0 counting on core 0 of each; the lowest on a tie.
pub(crate) fn least_loaded_core(
    ranges: &BTreeMap<GroupId, RangeDesc>,
    replicas: &[NodeId],
    cores: u16,
) -> u16 {
    let mut load = vec![replicas.len(); usize::from(cores)];
    load[1..].fill(0);
    for r in ranges.values() {
        let shared = r.replicas.iter().filter(|(n, _)| replicas.contains(n)).count();
        if let Some(l) = load.get_mut(usize::from(r.core)) {
            *l += shared;
        }
    }
    let best = load.iter().enumerate().min_by_key(|(c, l)| (**l, *c)).map_or(0, |(c, _)| c);
    best as u16
}

/// Whether the replicas are a non-empty list of distinct active nodes.
pub(crate) fn good_replicas(nodes: &[NodeId], facts: &dyn NodeFacts) -> bool {
    let mut sorted = nodes.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    !nodes.is_empty() && sorted.len() == nodes.len() && nodes.iter().all(|n| facts.active(*n))
}

impl DescTable<'_> {
    /// Adds a descriptor with its entry in the index by key.
    fn insert(&mut self, desc: RangeDesc) {
        self.index.insert(&desc.start, desc.group);
        self.ranges.insert(desc.group, desc);
    }

    fn range_mut(&mut self, g: GroupId) -> Result<&mut RangeDesc, Group0Error> {
        self.ranges.get_mut(&g).ok_or(Group0Error::NoSuchGroup)
    }

    /// Whether range `g` is either side of a recorded merge, or moves to another core: neither
    /// lets it split, merge or change replicas.
    pub fn in_change(&self, g: GroupId) -> bool {
        self.merges.contains_key(&g)
            || merging_in(self.merges, g).is_some()
            || self.relocations.contains_key(&g)
    }

    fn overlaps(&self, start: &[u8], end: &[u8]) -> bool {
        self.ranges.values().any(|r| {
            let before_end = r.end.is_empty() || start < r.end.as_slice();
            let after_start = end.is_empty() || end > r.start.as_slice();
            before_end && after_start
        })
    }

    /// A new range over `[start, end)` on `replicas`, on their least loaded core.
    pub fn create_range(
        &mut self,
        start: &[u8],
        end: &[u8],
        replicas: &[NodeId],
        facts: &dyn NodeFacts,
    ) -> Result<GroupId, Group0Error> {
        if !good_replicas(replicas, facts) {
            return Err(Group0Error::BadReplicas);
        }
        if (!end.is_empty() && end <= start) || self.overlaps(start, end) {
            return Err(Group0Error::BadBounds);
        }
        let group = self.ids.next().ok_or(Group0Error::OutOfGroupIds)?;
        // Every replica runs it on the same core: the smallest core count of its nodes bounds it.
        let cores = replicas.iter().map(|n| facts.cores(*n).unwrap_or(1).max(1)).min().unwrap_or(1);
        let mut nodes = replicas.to_vec();
        nodes.sort_unstable();
        let core = least_loaded_core(self.ranges, &nodes, cores);
        let desc = RangeDesc {
            group,
            start: start.to_vec(),
            end: end.to_vec(),
            core,
            replicas: nodes.into_iter().map(|n| (n, ReplicaRole::Voter)).collect(),
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
        };
        self.insert(desc);
        Ok(group)
    }

    /// Adds and removes replicas of `g` at replica epoch `epoch`.
    pub fn set_replicas(
        &mut self,
        g: GroupId,
        epoch: u64,
        add: &[NodeId],
        remove: &[NodeId],
        facts: &dyn NodeFacts,
    ) -> Result<(), Group0Error> {
        let all_known = add.iter().all(|n| facts.active(*n));
        let merging = self.in_change(g);
        let now_ms = self.now_ms;
        let r = self.range_mut(g)?;
        if merging || r.settling || r.replica_epoch != epoch || r.pending_split.is_some() {
            return Err(Group0Error::RangeBusy);
        }
        let held = r.nodes();
        let bad_add = !all_known || add.iter().any(|n| held.contains(n));
        let bad_remove = remove.iter().any(|n| !held.contains(n));
        let left = (held.len() + add.len()).saturating_sub(remove.len());
        if bad_add || bad_remove || left == 0 || (add.is_empty() && remove.is_empty()) {
            return Err(Group0Error::BadReplicas);
        }
        let kind = ChangeKind::Replicas { added: add.to_vec(), removed: remove.to_vec() };
        let change = change_of(r, now_ms, kind);
        r.replicas.retain(|(n, _)| !remove.contains(n));
        r.replicas.extend(add.iter().map(|n| (*n, ReplicaRole::Learner)));
        r.replicas.sort_unstable_by_key(|(n, _)| *n);
        r.replica_epoch += 1;
        r.settling = true;
        self.log.record(change);
        Ok(())
    }

    /// The replica change of `g` at epoch `epoch` settled: every replica votes.
    pub fn settle(&mut self, g: GroupId, epoch: u64) -> Result<(), Group0Error> {
        let r = self.range_mut(g)?;
        if !r.settling || r.replica_epoch != epoch {
            return Err(Group0Error::RangeBusy);
        }
        r.settling = false;
        for (_, role) in &mut r.replicas {
            *role = ReplicaRole::Voter;
        }
        Ok(())
    }

    /// A leader that published itself in `term`, or with term 0 the balancer's request; a
    /// publication of a term older than the one recorded changes nothing (007 FR-010).
    pub fn leader_hint(&mut self, g: GroupId, node: NodeId, term: u64) -> Result<(), Group0Error> {
        let r = self.range_mut(g)?;
        if !r.nodes().contains(&node) {
            return Err(Group0Error::BadReplicas);
        }
        if term == 0 || term >= r.leader_term {
            r.leader_hint = Some(node);
            r.leader_term = term;
        }
        Ok(())
    }

    /// Records the intent of splitting `g` at `at` and gives the new range its number now: the
    /// range's `Split` entry writes the new range's state under it (007 FR-007). A cut inside a
    /// capped collection of `capped` moves to its edge (015 research R1).
    pub fn split(
        &mut self,
        g: GroupId,
        at: &[u8],
        capped: &BTreeSet<(u32, u32)>,
    ) -> Result<GroupId, Group0Error> {
        let merging = self.in_change(g);
        let r = self.ranges.get(&g).ok_or(Group0Error::NoSuchGroup)?;
        if merging || r.pending_split.is_some() || r.settling {
            return Err(Group0Error::RangeBusy);
        }
        if at <= r.start.as_slice() || !r.contains(at) {
            return Err(Group0Error::BadSplit);
        }
        let is_capped = |t: u32, c: u32| capped.contains(&(t, c));
        // A change meta (014) stays with its document: the leader cuts before the document.
        let collection = |i: usize| Some(u32::from_be_bytes(at.get(i..i + 4)?.try_into().ok()?));
        if let (Some(t), Some(c)) = (collection(1), collection(5))
            && teseladb_keys::space::meta_document(at, is_capped(t, c)).is_some()
        {
            return Err(Group0Error::BadSplit);
        }
        let at = teseladb_keys::space::cut_outside(at, (&r.start, &r.end), is_capped)
            .ok_or(Group0Error::BadSplit)?;
        let new = self.ids.next().ok_or(Group0Error::OutOfGroupIds)?;
        self.range_mut(g)?.pending_split = Some(at);
        self.split_ids.insert(g, new);
        Ok(new)
    }

    /// The split of `g` at `at` as recorded: the new range takes `[at, end)`.
    pub fn complete_split(&mut self, g: GroupId, at: &[u8]) -> Result<GroupId, Group0Error> {
        let id = self.split_ids.get(&g).copied();
        let now_ms = self.now_ms;
        let r = self.range_mut(g)?;
        let Some(id) = id.filter(|_| r.pending_split.as_deref() == Some(at)) else {
            return Err(Group0Error::BadSplit);
        };
        let kind = ChangeKind::Split { at: at.to_vec(), other: id };
        let change = change_of(r, now_ms, kind);
        let mut new = r.clone();
        r.end = at.to_vec();
        r.generation += 1;
        r.pending_split = None;
        self.split_ids.remove(&g);
        new.group = id;
        new.start = at.to_vec();
        new.generation = 1;
        new.pending_split = None;
        new.leader_hint = None;
        let group = new.group;
        self.insert(new);
        self.log.record(change);
        Ok(group)
    }

    /// Records the intent of moving range `g` to core `core` of every node (007 FR-011): a core
    /// other than its own that every node of the range has, with no split, merge, replica change
    /// or other core change in progress.
    pub fn relocate(
        &mut self,
        index: u64,
        g: GroupId,
        core: u16,
        facts: &dyn NodeFacts,
    ) -> Result<(), Group0Error> {
        let r = self.ranges.get(&g).ok_or(Group0Error::NoSuchGroup)?;
        if r.settling || r.pending_split.is_some() || self.in_change(g) {
            return Err(Group0Error::RangeBusy);
        }
        let cores = r.nodes().iter().map(|n| facts.cores(*n).unwrap_or(0)).min();
        if core == r.core || cores.is_none_or(|c| core >= c.max(1)) {
            return Err(Group0Error::BadCore);
        }
        self.relocations.insert(g, (core, index));
        Ok(())
    }

    /// Every replica of `g` runs on `core`, the target of its recorded core change.
    pub fn complete_relocate(&mut self, g: GroupId, core: u16) -> Result<(), Group0Error> {
        if self.relocations.get(&g).is_none_or(|(c, _)| *c != core) {
            return Err(Group0Error::NoSuchRelocation);
        }
        self.relocations.remove(&g);
        let now_ms = self.now_ms;
        let r = self.range_mut(g)?;
        let change = change_of(r, now_ms, ChangeKind::Core { core });
        r.core = core;
        self.log.record(change);
        Ok(())
    }

    /// Records the intent of merging `right` into its left neighbour `left` (007 FR-009): both on
    /// the same core and replicas, with no split, replica change or other merge in progress.
    ///
    /// A range that holds keys of a capped collection of `capped` never merges: the collection
    /// keeps a range of its own (007 FR-009).
    pub fn merge(
        &mut self,
        index: u64,
        (left, right): (GroupId, GroupId),
        capped: &BTreeSet<(u32, u32)>,
    ) -> Result<(), Group0Error> {
        let (l, r) = match (self.ranges.get(&left), self.ranges.get(&right)) {
            (Some(l), Some(r)) => (l, r),
            _ => return Err(Group0Error::NoSuchGroup),
        };
        if l.end.is_empty()
            || l.end != r.start
            || holds_capped(l, capped)
            || holds_capped(r, capped)
        {
            return Err(Group0Error::BadBounds);
        }
        if l.core != r.core || l.replicas != r.replicas {
            return Err(Group0Error::BadReplicas);
        }
        let busy = |d: &RangeDesc| d.settling || d.pending_split.is_some();
        if busy(l) || busy(r) || self.in_change(left) || self.in_change(right) {
            return Err(Group0Error::RangeBusy);
        }
        self.merges.insert(left, (right, index));
        Ok(())
    }

    /// The merge of `right` into `left` as recorded, which `CompleteMerge` and `AbortMerge` end.
    pub fn end_merge(&mut self, left: GroupId, right: GroupId) -> Result<(), Group0Error> {
        if self.merges.get(&left).is_none_or(|(r, _)| *r != right) {
            return Err(Group0Error::NoSuchMerge);
        }
        self.merges.remove(&left);
        Ok(())
    }

    /// The merge of `right` into `left` completed: `left` takes `right`'s end.
    pub fn complete_merge(&mut self, left: GroupId, right: GroupId) -> Result<(), Group0Error> {
        self.end_merge(left, right)?;
        let gone = self.ranges.remove(&right).ok_or(Group0Error::NoSuchGroup)?;
        self.index.remove(&gone.start);
        let now_ms = self.now_ms;
        let l = self.range_mut(left)?;
        l.end = gone.end;
        l.generation += 1;
        let change = change_of(l, now_ms, ChangeKind::Merge { other: right });
        self.log.record(change);
        Ok(())
    }
}

/// The nodes as Group 0 knows them.
impl NodeFacts for BTreeMap<NodeId, crate::group0::NodeInfo> {
    fn active(&self, n: NodeId) -> bool {
        self.get(&n).is_some_and(|i| i.state == crate::command::NodeState::Active)
    }

    fn cores(&self, n: NodeId) -> Option<u16> {
        self.get(&n).map(|i| i.addrs.cores)
    }
}

impl Group0Sm {
    /// Adds a descriptor with its entry in the index by key.
    pub fn insert_range(&mut self, desc: RangeDesc) {
        self.range_index.insert(&desc.start, desc.group);
        self.ranges.insert(desc.group, desc);
    }

    /// Rebuilds the index by key from the descriptors.
    pub(crate) fn reindex(&mut self) {
        self.range_index = RangeIndex::default();
        for r in self.ranges.values() {
            self.range_index.insert(&r.start, r.group);
        }
    }

    /// The range that holds `key`.
    pub fn range_for(&self, key: &[u8]) -> Option<&RangeDesc> {
        let g = self.range_index.floor(key)?;
        self.ranges.get(&g).filter(|r| r.contains(key))
    }

    /// The ranges that hold some key of `[start, end)` (`end` empty: no end), in key order.
    pub fn ranges_over(&self, start: &[u8], end: &[u8]) -> Vec<&RangeDesc> {
        let first = self.range_for(start).filter(|r| r.start.as_slice() < start);
        let rest = self.range_index.starting_in(start, end).filter_map(|g| self.ranges.get(&g));
        first.into_iter().chain(rest).collect()
    }

    /// The descriptor table of Group 0 and its nodes.
    pub(crate) fn table(&mut self) -> (DescTable<'_>, &BTreeMap<NodeId, crate::group0::NodeInfo>) {
        let t = DescTable {
            ranges: &mut self.ranges,
            index: &mut self.range_index,
            split_ids: &mut self.split_ids,
            merges: &mut self.merges,
            relocations: &mut self.relocations,
            ids: Ids::Group0(&mut self.last_group),
            log: &mut self.changelog,
            now_ms: self.cluster_time_ms,
        };
        (t, &self.nodes)
    }
}

/// The capped collections, by tenant and number (015 FR-003, 007 FR-025).
pub type CappedSet = BTreeSet<(u32, u32)>;

/// Whether range `d` holds a key of a capped collection of `capped`, by tenant and number.
fn holds_capped(d: &RangeDesc, capped: &BTreeSet<(u32, u32)>) -> bool {
    capped.iter().any(|&(t, c)| {
        let start = teseladb_keys::KeyBuilder::collection_start(t, c);
        let end = teseladb_keys::KeyBuilder::collection_end(t, c);
        d.start.as_slice() < end.as_bytes()
            && (d.end.is_empty() || start.as_bytes() < d.end.as_slice())
    })
}
