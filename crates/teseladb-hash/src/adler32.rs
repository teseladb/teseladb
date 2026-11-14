//! Adler-32 (RFC 1950), the checksum of zlib streams.

pub(crate) const MOD: u32 = 65_521;
/// Maximum number of bytes that can be summed before `b` overflows a u32 and must be reduced (RFC 1950).
pub(crate) const NMAX: usize = 5_552;

/// Packs the two sums, already reduced modulo [`MOD`], into the checksum.
#[expect(
    clippy::cast_possible_truncation,
    reason = "both sums are below MOD < 2^16, so the packed value fits 32 bits"
)]
pub(crate) fn pack(a: u64, b: u64) -> u32 {
    ((b << 16) | a) as u32
}

/// Continues an Adler-32 with the portable implementation.
#[must_use]
pub fn adler32_portable(adler: u32, data: &[u8]) -> u32 {
    let mut a = adler & 0xffff;
    let mut b = adler >> 16;
    for chunk in data.chunks(NMAX) {
        let (blocks, rest) = chunk.as_chunks::<8>();
        for blk in blocks {
            for &x in blk {
                a += u32::from(x);
                b += a;
            }
        }
        for &x in rest {
            a += u32::from(x);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Adler-32 of `data` (initial value 1), vectorized when the CPU allows it.
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    crate::arch::adler32_update(1, data)
}

/// Adler-32 incremental.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adler32 {
    value: u32,
}

impl Default for Adler32 {
    fn default() -> Self {
        Adler32::new()
    }
}

impl Adler32 {
    /// Initial value (1).
    #[must_use]
    pub fn new() -> Adler32 {
        Adler32 { value: 1 }
    }

    /// Adds data.
    pub fn update(&mut self, data: &[u8]) {
        self.value = crate::arch::adler32_update(self.value, data);
    }

    /// Current value.
    #[must_use]
    pub fn value(&self) -> u32 {
        self.value
    }
}
