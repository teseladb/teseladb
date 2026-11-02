//! The tokens of Queryable Encryption, HMAC-SHA-256 chains from a data key (FR-009).
//!
//! `v` is a BSON value (the bytes after the element name), `u` a contention factor, and numbers
//! are 64-bit little-endian. The drivers derive the tokens that need a key; the server derives
//! the rest from the ones a payload carries:
//!
//! - from the root (the last 32 bytes of a 96-byte data key): `CollectionsLevel1Token = HMAC(root,
//!   1)`, `ServerTokenDerivationLevel1Token = HMAC(root, 2)` and `ServerDataEncryptionLevel1Token =
//!   HMAC(root, 3)`;
//! - from `CollectionsLevel1Token`: EDC 1, ESC 2, ECC 3 and ECOC 4;
//! - `…DerivedFromData = HMAC(token, v)`, `…AndContentionFactor = HMAC(that, u)`;
//! - `EDCTwiceDerived = HMAC(EDC…AndContentionFactor, 1)`, `ESCTwiceDerivedTag = HMAC(ESC…, 1)`
//!   and `ESCTwiceDerivedValue = HMAC(ESC…, 2)`;
//! - `ServerDerivedFromData = HMAC(ServerTokenDerivationLevel1Token, v)`, and from it the
//!   encryption tokens of a tag's count (1) and of its zeros (2);
//! - range: `AnchorPaddingTokenRoot = HMAC(ESCToken, 17 zero bytes)`, its key token 1 and value
//!   token 2;
//! - text: the exact, substring, suffix and prefix tokens are `HMAC(EDCToken | ESCToken |
//!   ServerTokenDerivationLevel1Token, 1 | 2 | 3 | 4)`.
//!
//! A tag of `__safeContent__` is `HMAC(EDCTwiceDerived, count)`.

use teseladb_crypto::HashAlgorithm;

/// Bytes of a token.
pub const TOKEN_LEN: usize = 32;

/// The zero bytes the anchor padding root is derived from.
const ANCHOR_PADDING_DATA: [u8; 17] = [0; 17];

/// A 32-byte token.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Token(pub [u8; TOKEN_LEN]);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Tokens are secrets of the client: never shown.
        f.write_str("Token(..)")
    }
}

impl Token {
    /// A token from exactly 32 bytes.
    pub fn from_slice(bytes: &[u8]) -> Option<Token> {
        <[u8; TOKEN_LEN]>::try_from(bytes).ok().map(Token)
    }

    /// `HMAC(self, data)`.
    pub fn derive(&self, data: &[u8]) -> Token {
        let out = HashAlgorithm::Sha256.hmac(&self.0, data);
        let mut t = [0u8; TOKEN_LEN];
        t.copy_from_slice(out.as_ref());
        Token(t)
    }

    /// `HMAC(self, n)` with `n` as 64-bit little-endian.
    pub fn derive_u64(&self, n: u64) -> Token {
        self.derive(&n.to_le_bytes())
    }

    /// `HMAC(self, hi || lo)`, both 64-bit little-endian: the key of an ESC anchor.
    pub fn derive_pair(&self, hi: u64, lo: u64) -> Token {
        let mut data = [0u8; 16];
        data[..8].copy_from_slice(&hi.to_le_bytes());
        data[8..].copy_from_slice(&lo.to_le_bytes());
        self.derive(&data)
    }

    /// The bytes.
    pub fn as_bytes(&self) -> &[u8; TOKEN_LEN] {
        &self.0
    }
}

/// The three level-one tokens of a root key.
pub fn collections_level1(root: &Token) -> Token {
    root.derive_u64(1)
}

/// `ServerTokenDerivationLevel1Token`.
pub fn server_token_derivation_level1(root: &Token) -> Token {
    root.derive_u64(2)
}

/// `ServerDataEncryptionLevel1Token`: encrypts the client's value inside an indexed value.
pub fn server_data_encryption_level1(root: &Token) -> Token {
    root.derive_u64(3)
}

/// The collection tokens of `CollectionsLevel1Token`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    /// Encrypted data collection: the tags.
    Edc = 1,
    /// Encrypted state collection: the counts.
    Esc = 2,
    /// Encrypted cache collection (version 1 only).
    Ecc = 3,
    /// Encrypted compaction collection: the log compaction reads.
    Ecoc = 4,
}

/// The token of collection `c`.
pub fn collection_token(level1: &Token, c: Collection) -> Token {
    level1.derive_u64(c as u64)
}

/// `EDCTwiceDerivedToken` of an `EDCDerivedFromDataTokenAndContentionFactor`.
pub fn edc_twice_derived(edc_cf: &Token) -> Token {
    edc_cf.derive_u64(1)
}

/// `ESCTwiceDerivedTagToken`: keys the ESC documents of a value.
pub fn esc_twice_derived_tag(esc_cf: &Token) -> Token {
    esc_cf.derive_u64(1)
}

/// `ESCTwiceDerivedValueToken`: encrypts the positions of an ESC anchor.
pub fn esc_twice_derived_value(esc_cf: &Token) -> Token {
    esc_cf.derive_u64(2)
}

/// `ServerCountAndContentionFactorEncryptionToken` of a `ServerDerivedFromDataToken`.
pub fn server_count_and_contention(server_derived: &Token) -> Token {
    server_derived.derive_u64(1)
}

/// `ServerZerosEncryptionToken` of a `ServerDerivedFromDataToken`.
pub fn server_zeros(server_derived: &Token) -> Token {
    server_derived.derive_u64(2)
}

/// `AnchorPaddingTokenRoot` of an `ESCToken` (range, compaction pads with it).
pub fn anchor_padding_root(esc: &Token) -> Token {
    esc.derive(&ANCHOR_PADDING_DATA)
}

/// `AnchorPaddingKeyToken`: keys the padding anchors.
pub fn anchor_padding_key(root: &Token) -> Token {
    root.derive_u64(1)
}

/// `AnchorPaddingValueToken`: encrypts the padding anchors.
pub fn anchor_padding_value(root: &Token) -> Token {
    root.derive_u64(2)
}

/// The four kinds of text search tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextKind {
    /// The whole string.
    Exact = 1,
    /// A substring.
    Substring = 2,
    /// A suffix.
    Suffix = 3,
    /// A prefix.
    Prefix = 4,
}

/// The text token of kind `k` of an EDC, ESC or `ServerTokenDerivationLevel1` token.
pub fn text_token(base: &Token, k: TextKind) -> Token {
    base.derive_u64(k as u64)
}

/// The tag of position `count` of a value: an element of `__safeContent__`.
pub fn tag(edc_twice: &Token, count: u64) -> Token {
    edc_twice.derive_u64(count)
}
