//! The `$clusterTime` signing keys of the node (013 FR-014, D-030): core 0 opens the keys Group 0
//! keeps sealed with the master key and publishes them in a [`KeySlot`] the client workers sign
//! and check with; while it leads Group 0 it keeps two keys ahead, each valid 90 days after the
//! one before as a reference router 9.0.4 does, and drops a key 90 days after it expired.

use std::sync::{Arc, PoisonError, RwLock};

use teseladb_auth::clustertime::{
    HmacKey, KEY_LEN, KEY_VALID_SECS, KeyRing, KeySealer, NONCE_LEN, Time,
};
use teseladb_crypto::rand::HmacDrbg;
use teseladb_meta::clustertime_keys::{ClusterTimeKeys, SealedKey};
use teseladb_meta::command::Group0Command;
use teseladb_types::Nanos;

/// Time between two looks at the keys while leading Group 0: a proposal that lost a race is
/// made again on the state that won it.
pub const KEY_CHECK: Nanos = Nanos::from_millis(2_000);

/// Bits of a cluster time's increment below the millisecond (005 FR-024).
const MS_SHIFT: u32 = 22;

/// The keys the client workers sign and check `$clusterTime` with.
#[derive(Debug, Default)]
pub struct KeySlot {
    ring: RwLock<Arc<KeyRing>>,
}

impl KeySlot {
    /// The keys now.
    pub fn ring(&self) -> Arc<KeyRing> {
        self.ring.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set(&self, ring: KeyRing) {
        *self.ring.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(ring);
    }
}

/// Core 0's part: opening, publishing and, on Group 0's leader, making the keys.
pub struct KeyDriver {
    sealer: KeySealer,
    slot: Arc<KeySlot>,
    random: HmacDrbg,
    /// The version of Group 0's keys last published.
    opened: Option<u64>,
    next: Nanos,
}

impl std::fmt::Debug for KeyDriver {
    /// Without the keys nor the generator's state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyDriver").field("opened", &self.opened).field("next", &self.next).finish()
    }
}

/// The cluster time of the wall clock `wall_ms` with a logical counter of 0 (005 FR-024).
pub fn time_of(wall_ms: u64) -> Time {
    let secs = u32::try_from(wall_ms / 1000).unwrap_or(u32::MAX);
    // Under 1 000 << 22, which fits in 32 bits.
    let inc = u32::try_from((wall_ms % 1000) << MS_SHIFT).unwrap_or(u32::MAX);
    Time { secs, inc }
}

impl KeyDriver {
    /// The driver of a node whose master key gives `sealer`; it publishes in `slot` and makes
    /// keys from `entropy`, which the node read from the system.
    pub fn new(sealer: KeySealer, slot: Arc<KeySlot>, entropy: &[u8]) -> KeyDriver {
        let random = HmacDrbg::new(entropy, &[], b"teseladb $clusterTime keys");
        KeyDriver { sealer, slot, random, opened: None, next: Nanos::ZERO }
    }

    /// Publishes Group 0's `keys` when they changed since the last time; returns how many would
    /// not open with this node's master key (they are left out, and the node says so in its log).
    pub fn publish(&mut self, keys: &ClusterTimeKeys) -> usize {
        if self.opened == Some(keys.version) {
            return 0;
        }
        let opened: Vec<HmacKey> = keys
            .keys
            .values()
            .filter_map(|k| self.sealer.open(k.id, k.expires_at, &k.sealed).ok())
            .collect();
        let unopened = keys.keys.len() - opened.len();
        self.slot.set(KeyRing { keys: opened });
        self.opened = Some(keys.version);
        unopened
    }

    /// What Group 0's leader proposes at `now`, with the wall clock at `wall_ms`: the keys that
    /// keep two ahead, each chained on the latest expiry, and the drops of the keys that expired
    /// 90 days ago or more.
    pub fn propose(
        &mut self,
        keys: &ClusterTimeKeys,
        now: Nanos,
        wall_ms: u64,
    ) -> Vec<Group0Command> {
        if now < self.next {
            return Vec::new();
        }
        self.next = now + KEY_CHECK;
        let at = time_of(wall_ms);
        let mut out: Vec<Group0Command> = keys
            .keys
            .values()
            .filter(|k| u64::from(k.expires_at) + u64::from(KEY_VALID_SECS) <= u64::from(at.secs))
            .map(|k| Group0Command::ExpireClusterTimeKey { id: k.id })
            .collect();
        // Only ids and expiries matter to what is due.
        let known = KeyRing {
            keys: keys
                .keys
                .values()
                .map(|k| HmacKey { id: k.id, key: [0; KEY_LEN], expires_at: k.expires_at })
                .collect(),
        };
        let mut failed = false;
        let random = &mut self.random;
        let due = known.due(at, &mut |key: &mut [u8; KEY_LEN]| {
            failed |= random.generate(key, &[]).is_err();
        });
        if failed {
            // The generator asks for a reseed only after 2^48 requests: never, at one key per
            // 90 days; without fresh bytes no key is made.
            return out;
        }
        let mut after = keys.last_expiry();
        for key in due {
            let mut nonce = [0u8; NONCE_LEN];
            if self.random.generate(&mut nonce, &[]).is_err() {
                break;
            }
            let sealed = SealedKey {
                id: key.id,
                expires_at: key.expires_at,
                sealed: self.sealer.seal(&key, nonce),
            };
            out.push(Group0Command::AddClusterTimeKey { key: sealed, after });
            after = key.expires_at;
        }
        out
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn driver() -> (KeyDriver, Arc<KeySlot>) {
        let slot = Arc::new(KeySlot::default());
        let sealer = KeySealer::new(&[5; 32]).unwrap();
        (KeyDriver::new(sealer, slot.clone(), &[1; 48]), slot)
    }

    fn apply(keys: &mut ClusterTimeKeys, cmds: &[Group0Command]) {
        for c in cmds {
            match c {
                Group0Command::AddClusterTimeKey { key, after } => assert!(keys.add(key, *after)),
                Group0Command::ExpireClusterTimeKey { id } => assert!(keys.expire(*id)),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    const WALL: u64 = 1_793_376_615_250;

    #[test]
    fn the_leader_keeps_two_keys_ahead_and_drops_old_ones() {
        let (mut d, slot) = driver();
        let mut keys = ClusterTimeKeys::default();
        let first = d.propose(&keys, Nanos::ZERO, WALL);
        assert_eq!(first.len(), 2);
        // Until the next look, nothing.
        assert!(d.propose(&keys, Nanos::from_millis(1), WALL).is_empty());
        apply(&mut keys, &first);
        assert_eq!(d.publish(&keys), 0);
        let ring = slot.ring();
        assert_eq!(ring.keys.len(), 2);
        let at = time_of(WALL);
        assert_eq!(at, Time { secs: 1_793_376_615, inc: 250 << 22 });
        assert_eq!(ring.keys[0].id, i64::try_from(at.as_u64()).unwrap());
        assert_eq!(ring.keys[0].expires_at, at.secs + KEY_VALID_SECS);
        assert_ne!(ring.keys[0].key, ring.keys[1].key);
        assert!(ring.signature(at).is_some());
        // Published once per version.
        assert_eq!(d.publish(&keys), 0);
        assert!(Arc::ptr_eq(&ring, &slot.ring()));
        assert!(d.propose(&keys, KEY_CHECK, WALL).is_empty());

        // After the first key expires, a third; 90 days later, the first goes.
        let later = WALL + u64::from(KEY_VALID_SECS) * 1000;
        let more = d.propose(&keys, KEY_CHECK + KEY_CHECK, later);
        assert_eq!(more.len(), 1);
        apply(&mut keys, &more);
        let much_later = later + u64::from(KEY_VALID_SECS) * 1000;
        let next = d.propose(&keys, Nanos::from_millis(10_000), much_later);
        let expired: Vec<_> = next
            .iter()
            .filter(|c| matches!(c, Group0Command::ExpireClusterTimeKey { .. }))
            .collect();
        assert_eq!(expired.len(), 1);
        apply(&mut keys, &next);
        assert_eq!(d.publish(&keys), 0);
        assert_eq!(slot.ring().keys.len(), keys.keys.len());
        assert!(format!("{d:?}").starts_with("KeyDriver"));
    }

    #[test]
    fn without_fresh_random_bytes_no_key_is_made() {
        let keys = ClusterTimeKeys::default();
        // The generator gives one key, then asks for a reseed: no key at all.
        let (mut d, _) = driver();
        d.random = HmacDrbg::new(&[1; 48], &[], b"t").with_reseed_interval(1);
        assert!(d.propose(&keys, Nanos::ZERO, WALL).is_empty());
        // Both keys, then no nonce to seal them: none proposed.
        let (mut d, _) = driver();
        d.random = HmacDrbg::new(&[1; 48], &[], b"t").with_reseed_interval(2);
        assert!(d.propose(&keys, Nanos::ZERO, WALL).is_empty());
    }

    #[test]
    fn keys_of_another_master_key_are_left_out() {
        let (mut d, _) = driver();
        let mut keys = ClusterTimeKeys::default();
        let first = d.propose(&keys, Nanos::ZERO, WALL);
        apply(&mut keys, &first);
        let stranger = KeySealer::new(&[6; 32]).unwrap();
        let mut other = KeyDriver::new(stranger, Arc::new(KeySlot::default()), &[2; 48]);
        assert_eq!(other.publish(&keys), 2);
        assert!(other.slot.ring().keys.is_empty());
    }
}
