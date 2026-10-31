//! X25519 (RFC 7748) and Ed25519 (RFC 8032).

use crate::bigint;
use crate::ct::ct_eq;
use crate::field25519::Fe;
use crate::sha512::Sha512;

/// X25519: the scalar multiplication `k·u` with the Montgomery ladder (RFC 7748, section 5).
pub fn x25519(scalar: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    let x1 = Fe::from_bytes(u);
    let (mut x2, mut z2, mut x3, mut z3) = (Fe::ONE, Fe::ZERO, x1, Fe::ONE);
    let mut swap = 0u64;
    for t in (0..255).rev() {
        let bit = u64::from((k[t / 8] >> (t % 8)) & 1);
        swap ^= bit;
        Fe::cswap(swap, &mut x2, &mut x3);
        Fe::cswap(swap, &mut z2, &mut z3);
        swap = bit;
        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul_small(121_665)));
    }
    Fe::cswap(swap, &mut x2, &mut x3);
    Fe::cswap(swap, &mut z2, &mut z3);
    x2.mul(z2.invert()).to_bytes()
}

/// The X25519 public key of a private scalar: `k·9`.
pub fn x25519_base(scalar: &[u8; 32]) -> [u8; 32] {
    let mut nine = [0u8; 32];
    nine[0] = 9;
    x25519(scalar, &nine)
}

/// Error of an X25519 agreement whose result is all zeros (a small-order peer key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeakPeerKey;

/// X25519 shared secret, rejecting the all-zero result as TLS 1.3 requires (RFC 8446, 7.4.2).
pub fn x25519_agree(scalar: &[u8; 32], peer: &[u8; 32]) -> Result<[u8; 32], WeakPeerKey> {
    let s = x25519(scalar, peer);
    if ct_eq(&s, &[0; 32]) { Err(WeakPeerKey) } else { Ok(s) }
}

const D: Fe =
    Fe([0x34dca135978a3, 0x1a8283b156ebd, 0x5e7a26001c029, 0x739c663a03cbb, 0x52036cee2b6ff]);
const D2: Fe =
    Fe([0x69b9426b2f159, 0x35050762add7a, 0x3cf44c0038052, 0x6738cc7407977, 0x2406d9dc56dff]);
const SQRT_M1: Fe =
    Fe([0x61b274a0ea0b0, 0xd5a5fc8f189d, 0x7ef5e9cbd0c60, 0x78595a6804c9e, 0x2b8324804fc1d]);
const BASE: Point = Point {
    x: Fe([0x62d608f25d51a, 0x412a4b4f6592a, 0x75b7171a4b31d, 0x1ff60527118fe, 0x216936d3cd6e5]),
    y: Fe([0x6666666666658, 0x4cccccccccccc, 0x1999999999999, 0x3333333333333, 0x6666666666666]),
    z: Fe::ONE,
    t: Fe([0x68ab3a5b7dda3, 0xeea2a5eadbb, 0x2af8df483c27e, 0x332b375274732, 0x67875f0fd78b7]),
};
/// The group order L = 2²⁵² + 27742317777372353535851937790883648493, little-endian limbs.
const L: [u64; 4] = [0x5812631a5cf5d3ed, 0x14def9dea2f79cd6, 0, 0x1000000000000000];

/// A point in extended twisted Edwards coordinates (x = X/Z, y = Y/Z, xy = T/Z).
#[derive(Clone, Copy, Debug)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl Point {
    const IDENTITY: Point = Point { x: Fe::ZERO, y: Fe::ONE, z: Fe::ONE, t: Fe::ZERO };

    /// Complete addition (Hisil–Wong–Carter–Dawson, a = −1).
    fn add(&self, o: &Point) -> Point {
        let a = self.y.sub(self.x).mul(o.y.sub(o.x));
        let b = self.y.add(self.x).mul(o.y.add(o.x));
        let c = self.t.mul(D2).mul(o.t);
        let d = self.z.mul(o.z).mul_small(2);
        let (e, f, g, h) = (b.sub(a), d.sub(c), d.add(c), b.add(a));
        Point { x: e.mul(f), y: g.mul(h), z: f.mul(g), t: e.mul(h) }
    }

    fn double(&self) -> Point {
        self.add(self)
    }

    fn neg(&self) -> Point {
        Point { x: self.x.neg(), y: self.y, z: self.z, t: self.t.neg() }
    }

    fn select(mask: u64, a: &Point, b: &Point) -> Point {
        Point {
            x: Fe::select(mask, a.x, b.x),
            y: Fe::select(mask, a.y, b.y),
            z: Fe::select(mask, a.z, b.z),
            t: Fe::select(mask, a.t, b.t),
        }
    }

    /// `k·self` for a 32-byte little-endian scalar, always doing the same operations.
    fn mul(&self, k: &[u8; 32]) -> Point {
        let mut table = [Point::IDENTITY; 16];
        for i in 1..16 {
            table[i] = table[i - 1].add(self);
        }
        let mut acc = Point::IDENTITY;
        for i in (0..64).rev() {
            for _ in 0..4 {
                acc = acc.double();
            }
            let w = u64::from((k[i / 2] >> (4 * (i % 2))) & 0xf);
            let mut pick = Point::IDENTITY;
            for (j, p) in table.iter().enumerate() {
                pick = Point::select(crate::ct::is_zero64(w ^ j as u64), p, &pick);
            }
            acc = acc.add(&pick);
        }
        acc
    }

    fn encode(&self) -> [u8; 32] {
        let zi = self.z.invert();
        let x = self.x.mul(zi);
        let mut out = self.y.mul(zi).to_bytes();
        out[31] |= (x.is_negative() as u8) << 7;
        out
    }

    /// Decodes a point (RFC 8032, 5.1.3), rejecting y ≥ p and x = 0 with the sign bit set.
    fn decode(b: &[u8; 32]) -> Option<Point> {
        let sign = u64::from(b[31] >> 7);
        let y = Fe::from_bytes(b);
        let mut canon = *b;
        canon[31] &= 0x7f;
        if y.to_bytes() != canon {
            return None;
        }
        let y2 = y.square();
        let u = y2.sub(Fe::ONE);
        let v = y2.mul(D).add(Fe::ONE);
        let v3 = v.square().mul(v);
        let v7 = v3.square().mul(v);
        let mut x = u.mul(v3).mul(u.mul(v7).pow_p58());
        let vx2 = v.mul(x.square());
        if vx2.ct_eq_mask(u) == 0 {
            if vx2.ct_eq_mask(u.neg()) == 0 {
                return None;
            }
            x = x.mul(SQRT_M1);
        }
        if x.is_zero_mask() != 0 && sign == 1 {
            return None;
        }
        if x.is_negative() != sign {
            x = x.neg();
        }
        Some(Point { x, y, z: Fe::ONE, t: x.mul(y) })
    }
}

fn scalar_reduce(wide: &[u8]) -> [u8; 32] {
    let mut limbs = [0u64; 8];
    for (i, b) in wide.iter().enumerate() {
        limbs[i / 8] |= u64::from(*b) << (8 * (i % 8));
    }
    let mut r = [0u64; 4];
    bigint::reduce(&limbs, &L, &mut r);
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (r[i / 8] >> (8 * (i % 8))) as u8;
    }
    out
}

/// (a·b + c) mod L for little-endian 32-byte scalars.
fn scalar_muladd(a: &[u8; 32], b: &[u8; 32], c: &[u8; 32]) -> [u8; 32] {
    let load = |s: &[u8; 32]| {
        let mut l = [0u64; 4];
        for (i, x) in s.iter().enumerate() {
            l[i / 8] |= u64::from(*x) << (8 * (i % 8));
        }
        l
    };
    let (a, b, c) = (load(a), load(b), load(c));
    let mut prod = [0u64; 9];
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let s = u128::from(prod[i + j]) + u128::from(a[i]) * u128::from(b[j]) + carry;
            prod[i + j] = s as u64;
            carry = s >> 64;
        }
        prod[i + 4] = carry as u64;
    }
    bigint::add_assign(&mut prod, &c);
    let mut r = [0u64; 4];
    bigint::reduce(&prod, &L, &mut r);
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (r[i / 8] >> (8 * (i % 8))) as u8;
    }
    out
}

fn sha512_parts(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize()
}

/// An Ed25519 key pair derived from a 32-byte seed.
#[derive(Clone)]
pub struct Ed25519KeyPair {
    scalar: [u8; 32],
    prefix: [u8; 32],
    public: [u8; 32],
}

impl std::fmt::Debug for Ed25519KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ed25519KeyPair(public {:02x?})", &self.public[..4])
    }
}

impl Drop for Ed25519KeyPair {
    fn drop(&mut self) {
        self.scalar = [0; 32];
        self.prefix = [0; 32];
        std::hint::black_box((&self.scalar, &self.prefix));
    }
}

impl Ed25519KeyPair {
    /// Expands the private key seed (RFC 8032, 5.1.5).
    pub fn from_seed(seed: &[u8; 32]) -> Ed25519KeyPair {
        let h = sha512_parts(&[seed]);
        let mut scalar = [0u8; 32];
        scalar.copy_from_slice(&h[..32]);
        scalar[0] &= 248;
        scalar[31] &= 127;
        scalar[31] |= 64;
        let mut prefix = [0u8; 32];
        prefix.copy_from_slice(&h[32..]);
        let public = BASE.mul(&scalar).encode();
        Ed25519KeyPair { scalar, prefix, public }
    }

    /// The 32-byte public key.
    pub fn public_key(&self) -> [u8; 32] {
        self.public
    }

    /// Signs `msg` (RFC 8032, 5.1.6).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let r = scalar_reduce(&sha512_parts(&[&self.prefix, msg]));
        let big_r = BASE.mul(&r).encode();
        let k = scalar_reduce(&sha512_parts(&[&big_r, &self.public, msg]));
        let s = scalar_muladd(&k, &self.scalar, &r);
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&big_r);
        sig[32..].copy_from_slice(&s);
        sig
    }
}

/// Verifies an Ed25519 signature with the cofactorless equation `S·B = R + k·A` (RFC 8032,
/// 5.1.7), rejecting S ≥ L and non-canonical point encodings.
pub fn ed25519_verify(public: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let (Ok(public), Ok(sig)) = (<&[u8; 32]>::try_from(public), <&[u8; 64]>::try_from(sig)) else {
        return false;
    };
    let Some(a) = Point::decode(public) else {
        return false;
    };
    let mut r_bytes = [0u8; 32];
    r_bytes.copy_from_slice(&sig[..32]);
    let mut s = [0u8; 32];
    s.copy_from_slice(&sig[32..]);
    let mut s_limbs = [0u64; 4];
    for (i, x) in s.iter().enumerate() {
        s_limbs[i / 8] |= u64::from(*x) << (8 * (i % 8));
    }
    if bigint::ct_lt(&s_limbs, &L) == 0 {
        return false;
    }
    if Point::decode(&r_bytes).is_none() {
        return false;
    }
    let k = scalar_reduce(&sha512_parts(&[&r_bytes, public, msg]));
    let check = BASE.mul(&s).add(&a.neg().mul(&k));
    check.encode() == r_bytes
}
