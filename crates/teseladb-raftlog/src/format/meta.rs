//! `node.meta`: the node's identity in a 4096-byte block (data-model §2).

use teseladb_hash::crc32c;

use super::{BLOCK, FORMAT_VERSION, FormatError, le_u32, le_u64};

const MAGIC: &[u8; 8] = b"TSLNODE1";

/// Contents of `node.meta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMeta {
    /// Cores the directory was created with.
    pub cores: u32,
    /// Cluster identifier.
    pub cluster_id: [u8; 16],
    /// Node identifier (0 = unassigned).
    pub node_id: u64,
    /// Creation time (ms since the Unix epoch).
    pub created_unix_ms: u64,
    /// Start-up counter.
    pub boot_epoch: u32,
    /// Upper bound of the node's HLC (ms): no mark above it was issued.
    pub hlc_upper_bound: u64,
    /// Reserved area from offset 60 (used by 013's encryption at rest).
    pub reserved: Vec<u8>,
}

/// Length of the reserved area.
pub const META_RESERVED: usize = BLOCK - 60 - 4;

impl NodeMeta {
    /// Encodes the block.
    #[must_use]
    pub fn encode(&self) -> [u8; BLOCK] {
        let mut b = [0u8; BLOCK];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.cores.to_le_bytes());
        b[16..32].copy_from_slice(&self.cluster_id);
        b[32..40].copy_from_slice(&self.node_id.to_le_bytes());
        b[40..48].copy_from_slice(&self.created_unix_ms.to_le_bytes());
        b[48..52].copy_from_slice(&self.boot_epoch.to_le_bytes());
        b[52..60].copy_from_slice(&self.hlc_upper_bound.to_le_bytes());
        let n = self.reserved.len().min(META_RESERVED);
        b[60..60 + n].copy_from_slice(&self.reserved[..n]);
        let crc = crc32c(&b[..BLOCK - 4]);
        b[BLOCK - 4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Decodes and validates.
    ///
    /// # Errors
    ///
    /// [`FormatError::Truncated`], [`FormatError::BadMagic`], [`FormatError::BadCrc`], [`FormatError::Version`] or
    /// [`FormatError::Invalid`] for bytes that are not a valid `node.meta`.
    pub fn decode(b: &[u8]) -> Result<NodeMeta, FormatError> {
        let b = b.get(..BLOCK).ok_or(FormatError::Truncated)?;
        if &b[0..8] != MAGIC {
            return Err(FormatError::BadMagic);
        }
        if crc32c(&b[..BLOCK - 4]) != le_u32(&b[BLOCK - 4..]) {
            return Err(FormatError::BadCrc);
        }
        let version = le_u32(&b[8..12]);
        if version != FORMAT_VERSION {
            return Err(FormatError::Version(version));
        }
        let cores = le_u32(&b[12..16]);
        if cores == 0 {
            return Err(FormatError::Invalid("cores = 0"));
        }
        let mut cluster_id = [0u8; 16];
        cluster_id.copy_from_slice(&b[16..32]);
        let reserved = &b[60..BLOCK - 4];
        let used = reserved.iter().rposition(|&x| x != 0).map_or(0, |p| p + 1);
        Ok(NodeMeta {
            cores,
            cluster_id,
            node_id: le_u64(&b[32..40]),
            created_unix_ms: le_u64(&b[40..48]),
            boot_epoch: le_u32(&b[48..52]),
            hlc_upper_bound: le_u64(&b[52..60]),
            reserved: reserved[..used].to_vec(),
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn sample() -> NodeMeta {
        NodeMeta {
            cores: 8,
            cluster_id: [7; 16],
            node_id: 3,
            created_unix_ms: 1_767_225_600_000,
            boot_epoch: 42,
            hlc_upper_bound: 1_767_225_601_000,
            reserved: vec![9, 9],
        }
    }

    #[test]
    fn round_trip() {
        let m = sample();
        assert_eq!(NodeMeta::decode(&m.encode()).expect("decode succeeds"), m);
    }

    #[test]
    fn detects_every_corrupted_byte() {
        let b = sample().encode();
        for i in (0..BLOCK).step_by(37).chain([BLOCK - 1]) {
            let mut c = b;
            c[i] ^= 1;
            assert!(NodeMeta::decode(&c).is_err(), "byte {i}");
        }
    }

    #[test]
    fn rejects_bad_fields() {
        assert_eq!(NodeMeta::decode(&[0; 10]), Err(FormatError::Truncated));
        let reseal = |mut b: [u8; BLOCK]| {
            let crc = crc32c(&b[..BLOCK - 4]);
            b[BLOCK - 4..].copy_from_slice(&crc.to_le_bytes());
            b
        };
        let mut b = sample().encode();
        b[8] = 9;
        assert_eq!(NodeMeta::decode(&reseal(b)), Err(FormatError::Version(9)));
        let mut b = sample().encode();
        b[12..16].fill(0);
        assert_eq!(NodeMeta::decode(&reseal(b)), Err(FormatError::Invalid("cores = 0")));
    }
}
