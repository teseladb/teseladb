//! CRC-32 IEEE 802.3 (reflected polynomial `0xEDB88320`), the one of gzip (RFC 1952), zip and PNG:
//! portable with slicing-by-8.

const POLY: u32 = 0xedb8_8320;

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

fn update(mut crc: u32, data: &[u8]) -> u32 {
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

/// Continues the CRC-32 `crc` (a value previously returned by [`crc32`] or this function) with `data`.
/// `crc32_append(crc32(a), b) == crc32(a ++ b)`.
#[must_use]
pub fn crc32_append(crc: u32, data: &[u8]) -> u32 {
    !update(!crc, data)
}

/// CRC-32 IEEE of `data`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    crc32_append(0, data)
}

/// CRC-32 IEEE incremental.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Crc32 {
    crc: u32,
}

impl Crc32 {
    /// Empty CRC.
    #[must_use]
    pub fn new() -> Crc32 {
        Crc32::default()
    }

    /// Adds data.
    pub fn update(&mut self, data: &[u8]) {
        self.crc = crc32_append(self.crc, data);
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
    fn known_values_and_append() {
        // Standard check values of CRC-32/ISO-HDLC.
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414f_a339);
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 + 7).to_le_bytes()[0]).collect();
        for cut in [0, 1, 7, 8, 9, 500, 1000] {
            assert_eq!(crc32_append(crc32(&data[..cut]), &data[cut..]), crc32(&data));
        }
        let mut c = Crc32::new();
        c.update(&data[..13]);
        c.update(&data[13..]);
        assert_eq!(c.value(), crc32(&data));
    }
}
