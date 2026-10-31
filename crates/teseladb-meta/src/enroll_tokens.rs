//! The enrollment tokens of the cluster in Group 0 (013 FR-005, contract `pki-and-enrollment.md`):
//! one use each, with an expiry. Group 0 keeps the SHA-256 of a token's secret, never the secret,
//! and judges the expiry against its own cluster time when it applies the use, so every replica
//! decides the same. Kinds 46 (`CreateEnrollToken`) and 47 (`ConsumeEnrollToken`).

use std::collections::BTreeMap;

use teseladb_types::{DecodeError, Decoder, Encoder};

/// Bytes of a token's id.
pub const TOKEN_ID_LEN: usize = 16;
/// Bytes of the SHA-256 of a token's secret.
pub const SECRET_HASH_LEN: usize = 32;
/// How long a used or expired token is kept, so that a late retry still learns why it failed:
/// one day.
pub const KEEP_MS: u64 = 24 * 3600 * 1000;

/// A token as Group 0 keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollToken {
    /// Its id.
    pub id: [u8; TOKEN_ID_LEN],
    /// SHA-256 of its secret.
    pub secret_hash: [u8; SECRET_HASH_LEN],
    /// It is valid before this cluster time (milliseconds).
    pub expires_ms: u64,
    /// Whether a node used it.
    pub used: bool,
}

/// Why a use was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// No such token, or not its secret (the two are not told apart).
    Unknown,
    /// Already used.
    Used,
    /// Past its expiry.
    Expired,
}

/// The tokens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnrollTokens {
    /// By id.
    pub tokens: BTreeMap<[u8; TOKEN_ID_LEN], EnrollToken>,
}

/// Equality that takes the same time whatever the bytes.
fn same(a: &[u8; SECRET_HASH_LEN], b: &[u8; SECRET_HASH_LEN]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl EnrollTokens {
    /// Adds `token` at cluster time `now_ms`, dropping the tokens expired more than a day ago;
    /// `false` when its id is taken.
    pub fn create(&mut self, token: &EnrollToken, now_ms: u64) -> bool {
        self.tokens.retain(|_, t| t.expires_ms.saturating_add(KEEP_MS) > now_ms);
        if self.tokens.contains_key(&token.id) {
            return false;
        }
        self.tokens.insert(token.id, token.clone());
        true
    }

    /// Uses token `id` with the hash of the secret a node gave, at cluster time `now_ms`.
    pub fn consume(
        &mut self,
        id: &[u8; TOKEN_ID_LEN],
        secret_hash: &[u8; SECRET_HASH_LEN],
        now_ms: u64,
    ) -> Result<(), TokenError> {
        let t = self.tokens.get_mut(id).ok_or(TokenError::Unknown)?;
        if !same(&t.secret_hash, secret_hash) {
            return Err(TokenError::Unknown);
        }
        if t.used {
            return Err(TokenError::Used);
        }
        if now_ms >= t.expires_ms {
            return Err(TokenError::Expired);
        }
        t.used = true;
        Ok(())
    }

    /// Writes the tokens into Group 0's snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        e.var(self.tokens.len() as u64);
        for t in self.tokens.values() {
            encode_token(t, e);
        }
    }

    /// Reads what [`EnrollTokens::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<EnrollTokens, DecodeError> {
        let mut out = EnrollTokens::default();
        for _ in 0..d.var()? {
            let t = decode_token(d)?;
            out.tokens.insert(t.id, t);
        }
        Ok(out)
    }
}

/// Writes a token.
pub fn encode_token(t: &EnrollToken, e: &mut Encoder) {
    e.raw(&t.id).raw(&t.secret_hash).u64(t.expires_ms).bool(t.used);
}

fn array<const N: usize>(d: &mut Decoder<'_>) -> Result<[u8; N], DecodeError> {
    let mut a = [0u8; N];
    a.copy_from_slice(d.raw(N)?);
    Ok(a)
}

/// Reads what [`encode_token`] wrote.
pub fn decode_token(d: &mut Decoder<'_>) -> Result<EnrollToken, DecodeError> {
    Ok(EnrollToken { id: array(d)?, secret_hash: array(d)?, expires_ms: d.u64()?, used: d.bool()? })
}

/// Reads a token id and a secret hash (the payload of a use).
pub fn decode_use(
    d: &mut Decoder<'_>,
) -> Result<([u8; TOKEN_ID_LEN], [u8; SECRET_HASH_LEN]), DecodeError> {
    Ok((array(d)?, array(d)?))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn token(id: u8, expires_ms: u64) -> EnrollToken {
        EnrollToken { id: [id; 16], secret_hash: [id; 32], expires_ms, used: false }
    }

    #[test]
    fn a_token_is_used_once_before_its_expiry() {
        let mut t = EnrollTokens::default();
        assert!(t.create(&token(1, 100), 0));
        assert!(!t.create(&token(1, 200), 0));
        assert_eq!(t.consume(&[1; 16], &[2; 32], 10), Err(TokenError::Unknown));
        assert_eq!(t.consume(&[9; 16], &[1; 32], 10), Err(TokenError::Unknown));
        assert_eq!(t.consume(&[1; 16], &[1; 32], 100), Err(TokenError::Expired));
        assert_eq!(t.consume(&[1; 16], &[1; 32], 99), Ok(()));
        assert_eq!(t.consume(&[1; 16], &[1; 32], 99), Err(TokenError::Used));
        // A day after its expiry the next creation forgets it.
        assert!(t.create(&token(2, 100), 100 + KEEP_MS - 1));
        assert_eq!(t.tokens.len(), 2);
        assert!(t.create(&token(3, u64::MAX), 100 + KEEP_MS));
        assert_eq!(t.tokens.len(), 1);
        let mut e = Encoder::new();
        t.encode(&mut e);
        let bytes = e.finish();
        assert_eq!(EnrollTokens::decode(&mut Decoder::new(&bytes)), Ok(t));
        assert!(EnrollTokens::decode(&mut Decoder::new(&bytes[..10])).is_err());
    }
}
