//! The cursors of a node (FR-017, data model §3): each one lives in the node that opened it,
//! whose number goes in the 10 high bits of its identifier so that any node knows where a
//! `getMore` goes (019). A cursor outside any session that nobody used for
//! [`CURSOR_TIMEOUT_MS`] expires unless it was opened with `noCursorTimeout`; a session's cursor
//! lives as long as its session (RefDb's `cursorTimeoutMillis` only reaps cursors without a
//! session, measured on 9.0.4). While a command uses a cursor, it is out of the table and cannot
//! expire.

use std::collections::BTreeMap;

/// Milliseconds a cursor lives without use (`cursorTimeoutMillis`).
pub const CURSOR_TIMEOUT_MS: u64 = 10 * 60_000;
/// Bits of a cursor identifier that hold the node's number.
pub const NODE_BITS: u32 = 10;
/// Bits of a cursor identifier that are random.
const RANDOM_BITS: u32 = 64 - NODE_BITS;

/// The identifier of a cursor opened by node `node` (its low [`NODE_BITS`] bits) with the random
/// number `random`; never 0, which means an exhausted cursor.
pub fn cursor_id(node: u64, random: u64) -> i64 {
    let low = random & ((1u64 << RANDOM_BITS) - 1);
    let low = if low == 0 { 1 } else { low };
    ((node & ((1 << NODE_BITS) - 1)) << RANDOM_BITS | low) as i64
}

/// The number (low bits) of the node that opened cursor `id`.
pub fn node_of(id: i64) -> u64 {
    (id as u64) >> RANDOM_BITS
}

/// A cursor and what it needs to go on.
#[derive(Clone, Debug, PartialEq)]
pub struct Cursor<S> {
    /// Namespace `db.collection` it reads.
    pub ns: String,
    /// Session it belongs to.
    pub lsid: Option<u128>,
    /// The user who opened it (`db.user`), when the node authenticates; only that user may go on
    /// with it.
    pub owner: Option<String>,
    /// Opened with `noCursorTimeout`.
    pub no_timeout: bool,
    /// Cluster time of its last use, in milliseconds.
    pub last_use_ms: u64,
    /// Where the reading is.
    pub state: S,
}

/// Why a cursor cannot be used; the namespace is checked before the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorError {
    /// It does not exist: never opened, exhausted, killed or expired (`CursorNotFound`).
    NotFound,
    /// It reads another namespace (`Unauthorized`, as RefDb answers).
    OtherNamespace,
    /// It was opened outside any session and the command has one (50799).
    NotInSession,
    /// It belongs to a session and the command has none (50800).
    SessionRequired,
    /// It belongs to another session (50801).
    OtherSession,
}

/// The cursors of a node.
#[derive(Clone, Debug)]
pub struct CursorTable<S> {
    cursors: BTreeMap<i64, Cursor<S>>,
    timeout_ms: u64,
}

impl<S> Default for CursorTable<S> {
    fn default() -> Self {
        CursorTable::with_timeout(CURSOR_TIMEOUT_MS)
    }
}

impl<S> CursorTable<S> {
    /// An empty table whose cursors expire after [`CURSOR_TIMEOUT_MS`].
    pub fn new() -> CursorTable<S> {
        CursorTable::default()
    }

    /// An empty table whose cursors expire after `timeout_ms` (`cursorTimeoutMillis`).
    pub fn with_timeout(timeout_ms: u64) -> CursorTable<S> {
        CursorTable { cursors: BTreeMap::new(), timeout_ms }
    }

    /// Milliseconds a cursor outside any session lives without use.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Changes the timeout (`setParameter cursorTimeoutMillis`); the next expiry uses it.
    pub fn set_timeout_ms(&mut self, timeout_ms: u64) {
        self.timeout_ms = timeout_ms;
    }

    /// Cursor `id`, if it is open and not taken.
    pub fn get(&self, id: i64) -> Option<&Cursor<S>> {
        self.cursors.get(&id)
    }

    /// Cursor `id` to change, if it is open and not taken.
    pub fn get_mut(&mut self, id: i64) -> Option<&mut Cursor<S>> {
        self.cursors.get_mut(&id)
    }

    /// Open cursors.
    pub fn len(&self) -> usize {
        self.cursors.len()
    }

    /// Whether no cursor is open.
    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }

    /// Whether `id` is open (taken cursors are not).
    pub fn contains(&self, id: i64) -> bool {
        self.cursors.contains_key(&id)
    }

    /// Keeps cursor `id` (a new one, or one given back after a `getMore`).
    pub fn put(&mut self, id: i64, cursor: Cursor<S>) {
        self.cursors.insert(id, cursor);
    }

    /// Takes cursor `id` for a `getMore` on `ns` in session `lsid` at cluster time `now_ms`;
    /// it stays taken until it is put back.
    pub fn take(
        &mut self,
        id: i64,
        ns: &str,
        lsid: Option<u128>,
        now_ms: u64,
    ) -> Result<Cursor<S>, CursorError> {
        let c = self.cursors.get(&id).ok_or(CursorError::NotFound)?;
        if c.ns != ns {
            return Err(CursorError::OtherNamespace);
        }
        match (c.lsid, lsid) {
            (None, Some(_)) => return Err(CursorError::NotInSession),
            (Some(_), None) => return Err(CursorError::SessionRequired),
            (Some(a), Some(b)) if a != b => return Err(CursorError::OtherSession),
            _ => {}
        }
        let mut c = self.cursors.remove(&id).ok_or(CursorError::NotFound)?;
        c.last_use_ms = now_ms;
        Ok(c)
    }

    /// Kills cursor `id` whatever it reads (`killCursors` on a `refrouter` 9.0 does not check the
    /// namespace, measured); whether it was open.
    pub fn remove(&mut self, id: i64) -> bool {
        self.cursors.remove(&id).is_some()
    }

    /// Kills the cursors of sessions `lsids`; their identifiers.
    pub fn kill_sessions(&mut self, lsids: &[u128]) -> Vec<i64> {
        self.kill_where(|c| c.lsid.is_some_and(|l| lsids.contains(&l)))
    }

    /// Kills every cursor that belongs to a session; their identifiers.
    pub fn kill_all_sessions(&mut self) -> Vec<i64> {
        self.kill_where(|c| c.lsid.is_some())
    }

    /// Kills the cursors outside any session unused for the table's timeout at `now_ms`, except
    /// `noCursorTimeout` ones; their identifiers.
    pub fn expire(&mut self, now_ms: u64) -> Vec<i64> {
        let timeout = self.timeout_ms;
        self.kill_where(|c| {
            c.lsid.is_none() && !c.no_timeout && now_ms.saturating_sub(c.last_use_ms) >= timeout
        })
    }

    fn kill_where(&mut self, dead: impl Fn(&Cursor<S>) -> bool) -> Vec<i64> {
        let ids: Vec<i64> =
            self.cursors.iter().filter(|(_, c)| dead(c)).map(|(id, _)| *id).collect();
        for id in &ids {
            self.cursors.remove(id);
        }
        ids
    }
}
