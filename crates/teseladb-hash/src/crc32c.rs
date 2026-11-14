//! CRC32C (Castagnoli, reflected polynomial `0x82F63B78`): portable with slicing-by-8 and
//! accelerated with the `ARMv8` or SSE4.2 CRC32 instructions.

const POLY: u32 = 0x82f6_3b78;

/// Slicing-by-8 tables, computed at compile time.
static TABLES: [[u32; 256]; 8] = build_tables();

const fn build_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        #[expect(clippy::cast_possible_truncation, reason = "i is below 256")]
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut n = 1;
    while n < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[n - 1][i];
            t[n][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            i += 1;
        }
        n += 1;
    }
    t
}

/// Continues a "raw" CRC32C (without the initial and final inversions) with `data`.
pub(crate) fn update_portable(mut crc: u32, data: &[u8]) -> u32 {
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let lo = crc ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        crc = TABLES[7][(lo & 0xff) as usize]
            ^ TABLES[6][((lo >> 8) & 0xff) as usize]
            ^ TABLES[5][((lo >> 16) & 0xff) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][c[4] as usize]
            ^ TABLES[2][c[5] as usize]
            ^ TABLES[1][c[6] as usize]
            ^ TABLES[0][c[7] as usize];
    }
    for &b in rest {
        crc = (crc >> 8) ^ TABLES[0][((crc ^ u32::from(b)) & 0xff) as usize];
    }
    crc
}

/// CRC32C of `data` with the portable implementation.
#[must_use]
pub fn crc32c_portable(data: &[u8]) -> u32 {
    !update_portable(!0, data)
}

/// Continues the CRC32C `crc` (a value previously returned by [`crc32c`] or this function) with `data`.
/// `crc32c_append(crc32c(a), b) == crc32c(a ++ b)`.
#[must_use]
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    !crate::arch::crc32c_update(!crc, data)
}

/// CRC32C of `data`, hardware-accelerated when available.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// CRC32C incremental.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crc32c {
    crc: u32,
}

impl Crc32c {
    /// Empty CRC.
    #[must_use]
    pub fn new() -> Crc32c {
        Crc32c::default()
    }

    /// Adds data.
    pub fn update(&mut self, data: &[u8]) {
        self.crc = crc32c_append(self.crc, data);
    }

    /// Current value.
    #[must_use]
    pub fn value(&self) -> u32 {
        self.crc
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn tables_match_runtime_construction() {
        // `TABLES` is evaluated at compile time; here it is recomputed at run time and compared.
        assert_eq!(build_tables(), TABLES);
    }

    #[test]
    fn table_spot_values() {
        assert_eq!(TABLES[0][1], 0xf26b_8303);
        assert_eq!(TABLES[0][255], 0xad7d_5351);
    }

    #[test]
    fn short_tail_paths() {
        for n in 0u8..20 {
            let v: Vec<u8> = (0..n).collect();
            assert_eq!(crc32c_portable(&v), crc32c(&v), "length {n}");
        }
        let mut c = Crc32c::new();
        c.update(b"1234");
        c.update(b"56789");
        assert_eq!(c.value(), 0xe306_9283);
    }
}
