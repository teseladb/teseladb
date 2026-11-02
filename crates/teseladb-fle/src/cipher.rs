//! AES-256-CTR with a random 16-byte IV in front, the server's cipher of Queryable Encryption: it
//! encrypts the client's value with the key id, the counts of a tag, the positions of an ESC anchor
//! and the compaction tokens of the ECOC (FR-009).

use teseladb_crypto::aes::Aes;
use teseladb_crypto::modes::ctr_xor;

use crate::tokens::Token;

/// Bytes of the IV in front of a ciphertext.
pub const IV_LEN: usize = 16;

/// `iv || AES-256-CTR(key, iv, plaintext)`.
pub fn encrypt(key: &Token, iv: [u8; IV_LEN], plaintext: &[u8]) -> Vec<u8> {
    let aes = Aes::new_256(key.as_bytes());
    let mut out = Vec::with_capacity(IV_LEN + plaintext.len());
    out.extend_from_slice(&iv);
    out.extend_from_slice(plaintext);
    ctr_xor(&aes, &iv, &mut out[IV_LEN..]);
    out
}

/// The plaintext of `iv || ciphertext`; `None` when it is shorter than an IV.
pub fn decrypt(key: &Token, sealed: &[u8]) -> Option<Vec<u8>> {
    let (iv, body) = sealed.split_first_chunk::<IV_LEN>()?;
    let aes = Aes::new_256(key.as_bytes());
    let mut out = body.to_vec();
    ctr_xor(&aes, iv, &mut out);
    Some(out)
}
