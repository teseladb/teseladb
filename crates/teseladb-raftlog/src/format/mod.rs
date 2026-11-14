//! On-disk formats (data-model). All little-endian; all checksums CRC32C.
//!
//! Every encoder has its decoder, which never panics: on invalid data it returns
//! [`FormatError`].

pub mod meta;
pub mod snapshot;
pub mod wal;

use core::fmt;

use teseladb_hash::crc32c;

/// Block size.
pub const BLOCK: usize = 4096;
/// Format version of every file of this version.
pub const FORMAT_VERSION: u32 = 1;
/// Padding byte of batch regions up to the block boundary. It is deliberately non-zero: a complete
/// region never contains whole zero sectors because of its padding, and a zero sector can only come
/// from a write that did not reach the disk (broken-tail rule, plan step 3).
pub const PAD_BYTE: u8 = 0x5a;

/// Error decoding an on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// Missing bytes.
    Truncated,
    /// Wrong `magic`.
    BadMagic,
    /// The checksum does not match.
    BadCrc,
    /// Unknown version.
    Version(u32),
    /// A field has an impossible value.
    Invalid(&'static str),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Truncated => f.write_str("truncated data"),
            FormatError::BadMagic => f.write_str("wrong magic"),
            FormatError::BadCrc => f.write_str("wrong checksum"),
            FormatError::Version(v) => write!(f, "unknown version {v}"),
            FormatError::Invalid(what) => write!(f, "invalid field: {what}"),
        }
    }
}

impl std::error::Error for FormatError {}

impl From<teseladb_types::DecodeError> for FormatError {
    fn from(_: teseladb_types::DecodeError) -> Self {
        FormatError::Truncated
    }
}

/// File type according to its `magic`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// WAL segment.
    Wal,
    /// Group snapshot.
    Snapshot,
}

impl FileKind {
    /// 8-byte `magic`.
    #[must_use]
    pub const fn magic(self) -> &'static [u8; 8] {
        match self {
            FileKind::Wal => b"TSLWAL01",
            FileKind::Snapshot => b"TSLSNP01",
        }
    }

    fn from_magic(m: &[u8]) -> Option<FileKind> {
        [FileKind::Wal, FileKind::Snapshot].into_iter().find(|k| k.magic() == m)
    }
}

/// Common 4096-byte header (data-model §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHeader {
    /// File type.
    pub kind: FileKind,
    /// Core.
    pub core_id: u32,
    /// Sequence number of the file.
    pub seq: u64,
    /// Node that wrote it.
    pub node_id: u64,
    /// Creation time (ms since the Unix epoch).
    pub created_unix_ms: u64,
    /// Reserved area from offset 40 (used by 006's encryption at rest).
    pub reserved: Vec<u8>,
}

/// Length of the common header's reserved area.
pub const HEADER_RESERVED: usize = BLOCK - 40 - 4;

impl FileHeader {
    /// Header without data in the reserved area.
    #[must_use]
    pub fn new(kind: FileKind, core_id: u32, seq: u64, node_id: u64, created_unix_ms: u64) -> Self {
        FileHeader { kind, core_id, seq, node_id, created_unix_ms, reserved: Vec::new() }
    }

    /// The data key wrapped with the master key at the start of the reserved area, in a file
    /// encrypted at rest; the reserved area is kept without its trailing zeros, so
    /// the key is padded back to its length.
    #[must_use]
    pub fn wrapped_key(&self) -> Option<[u8; teseladb_crypto::at_rest::WRAPPED_KEY_LEN]> {
        let mut w = [0u8; teseladb_crypto::at_rest::WRAPPED_KEY_LEN];
        let n = self.reserved.len().min(w.len());
        w[..n].copy_from_slice(&self.reserved[..n]);
        w.iter().any(|b| *b != 0).then_some(w)
    }

    /// Encodes the 4096-byte block.
    #[must_use]
    pub fn encode(&self) -> [u8; BLOCK] {
        let mut b = [0u8; BLOCK];
        b[0..8].copy_from_slice(self.kind.magic());
        b[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.core_id.to_le_bytes());
        b[16..24].copy_from_slice(&self.seq.to_le_bytes());
        b[24..32].copy_from_slice(&self.node_id.to_le_bytes());
        b[32..40].copy_from_slice(&self.created_unix_ms.to_le_bytes());
        let n = self.reserved.len().min(HEADER_RESERVED);
        b[40..40 + n].copy_from_slice(&self.reserved[..n]);
        let crc = crc32c(&b[..BLOCK - 4]);
        b[BLOCK - 4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Decodes and validates a block.
    ///
    /// # Errors
    ///
    /// [`FormatError::Truncated`], [`FormatError::BadMagic`], [`FormatError::BadCrc`], [`FormatError::Version`] or
    /// [`FormatError::Invalid`] for bytes that are not a valid file header.
    pub fn decode(b: &[u8]) -> Result<FileHeader, FormatError> {
        let b = b.get(..BLOCK).ok_or(FormatError::Truncated)?;
        let kind = FileKind::from_magic(&b[0..8]).ok_or(FormatError::BadMagic)?;
        if crc32c(&b[..BLOCK - 4]) != le_u32(&b[BLOCK - 4..]) {
            return Err(FormatError::BadCrc);
        }
        let version = le_u32(&b[8..12]);
        if version != FORMAT_VERSION {
            return Err(FormatError::Version(version));
        }
        let reserved = &b[40..BLOCK - 4];
        let used = reserved.iter().rposition(|&x| x != 0).map_or(0, |p| p + 1);
        Ok(FileHeader {
            kind,
            core_id: le_u32(&b[12..16]),
            seq: le_u64(&b[16..24]),
            node_id: le_u64(&b[24..32]),
            created_unix_ms: le_u64(&b[32..40]),
            reserved: reserved[..used].to_vec(),
        })
    }
}

/// Reads a little-endian u16 from a 2-byte slice.
pub(crate) fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// Reads a little-endian u32 from a 4-byte slice.
pub(crate) fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Reads a little-endian u64 from an 8-byte slice.
pub(crate) fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip_and_corruption() {
        for kind in [FileKind::Wal, FileKind::Snapshot] {
            let mut h = FileHeader::new(kind, 3, 0x1234, 9, 1_700_000_000_000);
            h.reserved = vec![1, 2, 3];
            let b = h.encode();
            assert_eq!(FileHeader::decode(&b).expect("decode succeeds"), h);
            // Any altered byte is detected.
            for i in [0usize, 9, 20, 41, 4000, 4093] {
                let mut c = b;
                c[i] ^= 0x40;
                assert!(FileHeader::decode(&c).is_err(), "byte {i}");
            }
        }
        assert_eq!(FileHeader::decode(&[0u8; 100]), Err(FormatError::Truncated));
        let mut b = FileHeader::new(FileKind::Wal, 0, 0, 0, 0).encode();
        b[8] = 2;
        let crc = crc32c(&b[..BLOCK - 4]);
        b[BLOCK - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(FileHeader::decode(&b), Err(FormatError::Version(2)));
    }

    #[test]
    fn error_messages() {
        for e in [
            FormatError::Truncated,
            FormatError::BadMagic,
            FormatError::BadCrc,
            FormatError::Version(3),
            FormatError::Invalid("x"),
        ] {
            assert!(!e.to_string().is_empty());
        }
        let d: FormatError = teseladb_types::DecodeError::BadVarint { offset: 0 }.into();
        assert_eq!(d, FormatError::Truncated);
    }
}
