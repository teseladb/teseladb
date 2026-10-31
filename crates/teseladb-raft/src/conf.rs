//! Group configuration and single-server changes with learners.

use teseladb_types::{Decoder, Encoder, NodeId};

use crate::message::DecodeError;

/// Configuration: voters and learners (sorted, without duplicates or overlaps).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ConfState {
    /// Voters.
    pub voters: Vec<NodeId>,
    /// Learners (receive the log, do not vote and do not count for the quorum).
    pub learners: Vec<NodeId>,
}

impl ConfState {
    /// Configuration with these voters and no learner.
    pub fn voters(voters: &[NodeId]) -> ConfState {
        let mut v = voters.to_vec();
        v.sort_unstable();
        v.dedup();
        ConfState { voters: v, learners: Vec::new() }
    }

    /// Votes needed for a majority.
    pub fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }

    /// Tells whether `id` votes.
    pub fn is_voter(&self, id: NodeId) -> bool {
        self.voters.binary_search(&id).is_ok()
    }

    /// Tells whether `id` is a learner.
    pub fn is_learner(&self, id: NodeId) -> bool {
        self.learners.binary_search(&id).is_ok()
    }

    /// All the members.
    pub fn members(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.voters.iter().chain(&self.learners).copied()
    }

    /// Encoding.
    pub fn encode(&self, e: &mut Encoder) {
        e.var(self.voters.len() as u64);
        for v in &self.voters {
            e.u64(v.0);
        }
        e.var(self.learners.len() as u64);
        for l in &self.learners {
            e.u64(l.0);
        }
    }

    /// Decoding (checks strict order and that they do not overlap).
    pub fn decode(d: &mut Decoder<'_>) -> Result<ConfState, DecodeError> {
        let list = |d: &mut Decoder<'_>| -> Result<Vec<NodeId>, DecodeError> {
            let at = d.position();
            let n = d.var()?;
            if n > (d.remaining() / 8) as u64 {
                return Err(DecodeError::InvalidValue { offset: at });
            }
            let mut v = Vec::with_capacity(n as usize);
            for _ in 0..n {
                v.push(NodeId(d.u64()?));
            }
            if v.windows(2).any(|w| w[0] >= w[1]) {
                return Err(DecodeError::InvalidValue { offset: at });
            }
            Ok(v)
        };
        let at = d.position();
        let c = ConfState { voters: list(d)?, learners: list(d)? };
        if c.learners.iter().any(|l| c.is_voter(*l)) {
            return Err(DecodeError::InvalidValue { offset: at });
        }
        Ok(c)
    }
}

/// Kind of configuration change (one server at a time).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfChangeKind {
    /// Add as a learner.
    AddLearner = 1,
    /// Promote a learner to voter.
    PromoteLearner = 2,
    /// Remove a voter or a learner.
    Remove = 3,
}

/// Configuration change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConfChange {
    /// Kind.
    pub kind: ConfChangeKind,
    /// Affected node.
    pub node: NodeId,
}

impl ConfChange {
    /// Encoding (the data of the configuration entry).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u8(self.kind as u8).u64(self.node.0);
        e.finish()
    }

    /// Decoding.
    pub fn from_bytes(b: &[u8]) -> Result<ConfChange, DecodeError> {
        let mut d = Decoder::new(b);
        let kind = match d.u8()? {
            1 => ConfChangeKind::AddLearner,
            2 => ConfChangeKind::PromoteLearner,
            3 => ConfChangeKind::Remove,
            _ => return Err(DecodeError::InvalidValue { offset: 0 }),
        };
        let node = NodeId(d.u64()?);
        d.finish()?;
        Ok(ConfChange { kind, node })
    }

    /// Applies the change; `Err` if it is not valid in that configuration (no effect).
    pub fn apply(&self, c: &ConfState) -> Result<ConfState, &'static str> {
        let mut out = c.clone();
        let n = self.node;
        match self.kind {
            ConfChangeKind::AddLearner => {
                if c.is_voter(n) || c.is_learner(n) {
                    return Err("already a member");
                }
                out.learners.push(n);
                out.learners.sort_unstable();
            }
            ConfChangeKind::PromoteLearner => {
                if !c.is_learner(n) {
                    return Err("not a learner");
                }
                out.learners.retain(|&l| l != n);
                out.voters.push(n);
                out.voters.sort_unstable();
            }
            ConfChangeKind::Remove => {
                if !c.is_voter(n) && !c.is_learner(n) {
                    return Err("not a member");
                }
                if c.voters == [n] {
                    return Err("cannot remove the last voter");
                }
                out.voters.retain(|&v| v != n);
                out.learners.retain(|&l| l != n);
            }
        }
        Ok(out)
    }
}
