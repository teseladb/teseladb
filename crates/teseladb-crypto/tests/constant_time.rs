//! Two-class timing test (013 T005, FR-018, research R3): each operation runs on a fixed secret
//! and on random secrets, interleaved at random; Welch's t on the cropped timings must stay
//! below 4.5, the threshold dudect uses (Reparaz, Balasch and Verbauwhede, DATE 2017). A
//! comparison that exits early must exceed it, so the test is known to detect leaks.
//!
//! Timing noise on a shared machine is not stationary, so a leak is reported only when five
//! measurements with different seeds all exceed the threshold, and the tests take a lock so they
//! never run at the same time as each other.

use std::hint::black_box;
use std::sync::Mutex;
use std::time::Instant;

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::aes::Aes;
use teseladb_crypto::chacha20poly1305::ChaCha20Poly1305;
use teseladb_crypto::ct::ct_eq;
use teseladb_crypto::curve25519::{Ed25519KeyPair, x25519};
use teseladb_crypto::ec::{EcPrivateKey, NamedCurve};
use teseladb_crypto::modes::{AesGcm, cbc_decrypt};

const THRESHOLD: f64 = 4.5;
const ATTEMPTS: u64 = 5;

static SERIAL: Mutex<()> = Mutex::new(());

/// xorshift64*: test inputs only.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn fill(&mut self, b: &mut [u8]) {
        for x in b {
            *x = self.next() as u8;
        }
    }
}

/// Welch's t between the two classes, after dropping the slowest 10 % of the pooled samples
/// (interrupts and frequency changes only ever add time).
fn welch_t(samples: &[(bool, f64)]) -> f64 {
    let mut pooled: Vec<f64> = samples.iter().map(|s| s.1).collect();
    pooled.sort_by(f64::total_cmp);
    let cut = pooled[pooled.len() * 9 / 10];
    let stats = |class: bool| {
        let v: Vec<f64> =
            samples.iter().filter(|s| s.0 == class && s.1 <= cut).map(|s| s.1).collect();
        let n = v.len() as f64;
        let mean = v.iter().sum::<f64>() / n;
        let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        (n, mean, var)
    };
    let (n0, m0, v0) = stats(false);
    let (n1, m1, v1) = stats(true);
    (m0 - m1) / (v0 / n0 + v1 / n1).sqrt()
}

/// Takes `samples` timings of `reps` calls of `run` on an input that `prepare(class, rng)` builds
/// outside the timed region, with the class chosen at random; returns |t|.
fn measure<T>(
    samples: usize,
    reps: usize,
    seed: u64,
    mut prepare: impl FnMut(bool, &mut Rng) -> T,
    mut run: impl FnMut(&T),
) -> f64 {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(samples);
    for i in 0..samples + samples / 10 {
        let class = rng.next() & 1 == 1;
        let input = prepare(class, &mut rng);
        let start = Instant::now();
        for _ in 0..reps {
            run(black_box(&input));
        }
        let elapsed = start.elapsed().as_nanos() as f64;
        if i >= samples / 10 {
            out.push((class, elapsed));
        }
    }
    welch_t(&out).abs()
}

/// The smallest |t| of [`ATTEMPTS`] measurements with different seeds: above the threshold only
/// when every attempt saw a difference.
fn min_t<T>(
    samples: usize,
    reps: usize,
    seed: u64,
    mut prepare: impl FnMut(bool, &mut Rng) -> T,
    mut run: impl FnMut(&T),
) -> f64 {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    (0..ATTEMPTS)
        .map(|a| measure(samples, reps, seed * 1000 + a + 1, &mut prepare, &mut run))
        .fold(f64::INFINITY, f64::min)
}

fn early_exit_eq(a: &[u8], b: &[u8]) -> bool {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return false;
        }
    }
    true
}

const SECRET: [u8; 512] = [0x5a; 512];

/// A random non-zero byte for the first position (class 0) or the last (class 1), chosen with a
/// mask: a branch on the class right before the timing would leave class-dependent state in the
/// branch predictor.
fn flips(class: bool, rng: &mut Rng) -> (u8, u8) {
    let flip = 1 | (rng.next() as u8);
    let mask = u8::from(class).wrapping_neg();
    (flip & !mask, flip & mask)
}

/// A guess that differs from [`SECRET`] in its first byte (class 0) or only in its last (class 1).
/// Both classes write both bytes in the same order: a class that wrote a different address just
/// before the timing would be measured differently by the store buffer, whatever the code under
/// test does.
fn guess(class: bool, rng: &mut Rng) -> [u8; 512] {
    let mut g = SECRET;
    let (first, last) = flips(class, rng);
    g[0] ^= first;
    g[511] ^= last;
    // Rewrite every byte so that the last stores before the timing are the same in both classes.
    g.map(black_box)
}

#[test]
fn an_early_exit_comparison_is_detected() {
    let t = min_t(20_000, 16, 1, guess, |g| {
        black_box(early_exit_eq(black_box(&SECRET), g));
    });
    assert!(t > THRESHOLD, "the leaky comparison was not detected: |t| = {t:.2}");
}

#[test]
fn tag_comparison_does_not_depend_on_where_the_tags_differ() {
    let t = min_t(20_000, 16, 2, guess, |g| {
        black_box(ct_eq(black_box(&SECRET), g));
    });
    assert!(t < THRESHOLD, "|t| = {t:.2}");
}

/// A copy of `sealed` with a wrong byte at the start (class 0) or the end (class 1) of the tag.
fn forge(sealed: &[u8], class: bool, rng: &mut Rng) -> Vec<u8> {
    let mut f = sealed.to_vec();
    let (first, last) = flips(class, rng);
    let n = f.len();
    f[n - 16] ^= first;
    f[n - 1] ^= last;
    f.into_iter().map(black_box).collect()
}

#[test]
fn aead_tag_checks_do_not_depend_on_the_wrong_tag() {
    let gcm = AesGcm::new(Aes::new(&[7; 16]).expect("key"));
    let sealed = gcm.seal(&[1; 12], b"aad", &[3; 64]);
    let t = min_t(
        4000,
        2,
        3,
        |c, r| forge(&sealed, c, r),
        |f| {
            black_box(gcm.open(&[1; 12], b"aad", f).is_err());
        },
    );
    assert!(t < THRESHOLD, "AES-GCM |t| = {t:.2}");
    let aead = ChaCha20Poly1305::new(&[9; 32]);
    let sealed = aead.seal(&[1; 12], b"aad", &[3; 64]);
    let t = min_t(
        4000,
        4,
        4,
        |c, r| forge(&sealed, c, r),
        |f| {
            black_box(aead.open(&[1; 12], b"aad", f).is_err());
        },
    );
    assert!(t < THRESHOLD, "ChaCha20-Poly1305 |t| = {t:.2}");
}

/// 32 zero bytes (class 0) or 32 random bytes (class 1).
fn secret_bytes<const N: usize>(class: bool, rng: &mut Rng) -> [u8; N] {
    let mut k = [0u8; N];
    if class {
        rng.fill(&mut k);
    }
    k
}

#[test]
fn aes_and_cbc_padding_checks_do_not_depend_on_the_key_or_padding() {
    let t = min_t(
        4000,
        8,
        5,
        |c, r| Aes::new(&secret_bytes::<32>(c, r)).expect("key"),
        |aes| {
            let mut block = [0u8; 16];
            aes.encrypt_block(black_box(&mut block));
            black_box(block);
        },
    );
    assert!(t < THRESHOLD, "AES |t| = {t:.2}");
    // Both classes have wrong padding, sixteen bytes of 16 but one: the first (class 0) or the
    // fifteenth (class 1). The result is an error either way; only its time could tell where.
    let aes = Aes::new(&[1; 16]).expect("key");
    let t = min_t(
        4000,
        4,
        6,
        |class, rng| {
            let mut last = [16u8; 16];
            let (first, other) = flips(class, rng);
            last[0] ^= first;
            last[14] ^= other;
            let mut c1 = [0x33u8; 16];
            aes.encrypt_block(&mut c1);
            let mut c2 = last;
            for (b, k) in c2.iter_mut().zip(&c1) {
                *b ^= k;
            }
            aes.encrypt_block(&mut c2);
            [c1, c2].concat().into_iter().map(black_box).collect::<Vec<u8>>()
        },
        |ct| {
            black_box(cbc_decrypt(&aes, &[0x33; 16], ct).is_err());
        },
    );
    assert!(t < THRESHOLD, "CBC padding |t| = {t:.2}");
}

#[test]
fn curve_operations_do_not_depend_on_the_secret_scalar() {
    let t = min_t(1500, 1, 7, secret_bytes::<32>, |k| {
        black_box(x25519(k, &[9; 32]));
    });
    assert!(t < THRESHOLD, "X25519 |t| = {t:.2}");
    let t = min_t(
        600,
        1,
        8,
        |c, r| Ed25519KeyPair::from_seed(&secret_bytes::<32>(c, r)),
        |kp| {
            black_box(kp.sign(b"m"));
        },
    );
    assert!(t < THRESHOLD, "Ed25519 |t| = {t:.2}");
    let t = min_t(
        300,
        1,
        9,
        |c, r| {
            let mut seed = secret_bytes::<40>(c, r);
            seed[0] |= 1;
            EcPrivateKey::from_random(NamedCurve::P256, &seed).expect("key")
        },
        |key| {
            black_box(key.sign_digest(HashAlgorithm::Sha256, &[5; 32]));
        },
    );
    assert!(t < THRESHOLD, "ECDSA P-256 |t| = {t:.2}");
}
