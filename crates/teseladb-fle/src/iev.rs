//! The indexed values the collection keeps (FR-001): equality (subtype 14), range (15) and text
//! (17). Each has the index key id, the original BSON type, the client's value encrypted again by
//! the server and one metadata block per tag:
//!
//! ```text
//! equality: 14 | S_KeyId[16] | type | ServerEncryptedValue | block
//! range:    15 | S_KeyId[16] | type | edge_count u8 | ServerEncryptedValue | block[edge_count]
//! text:     17 | S_KeyId[16] | type | edge_count u32 | substr u32 | suffix u32
//!                | ServerEncryptedValue | exact block | substr blocks | suffix blocks | prefix blocks
//! block:    EncryptCTR(count token, count u64 || contention u64)[32] | tag[32]
//!           | EncryptCTR(zeros token, 16 zero bytes)[32]
//! ```
//!
//! `ServerEncryptedValue` is `EncryptCTR(ServerDataEncryptionLevel1Token, K_KeyId ||
//! ClientEncryptedValue)`; its length is what the blocks leave. Numbers are little-endian.

use crate::cipher;
use crate::subtype::BlobSubtype;
use crate::tokens::{self, Token};

/// Bytes of a metadata block.
pub const BLOCK_LEN: usize = 96;
/// Bytes of a key id.
pub const KEY_ID_LEN: usize = 16;
/// Bytes of a server-encrypted value at the least: an IV, a key id and one AES block.
const MIN_SERVER_VALUE: usize = cipher::IV_LEN + KEY_ID_LEN + 1;

/// A tag with its encrypted count and zeros.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// `EncryptCTR(ServerCountAndContentionFactorEncryptionToken, count || contention)`.
    pub encrypted_count: [u8; 32],
    /// The tag.
    pub tag: Token,
    /// `EncryptCTR(ServerZerosEncryptionToken, 0^16)`.
    pub encrypted_zeros: [u8; 32],
}

impl Block {
    /// The block of position `count` and contention `contention` of a value whose
    /// `ServerDerivedFromDataToken` is `server`, with the two IVs given.
    pub fn new(
        server: &Token,
        (count, contention): (u64, u64),
        tag: Token,
        ivs: ([u8; 16], [u8; 16]),
    ) -> Block {
        let mut plain = [0u8; 16];
        plain[..8].copy_from_slice(&count.to_le_bytes());
        plain[8..].copy_from_slice(&contention.to_le_bytes());
        let c = cipher::encrypt(&tokens::server_count_and_contention(server), ivs.0, &plain);
        let z = cipher::encrypt(&tokens::server_zeros(server), ivs.1, &[0u8; 16]);
        let mut encrypted_count = [0u8; 32];
        encrypted_count.copy_from_slice(&c);
        let mut encrypted_zeros = [0u8; 32];
        encrypted_zeros.copy_from_slice(&z);
        Block { encrypted_count, tag, encrypted_zeros }
    }

    fn read(b: &[u8; BLOCK_LEN]) -> Block {
        let mut encrypted_count = [0u8; 32];
        encrypted_count.copy_from_slice(&b[..32]);
        let mut tag = [0u8; 32];
        tag.copy_from_slice(&b[32..64]);
        let mut encrypted_zeros = [0u8; 32];
        encrypted_zeros.copy_from_slice(&b[64..96]);
        Block { encrypted_count, tag: Token(tag), encrypted_zeros }
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.encrypted_count);
        out.extend_from_slice(&self.tag.0);
        out.extend_from_slice(&self.encrypted_zeros);
    }

    /// The count and contention factor, with the value's `ServerDerivedFromDataToken`.
    pub fn count_and_contention(&self, server: &Token) -> (u64, u64) {
        let plain =
            cipher::decrypt(&tokens::server_count_and_contention(server), &self.encrypted_count)
                .unwrap_or_default();
        let word = |i: usize| {
            let mut w = [0u8; 8];
            w.copy_from_slice(&plain[i..i + 8]);
            u64::from_le_bytes(w)
        };
        (word(0), word(8))
    }
}

/// The kind of an indexed value and its layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One block.
    Equality,
    /// One block per edge.
    Range,
    /// The exact block, then the substring, suffix and prefix ones.
    Text {
        /// Substring blocks.
        substrings: u32,
        /// Suffix blocks.
        suffixes: u32,
    },
}

/// An indexed value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedValue {
    /// Its layout.
    pub kind: Kind,
    /// The index key id.
    pub key_id: [u8; KEY_ID_LEN],
    /// The BSON type of the value in clear.
    pub bson_type: u8,
    /// `EncryptCTR(ServerDataEncryptionLevel1Token, K_KeyId || ClientEncryptedValue)`.
    pub server_value: Vec<u8>,
    /// The blocks, in the order of the layout.
    pub blocks: Vec<Block>,
}

/// Why bytes are no indexed value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let mut w = [0u8; 4];
    w.copy_from_slice(b.get(at..at + 4)?);
    Some(u32::from_le_bytes(w))
}

impl IndexedValue {
    /// Reads an indexed value (the bytes of the binary, subtype byte included).
    pub fn parse(b: &[u8]) -> Result<IndexedValue, Malformed> {
        let subtype = BlobSubtype::of(b).ok_or(Malformed)?;
        let key_id: [u8; KEY_ID_LEN] =
            b.get(1..1 + KEY_ID_LEN).and_then(|k| k.try_into().ok()).ok_or(Malformed)?;
        let bson_type = *b.get(1 + KEY_ID_LEN).ok_or(Malformed)?;
        let mut at = 2 + KEY_ID_LEN;
        let (kind, count) = match subtype {
            BlobSubtype::Fle2EqualityIndexed => (Kind::Equality, 1usize),
            BlobSubtype::Fle2RangeIndexed => {
                let n = *b.get(at).ok_or(Malformed)?;
                at += 1;
                (Kind::Range, usize::from(n))
            }
            BlobSubtype::Fle2TextIndexed => {
                let edges = u32_at(b, at).ok_or(Malformed)?;
                let substrings = u32_at(b, at + 4).ok_or(Malformed)?;
                let suffixes = u32_at(b, at + 8).ok_or(Malformed)?;
                at += 12;
                let fixed = u64::from(substrings) + u64::from(suffixes) + 1;
                if u64::from(edges) < fixed {
                    return Err(Malformed);
                }
                (
                    Kind::Text { substrings, suffixes },
                    usize::try_from(edges).map_err(|_| Malformed)?,
                )
            }
            _ => return Err(Malformed),
        };
        let blocks_len = count.checked_mul(BLOCK_LEN).ok_or(Malformed)?;
        let rest = b.len().checked_sub(at).ok_or(Malformed)?;
        let value_len = rest.checked_sub(blocks_len).ok_or(Malformed)?;
        if value_len < MIN_SERVER_VALUE || count == 0 {
            return Err(Malformed);
        }
        let server_value = b[at..at + value_len].to_vec();
        let blocks =
            b[at + value_len..].as_chunks::<BLOCK_LEN>().0.iter().map(Block::read).collect();
        Ok(IndexedValue { kind, key_id, bson_type, server_value, blocks })
    }

    /// The bytes of the binary.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            2 + KEY_ID_LEN + 12 + self.server_value.len() + self.blocks.len() * BLOCK_LEN,
        );
        let subtype = match self.kind {
            Kind::Equality => BlobSubtype::Fle2EqualityIndexed,
            Kind::Range => BlobSubtype::Fle2RangeIndexed,
            Kind::Text { .. } => BlobSubtype::Fle2TextIndexed,
        };
        out.push(subtype as u8);
        out.extend_from_slice(&self.key_id);
        out.push(self.bson_type);
        match self.kind {
            Kind::Equality => {}
            // At most 255 edges: what a 64-bit domain with sparsity 1 gives is 65 (checked by the
            // writer).
            Kind::Range => out.push(u8::try_from(self.blocks.len()).unwrap_or(u8::MAX)),
            Kind::Text { substrings, suffixes } => {
                out.extend_from_slice(
                    &u32::try_from(self.blocks.len()).unwrap_or(u32::MAX).to_le_bytes(),
                );
                out.extend_from_slice(&substrings.to_le_bytes());
                out.extend_from_slice(&suffixes.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.server_value);
        for b in &self.blocks {
            b.write(&mut out);
        }
        out
    }

    /// The tags of `__safeContent__` this value adds.
    pub fn tags(&self) -> impl Iterator<Item = Token> + '_ {
        self.blocks.iter().map(|b| b.tag)
    }

    /// `K_KeyId || ClientEncryptedValue`, with the index key's `ServerDataEncryptionLevel1Token`.
    pub fn client_value(&self, server_encryption: &Token) -> Option<Vec<u8>> {
        cipher::decrypt(server_encryption, &self.server_value)
    }
}
