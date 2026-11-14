//! Encryption at rest: data keys per file wrapped with the master key, blocks sealed
//! with AES-256-CBC and HMAC-SHA-256 truncated to 16 bytes (the default) or AES-256-GCM, a random
//! IV or nonce per block kept with its tag in the 32 bytes each format reserves, and a key that
//! refuses to seal past 2³² blocks.

use std::sync::atomic::{AtomicUsize, Ordering};

use teseladb_crypto::at_rest::{
    AtRestError, CipherMode, Encryption, KeyRing, MAX_BLOCKS_PER_KEY, TRAILER_LEN, WRAPPED_KEY_LEN,
};

const MASTER: [u8; 32] = [7; 32];
const DOMAIN: &str = "teseladb-crypto test";

fn ring(mode: CipherMode) -> KeyRing {
    KeyRing::new(
        &MASTER,
        mode,
        |b: &mut [u8]| {
            b.fill(1);
            Ok(())
        },
        DOMAIN,
    )
}

#[test]
fn blocks_round_trip_in_both_modes_and_hide_their_bytes() {
    for mode in [CipherMode::Cbc, CipherMode::Gcm] {
        let ring = ring(mode);
        let (key, wrapped) = ring.new_file_key().expect("a new data key");
        assert_eq!(wrapped.len(), WRAPPED_KEY_LEN);
        let plain = b"a secret document value, longer than one AES block".repeat(3);
        let (sealed, trailer) = key.seal(b"block 1", &plain).expect("the block seals");
        assert_eq!(trailer.len(), TRAILER_LEN);
        assert!(!sealed.windows(6).any(|w| w == b"secret"), "{mode:?}");
        assert_eq!(key.open(b"block 1", &sealed, &trailer).expect("the block opens"), plain);
        // The same block sealed again gets another IV or nonce: never derived from a position.
        let (again, trailer2) = key.seal(b"block 1", &plain).expect("the block seals");
        assert_ne!((again.clone(), trailer2), (sealed.clone(), trailer));
        // The key opens from its wrapped form, with the same master key only.
        let reopened = ring.open_file_key(&wrapped).expect("the wrapped key opens");
        assert_eq!(reopened.open(b"block 1", &sealed, &trailer).expect("the block opens"), plain);
        let other = KeyRing::new(
            &[8; 32],
            mode,
            |b: &mut [u8]| {
                b.fill(1);
                Ok(())
            },
            DOMAIN,
        );
        assert_eq!(other.open_file_key(&wrapped).err(), Some(AtRestError::WrongKey));
        // Any changed byte, another context or another key is refused.
        let mut bad = sealed.clone();
        bad[3] ^= 1;
        assert_eq!(key.open(b"block 1", &bad, &trailer), Err(AtRestError::Corrupt));
        let mut bad_trailer = trailer;
        bad_trailer[0] ^= 1;
        assert_eq!(key.open(b"block 1", &sealed, &bad_trailer), Err(AtRestError::Corrupt));
        assert_eq!(key.open(b"block 2", &sealed, &trailer), Err(AtRestError::Corrupt));
        let (third, _) = ring.new_file_key().expect("a new data key");
        assert_eq!(third.open(b"block 1", &sealed, &trailer), Err(AtRestError::Corrupt));
        // A GCM trailer whose last four bytes are not zero.
        if mode == CipherMode::Gcm {
            let mut padded = trailer;
            padded[TRAILER_LEN - 1] = 1;
            assert_eq!(key.open(b"block 1", &sealed, &padded), Err(AtRestError::Corrupt));
        }
        // Empty blocks too.
        let (e, t) = key.seal(b"", b"").expect("the block seals");
        assert_eq!(key.open(b"", &e, &t).expect("the block opens"), b"");
        assert_eq!(key.mode(), mode);
    }
}

#[test]
fn wrapped_keys_say_their_mode_and_reject_damage() {
    let cbc = ring(CipherMode::Cbc);
    let (_, wrapped) = cbc.new_file_key().expect("a new data key");
    // A ring of the other mode still opens it: the mode is the file's.
    let gcm = ring(CipherMode::Gcm);
    assert_eq!(gcm.open_file_key(&wrapped).expect("the wrapped key opens").mode(), CipherMode::Cbc);
    for i in 0..WRAPPED_KEY_LEN {
        let mut bad = wrapped;
        bad[i] ^= 0x10;
        assert!(cbc.open_file_key(&bad).is_err(), "byte {i}");
    }
    assert_eq!(cbc.open_file_key(&wrapped[1..]).err(), Some(AtRestError::WrongKey));
    assert_eq!(cbc.open_file_key(&[0; WRAPPED_KEY_LEN]).err(), Some(AtRestError::WrongKey));
    assert_eq!(CipherMode::parse("AES256-CBC"), Some(CipherMode::Cbc));
    assert_eq!(CipherMode::parse("AES256-GCM"), Some(CipherMode::Gcm));
    assert!(CipherMode::parse("AES128-CBC").is_none());
    assert_eq!(CipherMode::Gcm.as_str(), "AES256-GCM");
    for e in [AtRestError::WrongKey, AtRestError::Corrupt, AtRestError::Exhausted] {
        assert!(!e.to_string().is_empty());
    }
    assert_eq!(cbc.mode(), CipherMode::Cbc);
    // A shared ring is the same ring; another one is not.
    let shared = Encryption(std::sync::Arc::new(ring(CipherMode::Cbc)));
    assert_eq!(shared, shared.clone());
    assert_eq!(shared.ring().mode(), CipherMode::Cbc);
    assert_ne!(shared, Encryption(std::sync::Arc::new(ring(CipherMode::Cbc))));
    // No key bytes in what is printed.
    let (key, _) = cbc.new_file_key().expect("a new data key");
    assert!(!format!("{key:?}{cbc:?}").contains("07, 07"));
}

#[test]
fn a_key_stops_sealing_after_its_last_block() {
    let ring = ring(CipherMode::Gcm);
    let (key, _) = ring.new_file_key().expect("a new data key");
    key.set_sealed_for_tests(MAX_BLOCKS_PER_KEY - 1);
    assert!(!key.exhausted());
    let (s, t) = key.seal(b"", b"last").expect("the block seals");
    assert!(key.exhausted());
    assert_eq!(key.seal(b"", b"one more").err(), Some(AtRestError::Exhausted));
    // What it sealed still opens.
    assert_eq!(key.open(b"", &s, &t).expect("the block opens"), b"last");
    assert_eq!(MAX_BLOCKS_PER_KEY, 1 << 32);
}

static SEEDS: AtomicUsize = AtomicUsize::new(0);

#[expect(clippy::unnecessary_wraps, reason = "the signature of a seed source")]
fn counted_seed(b: &mut [u8]) -> std::io::Result<()> {
    SEEDS.fetch_add(1, Ordering::SeqCst);
    b.fill(5);
    Ok(())
}

fn failing_seed(_: &mut [u8]) -> std::io::Result<()> {
    Err(std::io::Error::other("no entropy"))
}

static FIRST_GIVEN: AtomicUsize = AtomicUsize::new(0);

/// Seeds once, then fails.
fn first_seed_only(b: &mut [u8]) -> std::io::Result<()> {
    if FIRST_GIVEN.fetch_add(1, Ordering::SeqCst) > 0 {
        return failing_seed(b);
    }
    b.fill(6);
    Ok(())
}

/// Each thread draws from its own generator of the ring: seeded on its first draw,
/// reseeded when it runs out, and different from another thread's even with the same seed.
#[test]
fn each_thread_seeds_its_own_generator_and_reseeds_it() {
    let ring = KeyRing::new(&MASTER, CipherMode::Gcm, counted_seed, DOMAIN).with_reseed_interval(4);
    let before = SEEDS.load(Ordering::SeqCst);
    let (key, wrapped) = ring.new_file_key().expect("a new data key");
    assert_eq!(SEEDS.load(Ordering::SeqCst), before + 1);
    // A new key takes two requests and a block one: the fifth request reseeds.
    for _ in 0..2 {
        key.seal(b"", b"x").expect("the block seals");
    }
    assert_eq!(SEEDS.load(Ordering::SeqCst), before + 1);
    let (s, t) = key.seal(b"", b"x").expect("the block seals");
    assert_eq!(SEEDS.load(Ordering::SeqCst), before + 2);
    assert_eq!(key.open(b"", &s, &t).expect("the block opens"), b"x");
    // Another thread with the same seed draws other nonces, under the same key.
    let ring = std::sync::Arc::new(ring);
    let nonces = |ring: std::sync::Arc<KeyRing>| {
        std::thread::spawn(move || {
            let key = ring.open_file_key(&wrapped).expect("the wrapped key opens");
            let (_, t) = key.seal(b"", b"y").expect("the block seals");
            t[..12].to_vec()
        })
        .join()
        .expect("the thread ends")
    };
    assert_ne!(nonces(ring.clone()), nonces(ring));
}

#[test]
fn a_seed_source_that_fails_makes_no_key() {
    let ring = KeyRing::new(&MASTER, CipherMode::Cbc, failing_seed, DOMAIN);
    assert_eq!(ring.new_file_key().err(), Some(AtRestError::Exhausted));
    // A generator that may never draw asks for a seed at every request.
    let fixed = |b: &mut [u8]| {
        b.fill(7);
        Ok(())
    };
    let ring = KeyRing::new(&MASTER, CipherMode::Cbc, fixed, DOMAIN).with_reseed_interval(0);
    assert_eq!(ring.new_file_key().err(), Some(AtRestError::Exhausted));
    // A source that fails when the generator runs out stops the next block.
    let ring =
        KeyRing::new(&MASTER, CipherMode::Cbc, first_seed_only, DOMAIN).with_reseed_interval(2);
    let (key, _) = ring.new_file_key().expect("a new data key");
    assert_eq!(key.seal(b"", b"x").err(), Some(AtRestError::Exhausted));
}

#[test]
fn a_key_wrapped_under_one_domain_does_not_open_under_another() {
    for mode in [CipherMode::Cbc, CipherMode::Gcm] {
        assert_eq!(ring(mode).domain(), DOMAIN);
        let (_, wrapped) = ring(mode).new_file_key().expect("a new data key");
        let other = KeyRing::new(
            &MASTER,
            mode,
            |b: &mut [u8]| {
                b.fill(1);
                Ok(())
            },
            "another domain",
        );
        assert_eq!(other.open_file_key(&wrapped).err(), Some(AtRestError::WrongKey));
        assert!(ring(mode).open_file_key(&wrapped).is_ok());
    }
}
