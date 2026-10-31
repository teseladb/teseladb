//! `$clusterTime` signatures (013 T025, FR-014): the signature of a `refrouter` 9.0.4 with
//! `--keyFile` (key read from its `admin.system.keys`, 30-10-2026), which key signs which time,
//! the checks of a time a client sends with RefDb's errors, and the rotation of the keys.

use teseladb_auth::clustertime;
use teseladb_auth::clustertime::{
    ClusterTimeError, HmacKey, KEY_LEN, KEY_VALID_SECS, KeyRing, KeySealer, SEALED_LEN, SealError,
    Time, sign,
};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn key20(s: &str) -> [u8; KEY_LEN] {
    hex(s).try_into().unwrap()
}

/// The keys of the measured `refrouter`.
fn measured() -> KeyRing {
    KeyRing {
        keys: vec![
            HmacKey {
                id: 7_702_466_350_031_044_650,
                key: key20("cedfad55cc5e0cc004ab272b5c3fb7b9ff3d390e"),
                expires_at: 1_801_146_198,
            },
            HmacKey { id: 7_702_466_350_031_044_651, key: [7; KEY_LEN], expires_at: 1_808_922_198 },
        ],
    }
}

const NOW: Time = Time { secs: 1_793_376_615, inc: 1 };

#[test]
fn signs_as_refdb() {
    let ring = measured();
    let (hash, id) = ring.signature(NOW).unwrap();
    assert_eq!(hash.to_vec(), hex("5cc4cc2e629897e88e2dd62f4a351f286b6b2cb4"));
    assert_eq!(id, 7_702_466_350_031_044_650);
    // One signature covers the increments that differ only in their low 16 bits.
    assert_eq!(sign(&ring.keys[0].key, Time { secs: NOW.secs, inc: 0xFFFF }), hash);
    assert_ne!(sign(&ring.keys[0].key, Time { secs: NOW.secs, inc: 0x1_0000 }), hash);
    // Past its `expiresAt` the next key signs; past both, none.
    let later = Time { secs: 1_801_146_198, inc: 0 };
    assert_eq!(ring.signature(later).unwrap().1, 7_702_466_350_031_044_651);
    assert!(ring.signature(Time { secs: 1_808_922_198, inc: 0 }).is_none());
    assert!(KeyRing::default().signature(NOW).is_none());
}

#[test]
fn checks_a_client_time_as_refdb() {
    let ring = measured();
    let (hash, id) = ring.signature(NOW).unwrap();
    let ahead = Time { secs: NOW.secs + 10, inc: 1 };
    let current = NOW;
    // Not past the node's own time: taken as is, whatever its signature.
    assert_eq!(ring.check(NOW, current, Some((&[1; KEY_LEN], 5))), Ok(()));
    assert_eq!(ring.check(Time { secs: NOW.secs - 10, inc: 1 }, current, None), Ok(()));
    assert_eq!(ring.check(NOW, current, Some((&hash, id))), Ok(()));
    // Later: a valid signature, or RefDb's errors.
    let good = sign(&ring.keys[0].key, ahead);
    assert_eq!(ring.check(ahead, current, Some((&good, id))), Ok(()));
    assert_eq!(
        ring.check(ahead, current, Some((&[1; KEY_LEN], id))),
        Err(ClusterTimeError::Mismatch)
    );
    let unknown = ring.check(ahead, current, Some((&[1; KEY_LEN], 5)));
    assert_eq!(unknown, Err(ClusterTimeError::KeyNotFound(ahead, 5)));
    assert_eq!(
        unknown.unwrap_err().message(),
        format!(
            "No keys found for HMAC that is valid for time: {{ ts: Timestamp({}, 1) }} with id: 5",
            NOW.secs + 10
        )
    );
    assert_eq!(ring.check(ahead, current, None), Err(ClusterTimeError::KeyNotFound(ahead, 0)));
    // A hash that is not 20 bytes is refused when it is read, whatever the time.
    let short = clustertime::hash(&[1; 5]);
    assert_eq!(short, Err(ClusterTimeError::HashLength(5)));
    assert_eq!(short.unwrap_err().message(), "Unsupported SHA1Block hash length: 5");
    assert_eq!(clustertime::hash(&good), Ok(good));
    assert_eq!(ClusterTimeError::Mismatch.message(), "Proof does not match the cluster time");
    // A key past its `expiresAt` signs nothing.
    let expired = Time { secs: 1_801_146_198, inc: 0 };
    let s = sign(&ring.keys[0].key, expired);
    assert_eq!(
        ring.check(expired, current, Some((&s, id))),
        Err(ClusterTimeError::KeyNotFound(expired, id))
    );
}

#[test]
fn keys_rotate_two_ahead() {
    let mut n = 0u8;
    let mut random = |k: &mut [u8; KEY_LEN]| {
        n += 1;
        *k = [n; KEY_LEN];
    };
    let start = Time { secs: 1_000, inc: 4 };
    let first = KeyRing::default().due(start, &mut random);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].id, i64::try_from(start.as_u64()).unwrap());
    assert_eq!(first[1].id, first[0].id + 1);
    assert_eq!(first[0].expires_at, 1_000 + KEY_VALID_SECS);
    assert_eq!(first[1].expires_at, 1_000 + 2 * KEY_VALID_SECS);
    assert_ne!(first[0].key, first[1].key);
    let mut ring = KeyRing { keys: first };
    assert!(ring.due(start, &mut random).is_empty());
    // Once the first expires, one more key 90 days after the last.
    let later = Time { secs: 1_000 + KEY_VALID_SECS, inc: 0 };
    let more = ring.due(later, &mut random);
    assert_eq!(more.len(), 1);
    assert_eq!(more[0].expires_at, 1_000 + 3 * KEY_VALID_SECS);
    ring.keys.extend(more);
    assert_eq!(ring.signature(later).unwrap().1, ring.keys[1].id);
    assert!(format!("{:?}", ring.keys[0]).contains("expires_at"));
    assert!(!format!("{:?}", ring.keys[0]).contains("key:"));
}

#[test]
fn keys_are_sealed_with_the_master_key() {
    let master = [9u8; 32];
    let sealer = KeySealer::new(&master).unwrap();
    let key = measured().keys[0].clone();
    let sealed = sealer.seal(&key, [3; 12]);
    assert_eq!(sealed.len(), SEALED_LEN);
    // Nothing of the key is in the clear.
    assert!(!sealed.windows(4).any(|w| key.key.windows(4).any(|k| k == w)));
    assert_eq!(sealer.open(key.id, key.expires_at, &sealed), Ok(key.clone()));
    // Another nonce, another text; the same key back.
    let other = sealer.seal(&key, [4; 12]);
    assert_ne!(other, sealed);
    assert_eq!(sealer.open(key.id, key.expires_at, &other), Ok(key.clone()));
    // The id and the expiry are bound to the text: moving it to another key fails.
    assert_eq!(sealer.open(key.id + 1, key.expires_at, &sealed), Err(SealError));
    assert_eq!(sealer.open(key.id, key.expires_at + 1, &sealed), Err(SealError));
    // Another master key, a changed byte or a wrong length open nothing.
    let stranger = KeySealer::new(&[8u8; 32]).unwrap();
    assert_eq!(stranger.open(key.id, key.expires_at, &sealed), Err(SealError));
    let mut flipped = sealed.clone();
    flipped[20] ^= 1;
    assert_eq!(sealer.open(key.id, key.expires_at, &flipped), Err(SealError));
    assert_eq!(sealer.open(key.id, key.expires_at, &sealed[..SEALED_LEN - 1]), Err(SealError));
    // Its debug output shows no key material.
    assert_eq!(format!("{sealer:?}"), "KeySealer");
}

#[test]
fn the_last_expiry_chains_new_keys() {
    let ring = measured();
    assert_eq!(ring.last_expiry(), 1_808_922_198);
    assert_eq!(KeyRing::default().last_expiry(), 0);
}
