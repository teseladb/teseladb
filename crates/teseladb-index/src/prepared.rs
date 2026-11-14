//! Indexes with `prepareUnique` (FR-004, FR-007, contract `index-commands.md`): their entries
//! keep the format of a non-unique index, with the primary key at the end of the key, but a write
//! cannot add an entry whose values another document already has (measured on a reference router
//! 9.0.4: the duplicates that were there stay, a new one is a `DuplicateKey`). The values without
//! the primary key are a prefix of every entry that has them, so the layer that runs the write
//! reads the entries under that prefix first (a [`Probe`]) and the write sees, among the keys it
//! latches, whether one of them is alive and holds another primary key.
//!
//! Each value has a guard key among the guards of its index (the index number with its top bit
//! set) whose version every write that adds an entry with the value raises. The write latches the
//! guard and compares its version with the one its probe saw: a write that added the value after
//! the probe raised it, and the probe is made again. So two writes that add the same value never
//! both pass.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_keys::{KeyBuilder, KeyParts};

/// The bit that turns an index number into the number of its guards.
pub const GUARDS: u32 = 1 << 31;
/// How many entries the first probe of a value reads: one may be the document's own.
pub const FIRST_PROBE: u32 = 2;

/// The number of the guards of index `index`.
pub fn guards_of(index: u32) -> u32 {
    index | GUARDS
}

/// Why a key or a value is not what a prepared index keeps.
pub const NOT_AN_INDEX_PREFIX: &str = "an index prefix outside the data space";
/// A guard that holds something other than a version.
pub const NOT_A_VERSION: &str = "a guard is not 8 bytes";

/// The guard key of the values whose entries start with `prefix`.
pub fn guard_key(prefix: &[u8]) -> Result<Vec<u8>, &'static str> {
    match KeyParts::of(prefix).map_err(|_| NOT_AN_INDEX_PREFIX)? {
        KeyParts::Data { tenant, collection, index, rest } => {
            let mut out =
                KeyBuilder::index_prefix(tenant, collection, guards_of(index)).as_bytes().to_vec();
            out.extend_from_slice(rest);
            Ok(out)
        }
        _ => Err(NOT_AN_INDEX_PREFIX),
    }
}

/// The version a guard holds: 8 bytes, big-endian.
pub fn version_of(value: &[u8]) -> Result<u64, &'static str> {
    let bytes: [u8; 8] = value.try_into().map_err(|_| NOT_A_VERSION)?;
    Ok(u64::from_be_bytes(bytes))
}

/// What the layer that runs a write read for one value before the write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The value's guard key.
    pub guard: Vec<u8>,
    /// The guard's version then (`None`: no guard yet).
    pub version: Option<u64>,
    /// The entry keys under the value's prefix, in key order, at most `limit` of them.
    pub found: Vec<Vec<u8>>,
    /// How many entries the probe read at most.
    pub limit: u32,
}

/// A probe a write needs before it can decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeRequest {
    /// The value's guard key.
    pub guard: Vec<u8>,
    /// The prefix of the value's entries.
    pub prefix: Vec<u8>,
    /// How many entries to read at most.
    pub limit: u32,
}

/// Why a write cannot add an entry of a prepared index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparedError {
    /// It has to probe the value first, or again.
    Probe(ProbeRequest),
    /// Another document has the values of the entry whose prefix this is.
    Duplicate {
        /// The prefix.
        prefix: Vec<u8>,
    },
    /// A prefix or a guard is not what a prepared index keeps.
    Malformed(&'static str),
}

/// The entries of prepared indexes a write's statements have added (`true`) or removed so far,
/// and the guards it raised.
#[derive(Clone, Debug, Default)]
pub struct PreparedGuard {
    entries: BTreeMap<Vec<u8>, bool>,
    raised: BTreeSet<Vec<u8>>,
}

impl PreparedGuard {
    /// Admits one statement: it removes the entry keys `removed` and adds the entries `added`
    /// (prefix and key). `probes` are the write's probes and `stored` the stored value of a key
    /// the write latched. A statement refused records nothing. The guards it raises, with their
    /// new values, are the result.
    pub fn admit<'s>(
        &mut self,
        removed: &[Vec<u8>],
        added: &[(Vec<u8>, Vec<u8>)],
        probes: &[Probe],
        stored: &dyn Fn(&[u8]) -> Option<&'s [u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, PreparedError> {
        let mut raise = Vec::new();
        for (prefix, key) in added {
            let guard = guard_key(prefix).map_err(PreparedError::Malformed)?;
            let ask = |limit| {
                PreparedError::Probe(ProbeRequest {
                    guard: guard.clone(),
                    prefix: prefix.clone(),
                    limit,
                })
            };
            let Some(probe) = probes.iter().find(|p| p.guard == guard) else {
                return Err(ask(FIRST_PROBE));
            };
            let now =
                stored(&guard).map(version_of).transpose().map_err(PreparedError::Malformed)?;
            if now != probe.version {
                return Err(ask(probe.limit));
            }
            let alive = |k: &Vec<u8>| {
                k != key
                    && !removed.contains(k)
                    && self.entries.get(k).copied().unwrap_or_else(|| stored(k).is_some())
            };
            let earlier =
                self.entries.iter().any(|(k, &on)| on && k.starts_with(prefix) && alive(k));
            if earlier || probe.found.iter().any(alive) {
                return Err(PreparedError::Duplicate { prefix: prefix.clone() });
            }
            // Every entry the probe saw is gone: one it did not read may be there.
            if probe.found.len() >= probe.limit as usize {
                return Err(ask(probe.limit.saturating_mul(2)));
            }
            if !self.raised.contains(&guard) && !raise.iter().any(|(g, _)| g == &guard) {
                let next = now.unwrap_or(0) + 1;
                raise.push((guard, next.to_be_bytes().to_vec()));
            }
        }
        for k in removed {
            self.entries.insert(k.clone(), false);
        }
        for (_, k) in added {
            self.entries.insert(k.clone(), true);
        }
        self.raised.extend(raise.iter().map(|(g, _)| g.clone()));
        Ok(raise)
    }
}
