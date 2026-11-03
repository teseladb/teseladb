//! Meta-ranges (007 FR-014, D-012, plan «Meta-rangos», `spec/tla/Meta2Handoff.tla`).
//!
//! In space `04`, level 2 of the key format of 003, a descriptor's key is `04 02 00 ‖ end` and the
//! range with no end has `04 02 01`, so the keys follow the ranges: the descriptor that holds key
//! `k` is the first whose key is greater than `04 02 00 ‖ k` ([`lookup_key`]). A Meta2 range holds
//! the descriptors whose keys fall in an interval of that space; [`Meta1`], in Group 0, keeps the
//! end of each interval with its group, replicas and core.
//!
//! Group 0's leader proposes the handoff ([`SplitRate::handoff_due`]) when its last snapshot
//! passes `meta2.snapshotBytes` or every one of the last 60 seconds had more than
//! `meta2.splitsPerSecond` completed splits (64 MiB and 50 without a setting, D-012). From the
//! index of `HandoffMeta2` Group 0 answers every order on a data descriptor with the Meta2 range
//! that holds it. Each Meta2 range is born with the descriptors of its interval as they were at
//! that index ([`Meta2Sm::seed`], from each replica's own copy of Group 0), applies the same
//! orders with the same epoch and generation conditions ([`crate::descriptors::DescTable`]), gives
//! new ranges numbers from its block (its group in the high 32 bits) and sends back an order on a
//! descriptor of another interval. Once it commits its first entry it proposes `Meta2Seeded` and
//! Group 0 drops that interval's descriptors. A merge never crosses the line between two Meta2
//! ranges. The cluster facts a Meta2 range checks (nodes with their cores and state, and the
//! capped collections) reach it in [`Meta2Command::Cluster`], which Group 0's leader proposes
//! whenever they change.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_keys::space::{KeyBuilder, META};
use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

use crate::changelog::Changelog;
use crate::command::{Group0Command, kind};
use crate::descriptors::{CappedSet, DescTable, Ids, NodeFacts, RangeIndex, merging_in};
use crate::group0::{
    Group0Error, Group0Result, Group0Sm, RangeDesc, ReplicaRole, get_range, get_replicas,
    put_range, put_replicas,
};

/// Level of the descriptors of data ranges in space `04`.
pub const LEVEL: u8 = 2;
/// Byte after the level of a range with an end.
const BOUNDED: u8 = 0x00;
/// Byte after the level of the range with no end.
const UNBOUNDED: u8 = 0x01;
/// Group 0 setting: snapshot size, in bytes, past which the handoff is due.
pub const SNAPSHOT_BYTES_SETTING: &[u8] = b"meta2.snapshotBytes";
/// Group 0 setting: completed splits per second that, sustained, make the handoff due.
pub const SPLITS_PER_SECOND_SETTING: &[u8] = b"meta2.splitsPerSecond";
/// Snapshot size without a setting (D-012).
pub const DEFAULT_SNAPSHOT_BYTES: u64 = 64 << 20;
/// Splits per second without a setting (D-012).
pub const DEFAULT_SPLITS_PER_SECOND: u64 = 50;
/// Seconds every one of which must pass the rate.
pub const WINDOW_SECONDS: u64 = 60;
/// Version of a Meta2 range's snapshot (4: when each range was last split or moved, 018 FR-017).
pub const SNAPSHOT_VERSION: u8 = 4;
/// The version before the ranges' marks (03-11-2026), still read: every range as never moved.
pub const SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS: u8 = 3;
/// The version before the tenants of the capped collections, still read: all in tenant 0.
pub const SNAPSHOT_VERSION_WITHOUT_TENANTS: u8 = 2;
/// The version before the changelog, still read: no change recorded.
pub const SNAPSHOT_VERSION_WITHOUT_CHANGELOG: u8 = 1;

/// The key in space `04` of the descriptor of a range ending at `range_end` (empty: no end).
pub fn descriptor_key(range_end: &[u8]) -> Vec<u8> {
    let rest = if range_end.is_empty() {
        vec![UNBOUNDED]
    } else {
        let mut r = vec![BOUNDED];
        r.extend_from_slice(range_end);
        r
    };
    KeyBuilder::meta(LEVEL, &rest).as_bytes().to_vec()
}

/// The key to look `key` up with: its descriptor is the first whose key is greater.
pub fn lookup_key(key: &[u8]) -> Vec<u8> {
    let mut rest = vec![BOUNDED];
    rest.extend_from_slice(key);
    KeyBuilder::meta(LEVEL, &rest).as_bytes().to_vec()
}

/// Whether the descriptor of a range ending at `range_end` falls in `[start, end)` of space `04`
/// (`end` empty: no end).
pub fn in_interval(range_end: &[u8], start: &[u8], end: &[u8]) -> bool {
    let k = descriptor_key(range_end);
    k.as_slice() >= start && (end.is_empty() || k.as_slice() < end)
}

/// A Meta2 range as `Meta1` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta2Range {
    /// End of its interval in space `04` (empty: no end).
    pub end: Vec<u8>,
    /// Its Raft group (Group 0 numbers it when it applies the handoff).
    pub group: GroupId,
    /// Group 0's voters vote, the other nodes learn.
    pub replicas: Vec<(NodeId, ReplicaRole)>,
    /// The core of every node that holds it.
    pub core: u16,
}

impl Meta2Range {
    /// Writes it.
    pub fn encode(&self, e: &mut Encoder) {
        e.bytes(&self.end).u64(self.group.0);
        put_replicas(e, &self.replicas);
        e.u16(self.core);
    }

    /// Reads what [`Meta2Range::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Meta2Range, DecodeError> {
        Ok(Meta2Range {
            end: d.bytes()?.to_vec(),
            group: GroupId(d.u64()?),
            replicas: get_replicas(d)?,
            core: d.u16()?,
        })
    }
}

/// Whether `ranges` split space `04` in order: at least one, every end but the last inside the
/// space and growing, the last with no end.
pub(crate) fn partitions(ranges: &[Meta2Range]) -> bool {
    let Some((last, rest)) = ranges.split_last() else { return false };
    let prefix = [META, LEVEL];
    let inside = rest.iter().all(|r| r.end.starts_with(&prefix) && r.end.len() > prefix.len());
    inside && last.end.is_empty() && rest.windows(2).all(|w| w[0].end < w[1].end)
}

/// Which Meta2 range holds which descriptors (in Group 0 after the handoff).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta1 {
    /// The Meta2 ranges in key order, the last with no end.
    pub ranges: Vec<Meta2Range>,
    /// The cluster facts as they were at the handoff: every Meta2 range is born with them.
    pub facts: Meta2Cluster,
    /// The Meta2 ranges whose interval Group 0 dropped (`Meta2Seeded`): a node whose copy of
    /// Group 0 has applied it can no longer build such a range as it was born.
    pub seeded: BTreeSet<GroupId>,
}

impl Meta1 {
    /// The Meta2 range whose interval holds `meta_key`.
    pub fn holder(&self, meta_key: &[u8]) -> Option<&Meta2Range> {
        self.ranges.iter().find(|r| r.end.is_empty() || r.end.as_slice() > meta_key)
    }

    /// The Meta2 range that holds the descriptor of the range ending at `range_end`.
    pub fn holder_of(&self, range_end: &[u8]) -> Option<&Meta2Range> {
        self.holder(&descriptor_key(range_end))
    }

    /// The interval `[start, end)` of Meta2 range `g` in space `04`.
    pub fn interval(&self, g: GroupId) -> Option<(Vec<u8>, Vec<u8>)> {
        let i = self.ranges.iter().position(|r| r.group == g)?;
        let start = i.checked_sub(1).map_or(Vec::new(), |p| self.ranges[p].end.clone());
        Some((start, self.ranges[i].end.clone()))
    }

    /// Writes it.
    pub fn encode(&self, e: &mut Encoder) {
        self.encode_with(e, true);
    }

    /// [`Meta1::encode`], in the format before the tenants of the capped collections without
    /// `tenants`.
    pub fn encode_with(&self, e: &mut Encoder, tenants: bool) {
        e.var(self.ranges.len() as u64);
        for r in &self.ranges {
            r.encode(e);
        }
        self.facts.encode_with(e, tenants);
        e.var(self.seeded.len() as u64);
        for g in &self.seeded {
            e.u64(g.0);
        }
    }

    /// Reads what [`Meta1::encode_with`] wrote.
    pub fn decode_with(d: &mut Decoder<'_>, tenants: bool) -> Result<Meta1, DecodeError> {
        let mut ranges = Vec::new();
        for _ in 0..d.var()? {
            ranges.push(Meta2Range::decode(d)?);
        }
        let facts = Meta2Cluster::decode_with(d, tenants)?;
        let mut seeded = BTreeSet::new();
        for _ in 0..d.var()? {
            seeded.insert(GroupId(d.u64()?));
        }
        Ok(Meta1 { ranges, facts, seeded })
    }
}

/// The two thresholds of the handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// Snapshot size, in bytes, past which the handoff is due.
    pub snapshot_bytes: u64,
    /// Completed splits per second that, sustained for the window, make it due.
    pub splits_per_second: u64,
}

impl Thresholds {
    /// The thresholds of Group 0's settings; a missing or non-numeric value keeps the default.
    pub fn from_settings(settings: &BTreeMap<Vec<u8>, Vec<u8>>) -> Thresholds {
        let read = |key: &[u8], default: u64| {
            settings
                .get(key)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        Thresholds {
            snapshot_bytes: read(SNAPSHOT_BYTES_SETTING, DEFAULT_SNAPSHOT_BYTES),
            splits_per_second: read(SPLITS_PER_SECOND_SETTING, DEFAULT_SPLITS_PER_SECOND),
        }
    }
}

/// The completed splits of the last [`WINDOW_SECONDS`], by second of the cluster time of each
/// `CompleteSplit` (Group 0's leader counts them as it applies them).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitRate {
    per_second: BTreeMap<u64, u64>,
}

impl SplitRate {
    /// A split completed at cluster time `ms`.
    pub fn record(&mut self, ms: u64) {
        *self.per_second.entry(ms / 1_000).or_insert(0) += 1;
        let newest = self.per_second.keys().next_back().copied().unwrap_or(0);
        let oldest = newest.saturating_sub(WINDOW_SECONDS);
        self.per_second = self.per_second.split_off(&oldest);
    }

    /// Seconds with splits kept.
    pub fn len(&self) -> usize {
        self.per_second.len()
    }

    /// Whether no split is kept.
    pub fn is_empty(&self) -> bool {
        self.per_second.is_empty()
    }

    /// Whether the handoff is due at cluster time `now_ms` with a last snapshot of
    /// `snapshot_len` bytes: past the size, or every one of the last whole seconds before
    /// `now_ms`'s second above the rate.
    pub fn handoff_due(&self, t: &Thresholds, snapshot_len: u64, now_ms: u64) -> bool {
        if snapshot_len > t.snapshot_bytes {
            return true;
        }
        let now = now_ms / 1_000;
        let Some(first) = now.checked_sub(WINDOW_SECONDS) else { return false };
        (first..now).all(|s| self.per_second.get(&s).is_some_and(|n| *n > t.splits_per_second))
    }
}

/// The cluster facts a Meta2 range checks its orders against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta2Cluster {
    /// Every node: its cores and whether it is active.
    pub nodes: Vec<(NodeId, u16, bool)>,
    /// The capped collections, by tenant and number, which a split never cuts.
    pub capped: Vec<(u32, u32)>,
}

/// An entry of a Meta2 range's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Meta2Command {
    /// An order on a descriptor, as Group 0 took it.
    Descriptor(Group0Command),
    /// The cluster facts, whole.
    Cluster(Meta2Cluster),
    /// The range's first entry: its state as it is born, as a snapshot of it. A replica already
    /// seeded ignores it.
    Seed(Vec<u8>),
}

impl Meta2Cluster {
    /// Writes it, with the tenant of each capped collection or, in the format before tenants,
    /// without it.
    pub fn encode_with(&self, e: &mut Encoder, tenants: bool) {
        e.var(self.nodes.len() as u64);
        for (n, cores, active) in &self.nodes {
            e.u64(n.0).u16(*cores).bool(*active);
        }
        put_capped(e, &self.capped, tenants);
    }

    /// Reads what [`Meta2Cluster::encode_with`] wrote.
    pub fn decode_with(d: &mut Decoder<'_>, tenants: bool) -> Result<Meta2Cluster, DecodeError> {
        let mut c = Meta2Cluster::default();
        for _ in 0..d.var()? {
            c.nodes.push((NodeId(d.u64()?), d.u16()?, d.bool()?));
        }
        c.capped = get_capped(d, tenants)?;
        Ok(c)
    }

    /// The facts of Group 0's state `g0`: every node with its cores and state, and the capped
    /// collections.
    pub fn of(g0: &Group0Sm) -> Meta2Cluster {
        let active = |s| s == crate::command::NodeState::Active;
        Meta2Cluster {
            nodes: g0.nodes.values().map(|n| (n.id, n.addrs.cores, active(n.state))).collect(),
            capped: g0.capped_collections().into_iter().collect(),
        }
    }
}

const DESCRIPTOR: u8 = 0;
/// The cluster facts in the format before the tenants of the capped collections, still read.
const CLUSTER_WITHOUT_TENANTS: u8 = 1;
const SEED: u8 = 2;
/// The cluster facts with the tenant of each capped collection (007 FR-025).
const CLUSTER: u8 = 3;

/// Writes capped collections `capped`: their numbers, then, with `tenants`, their tenants.
fn put_capped<'a>(
    e: &mut Encoder,
    capped: impl IntoIterator<Item = &'a (u32, u32)> + Copy,
    tenants: bool,
) {
    let n = capped.into_iter().count();
    e.var(n as u64);
    for (_, id) in capped {
        e.u32(*id);
    }
    if tenants {
        for (t, _) in capped {
            e.u32(*t);
        }
    }
}

/// Reads what [`put_capped`] wrote; without `tenants`, every collection is tenant 0's.
fn get_capped(d: &mut Decoder<'_>, tenants: bool) -> Result<Vec<(u32, u32)>, DecodeError> {
    let n = d.var()?;
    if n > d.remaining() as u64 / 4 {
        return Err(DecodeError::InvalidValue { offset: d.position() });
    }
    let mut ids = Vec::new();
    for _ in 0..n {
        ids.push(d.u32()?);
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let t = if tenants { d.u32()? } else { 0 };
        out.push((t, id));
    }
    Ok(out)
}

/// Whether a Group 0 command of kind `k` is an order on a descriptor.
fn is_descriptor_order(k: u16) -> bool {
    matches!(
        k,
        kind::CREATE_RANGE
            | kind::SET_REPLICAS
            | kind::REPLICAS_SETTLED
            | kind::SET_LEADER_HINT
            | kind::SPLIT_RANGE
            | kind::COMPLETE_SPLIT
            | kind::MERGE_RANGES
            | kind::COMPLETE_MERGE
            | kind::ABORT_MERGE
            | kind::RELOCATE_RANGE
            | kind::COMPLETE_RELOCATE
    )
}

impl Meta2Command {
    /// Its encoding: a tag and the command.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        match self {
            Meta2Command::Descriptor(c) => {
                e.u8(DESCRIPTOR).raw(&c.encode());
            }
            Meta2Command::Cluster(c) => {
                e.u8(CLUSTER);
                c.encode_with(&mut e, true);
            }
            Meta2Command::Seed(state) => {
                e.u8(SEED).raw(state);
            }
        }
        e.finish()
    }

    /// Reads what [`Meta2Command::encode`] wrote; a Group 0 command that is not an order on a
    /// descriptor is refused.
    pub fn decode(b: &[u8]) -> Result<Meta2Command, DecodeError> {
        let mut d = Decoder::new(b);
        let at = d.position();
        let c = match d.u8()? {
            DESCRIPTOR => {
                let cmd = Group0Command::decode(d.raw(d.remaining())?)?;
                if !is_descriptor_order(cmd.kind()) {
                    return Err(DecodeError::InvalidValue { offset: at + 1 });
                }
                Meta2Command::Descriptor(cmd)
            }
            CLUSTER => Meta2Command::Cluster(Meta2Cluster::decode_with(&mut d, true)?),
            CLUSTER_WITHOUT_TENANTS => {
                Meta2Command::Cluster(Meta2Cluster::decode_with(&mut d, false)?)
            }
            SEED => Meta2Command::Seed(d.raw(d.remaining())?.to_vec()),
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        d.finish()?;
        Ok(c)
    }
}

/// A Meta2 range's state: the descriptors of its interval with their changes in progress.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta2Sm {
    /// Its group.
    pub group: GroupId,
    /// Its interval in space `04`: `[start, end)` (`end` empty: no end).
    pub start: Vec<u8>,
    /// See `start`.
    pub end: Vec<u8>,
    /// `Meta1` as Group 0 recorded it, to name the holder of another interval.
    pub meta1: Option<Meta1>,
    /// Descriptors by group.
    pub ranges: BTreeMap<GroupId, RangeDesc>,
    /// The ranges by first key, derived from `ranges`.
    pub range_index: RangeIndex,
    /// The number each recorded split gives its new range.
    pub split_ids: BTreeMap<GroupId, GroupId>,
    /// The merges in progress, by the range that absorbs.
    pub merges: BTreeMap<GroupId, (GroupId, u64)>,
    /// The core changes in progress, by range.
    pub relocations: BTreeMap<GroupId, (u16, u64)>,
    /// The nodes: cores and whether active.
    pub nodes: BTreeMap<NodeId, (u16, bool)>,
    /// The capped collections, by tenant and number.
    pub capped: CappedSet,
    /// Last number given in its block.
    pub last_id: u64,
    /// Last applied index.
    pub applied: u64,
    /// It applied its first entry, the seed with which the range is born: until then it takes
    /// no order.
    pub seeded: bool,
    /// The history of the changes completed here (`config.changelog`).
    pub changelog: Changelog,
    /// The highest mark of the entries applied, in milliseconds.
    pub time_ms: u64,
}

impl NodeFacts for BTreeMap<NodeId, (u16, bool)> {
    fn active(&self, n: NodeId) -> bool {
        self.get(&n).is_some_and(|(_, a)| *a)
    }

    fn cores(&self, n: NodeId) -> Option<u16> {
        self.get(&n).map(|(c, _)| *c)
    }
}

impl Meta2Sm {
    /// Meta2 range `group` as it is born, from `g0`, a copy of Group 0 that applied the handoff:
    /// the descriptors of its interval (no order changes them in Group 0 after the handoff) and
    /// the cluster facts `Meta1` recorded then. Any copy builds the same state until it applies
    /// the range's `Meta2Seeded`; then, as for a range that is not one of `Meta1`, `None`. The
    /// range's leader proposes it as the range's first entry ([`Meta2Command::Seed`]).
    pub fn seed(g0: &Group0Sm, group: GroupId) -> Option<Meta2Sm> {
        let m1 = g0.meta1.as_ref().filter(|m| !m.seeded.contains(&group))?;
        let (start, end) = m1.interval(group)?;
        let mine = |r: &RangeDesc| in_interval(&r.end, &start, &end);
        let ranges: BTreeMap<GroupId, RangeDesc> =
            g0.ranges.values().filter(|r| mine(r)).map(|r| (r.group, r.clone())).collect();
        let held = |g: &GroupId| ranges.contains_key(g);
        let mut sm = Meta2Sm {
            group,
            split_ids: g0
                .split_ids
                .iter()
                .filter(|(g, _)| held(g))
                .map(|(a, b)| (*a, *b))
                .collect(),
            merges: g0.merges.iter().filter(|(g, _)| held(g)).map(|(a, b)| (*a, *b)).collect(),
            relocations: g0
                .relocations
                .iter()
                .filter(|(g, _)| held(g))
                .map(|(a, b)| (*a, *b))
                .collect(),
            nodes: m1.facts.nodes.iter().map(|(n, c, a)| (*n, (*c, *a))).collect(),
            capped: m1.facts.capped.iter().copied().collect(),
            // Without the seeded ranges, which differ between copies of Group 0.
            meta1: Some(Meta1 { seeded: BTreeSet::new(), ..m1.clone() }),
            start,
            end,
            ranges,
            seeded: true,
            ..Meta2Sm::default()
        };
        sm.reindex();
        Some(sm)
    }

    /// A replica of Meta2 range `group` as every node creates it: empty until it applies the
    /// seed, its first entry.
    pub fn new(group: GroupId) -> Meta2Sm {
        Meta2Sm { group, ..Meta2Sm::default() }
    }

    /// The seed entry of the range born as `born` ([`Meta2Sm::seed`]).
    pub fn seed_command(born: &Meta2Sm) -> Meta2Command {
        Meta2Command::Seed(born.encode_snapshot())
    }

    fn reindex(&mut self) {
        self.range_index = RangeIndex::default();
        for r in self.ranges.values() {
            self.range_index.insert(&r.start, r.group);
        }
    }

    /// The range that holds `key`, if its descriptor is here.
    pub fn range_for(&self, key: &[u8]) -> Option<&RangeDesc> {
        let g = self.range_index.floor(key)?;
        self.ranges.get(&g).filter(|r| r.contains(key))
    }

    fn table(&mut self) -> (DescTable<'_>, &BTreeMap<NodeId, (u16, bool)>, &CappedSet) {
        let t = DescTable {
            ranges: &mut self.ranges,
            index: &mut self.range_index,
            split_ids: &mut self.split_ids,
            merges: &mut self.merges,
            relocations: &mut self.relocations,
            ids: Ids::Block { owner: self.group, last: &mut self.last_id },
            log: &mut self.changelog,
            now_ms: self.time_ms,
        };
        (t, &self.nodes, &self.capped)
    }

    /// The answer for the descriptor of a range ending at `range_end` held elsewhere.
    fn elsewhere(&self, range_end: &[u8]) -> Group0Result {
        match self.meta1.as_ref().and_then(|m| m.holder_of(range_end)) {
            Some(h) if h.group != self.group => Group0Result::Meta1(h.clone()),
            _ => Group0Result::Err(Group0Error::NoSuchGroup),
        }
    }

    /// The first key a range created here may start at: past every range of the intervals
    /// before this one.
    fn floor(&self) -> &[u8] {
        self.start.get(3..).unwrap_or(&[])
    }

    fn create(&mut self, start: &[u8], end: &[u8], replicas: &[NodeId]) -> Group0Result {
        if !in_interval(end, &self.start, &self.end) {
            return match self.meta1.as_ref().and_then(|m| m.holder_of(end)) {
                Some(h) => Group0Result::Meta1(h.clone()),
                None => Group0Result::Err(Group0Error::BadBounds),
            };
        }
        if start < self.floor() {
            return Group0Result::Err(Group0Error::BadBounds);
        }
        let (mut t, nodes, _) = self.table();
        t.create_range(start, end, replicas, nodes)
            .map_or_else(Group0Result::Err, Group0Result::Range)
    }

    /// A merge of `right` into `left`: both here, or the answer that names where `left` is.
    fn merge_order(
        &mut self,
        index: u64,
        left: GroupId,
        right: GroupId,
        cmd: &Group0Command,
    ) -> Group0Result {
        if !self.ranges.contains_key(&left) {
            return match self.ranges.get(&right) {
                Some(r) => self.elsewhere(&r.start),
                None => Group0Result::Err(Group0Error::NoSuchGroup),
            };
        }
        let (mut t, _, capped) = self.table();
        let r = match cmd {
            Group0Command::MergeRanges { .. } if !t.ranges.contains_key(&right) => {
                // The right range is another Meta2 range's: merges never cross the line.
                Err(Group0Error::BadBounds)
            }
            Group0Command::MergeRanges { .. } => t.merge(index, (left, right), capped),
            Group0Command::CompleteMerge { .. } => t.complete_merge(left, right),
            _ => t.end_merge(left, right),
        };
        r.map_or_else(Group0Result::Err, |()| Group0Result::Ok)
    }

    /// Applies an order on a descriptor at `index`.
    fn order(&mut self, index: u64, cmd: &Group0Command) -> Group0Result {
        let r = match cmd {
            Group0Command::CreateRange { start, end, replicas } => {
                return self.create(start, end, replicas);
            }
            Group0Command::MergeRanges { left, right }
            | Group0Command::CompleteMerge { left, right }
            | Group0Command::AbortMerge { left, right } => {
                return self.merge_order(index, *left, *right, cmd);
            }
            Group0Command::SetReplicas { group, .. }
            | Group0Command::ReplicasSettled { group, .. }
            | Group0Command::SetLeaderHint { group, .. }
            | Group0Command::SplitRange { group, .. }
            | Group0Command::CompleteSplit { group, .. }
            | Group0Command::RelocateRange { group, .. }
            | Group0Command::CompleteRelocate { group, .. }
                if !self.ranges.contains_key(group) =>
            {
                Err(Group0Error::NoSuchGroup)
            }
            Group0Command::SetReplicas { group, epoch, add, remove } => {
                let (mut t, nodes, _) = self.table();
                t.set_replicas(*group, *epoch, add, remove, nodes).map(|()| None)
            }
            Group0Command::ReplicasSettled { group, epoch } => {
                self.table().0.settle(*group, *epoch).map(|()| None)
            }
            Group0Command::SetLeaderHint { group, node, term } => {
                self.table().0.leader_hint(*group, *node, *term).map(|()| None)
            }
            Group0Command::SplitRange { group, at } => {
                let (mut t, _, capped) = self.table();
                t.split(*group, at, capped).map(Some)
            }
            Group0Command::CompleteSplit { group, at, .. } => {
                self.table().0.complete_split(*group, at).map(Some)
            }
            Group0Command::RelocateRange { group, core } => {
                let (mut t, nodes, _) = self.table();
                t.relocate(index, *group, *core, nodes).map(|()| None)
            }
            Group0Command::CompleteRelocate { group, core } => {
                self.table().0.complete_relocate(*group, *core).map(|()| None)
            }
            // `Meta2Command::decode` lets no other command through.
            _ => Err(Group0Error::NoSuchGroup),
        };
        match r {
            Ok(Some(g)) => Group0Result::Range(g),
            Ok(None) => Group0Result::Ok,
            Err(e) => Group0Result::Err(e),
        }
    }

    /// Applies a decoded entry at `index`.
    pub fn apply_command(&mut self, index: u64, cmd: &Meta2Command) -> Group0Result {
        match cmd {
            Meta2Command::Seed(state) if !self.seeded => {
                match Meta2Sm::decode_snapshot(state) {
                    Ok(born) if born.group == self.group && born.seeded => {
                        *self = Meta2Sm { applied: self.applied, ..born };
                        Group0Result::Ok
                    }
                    // A seed of another range, or not one: it changes nothing.
                    _ => Group0Result::Err(Group0Error::BadBounds),
                }
            }
            Meta2Command::Seed(_) => Group0Result::Ok,
            _ if !self.seeded => Group0Result::Err(Group0Error::RangeBusy),
            Meta2Command::Descriptor(c) => self.order(index, c),
            Meta2Command::Cluster(c) => {
                self.nodes = c.nodes.iter().map(|(n, cores, a)| (*n, (*cores, *a))).collect();
                self.capped = c.capped.iter().copied().collect();
                Group0Result::Ok
            }
        }
    }

    fn encode_snapshot(&self) -> Vec<u8> {
        self.encode_snapshot_as(SNAPSHOT_VERSION)
    }

    /// The snapshot in format `version`: the current one, or the one before it, without the
    /// ranges' marks.
    pub fn encode_snapshot_as(&self, version: u8) -> Vec<u8> {
        let marks = version > SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS;
        let mut e = Encoder::new();
        e.u8(version).u64(self.group.0).bytes(&self.start).bytes(&self.end);
        e.bool(self.meta1.is_some());
        if let Some(m) = &self.meta1 {
            m.encode(&mut e);
        }
        e.var(self.ranges.len() as u64);
        for r in self.ranges.values() {
            put_range(&mut e, r, marks);
        }
        e.var(self.split_ids.len() as u64);
        for (g, new) in &self.split_ids {
            e.u64(g.0).u64(new.0);
        }
        e.var(self.merges.len() as u64);
        for (l, (r, at)) in &self.merges {
            e.u64(l.0).u64(r.0).u64(*at);
        }
        e.var(self.relocations.len() as u64);
        for (g, (core, at)) in &self.relocations {
            e.u64(g.0).u16(*core).u64(*at);
        }
        e.var(self.nodes.len() as u64);
        for (n, (cores, active)) in &self.nodes {
            e.u64(n.0).u16(*cores).bool(*active);
        }
        put_capped(&mut e, &self.capped, true);
        e.u64(self.last_id).u64(self.applied).bool(self.seeded);
        e.u64(self.time_ms);
        self.changelog.encode(&mut e);
        e.finish()
    }

    fn decode_snapshot(b: &[u8]) -> Result<Meta2Sm, DecodeError> {
        let mut d = Decoder::new(b);
        let at = d.position();
        let version = d.u8()?;
        if !(SNAPSHOT_VERSION_WITHOUT_CHANGELOG..=SNAPSHOT_VERSION).contains(&version) {
            return Err(DecodeError::InvalidValue { offset: at });
        }
        let mut sm = Meta2Sm {
            group: GroupId(d.u64()?),
            start: d.bytes()?.to_vec(),
            end: d.bytes()?.to_vec(),
            ..Meta2Sm::default()
        };
        if d.bool()? {
            let tenants = version > SNAPSHOT_VERSION_WITHOUT_TENANTS;
            sm.meta1 = Some(Meta1::decode_with(&mut d, tenants)?);
        }
        for _ in 0..d.var()? {
            let r = get_range(&mut d, version > SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS)?;
            sm.ranges.insert(r.group, r);
        }
        sm.reindex();
        for _ in 0..d.var()? {
            sm.split_ids.insert(GroupId(d.u64()?), GroupId(d.u64()?));
        }
        for _ in 0..d.var()? {
            sm.merges.insert(GroupId(d.u64()?), (GroupId(d.u64()?), d.u64()?));
        }
        for _ in 0..d.var()? {
            sm.relocations.insert(GroupId(d.u64()?), (d.u16()?, d.u64()?));
        }
        for _ in 0..d.var()? {
            sm.nodes.insert(NodeId(d.u64()?), (d.u16()?, d.bool()?));
        }
        let tenants = version > SNAPSHOT_VERSION_WITHOUT_TENANTS;
        sm.capped = get_capped(&mut d, tenants)?.into_iter().collect();
        sm.last_id = d.u64()?;
        sm.applied = d.u64()?;
        sm.seeded = d.bool()?;
        if version > SNAPSHOT_VERSION_WITHOUT_CHANGELOG {
            sm.time_ms = d.u64()?;
            sm.changelog = Changelog::decode(&mut d)?;
        }
        d.finish()?;
        Ok(sm)
    }

    /// The merge that absorbs range `g`, if one is recorded here.
    pub fn merging(&self, g: GroupId) -> Option<(GroupId, u64)> {
        merging_in(&self.merges, g)
    }
}

impl StateMachine for Meta2Sm {
    type Output = Group0Result;

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<Group0Result, ApplyError> {
        let cmd = Meta2Command::decode(payload).map_err(|e| ApplyError(format!("{e:?}")))?;
        self.applied = ctx.index;
        self.time_ms = self.time_ms.max(ctx.ts.physical);
        Ok(self.apply_command(ctx.index, &cmd))
    }

    fn skip(&mut self, index: u64) {
        self.applied = index;
    }

    fn snapshot(&self) -> Vec<u8> {
        self.encode_snapshot()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        *self = Meta2Sm::decode_snapshot(snap).map_err(|e| RestoreError(format!("{e:?}")))?;
        Ok(())
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}
