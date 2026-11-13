//! A group handed over from one core of a node to another (007 FR-011, `CoreRelocate.tla`). The
//! old core stops the group ([`StateMachine::halted`], then `close_group`) and reads what the
//! target core needs from its log ([`MultiRaftCore::export_group`]); the target core writes it
//! whole to its own log ([`MultiRaftCore::import_group`]) without starting it. Each core says
//! when its part is durable ([`MultiRaftCore::take_durable`]): the old core removes its copy only
//! once the import is durable, and the target core starts the group only once that removal is
//! durable, so two copies never run.

use teseladb_hlc::Hlc;
use teseladb_raft::ConfState;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{HardState, LogEntry, ReleaseProvider, SnapshotMeta};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, NodeId};

use super::{Led, MultiRaftCore, Wait, wrap_snapshot};
use crate::error::OpError;
use crate::frame::PeerFrame;
use crate::log_store::{decode_conf, encode_conf};
use crate::state_machine::StateMachine;

/// What a core hands over of a group: its Raft state, its state machine's state at `index` and
/// the entries of its log past `index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupImport {
    /// Its configuration.
    pub conf: ConfState,
    /// Its hard state (term, vote, commit).
    pub hard: HardState,
    /// The index whose state `state` is.
    pub index: u64,
    /// The term of the entry at `index`.
    pub term: u64,
    /// The snapshot at `index`, as the core stores its own.
    pub state: Vec<u8>,
    /// The entries past `index`, committed or not.
    pub entries: Vec<LogEntry>,
    /// The term this replica led when it was handed over, if it led: every read of the group's
    /// keys was then its node's, and the core that takes it on the same node may win its next
    /// term without the margin over its wall clock (005 FR-005).
    pub led: Option<u64>,
}

/// The state of a group at one of its entries, taken while it runs (007 FR-011): what an
/// export that leaves later starts from ([`MultiRaftCore::export_group_from`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportBase {
    /// The index whose state `state` is.
    pub index: u64,
    /// The term of the entry at `index`.
    pub term: u64,
    /// The snapshot at `index`, as the core stores its own.
    pub state: Vec<u8>,
    /// The group's configuration then: the export leaves only if it is still the same.
    pub conf: ConfState,
    /// The mark of the last entry applied then, for a state taken while the group runs
    /// ([`MultiRaftCore::capture_export`]): every version the group holds at `index` is at or
    /// below it, and every later entry is above it.
    pub mark: Hlc,
}

/// A hand-over step that reached the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Durable {
    /// The group was removed from this core ([`MultiRaftCore::retire_group`]).
    Removed(GroupId),
    /// The group was written whole to this core's log ([`MultiRaftCore::import_group`]).
    Imported(GroupId),
}

impl<S: StateMachine> MultiRaftCore<S> {
    /// Stops group `g` and gives what the core hands over of it: its configuration, its hard
    /// state, its state machine's snapshot at the index whose state it holds and the entries of
    /// its log past that index. The group is closed as by `close_group` (refused with `Busy`
    /// while something of it is in flight with the disk); its log stays until
    /// [`MultiRaftCore::retire_group`].
    pub fn export_group(&mut self, g: GroupId) -> Result<GroupImport, OpError> {
        // A halted group that can be handed over drops what is only in memory: its own snapshot
        // waiting for the release floor (it would never apply the entry that saves it) and a
        // leader's snapshot received whole (it installs none). The export carries the state; the
        // copy on the other core takes a snapshot from the leader if it needs one. One that
        // cannot be handed over yet keeps them: a snapshot from the leader is how it gets there.
        if self.hands_over(g) {
            self.groups.get_mut(&g).ok_or(OpError::GroupNotFound)?.pending_snap = None;
            self.received.remove(&g);
            self.snapshot_reqs.remove(&g);
        }
        let grp = self.groups.get(&g).ok_or(OpError::GroupNotFound)?;
        // Checked before its terms are read: a group still loading has none yet.
        if self.close_refused(g, grp) {
            return Err(OpError::Busy);
        }
        let base = self.export_base(g)?;
        self.export_group_from(g, base)
    }

    /// The state of group `g` at the entry whose state its machine holds now.
    fn export_base(&self, g: GroupId) -> Result<ExportBase, OpError> {
        let grp = self.groups.get(&g).ok_or(OpError::GroupNotFound)?;
        let index = grp.sm.snapshot_state_index();
        let log = self.store.group(g).ok_or(OpError::GroupNotFound)?;
        if index < log.snapshot_index || index > log.last_index() {
            return Err(OpError::Unavailable);
        }
        // The terms of the entries since the last snapshot join those below it, as in a snapshot
        // the core takes: the log has every entry above its snapshot.
        let mut terms = grp.terms.clone();
        terms.extend(index, |i| log.term(i).expect("an entry above the last snapshot"));
        let state = wrap_snapshot(grp.last_mark.physical, &terms, &grp.sm.snapshot()).to_vec();
        let term = log.term(index).ok_or(OpError::Unavailable)?;
        let mark = Hlc { physical: 0, logical: 0 };
        Ok(ExportBase { index, term, state, conf: grp.raw.conf().clone(), mark })
    }

    /// The mark of the last entry of group `g` at or below `index` that carries one, if its log
    /// still has it.
    fn mark_at(&self, g: GroupId, index: u64) -> Option<Hlc> {
        let from = self.store.group(g)?.snapshot_index + 1;
        if index < from {
            return None;
        }
        let entries = self.store.entries(g, from, index + 1, u64::MAX).ok()?;
        let marked = |e: &LogEntry| match e.kind {
            EntryKind::Normal => crate::envelope::decode(&e.data).ok().map(|v| v.ts),
            _ => None,
        };
        entries.iter().rev().find_map(marked)
    }

    /// Takes the state of group `g` at its last applied entry while it goes on running, and
    /// holds its log from there (007 FR-011): [`MultiRaftCore::export_group_from`] hands that
    /// state over later with the entries that followed, which the core that takes the group
    /// applies as after a restart. A range that stopped for its whole export was served by no
    /// one meanwhile where it has a single voter (H-059 of the native tests).
    /// [`MultiRaftCore::release_export`] lets go of the log if the export is given up.
    pub fn capture_export(&mut self, g: GroupId) -> Result<ExportBase, OpError> {
        let grp = self.groups.get(&g).ok_or(OpError::GroupNotFound)?;
        if grp.stopped || grp.loading || grp.installing {
            return Err(OpError::Unavailable);
        }
        let base = self.export_base(g)?;
        // Without the mark of its last entry there is no telling the versions of that state
        // from the later ones: the caller stops the group first.
        let mark = self.mark_at(g, base.index).ok_or(OpError::Unavailable)?;
        self.groups.get_mut(&g).expect("checked").export_hold = Some(base.index);
        self.declare_floor(g);
        Ok(ExportBase { mark, ..base })
    }

    /// Lets go of the log [`MultiRaftCore::capture_export`] held for group `g`.
    pub fn release_export(&mut self, g: GroupId) {
        let held = self.groups.get_mut(&g).and_then(|grp| grp.export_hold.take());
        if held.is_none() {
            return;
        }
        match S::HOLDS_LOG {
            true => self.declare_floor(g),
            // Only the export held it: nothing does now.
            false => self.store.set_release_floor(g, ReleaseProvider::Lsm, None),
        }
    }

    /// [`MultiRaftCore::export_group`] with the state `base` holds and the entries of the log
    /// past it. `Unavailable` if the log no longer has them, or holds another entry at
    /// `base.index`, or the group's configuration changed since: the caller exports the group
    /// as it is now.
    pub fn export_group_from(
        &mut self,
        g: GroupId,
        base: ExportBase,
    ) -> Result<GroupImport, OpError> {
        if self.hands_over(g) {
            self.groups.get_mut(&g).ok_or(OpError::GroupNotFound)?.pending_snap = None;
            self.received.remove(&g);
            self.snapshot_reqs.remove(&g);
        }
        let grp = self.groups.get(&g).ok_or(OpError::GroupNotFound)?;
        if self.close_refused(g, grp) {
            return Err(OpError::Busy);
        }
        let ExportBase { index, term, state, conf, mark: _ } = base;
        let log = self.store.group(g).ok_or(OpError::GroupNotFound)?;
        let same_entry = log.term(index) == Some(term);
        if index < log.snapshot_index || !same_entry || *grp.raw.conf() != conf {
            return Err(OpError::Unavailable);
        }
        self.close_group(g)?;
        // What this node served of its reads goes with the replica. One that never led keeps
        // nothing: its parent stays in this core.
        let led = self.led.remove(&g).filter(|l| l.parent.is_none()).map(|l| l.term);
        let log = self.store.group(g).ok_or(OpError::GroupNotFound)?;
        let (hard, last) = (log.hard, log.last_index());
        let entries = match last > index {
            true => self
                .store
                .entries(g, index + 1, last + 1, u64::MAX)
                .map_err(|_| OpError::Unavailable)?
                .to_vec(),
            false => Vec::new(),
        };
        Ok(GroupImport { conf, hard, index, term, state, entries, led })
    }

    /// Writes group `g` whole to this core's log from `imp`, without starting it:
    /// [`Durable::Imported`] says when it is durable, and `create_group` then opens it as a
    /// recovered group. `GroupExists` if the core has the group or its log.
    pub fn import_group(&mut self, g: GroupId, imp: GroupImport) -> Result<(), OpError> {
        if self.groups.contains_key(&g) || self.store.group(g).is_some() {
            return Err(OpError::GroupExists);
        }
        let conf = encode_conf(&imp.conf);
        decode_conf(&conf).map_err(|_| OpError::InvalidConfChange)?;
        let unavailable = |_| OpError::Unavailable;
        let t =
            self.store.create_group(g, conf.clone(), Durability::Strict).map_err(unavailable)?;
        self.waits.insert(t, Wait::Nothing);
        // The commit goes past the snapshot only with the entries it covers.
        let early = HardState { commit: imp.hard.commit.min(imp.index), ..imp.hard };
        let t = self.store.append_raft(g, &[], Some(early), Durability::Strict);
        self.waits.insert(t.map_err(unavailable)?, Wait::Nothing);
        let meta = SnapshotMeta { index: imp.index, term: imp.term, conf };
        let t = self.store.save_snapshot(g, meta, Bytes::from_vec(imp.state));
        let wait = Wait::Import { group: g, hard: imp.hard, entries: imp.entries };
        self.waits.insert(t.map_err(unavailable)?, wait);
        // What its node served of its reads comes with the replica (005 FR-005).
        match imp.led {
            Some(term) => self.led.insert(g, Led { term, parent: None }),
            None => self.led.remove(&g),
        };
        Ok(())
    }

    /// The snapshot of imported group `g` is durable: its hard state and the entries past it go
    /// to the log, and the import is durable with them.
    pub(super) fn imported(&mut self, g: GroupId, hard: HardState, entries: &[LogEntry]) {
        // Refused only if the store stopped: then nothing is durable and no note leaves.
        if let Ok(t) = self.store.append_raft(g, entries, Some(hard), Durability::Strict) {
            self.waits.insert(t, Wait::Note(Durable::Imported(g)));
        }
    }

    /// Removes group `g`, running or only in the log, from this core: [`Durable::Removed`] says
    /// when the removal is durable.
    pub fn retire_group(&mut self, g: GroupId) -> Result<(), OpError> {
        let open = self.groups.remove(&g).is_some();
        if !open && self.store.group(g).is_none() {
            return Err(OpError::GroupNotFound);
        }
        let t = self.forget_group(g).ok_or(OpError::Unavailable)?;
        self.waits.insert(t, Wait::Note(Durable::Removed(g)));
        Ok(())
    }

    /// The hand-over steps that reached the disk since the last call.
    pub fn take_durable(&mut self) -> Vec<Durable> {
        std::mem::take(&mut self.durable)
    }

    /// The frames of groups this core does not have, by the node that sent them.
    /// The layer above hands each one to the core of the node that runs its group, if another
    /// does ([`MultiRaftCore::on_stray`]); the frames of a range that moves between cores reach
    /// every replica, also one that missed the change (007 FR-011).
    pub fn take_strays(&mut self) -> Vec<(NodeId, PeerFrame)> {
        std::mem::take(&mut self.strays)
    }

    /// A frame another core of the node received for a group this core runs; what it carries
    /// for groups this core does not run either is dropped.
    pub fn on_stray(&mut self, from: NodeId, frame: PeerFrame, now: Nanos) {
        self.from_core = true;
        self.on_peer_message(from, frame, now);
        self.from_core = false;
    }
}
