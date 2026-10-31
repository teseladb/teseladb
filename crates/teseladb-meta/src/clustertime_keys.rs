//! The `$clusterTime` signing keys of the cluster in Group 0 (013 FR-014, data model §4): each
//! with its id, its expiry and the key sealed with the master key, which the nodes open and this
//! crate does not. Group 0's leader adds them two ahead and drops the old ones; a key names the
//! expiry it follows (`after`), so that two proposals made on the same state add one key. Kinds
//! 44 (`AddClusterTimeKey`) and 45 (`ExpireClusterTimeKey`).

use std::collections::BTreeMap;

use teseladb_types::{DecodeError, Decoder, Encoder};

/// A signing key as Group 0 keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedKey {
    /// Its id: the cluster time at which it was made.
    pub id: i64,
    /// It signs times before this second.
    pub expires_at: u32,
    /// The key sealed with the master key.
    pub sealed: Vec<u8>,
}

impl SealedKey {
    /// Writes the key.
    pub fn encode(&self, e: &mut Encoder) {
        e.i64(self.id).u32(self.expires_at).bytes(&self.sealed);
    }

    /// Reads what [`SealedKey::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<SealedKey, DecodeError> {
        Ok(SealedKey { id: d.i64()?, expires_at: d.u32()?, sealed: d.bytes()?.to_vec() })
    }
}

/// The signing keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClusterTimeKeys {
    /// The keys by id.
    pub keys: BTreeMap<i64, SealedKey>,
    /// Changes applied so far: a node opens the keys again when it moves.
    pub version: u64,
}

impl ClusterTimeKeys {
    /// The latest expiry of the keys (0 without keys).
    pub fn last_expiry(&self) -> u32 {
        self.keys.values().map(|k| k.expires_at).max().unwrap_or(0)
    }

    /// Adds `key` if the latest expiry is still `after` and its id is free; `false` changes
    /// nothing (another proposal made on the same state came first).
    pub fn add(&mut self, key: &SealedKey, after: u32) -> bool {
        if self.last_expiry() != after || key.expires_at <= after || self.keys.contains_key(&key.id)
        {
            return false;
        }
        self.keys.insert(key.id, key.clone());
        self.version += 1;
        true
    }

    /// Drops key `id`; `false` when there is none.
    pub fn expire(&mut self, id: i64) -> bool {
        let gone = self.keys.remove(&id).is_some();
        if gone {
            self.version += 1;
        }
        gone
    }

    /// Writes the keys into Group 0's snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        e.u64(self.version).var(self.keys.len() as u64);
        for k in self.keys.values() {
            k.encode(e);
        }
    }

    /// Reads what [`ClusterTimeKeys::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<ClusterTimeKeys, DecodeError> {
        let mut keys = ClusterTimeKeys { version: d.u64()?, ..ClusterTimeKeys::default() };
        for _ in 0..d.var()? {
            let k = SealedKey::decode(d)?;
            keys.keys.insert(k.id, k);
        }
        Ok(keys)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn key(id: i64, expires_at: u32) -> SealedKey {
        SealedKey { id, expires_at, sealed: vec![id as u8; 48] }
    }

    #[test]
    fn keys_chain_on_the_last_expiry() {
        let mut keys = ClusterTimeKeys::default();
        assert_eq!(keys.last_expiry(), 0);
        assert!(keys.add(&key(1, 100), 0));
        // The second proposal made on the same state adds nothing.
        assert!(!keys.add(&key(2, 100), 0));
        assert!(keys.add(&key(2, 200), 100));
        // An expiry not past `after`, or an id taken, adds nothing either.
        assert!(!keys.add(&key(3, 200), 200));
        assert!(!keys.add(&key(2, 300), 200));
        assert_eq!(keys.version, 2);
        assert!(keys.expire(1));
        assert!(!keys.expire(1));
        assert_eq!(keys.version, 3);
        assert_eq!(keys.last_expiry(), 200);
        let mut e = Encoder::new();
        keys.encode(&mut e);
        let bytes = e.finish();
        assert_eq!(ClusterTimeKeys::decode(&mut Decoder::new(&bytes)), Ok(keys));
        assert!(ClusterTimeKeys::decode(&mut Decoder::new(&bytes[..9])).is_err());
    }
}
