//! Logical sessions (FR-016, data model §2): each `lsid` the drivers send, with the cluster time
//! of its last use and the highest `txnNumber` seen. A session nobody used for
//! [`SESSION_TIMEOUT_MS`] expires; ending or killing one kills its cursors, which the caller does
//! with the identifiers these functions give.

use std::collections::{BTreeMap, BTreeSet};

/// Milliseconds a session lives without use (`logicalSessionTimeoutMinutes`: 30).
pub const SESSION_TIMEOUT_MS: u64 = 30 * 60_000;

/// A logical session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
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
    ending: BTreeSet<u128>,
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

    /// The sessions used at or after `since_ms`, in order, with their last use.
    pub fn used_since(&self, since_ms: u64) -> Vec<(u128, u64)> {
        self.sessions
            .iter()
            .filter(|(l, s)| s.last_use_ms >= since_ms && !self.ending.contains(l))
            .map(|(l, s)| (*l, s.last_use_ms))
            .collect()
    }

    /// A command of session `lsid` at cluster time `now_ms` (also `refreshSessions`): it starts
    /// the session if it is new.
    pub fn touch(&mut self, lsid: u128, now_ms: u64) -> &mut Session {
        let s =
            self.sessions.entry(lsid).or_insert(Session { last_use_ms: now_ms, txn_number: None });
        s.last_use_ms = s.last_use_ms.max(now_ms);
        s
    }

    /// Ends sessions `lsids` (`endSessions`, `killSessions`); those the node used.
    pub fn end(&mut self, lsids: &[u128]) -> Vec<u128> {
        lsids.iter().filter(|l| self.sessions.remove(l).is_some()).copied().collect()
    }

    /// Marks sessions to end at the next refresh (`endSessions`): RefDb ends them and kills
    /// their cursors when its session cache refreshes, not at once.
    pub fn mark_ending(&mut self, lsids: &[u128]) {
        self.ending.extend(lsids.iter().copied());
    }

    /// The sessions marked to end, which the refresh ends now.
    pub fn take_ending(&mut self) -> Vec<u128> {
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
