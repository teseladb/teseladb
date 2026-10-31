//! Versions of a key (FR-013): the logical key followed by its HLC timestamp inverted, 12 bytes, so
//! that a key's versions sort from the newest to the oldest and the logical key comes out by
//! cutting the suffix, without decoding anything.

use std::fmt;

/// Length of the version suffix.
pub const VERSION_LEN: usize = 12;

/// An HLC timestamp: 8 physical bytes and 4 logical ones (005 FR-003).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hlc {
    /// Physical part.
    pub physical: u64,
    /// Logical part.
    pub logical: u32,
}

impl Hlc {
    /// Its 12 big-endian bytes, which sort as the timestamps do.
    pub fn to_be_bytes(self) -> [u8; VERSION_LEN] {
        let mut out = [0; VERSION_LEN];
        out[..8].copy_from_slice(&self.physical.to_be_bytes());
        out[8..].copy_from_slice(&self.logical.to_be_bytes());
        out
    }

    /// From its 12 big-endian bytes.
    pub fn from_be_bytes(b: [u8; VERSION_LEN]) -> Hlc {
        Hlc {
            physical: u64::from_be_bytes(b[..8].try_into().expect("8 bytes")),
            logical: u32::from_be_bytes(b[8..].try_into().expect("4 bytes")),
        }
    }
}

/// A logical key with a version suffix.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VersionedKey(pub(crate) Vec<u8>);

/// Bytes that are not a key of the expected shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyError(pub(crate) &'static str);

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a key: {}", self.0)
    }
}

impl std::error::Error for KeyError {}

impl VersionedKey {
    /// `logical` with the version `ts`.
    pub(crate) fn new(logical: &[u8], ts: Hlc) -> VersionedKey {
        let mut out = Vec::with_capacity(logical.len() + VERSION_LEN);
        out.extend_from_slice(logical);
        out.extend(ts.to_be_bytes().iter().map(|b| !b));
        VersionedKey(out)
    }

    /// Its bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The logical key and the timestamp of versioned bytes.
    pub fn split(bytes: &[u8]) -> Result<(&[u8], Hlc), KeyError> {
        let cut =
            bytes.len().checked_sub(VERSION_LEN).ok_or(KeyError("shorter than its version"))?;
        let mut suffix = [0; VERSION_LEN];
        for (s, b) in suffix.iter_mut().zip(&bytes[cut..]) {
            *s = !b;
        }
        Ok((&bytes[..cut], Hlc::from_be_bytes(suffix)))
    }
}
