//! The NIST curves P-256, P-384 and P-521 (FIPS 186-5, SP 800-186): ECDH and ECDSA with deterministic
//! nonces (RFC 6979). Points use projective coordinates and the complete addition formulas for
//! a = −3 of Renes, Costello and Batina (EUROCRYPT 2016, algorithm 4), so every scalar
//! multiplication runs the same operations whatever the scalar.

use std::sync::OnceLock;

use crate::bigint::{self, Mont, add_assign, cmov, ct_is_zero, ct_lt, sub_assign};
use crate::ct::{is_zero64, mask64};
use crate::digest::HashAlgorithm;

const W: usize = 9;
type Limbs = [u64; W];

/// A supported curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedCurve {
    /// P-256 (secp256r1).
    P256,
    /// P-384 (secp384r1).
    P384,
    /// P-521 (secp521r1).
    P521,
}

struct Params {
    n: usize,
    /// Bits of the order q.
    bits: usize,
    bytes: usize,
    p: Mont,
    q: Mont,
    b: Limbs,
    gx: Limbs,
    gy: Limbs,
}

fn limbs(v: &[u64]) -> Limbs {
    let mut l = [0u64; W];
    l[..v.len()].copy_from_slice(v);
    l
}

fn params(curve: NamedCurve) -> &'static Params {
    static P256: OnceLock<Params> = OnceLock::new();
    static P384: OnceLock<Params> = OnceLock::new();
    static P521: OnceLock<Params> = OnceLock::new();
    match curve {
        NamedCurve::P256 => P256.get_or_init(|| {
            build(
                256,
                &[0xffffffffffffffff, 0xffffffff, 0x0, 0xffffffff00000001],
                &[0xf3b9cac2fc632551, 0xbce6faada7179e84, 0xffffffffffffffff, 0xffffffff00000000],
                &[0x3bce3c3e27d2604b, 0x651d06b0cc53b0f6, 0xb3ebbd55769886bc, 0x5ac635d8aa3a93e7],
                &[0xf4a13945d898c296, 0x77037d812deb33a0, 0xf8bce6e563a440f2, 0x6b17d1f2e12c4247],
                &[0xcbb6406837bf51f5, 0x2bce33576b315ece, 0x8ee7eb4a7c0f9e16, 0x4fe342e2fe1a7f9b],
            )
        }),
        NamedCurve::P384 => P384.get_or_init(|| {
            build(
                384,
                &[
                    0xffffffff,
                    0xffffffff00000000,
                    0xfffffffffffffffe,
                    0xffffffffffffffff,
                    0xffffffffffffffff,
                    0xffffffffffffffff,
                ],
                &[
                    0xecec196accc52973,
                    0x581a0db248b0a77a,
                    0xc7634d81f4372ddf,
                    0xffffffffffffffff,
                    0xffffffffffffffff,
                    0xffffffffffffffff,
                ],
                &[
                    0x2a85c8edd3ec2aef,
                    0xc656398d8a2ed19d,
                    0x0314088f5013875a,
                    0x181d9c6efe814112,
                    0x988e056be3f82d19,
                    0xb3312fa7e23ee7e4,
                ],
                &[
                    0x3a545e3872760ab7,
                    0x5502f25dbf55296c,
                    0x59f741e082542a38,
                    0x6e1d3b628ba79b98,
                    0x8eb1c71ef320ad74,
                    0xaa87ca22be8b0537,
                ],
                &[
                    0x7a431d7c90ea0e5f,
                    0x0a60b1ce1d7e819d,
                    0xe9da3113b5f0b8c0,
                    0xf8f41dbd289a147c,
                    0x5d9e98bf9292dc29,
                    0x3617de4a96262c6f,
                ],
            )
        }),
        NamedCurve::P521 => P521.get_or_init(|| {
            let ones = 0xffffffffffffffff;
            build(
                521,
                &[ones, ones, ones, ones, ones, ones, ones, ones, 0x1ff],
                &[
                    0xbb6fb71e91386409,
                    0x3bb5c9b8899c47ae,
                    0x7fcc0148f709a5d0,
                    0x51868783bf2f966b,
                    0xfffffffffffffffa,
                    ones,
                    ones,
                    ones,
                    0x1ff,
                ],
                &[
                    0xef451fd46b503f00,
                    0x3573df883d2c34f1,
                    0x1652c0bd3bb1bf07,
                    0x56193951ec7e937b,
                    0xb8b489918ef109e1,
                    0xa2da725b99b315f3,
                    0x929a21a0b68540ee,
                    0x953eb9618e1c9a1f,
                    0x51,
                ],
                &[
                    0xf97e7e31c2e5bd66,
                    0x3348b3c1856a429b,
                    0xfe1dc127a2ffa8de,
                    0xa14b5e77efe75928,
                    0xf828af606b4d3dba,
                    0x9c648139053fb521,
                    0x9e3ecb662395b442,
                    0x858e06b70404e9cd,
                    0xc6,
                ],
                &[
                    0x88be94769fd16650,
                    0x353c7086a272c240,
                    0xc550b9013fad0761,
                    0x97ee72995ef42640,
                    0x17afbd17273e662c,
                    0x98f54449579b4468,
                    0x5c8a5fb42c7d1bd9,
                    0x39296a789a3bc004,
                    0x118,
                ],
            )
        }),
    }
}

fn build(bits: usize, p: &[u64], q: &[u64], b: &[u64], gx: &[u64], gy: &[u64]) -> Params {
    let n = p.len();
    let pm = Mont::new(p);
    let mut params = Params {
        n,
        bits,
        bytes: bits.div_ceil(8),
        p: pm,
        q: Mont::new(q),
        b: [0; W],
        gx: [0; W],
        gy: [0; W],
    };
    params.b = params.to_mont(&limbs(b));
    params.gx = params.to_mont(&limbs(gx));
    params.gy = params.to_mont(&limbs(gy));
    params
}

impl Params {
    fn to_mont(&self, a: &Limbs) -> Limbs {
        let mut r = [0; W];
        self.p.to_mont(&a[..self.n], &mut r[..self.n]);
        r
    }
    fn mont_to_plain(&self, a: &Limbs) -> Limbs {
        let mut r = [0; W];
        self.p.mont_to_plain(&a[..self.n], &mut r[..self.n]);
        r
    }
    fn mul(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let mut r = [0; W];
        self.p.mul(&a[..self.n], &b[..self.n], &mut r[..self.n]);
        r
    }
    fn add(&self, a: &Limbs, b: &Limbs) -> Limbs {
        mod_add(&self.p.m, a, b)
    }
    fn sub(&self, a: &Limbs, b: &Limbs) -> Limbs {
        mod_sub(&self.p.m, a, b)
    }
    fn one(&self) -> Limbs {
        let mut r = [0; W];
        self.p.one(&mut r[..self.n]);
        r
    }
}

fn mod_add(m: &[u64], a: &Limbs, b: &Limbs) -> Limbs {
    let n = m.len();
    let mut r = *a;
    let carry = add_assign(&mut r[..n], &b[..n]);
    let mut t = r;
    let borrow = sub_assign(&mut t[..n], m);
    // Keep r − m unless the subtraction borrowed without a carry to absorb it.
    cmov(mask64(carry | (borrow ^ 1)), &mut r[..n], &t[..n]);
    r
}

fn mod_sub(m: &[u64], a: &Limbs, b: &Limbs) -> Limbs {
    let n = m.len();
    let mut r = *a;
    let borrow = sub_assign(&mut r[..n], &b[..n]);
    let mut t = r;
    add_assign(&mut t[..n], m);
    cmov(mask64(borrow), &mut r[..n], &t[..n]);
    r
}

#[derive(Clone, Copy)]
struct Point {
    x: Limbs,
    y: Limbs,
    z: Limbs,
}

impl Params {
    fn identity(&self) -> Point {
        Point { x: [0; W], y: self.one(), z: [0; W] }
    }

    fn add_points(&self, p: &Point, q: &Point) -> Point {
        let (x1, y1, z1, x2, y2, z2) = (&p.x, &p.y, &p.z, &q.x, &q.y, &q.z);
        let mut t0 = self.mul(x1, x2);
        let mut t1 = self.mul(y1, y2);
        let mut t2 = self.mul(z1, z2);
        let mut t3 = self.add(x1, y1);
        let mut t4 = self.add(x2, y2);
        t3 = self.mul(&t3, &t4);
        t4 = self.add(&t0, &t1);
        t3 = self.sub(&t3, &t4);
        t4 = self.add(y1, z1);
        let mut x3 = self.add(y2, z2);
        t4 = self.mul(&t4, &x3);
        x3 = self.add(&t1, &t2);
        t4 = self.sub(&t4, &x3);
        x3 = self.add(x1, z1);
        let mut y3 = self.add(x2, z2);
        x3 = self.mul(&x3, &y3);
        y3 = self.add(&t0, &t2);
        y3 = self.sub(&x3, &y3);
        let mut z3 = self.mul(&self.b, &t2);
        x3 = self.sub(&y3, &z3);
        z3 = self.add(&x3, &x3);
        x3 = self.add(&x3, &z3);
        z3 = self.sub(&t1, &x3);
        x3 = self.add(&t1, &x3);
        y3 = self.mul(&self.b, &y3);
        t1 = self.add(&t2, &t2);
        t2 = self.add(&t1, &t2);
        y3 = self.sub(&y3, &t2);
        y3 = self.sub(&y3, &t0);
        t1 = self.add(&y3, &y3);
        y3 = self.add(&t1, &y3);
        t1 = self.add(&t0, &t0);
        t0 = self.add(&t1, &t0);
        t0 = self.sub(&t0, &t2);
        t1 = self.mul(&t4, &y3);
        t2 = self.mul(&t0, &y3);
        y3 = self.mul(&x3, &z3);
        y3 = self.add(&y3, &t2);
        x3 = self.mul(&t3, &x3);
        x3 = self.sub(&x3, &t1);
        z3 = self.mul(&t4, &z3);
        t1 = self.mul(&t3, &t0);
        z3 = self.add(&z3, &t1);
        Point { x: x3, y: y3, z: z3 }
    }

    /// `k·p` for a big-endian scalar of `self.bytes` bytes, with a 4-bit window read by scanning.
    fn scalar_mul(&self, p: &Point, k: &[u8]) -> Point {
        let mut table = [self.identity(); 16];
        for i in 1..16 {
            table[i] = self.add_points(&table[i - 1], p);
        }
        let mut acc = self.identity();
        for &byte in k {
            for nibble in [byte >> 4, byte & 0xf] {
                for _ in 0..4 {
                    acc = self.add_points(&acc, &acc);
                }
                let mut pick = self.identity();
                for (j, e) in table.iter().enumerate() {
                    let m = is_zero64(u64::from(nibble) ^ j as u64);
                    cmov(m, &mut pick.x, &e.x);
                    cmov(m, &mut pick.y, &e.y);
                    cmov(m, &mut pick.z, &e.z);
                }
                acc = self.add_points(&acc, &pick);
            }
        }
        acc
    }

    /// Affine coordinates (plain, not Montgomery), or `None` for the point at infinity.
    fn to_affine(&self, p: &Point) -> Option<(Limbs, Limbs)> {
        if ct_is_zero(&p.z[..self.n]) != 0 {
            return None;
        }
        let mut zi = [0; W];
        self.p.invert_prime(&p.z[..self.n], &mut zi[..self.n]);
        Some((self.mont_to_plain(&self.mul(&p.x, &zi)), self.mont_to_plain(&self.mul(&p.y, &zi))))
    }

    fn generator(&self) -> Point {
        Point { x: self.gx, y: self.gy, z: self.one() }
    }

    /// Checks x, y < p and y² = x³ − 3x + b; returns the point in Montgomery form.
    fn point_from_affine(&self, x: &Limbs, y: &Limbs) -> Option<Point> {
        let n = self.n;
        if ct_lt(&x[..n], &self.p.m) == 0 || ct_lt(&y[..n], &self.p.m) == 0 {
            return None;
        }
        let (xm, ym) = (self.to_mont(x), self.to_mont(y));
        let lhs = self.mul(&ym, &ym);
        let x3 = self.mul(&self.mul(&xm, &xm), &xm);
        let three_x = self.add(&self.add(&xm, &xm), &xm);
        let rhs = self.add(&self.sub(&x3, &three_x), &self.b);
        if lhs[..n] != rhs[..n] {
            return None;
        }
        Some(Point { x: xm, y: ym, z: self.one() })
    }

    fn encode_point(&self, x: &Limbs, y: &Limbs) -> Vec<u8> {
        let mut out = vec![0u8; 1 + 2 * self.bytes];
        out[0] = 4;
        bigint::to_be_bytes(x, &mut out[1..=self.bytes]);
        bigint::to_be_bytes(y, &mut out[1 + self.bytes..]);
        out
    }

    fn decode_point(&self, enc: &[u8]) -> Option<Point> {
        if enc.len() != 1 + 2 * self.bytes || enc[0] != 4 {
            return None;
        }
        let (mut x, mut y) = ([0u64; W], [0u64; W]);
        bigint::from_be_bytes(&enc[1..=self.bytes], &mut x);
        bigint::from_be_bytes(&enc[1 + self.bytes..], &mut y);
        self.point_from_affine(&x, &y)
    }

    /// A scalar in [1, q−1] from big-endian bytes.
    fn scalar(&self, bytes: &[u8]) -> Option<Limbs> {
        let mut s = [0u64; W];
        if !bigint::from_be_bytes(bytes, &mut s[..self.n]) {
            return None;
        }
        if ct_is_zero(&s[..self.n]) != 0 || ct_lt(&s[..self.n], &self.q.m) == 0 {
            return None;
        }
        Some(s)
    }

    fn scalar_bytes(&self, s: &Limbs) -> Vec<u8> {
        let mut out = vec![0u8; self.bytes];
        bigint::to_be_bytes(&s[..self.n], &mut out);
        out
    }

    /// The leftmost bits of a hash as an integer modulo q (FIPS 186-5, 6.4.1 and RFC 6979 bits2int).
    fn bits2int_mod_q(&self, h: &[u8]) -> Limbs {
        let take = h.len().min(self.bytes);
        let mut e = [0u64; W];
        bigint::from_be_bytes(&h[..take], &mut e[..self.n]);
        let mut r = [0u64; W];
        bigint::reduce(&e[..self.n], &self.q.m, &mut r[..self.n]);
        r
    }

    fn q_mul(&self, a: &Limbs, b: &Limbs) -> Limbs {
        // a·b mod q via Montgomery: (a·R)·b·R⁻¹.
        let n = self.n;
        let (mut am, mut r) = ([0; W], [0; W]);
        self.q.to_mont(&a[..n], &mut am[..n]);
        self.q.mul(&am[..n], &b[..n], &mut r[..n]);
        r
    }

    fn q_inv(&self, a: &Limbs) -> Limbs {
        let n = self.n;
        let (mut am, mut inv, mut r) = ([0; W], [0; W], [0; W]);
        self.q.to_mont(&a[..n], &mut am[..n]);
        self.q.invert_prime(&am[..n], &mut inv[..n]);
        self.q.mont_to_plain(&inv[..n], &mut r[..n]);
        r
    }
}

impl NamedCurve {
    /// Length of a scalar or coordinate in bytes.
    pub fn scalar_len(self) -> usize {
        params(self).bytes
    }

    /// The hash that goes with the curve in TLS and X.509 (SHA-256, SHA-384 or SHA-512).
    pub fn hash(self) -> HashAlgorithm {
        match self {
            NamedCurve::P256 => HashAlgorithm::Sha256,
            NamedCurve::P384 => HashAlgorithm::Sha384,
            NamedCurve::P521 => HashAlgorithm::Sha512,
        }
    }
}

/// Error for a private scalar outside [1, n−1], a point not on the curve or a result at infinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidKey;

/// An ECDSA and ECDH private key.
#[derive(Clone)]
pub struct EcPrivateKey {
    curve: NamedCurve,
    d: Limbs,
}

impl std::fmt::Debug for EcPrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EcPrivateKey({:?})", self.curve)
    }
}

impl Drop for EcPrivateKey {
    fn drop(&mut self) {
        self.d = [0; W];
        std::hint::black_box(&self.d);
    }
}

/// Random bytes [`EcPrivateKey::p256_from_random`] takes: the scalar's 32 and 8 more.
pub const P256_RANDOM_LEN: usize = 40;

impl EcPrivateKey {
    /// Reads the big-endian private scalar.
    pub fn from_bytes(curve: NamedCurve, d: &[u8]) -> Result<EcPrivateKey, InvalidKey> {
        params(curve).scalar(d).map(|d| EcPrivateKey { curve, d }).ok_or(InvalidKey)
    }

    /// Derives a key from at least `scalar_len + 8` random bytes (FIPS 186-5, A.2.1): the bytes
    /// reduced modulo n − 1, plus one.
    pub fn from_random(curve: NamedCurve, random: &[u8]) -> Result<EcPrivateKey, InvalidKey> {
        let p = params(curve);
        if random.len() < p.bytes + 8 || random.len() > 2 * p.bytes {
            return Err(InvalidKey);
        }
        Ok(EcPrivateKey::reduce_random(curve, random))
    }

    /// [`EcPrivateKey::from_random`] for P-256 from exactly [`P256_RANDOM_LEN`] bytes, a length
    /// the type already checks.
    pub fn p256_from_random(random: &[u8; P256_RANDOM_LEN]) -> EcPrivateKey {
        EcPrivateKey::reduce_random(NamedCurve::P256, random)
    }

    /// The reduction of [`EcPrivateKey::from_random`], for a length already checked.
    fn reduce_random(curve: NamedCurve, random: &[u8]) -> EcPrivateKey {
        let p = params(curve);
        let mut wide = [0u64; 2 * W];
        bigint::from_be_bytes(random, &mut wide);
        let mut nm1 = [0u64; W];
        nm1[..p.n].copy_from_slice(&p.q.m);
        sub_assign(&mut nm1[..p.n], &[1]);
        let mut d = [0u64; W];
        bigint::reduce(&wide, &nm1[..p.n], &mut d[..p.n]);
        add_assign(&mut d[..p.n], &[1]);
        EcPrivateKey { curve, d }
    }

    /// The curve.
    pub fn curve(&self) -> NamedCurve {
        self.curve
    }

    /// The private scalar, big-endian.
    pub fn to_bytes(&self) -> Vec<u8> {
        params(self.curve).scalar_bytes(&self.d)
    }

    /// The public key as an uncompressed point (`04 ‖ x ‖ y`).
    pub fn public_key(&self) -> Vec<u8> {
        let p = params(self.curve);
        let q = p.scalar_mul(&p.generator(), &p.scalar_bytes(&self.d));
        // d ∈ [1, n−1], so d·G is never the point at infinity.
        let (x, y) = p.to_affine(&q).unwrap_or(([0; W], [0; W]));
        p.encode_point(&x, &y)
    }

    /// ECDH: the x coordinate of `d·peer` (SP 800-56A), checking that the peer point is valid.
    pub fn agree(&self, peer: &[u8]) -> Result<Vec<u8>, InvalidKey> {
        let p = params(self.curve);
        let point = p.decode_point(peer).ok_or(InvalidKey)?;
        let s = p.scalar_mul(&point, &p.scalar_bytes(&self.d));
        let (x, _) = p.to_affine(&s).ok_or(InvalidKey)?;
        Ok(p.scalar_bytes(&x))
    }

    /// Deterministic ECDSA (RFC 6979) of a message digest; returns `r ‖ s`.
    pub fn sign_digest(&self, hash: HashAlgorithm, digest: &[u8]) -> Vec<u8> {
        let p = params(self.curve);
        let x = p.scalar_bytes(&self.d);
        let e = p.bits2int_mod_q(digest);
        let h1 = p.scalar_bytes(&e);
        let mut k_gen = Rfc6979::new(hash, &x, &h1);
        loop {
            let k_bytes = k_gen.next(p);
            let Some(k) = p.scalar(&k_bytes) else { continue };
            let Some((rx, _)) = p.to_affine(&p.scalar_mul(&p.generator(), &k_bytes)) else {
                continue;
            };
            let mut r = [0u64; W];
            bigint::reduce(&rx[..p.n], &p.q.m, &mut r[..p.n]);
            if ct_is_zero(&r[..p.n]) != 0 {
                continue;
            }
            // s = k⁻¹ (e + r·d) mod n
            let rd = p.q_mul(&r, &self.d);
            let sum = mod_add(&p.q.m, &e, &rd);
            let s = p.q_mul(&p.q_inv(&k), &sum);
            if ct_is_zero(&s[..p.n]) != 0 {
                continue;
            }
            let mut out = p.scalar_bytes(&r);
            out.extend_from_slice(&p.scalar_bytes(&s));
            return out;
        }
    }

    /// Hashes `msg` with the curve's hash and signs it; returns `r ‖ s`.
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        let hash = self.curve.hash();
        self.sign_digest(hash, hash.hash(msg).as_ref())
    }
}

/// RFC 6979, section 3.2: HMAC_DRBG keyed with the private key and the message hash.
struct Rfc6979 {
    hash: HashAlgorithm,
    k: Vec<u8>,
    v: Vec<u8>,
}

impl Rfc6979 {
    fn new(hash: HashAlgorithm, x: &[u8], h1: &[u8]) -> Rfc6979 {
        let len = hash.output_len();
        let mut g = Rfc6979 { hash, k: vec![0; len], v: vec![1; len] };
        for sep in [0u8, 1] {
            g.k = g.mac(&[&g.v, &[sep], x, h1]);
            g.v = g.mac(&[&g.v]);
        }
        g
    }

    fn mac(&self, parts: &[&[u8]]) -> Vec<u8> {
        self.hash.hmac_parts(&self.k, parts).as_ref().to_vec()
    }

    fn next(&mut self, p: &Params) -> Vec<u8> {
        let mut t = Vec::with_capacity(p.bytes);
        while t.len() < p.bytes {
            self.v = self.mac(&[&self.v]);
            t.extend_from_slice(&self.v);
        }
        t.truncate(p.bytes);
        // bits2int keeps the leftmost qlen bits (RFC 6979, 2.3.2): P-521 drops the low 7.
        shift_right(&mut t, 8 * p.bytes - p.bits);
        // Prepares the next candidate in case this one is rejected (RFC 6979, 3.2 step h.3).
        self.k = self.mac(&[&self.v, &[0]]);
        self.v = self.mac(&[&self.v]);
        t
    }
}

/// Shifts a big-endian byte string right by `bits` (fewer than 8).
fn shift_right(t: &mut [u8], bits: usize) {
    if bits == 0 {
        return;
    }
    for i in (0..t.len()).rev() {
        let high = if i == 0 { 0 } else { t[i - 1] << (8 - bits) };
        t[i] = (t[i] >> bits) | high;
    }
}

/// Verifies an ECDSA signature `r ‖ s` over a message digest with an uncompressed public key.
pub fn ecdsa_verify_digest(curve: NamedCurve, public: &[u8], digest: &[u8], sig: &[u8]) -> bool {
    let p = params(curve);
    if sig.len() != 2 * p.bytes {
        return false;
    }
    let (Some(r), Some(s), Some(q)) =
        (p.scalar(&sig[..p.bytes]), p.scalar(&sig[p.bytes..]), p.decode_point(public))
    else {
        return false;
    };
    let e = p.bits2int_mod_q(digest);
    let w = p.q_inv(&s);
    let u1 = p.q_mul(&e, &w);
    let u2 = p.q_mul(&r, &w);
    let a = p.scalar_mul(&p.generator(), &p.scalar_bytes(&u1));
    let b = p.scalar_mul(&q, &p.scalar_bytes(&u2));
    let Some((x, _)) = p.to_affine(&p.add_points(&a, &b)) else {
        return false;
    };
    let mut v = [0u64; W];
    bigint::reduce(&x[..p.n], &p.q.m, &mut v[..p.n]);
    v[..p.n] == r[..p.n]
}

/// Hashes `msg` with `hash` and verifies the signature `r ‖ s`.
pub fn ecdsa_verify(
    curve: NamedCurve,
    public: &[u8],
    hash: HashAlgorithm,
    msg: &[u8],
    sig: &[u8],
) -> bool {
    ecdsa_verify_digest(curve, public, hash.hash(msg).as_ref(), sig)
}

/// Converts a DER `Ecdsa-Sig-Value` (SEQUENCE of two INTEGERs, RFC 3279) to `r ‖ s`, accepting
/// only strict DER: minimal lengths, no superfluous leading zeros and positive values.
pub fn ecdsa_sig_from_der(curve: NamedCurve, der: &[u8]) -> Option<Vec<u8>> {
    let len = params(curve).bytes;
    let body = der_take(der, 0x30)?;
    if !body.1.is_empty() {
        return None;
    }
    let (r, rest) = der_take(body.0, 0x02)?;
    let (s, rest) = der_take(rest, 0x02)?;
    if !rest.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(2 * len);
    for int in [r, s] {
        let v = der_uint(int)?;
        if v.len() > len {
            return None;
        }
        out.extend(std::iter::repeat_n(0, len - v.len()));
        out.extend_from_slice(v);
    }
    Some(out)
}

/// Converts `r ‖ s` to DER.
pub fn ecdsa_sig_to_der(sig: &[u8]) -> Vec<u8> {
    let half = sig.len() / 2;
    let mut body = Vec::new();
    for part in [&sig[..half], &sig[half..]] {
        let start = part.iter().position(|&b| b != 0).unwrap_or(part.len() - 1);
        let v = &part[start..];
        let pad = v[0] & 0x80 != 0;
        body.push(0x02);
        der_push_len(&mut body, v.len() + usize::from(pad));
        if pad {
            body.push(0);
        }
        body.extend_from_slice(v);
    }
    let mut out = vec![0x30];
    der_push_len(&mut out, body.len());
    out.extend_from_slice(&body);
    out
}

/// Appends a DER length: an ECDSA signature on P-521 is at most 139 bytes, so one length byte
/// after 0x81 is always enough.
fn der_push_len(out: &mut Vec<u8>, len: usize) {
    debug_assert!(len < 0x100, "DER length {len}");
    if len >= 0x80 {
        out.push(0x81);
    }
    out.push(len as u8);
}

/// One TLV with `tag` and a minimal length (short form, or 0x81 for 128 to 255): returns
/// (contents, rest).
fn der_take(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (len, rest) = match input {
        [t, len, rest @ ..] if *t == tag && *len < 0x80 => (usize::from(*len), rest),
        [t, 0x81, len, rest @ ..] if *t == tag && *len >= 0x80 => (usize::from(*len), rest),
        _ => return None,
    };
    (rest.len() >= len).then(|| rest.split_at(len))
}

/// The magnitude of a positive minimal DER INTEGER.
fn der_uint(v: &[u8]) -> Option<&[u8]> {
    match v {
        [] => None,
        [b, ..] if b & 0x80 != 0 => None,
        [0, next, ..] if next & 0x80 == 0 => None,
        [0, rest @ ..] if !rest.is_empty() => Some(rest),
        _ => Some(v),
    }
}
