//! Logical sessions (FR-016, data model §2): each `lsid` the drivers send, with the user who
//! started it, the cluster time of its last use and the highest `txnNumber` seen. A session
//! nobody used for [`SESSION_TIMEOUT_MS`] expires; ending or killing one kills its cursors, which
//! the caller does with the identifiers these functions give.

use std::collections::{BTreeMap, BTreeSet};

/// Milliseconds a session lives without use (`logicalSessionTimeoutMinutes`: 30).
pub const SESSION_TIMEOUT_MS: u64 = 30 * 60_000;

/// The owner of a session, the `uid` of its `lsid`: the SHA-256 of `user@db` of the user who
/// started it, or of the empty name without authentication.
pub type Uid = [u8; 32];

/// The sessions a kill or an `endSessions` reaches: every session of an owner, or the one of
/// that owner with this `lsid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionMatch {
    /// The owner.
    pub uid: Uid,
    /// The session, or every session of the owner.
    pub lsid: Option<u128>,
}

impl SessionMatch {
    /// Every session of `uid`.
    pub fn owner(uid: Uid) -> SessionMatch {
        SessionMatch { uid, lsid: None }
    }

    /// Session `lsid` of `uid`.
    pub fn one(lsid: u128, uid: Uid) -> SessionMatch {
        SessionMatch { uid, lsid: Some(lsid) }
    }

    /// Whether it reaches session `lsid` of `uid`.
    pub fn reaches(&self, lsid: u128, uid: &Uid) -> bool {
        self.uid == *uid && self.lsid.is_none_or(|l| l == lsid)
    }
}

/// Who started a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    /// The `uid` of its `lsid`.
    pub uid: Uid,
    /// The user's `user@db`, when the node authenticates.
    pub user: Option<String>,
}

/// A logical session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Who started it.
    pub owner: Owner,
    /// Cluster time of its last use, in milliseconds.
    pub last_use_ms: u64,
    /// Highest `txnNumber` seen.
    pub txn_number: Option<i64>,
}

/// The sessions a node uses.
#[derive(Clone, Debug)]
pub struct SessionTable {
    sessions: BTreeMap<u128, Session>,
    timeout_ms: u64,
    /// Sessions `endSessions` named, ended at the next refresh of the record.
    ending: BTreeSet<SessionMatch>,
}

impl Default for SessionTable {
    fn default() -> Self {
        SessionTable::with_timeout(SESSION_TIMEOUT_MS)
    }
}

impl SessionTable {
    /// An empty table whose sessions expire after [`SESSION_TIMEOUT_MS`].
    pub fn new() -> SessionTable {
        SessionTable::default()
    }

    /// An empty table whose sessions expire after `timeout_ms`
    /// (`localLogicalSessionTimeoutMinutes`).
    pub fn with_timeout(timeout_ms: u64) -> SessionTable {
        SessionTable { sessions: BTreeMap::new(), timeout_ms, ending: BTreeSet::new() }
    }

    /// Milliseconds a session lives without use.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Session `lsid`, if the node uses it.
    pub fn get(&self, lsid: u128) -> Option<&Session> {
        self.sessions.get(&lsid)
    }

    /// Sessions in use.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether no session is in use.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// The identifiers of the sessions in use, in order.
    pub fn ids(&self) -> Vec<u128> {
        self.sessions.keys().copied().collect()
    }

    /// The sessions used at or after `since_ms`, in order, with their owner and last use.
    pub fn used_since(&self, since_ms: u64) -> Vec<(u128, Owner, u64)> {
        self.sessions
            .iter()
            .filter(|(l, s)| {
                s.last_use_ms >= since_ms
                    && !self.ending.iter().any(|m| m.reaches(**l, &s.owner.uid))
            })
            .map(|(l, s)| (*l, s.owner.clone(), s.last_use_ms))
            .collect()
    }

    /// A command of `owner` in session `lsid` at cluster time `now_ms` (also
    /// `refreshSessions`): it starts the session if it is new, owned by `owner`.
    pub fn touch(&mut self, lsid: u128, owner: &Owner, now_ms: u64) -> &mut Session {
        let s = self.sessions.entry(lsid).or_insert_with(|| Session {
            owner: owner.clone(),
            last_use_ms: now_ms,
            txn_number: None,
        });
        s.last_use_ms = s.last_use_ms.max(now_ms);
        s
    }

    /// Ends sessions `lsids` (`endSessions`, `killSessions`); those the node used.
    pub fn end(&mut self, lsids: &[u128]) -> Vec<u128> {
        lsids.iter().filter(|l| self.sessions.remove(l).is_some()).copied().collect()
    }

    /// Ends the sessions any of `matches` reaches (`killSessions`, `killAllSessions` with users,
    /// `killAllSessionsByPattern`); their identifiers, in order.
    pub fn end_matching(&mut self, matches: &[SessionMatch]) -> Vec<u128> {
        let dead: Vec<u128> = self
            .sessions
            .iter()
            .filter(|(l, s)| matches.iter().any(|m| m.reaches(**l, &s.owner.uid)))
            .map(|(l, _)| *l)
            .collect();
        self.end(&dead)
    }

    /// Marks sessions to end at the next refresh (`endSessions`): the reference server ends them
    /// and kills their cursors when its session cache refreshes, not at once.
    pub fn mark_ending(&mut self, matches: &[SessionMatch]) {
        self.ending.extend(matches.iter().copied());
    }

    /// The sessions marked to end, which the refresh ends now.
    pub fn take_ending(&mut self) -> Vec<SessionMatch> {
        std::mem::take(&mut self.ending).into_iter().collect()
    }

    /// Ends every session (`killAllSessions`); those the node used.
    pub fn end_all(&mut self) -> Vec<u128> {
        std::mem::take(&mut self.sessions).into_keys().collect()
    }

    /// Ends the sessions unused for the table's timeout at `now_ms`; their identifiers.
    pub fn expire(&mut self, now_ms: u64) -> Vec<u128> {
        let timeout = self.timeout_ms;
        let dead: Vec<u128> = self
            .sessions
            .iter()
            .filter(|(_, s)| now_ms.saturating_sub(s.last_use_ms) >= timeout)
            .map(|(l, _)| *l)
            .collect();
        self.end(&dead)
    }
}
