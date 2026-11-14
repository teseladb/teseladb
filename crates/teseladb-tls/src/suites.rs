//! Cipher suites, key exchange groups and signature schemes (RFC 8446, appendix B.4 and 4.2.3,
//! 4.2.7), with the record AEADs.

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::aes::Aes;
use teseladb_crypto::chacha20poly1305::ChaCha20Poly1305;
use teseladb_crypto::modes::AesGcm;

use crate::{Alert, Error};

/// AEAD tag length of every suite.
pub const TAG_LEN: usize = 16;
/// AEAD nonce length of every suite.
pub const NONCE_LEN: usize = 12;

/// A TLS 1.3 cipher suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CipherSuite {
    /// `TLS_AES_128_GCM_SHA256`.
    Aes128GcmSha256,
    /// `TLS_AES_256_GCM_SHA384`.
    Aes256GcmSha384,
    /// `TLS_CHACHA20_POLY1305_SHA256`.
    ChaCha20Poly1305Sha256,
}

impl CipherSuite {
    /// Every suite, in this crate's order of preference.
    pub const ALL: [CipherSuite; 3] = [
        CipherSuite::Aes128GcmSha256,
        CipherSuite::Aes256GcmSha384,
        CipherSuite::ChaCha20Poly1305Sha256,
    ];

    /// The code point.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            CipherSuite::Aes128GcmSha256 => 0x1301,
            CipherSuite::Aes256GcmSha384 => 0x1302,
            CipherSuite::ChaCha20Poly1305Sha256 => 0x1303,
        }
    }

    /// The suite of a code point.
    #[must_use]
    pub fn from_code(code: u16) -> Option<CipherSuite> {
        CipherSuite::ALL.into_iter().find(|s| s.code() == code)
    }

    /// Whether FIPS 140-3 approves the suite: the AES-GCM ones.
    #[must_use]
    pub fn is_fips_approved(self) -> bool {
        self != CipherSuite::ChaCha20Poly1305Sha256
    }

    /// The handshake hash.
    #[must_use]
    pub fn hash(self) -> HashAlgorithm {
        match self {
            CipherSuite::Aes256GcmSha384 => HashAlgorithm::Sha384,
            CipherSuite::Aes128GcmSha256 | CipherSuite::ChaCha20Poly1305Sha256 => {
                HashAlgorithm::Sha256
            }
        }
    }

    /// The AEAD key length.
    #[must_use]
    pub fn key_len(self) -> usize {
        match self {
            CipherSuite::Aes128GcmSha256 => 16,
            CipherSuite::Aes256GcmSha384 | CipherSuite::ChaCha20Poly1305Sha256 => 32,
        }
    }

    /// The AEAD with `key`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::InternalError`] for a key of the wrong length.
    pub fn aead(self, key: &[u8]) -> Result<Aead, Error> {
        let internal = || Error::Local(Alert::InternalError);
        Ok(match self {
            CipherSuite::Aes128GcmSha256 | CipherSuite::Aes256GcmSha384 => {
                Aead::AesGcm(Box::new(AesGcm::new(Aes::new(key).map_err(|_| internal())?)))
            }
            CipherSuite::ChaCha20Poly1305Sha256 => {
                Aead::ChaCha(ChaCha20Poly1305::new(key.try_into().map_err(|_| internal())?))
            }
        })
    }
}

/// A record protection AEAD.
#[derive(Clone)]
pub enum Aead {
    /// AES-GCM.
    AesGcm(Box<AesGcm>),
    /// ChaCha20-Poly1305.
    ChaCha(ChaCha20Poly1305),
}

impl std::fmt::Debug for Aead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Aead::AesGcm(_) => "Aead(AES-GCM)",
            Aead::ChaCha(_) => "Aead(ChaCha20-Poly1305)",
        })
    }
}

impl Aead {
    /// Encrypts `data` in place and returns the tag.
    pub fn seal(&self, nonce: &[u8; NONCE_LEN], aad: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
        match self {
            Aead::AesGcm(a) => a.seal_in_place(nonce, aad, data),
            Aead::ChaCha(c) => c.seal_in_place(nonce, aad, data),
        }
    }

    /// Checks the tag and decrypts `data` in place.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::BadRecordMac`] when the record does not authenticate.
    pub fn open(
        &self,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        data: &mut [u8],
        tag: &[u8],
    ) -> Result<(), Error> {
        let r = match self {
            Aead::AesGcm(a) => a.open_in_place(nonce, aad, data, tag),
            Aead::ChaCha(c) => c.open_in_place(nonce, aad, data, tag),
        };
        r.map_err(|_| Error::Local(Alert::BadRecordMac))
    }
}

/// A key exchange group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NamedGroup {
    /// x25519.
    X25519,
    /// secp256r1.
    Secp256r1,
    /// secp384r1.
    Secp384r1,
    /// secp521r1.
    Secp521r1,
}

impl NamedGroup {
    /// Every group, in this crate's order of preference.
    pub const ALL: [NamedGroup; 4] =
        [NamedGroup::X25519, NamedGroup::Secp256r1, NamedGroup::Secp384r1, NamedGroup::Secp521r1];

    /// The code point.
    #[must_use]
    pub fn code(self) -> u16 {
        match self {
            NamedGroup::X25519 => 0x001d,
            NamedGroup::Secp256r1 => 0x0017,
            NamedGroup::Secp384r1 => 0x0018,
            NamedGroup::Secp521r1 => 0x0019,
        }
    }

    /// Whether FIPS 140-3 approves the group for key exchange: the NIST curves (SP 800-56A).
    #[must_use]
    pub fn is_fips_approved(self) -> bool {
        self != NamedGroup::X25519
    }

    /// The group of a code point.
    #[must_use]
    pub fn from_code(code: u16) -> Option<NamedGroup> {
        NamedGroup::ALL.into_iter().find(|g| g.code() == code)
    }
}

/// A signature scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SignatureScheme {
    /// `ecdsa_secp256r1_sha256`.
    EcdsaP256Sha256,
    /// `ecdsa_secp384r1_sha384`.
    EcdsaP384Sha384,
    /// `ecdsa_secp521r1_sha512`.
    EcdsaP521Sha512,
    /// `rsa_pss_rsae_sha256`.
    RsaPssRsaeSha256,
    /// `rsa_pss_rsae_sha384`.
    RsaPssRsaeSha384,
    /// `rsa_pss_rsae_sha512`.
    RsaPssRsaeSha512,
    /// ed25519.
    Ed25519,
    /// `rsa_pss_pss_sha256`.
    RsaPssPssSha256,
    /// `rsa_pss_pss_sha384`.
    RsaPssPssSha384,
    /// `rsa_pss_pss_sha512`.
    RsaPssPssSha512,
    /// `rsa_pkcs1_sha256` (certificates and TLS 1.2).
    RsaPkcs1Sha256,
    /// `rsa_pkcs1_sha384` (certificates and TLS 1.2).
    RsaPkcs1Sha384,
    /// `rsa_pkcs1_sha512` (certificates and TLS 1.2).
    RsaPkcs1Sha512,
}

impl SignatureScheme {
    /// The schemes this crate accepts in `CertificateVerify`, in order of preference.
    pub const HANDSHAKE: [SignatureScheme; 10] = [
        SignatureScheme::EcdsaP256Sha256,
        SignatureScheme::EcdsaP384Sha384,
        SignatureScheme::EcdsaP521Sha512,
        SignatureScheme::Ed25519,
        SignatureScheme::RsaPssRsaeSha256,
        SignatureScheme::RsaPssRsaeSha384,
        SignatureScheme::RsaPssRsaeSha512,
        SignatureScheme::RsaPssPssSha256,
        SignatureScheme::RsaPssPssSha384,
        SignatureScheme::RsaPssPssSha512,
    ];

    /// Every scheme with its code point.
    const CODES: [(SignatureScheme, u16); 13] = [
        (SignatureScheme::EcdsaP256Sha256, 0x0403),
        (SignatureScheme::EcdsaP384Sha384, 0x0503),
        (SignatureScheme::EcdsaP521Sha512, 0x0603),
        (SignatureScheme::RsaPssRsaeSha256, 0x0804),
        (SignatureScheme::RsaPssRsaeSha384, 0x0805),
        (SignatureScheme::RsaPssRsaeSha512, 0x0806),
        (SignatureScheme::Ed25519, 0x0807),
        (SignatureScheme::RsaPssPssSha256, 0x0809),
        (SignatureScheme::RsaPssPssSha384, 0x080a),
        (SignatureScheme::RsaPssPssSha512, 0x080b),
        (SignatureScheme::RsaPkcs1Sha256, 0x0401),
        (SignatureScheme::RsaPkcs1Sha384, 0x0501),
        (SignatureScheme::RsaPkcs1Sha512, 0x0601),
    ];

    /// The code point.
    #[must_use]
    pub fn code(self) -> u16 {
        SignatureScheme::CODES.iter().find(|(s, _)| *s == self).map_or(0, |(_, c)| *c)
    }

    /// The scheme of a code point.
    #[must_use]
    pub fn from_code(code: u16) -> Option<SignatureScheme> {
        SignatureScheme::CODES.iter().find(|(_, c)| *c == code).map(|(s, _)| *s)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn code_points_round_trip() {
        for s in CipherSuite::ALL {
            assert_eq!(CipherSuite::from_code(s.code()), Some(s));
            assert!(s.aead(&vec![0; s.key_len()]).is_ok());
            assert_eq!(s.aead(&[0; 5]).err(), Some(Error::Local(Alert::InternalError)));
        }
        assert_eq!(CipherSuite::from_code(0x1304), None);
        for g in NamedGroup::ALL {
            assert_eq!(NamedGroup::from_code(g.code()), Some(g));
            assert_eq!(g.is_fips_approved(), g != NamedGroup::X25519);
        }
        for s in CipherSuite::ALL {
            assert_eq!(s.is_fips_approved(), s != CipherSuite::ChaCha20Poly1305Sha256);
        }
        for (s, c) in SignatureScheme::CODES {
            assert_eq!(SignatureScheme::from_code(c), Some(s));
            assert_eq!(s.code(), c);
        }
        assert_eq!(SignatureScheme::from_code(0x0201), None);
        let a = CipherSuite::ChaCha20Poly1305Sha256.aead(&[1; 32]).expect("aead succeeds");
        let mut data = *b"hello";
        let tag = a.seal(&[0; 12], b"aad", &mut data);
        assert_eq!(a.open(&[0; 12], b"aad", &mut data, &tag), Ok(()));
        assert_eq!(&data, b"hello");
        assert_eq!(
            a.open(&[0; 12], b"bad", &mut data, &tag),
            Err(Error::Local(Alert::BadRecordMac))
        );
        assert_eq!(format!("{a:?}"), "Aead(ChaCha20-Poly1305)");
        assert_eq!(
            format!("{:?}", CipherSuite::Aes128GcmSha256.aead(&[0; 16]).expect("aead succeeds")),
            "Aead(AES-GCM)"
        );
    }
}
