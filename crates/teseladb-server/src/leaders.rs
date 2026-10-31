//! What the leader of each range of a core does with the leader its descriptor names (007 FR-010,
//! plan «Equilibrio de réplicas y líderes»): it publishes itself in Group 0 with its term, or hands
//! its leadership to the voter the balancer asks for, and publishes itself again if that does not
//! happen in [`TRANSFER_WAIT`].

use std::collections::BTreeMap;

use teseladb_meta::command::Group0Command;
use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_raft::Role;
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::ranges::Ranges;

/// How long a leader waits for a transfer it started before it publishes itself again.
pub const TRANSFER_WAIT: Nanos = Nanos::from_secs(3);

/// What a range's descriptor says of its leader: the node, the term it published itself in (0: the
/// balancer's request) and the range's voters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hint {
    /// The node named.
    pub node: Option<NodeId>,
    /// Its term; 0 for a request.
    pub term: u64,
    /// The range's voters.
    pub voters: Vec<NodeId>,
}

/// What the leader of a range does now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing: the descriptor names it in its term, or a transfer is under way.
    Nothing,
    /// Hands the leadership to this voter.
    Transfer(NodeId),
    /// Publishes itself with its term.
    Publish,
}

/// The decision of leader `me` in `term` with descriptor `hint`; `asked` is when it started a
/// transfer in this term, and `published` the last term it published itself in and when.
pub fn decide(
    hint: &Hint,
    (me, term): (NodeId, u64),
    asked: Option<Nanos>,
    published: Option<(u64, Nanos)>,
    now: Nanos,
) -> Action {
    match hint.node {
        Some(n) if n == me && hint.term == term => Action::Nothing,
        Some(n) if hint.term == 0 && n != me && hint.voters.contains(&n) => match asked {
            None => Action::Transfer(n),
            Some(at) if now.saturating_since(at) < TRANSFER_WAIT => Action::Nothing,
            Some(_) => publish(term, published, now),
        },
        _ => publish(term, published, now),
    }
}

/// Publishes once per term, and again if the descriptor still does not name it after
/// [`TRANSFER_WAIT`]: a command lost on its way to Group 0 is sent again.
fn publish(term: u64, published: Option<(u64, Nanos)>, now: Nanos) -> Action {
    match published {
        Some((t, at)) if t == term && now.saturating_since(at) < TRANSFER_WAIT => Action::Nothing,
        _ => Action::Publish,
    }
}

/// The leadership of a core's ranges.
#[derive(Debug, Default)]
pub struct Leaders {
    hints: BTreeMap<GroupId, Hint>,
    /// The term and the time of the transfer each range's leader started.
    asked: BTreeMap<GroupId, (u64, Nanos)>,
    /// The last term each range's leader published itself in, and when.
    published: BTreeMap<GroupId, (u64, Nanos)>,
}

impl Leaders {
    /// Group 0's descriptors.
    pub fn set(&mut self, ranges: &Ranges) {
        self.hints = ranges
            .descs
            .iter()
            .map(|d| {
                let hint = Hint { node: d.leader_hint, term: d.leader_term, voters: d.voters() };
                (d.group, hint)
            })
            .collect();
        self.asked.retain(|g, _| self.hints.contains_key(g));
        self.published.retain(|g, _| self.hints.contains_key(g));
    }

    /// Each range this core leads acts on its descriptor; the publications go to Group 0.
    pub fn step<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        me: NodeId,
        now: Nanos,
    ) -> Vec<Group0Command> {
        let mut out = Vec::new();
        for (g, hint) in &self.hints {
            let Some(st) = core.status(*g).filter(|s| s.role == Role::Leader) else { continue };
            let term = st.hard_state.term;
            let asked = self.asked.get(g).filter(|(t, _)| *t == term).map(|(_, at)| *at);
            match decide(hint, (me, term), asked, self.published.get(g).copied(), now) {
                Action::Nothing => {}
                Action::Transfer(to) => {
                    if core.transfer_leader(*g, to) {
                        self.asked.insert(*g, (term, now));
                    }
                }
                Action::Publish => {
                    self.published.insert(*g, (term, now));
                    out.push(Group0Command::SetLeaderHint { group: *g, node: me, term });
                }
            }
        }
        out
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    const ME: NodeId = NodeId(1);
    const OTHER: NodeId = NodeId(2);

    fn hint(node: Option<NodeId>, term: u64) -> Hint {
        Hint { node, term, voters: vec![ME, OTHER] }
    }

    #[test]
    fn a_leader_publishes_itself_once_per_term() {
        let now = Nanos::from_secs(10);
        assert_eq!(decide(&hint(None, 0), (ME, 4), None, None, now), Action::Publish);
        let sent = Some((4, now));
        assert_eq!(decide(&hint(None, 0), (ME, 4), None, sent, now), Action::Nothing);
        // Lost on its way: sent again once the wait is over.
        let later = now + TRANSFER_WAIT;
        assert_eq!(decide(&hint(None, 0), (ME, 4), None, sent, later), Action::Publish);
        // An older publication, of this node or another.
        let old = Some((3, now));
        assert_eq!(decide(&hint(Some(ME), 3), (ME, 4), None, old, now), Action::Publish);
        assert_eq!(decide(&hint(Some(OTHER), 3), (ME, 4), None, None, now), Action::Publish);
        assert_eq!(decide(&hint(Some(ME), 4), (ME, 4), None, None, now), Action::Nothing);
        // A request for a node that no longer replicates the range.
        let gone = Hint { node: Some(NodeId(7)), term: 0, voters: vec![ME, OTHER] };
        assert_eq!(decide(&gone, (ME, 4), None, None, now), Action::Publish);
    }

    #[test]
    fn a_request_hands_the_leadership_over_and_a_stuck_one_gives_way() {
        let asked_at = Nanos::from_secs(10);
        let req = hint(Some(OTHER), 0);
        assert_eq!(decide(&req, (ME, 4), None, None, asked_at), Action::Transfer(OTHER));
        let soon = asked_at + Nanos::from_secs(1);
        assert_eq!(decide(&req, (ME, 4), Some(asked_at), None, soon), Action::Nothing);
        let late = asked_at + TRANSFER_WAIT;
        assert_eq!(decide(&req, (ME, 4), Some(asked_at), None, late), Action::Publish);
        let sent = Some((4, late));
        assert_eq!(decide(&req, (ME, 4), Some(asked_at), sent, late), Action::Nothing);
        // A request for itself: it publishes itself with its term.
        assert_eq!(decide(&hint(Some(ME), 0), (ME, 4), None, None, late), Action::Publish);
    }
}
