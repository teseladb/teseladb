//! Ephemeral key exchange (RFC 8446, 4.2.8.2 and 7.4) and the `CertificateVerify` signatures
//! (4.4.3).

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::curve25519::{x25519_agree, x25519_base};
use teseladb_crypto::ec::{EcPrivateKey, NamedCurve, ecdsa_sig_to_der};
use teseladb_x509::{Certificate, PrivateKey, PublicKey, SignatureAlgorithm};

use crate::keyschedule::Secret;
use crate::suites::{NamedGroup, SignatureScheme};
use crate::{Alert, Error};

/// Random bytes an ephemeral key needs: P-521 asks for its scalar length plus 8.
pub const KEY_RANDOM_LEN: usize = 74;

/// An ephemeral private key.
pub enum Ephemeral {
    /// x25519.
    X25519([u8; 32]),
    /// secp256r1, secp384r1 or secp521r1.
    Ec(NamedGroup, EcPrivateKey),
}

impl Drop for Ephemeral {
    fn drop(&mut self) {
        if let Ephemeral::X25519(k) = self {
            *k = [0; 32];
            std::hint::black_box(k);
        }
    }
}

fn curve(group: NamedGroup) -> Option<NamedCurve> {
    match group {
        NamedGroup::X25519 => None,
        NamedGroup::Secp256r1 => Some(NamedCurve::P256),
        NamedGroup::Secp384r1 => Some(NamedCurve::P384),
        NamedGroup::Secp521r1 => Some(NamedCurve::P521),
    }
}

impl Ephemeral {
    /// A key for `group` from `random`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::InternalError`] when the random bytes do not make a key of the group.
    pub fn generate(group: NamedGroup, random: &[u8; KEY_RANDOM_LEN]) -> Result<Ephemeral, Error> {
        match curve(group) {
            None => {
                let mut k = [0u8; 32];
                k.copy_from_slice(&random[..32]);
                Ok(Ephemeral::X25519(k))
            }
            Some(c) => {
                let key = EcPrivateKey::from_random(c, &random[..c.scalar_len() + 8])
                    .ok()
                    .ok_or(Error::Local(Alert::InternalError))?;
                Ok(Ephemeral::Ec(group, key))
            }
        }
    }

    /// The group.
    #[must_use]
    pub fn group(&self) -> NamedGroup {
        match self {
            Ephemeral::X25519(_) => NamedGroup::X25519,
            Ephemeral::Ec(g, _) => *g,
        }
    }

    /// The public key as sent in `key_share`.
    #[must_use]
    pub fn public(&self) -> Vec<u8> {
        match self {
            Ephemeral::X25519(k) => x25519_base(k).to_vec(),
            Ephemeral::Ec(_, k) => k.public_key(),
        }
    }

    /// The shared secret with the peer's share; an invalid share is `illegal_parameter`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::IllegalParameter`] for a share that is not a point or key of the group,
    /// or that gives an all-zero secret.
    pub fn agree(&self, peer: &[u8]) -> Result<Secret, Error> {
        let bad = || Error::Local(Alert::IllegalParameter);
        match self {
            Ephemeral::X25519(k) => {
                let p: [u8; 32] = peer.try_into().map_err(|_| bad())?;
                x25519_agree(k, &p).map(|s| Secret(s.to_vec())).map_err(|_| bad())
            }
            Ephemeral::Ec(_, k) => k.agree(peer).map(Secret).map_err(|_| bad()),
        }
    }
}

/// The content a `CertificateVerify` signs: 64 spaces, the context string, a zero byte and the
/// transcript hash.
#[must_use]
pub fn verify_content(server: bool, transcript_hash: &[u8]) -> Vec<u8> {
    let context: &[u8] = if server {
        b"TLS 1.3, server CertificateVerify"
    } else {
        b"TLS 1.3, client CertificateVerify"
    };
    let mut out = vec![0x20; 64];
    out.extend_from_slice(context);
    out.push(0);
    out.extend_from_slice(transcript_hash);
    out
}

/// The X.509 algorithm of a TLS 1.3 handshake signature scheme, with the curve it requires.
fn algorithm(scheme: SignatureScheme) -> Option<(SignatureAlgorithm, Option<NamedCurve>)> {
    let pss =
        |hash: HashAlgorithm| SignatureAlgorithm::RsaPss { hash, salt_len: hash.output_len() };
    Some(match scheme {
        SignatureScheme::EcdsaP256Sha256 => {
            (SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha256), Some(NamedCurve::P256))
        }
        SignatureScheme::EcdsaP384Sha384 => {
            (SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha384), Some(NamedCurve::P384))
        }
        SignatureScheme::EcdsaP521Sha512 => {
            (SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha512), Some(NamedCurve::P521))
        }
        SignatureScheme::RsaPssRsaeSha256 | SignatureScheme::RsaPssPssSha256 => {
            (pss(HashAlgorithm::Sha256), None)
        }
        SignatureScheme::RsaPssRsaeSha384 | SignatureScheme::RsaPssPssSha384 => {
            (pss(HashAlgorithm::Sha384), None)
        }
        SignatureScheme::RsaPssRsaeSha512 | SignatureScheme::RsaPssPssSha512 => {
            (pss(HashAlgorithm::Sha512), None)
        }
        SignatureScheme::Ed25519 => (SignatureAlgorithm::Ed25519, None),
        SignatureScheme::RsaPkcs1Sha256
        | SignatureScheme::RsaPkcs1Sha384
        | SignatureScheme::RsaPkcs1Sha512 => return None,
    })
}

/// Verifies a `CertificateVerify` signature with the leaf's key; the scheme must be one this side
/// offered and must fit the key.
///
/// # Errors
///
/// [`Error::Local`] with [`Alert::DecryptError`] when the signature does not verify, and with
/// [`Alert::IllegalParameter`] for a scheme the key does not take.
pub fn verify(
    leaf: &Certificate,
    scheme: u16,
    offered: &[SignatureScheme],
    content: &[u8],
    sig: &[u8],
) -> Result<(), Error> {
    let illegal = Error::Local(Alert::IllegalParameter);
    let s = SignatureScheme::from_code(scheme).filter(|s| offered.contains(s)).ok_or(illegal)?;
    let (alg, curve_needed) = algorithm(s).ok_or(illegal)?;
    if let (Some(c), PublicKey::Ec { curve, .. }) = (curve_needed, &leaf.public_key)
        && c != *curve
    {
        return Err(illegal);
    }
    leaf.public_key.verify(alg, content, sig).map_err(|e| match e {
        teseladb_x509::Error::InvalidKey => illegal,
        _ => Error::Local(Alert::DecryptError),
    })
}

/// The scheme this side signs with: the first of `peer` that fits `key`.
#[must_use]
pub fn choose_scheme(key: &PrivateKey, peer: &[u16]) -> Option<SignatureScheme> {
    let fits = |s: SignatureScheme| match (key, s) {
        (PrivateKey::Ec(k), SignatureScheme::EcdsaP256Sha256) => k.curve() == NamedCurve::P256,
        (PrivateKey::Ec(k), SignatureScheme::EcdsaP384Sha384) => k.curve() == NamedCurve::P384,
        (PrivateKey::Ec(k), SignatureScheme::EcdsaP521Sha512) => k.curve() == NamedCurve::P521,
        (PrivateKey::Ed25519(..), SignatureScheme::Ed25519)
        | (
            PrivateKey::Rsa(..),
            SignatureScheme::RsaPssRsaeSha256
            | SignatureScheme::RsaPssRsaeSha384
            | SignatureScheme::RsaPssRsaeSha512,
        ) => true,
        _ => false,
    };
    peer.iter().filter_map(|&c| SignatureScheme::from_code(c)).find(|&s| fits(s))
}

/// Signs `content` with `scheme`; RSA-PSS takes its salt from `salt`.
///
/// # Errors
///
/// [`Error::Local`] with [`Alert::InternalError`] when the key cannot sign with the scheme.
pub fn sign(
    key: &PrivateKey,
    scheme: SignatureScheme,
    content: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>, Error> {
    let internal = || Error::Local(Alert::InternalError);
    match (key, algorithm(scheme)) {
        (PrivateKey::Ec(k), Some((SignatureAlgorithm::Ecdsa(_), _))) => {
            Ok(ecdsa_sig_to_der(&k.sign(content)))
        }
        (PrivateKey::Ed25519(k, _), Some((SignatureAlgorithm::Ed25519, _))) => {
            Ok(k.sign(content).to_vec())
        }
        (PrivateKey::Rsa(k, _), Some((SignatureAlgorithm::RsaPss { hash, salt_len }, _))) => {
            k.sign_pss(hash, content, &salt[..salt_len]).ok().ok_or(internal())
        }
        _ => Err(internal()),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_x509::Name;
    use teseladb_x509::generate::{self, CertificateParams};

    #[test]
    fn every_group_agrees() {
        for g in NamedGroup::ALL {
            let a = Ephemeral::generate(g, &[1; KEY_RANDOM_LEN]).expect("generate succeeds");
            let b = Ephemeral::generate(g, &[2; KEY_RANDOM_LEN]).expect("generate succeeds");
            assert_eq!(a.group(), g);
            assert_eq!(
                a.agree(&b.public()).expect("agree succeeds"),
                b.agree(&a.public()).expect("agree succeeds")
            );
            assert_eq!(a.agree(&[0; 3]).err(), Some(Error::Local(Alert::IllegalParameter)));
        }
        let x = Ephemeral::generate(NamedGroup::X25519, &[1; KEY_RANDOM_LEN])
            .expect("generate succeeds");
        assert_eq!(x.agree(&[0; 32]).err(), Some(Error::Local(Alert::IllegalParameter)));
    }

    #[test]
    fn signatures_by_key_type() {
        let keys = [
            PrivateKey::ed25519(&[3; 32]),
            PrivateKey::generate_ec(NamedCurve::P256, &[4; 40]).expect("generate_ec succeeds"),
            PrivateKey::generate_ec(NamedCurve::P384, &[5; 56]).expect("generate_ec succeeds"),
        ];
        let all: Vec<u16> = SignatureScheme::HANDSHAKE.iter().map(|s| s.code()).collect();
        for key in &keys {
            let p = CertificateParams::node([1; 16], Name::default(), 0, 1 << 40);
            let mut p = p;
            p.alt_names = vec![teseladb_x509::ext::GeneralName::Dns("x".into())];
            let cert = generate::self_signed(&p, key).expect("self_signed succeeds");
            let scheme = choose_scheme(key, &all).expect("choose_scheme succeeds");
            let content = verify_content(true, &[9; 32]);
            let sig = sign(key, scheme, &content, &[0; 64]).expect("sign succeeds");
            assert_eq!(
                verify(&cert, scheme.code(), &SignatureScheme::HANDSHAKE, &content, &sig),
                Ok(())
            );
            assert_eq!(
                verify(&cert, scheme.code(), &SignatureScheme::HANDSHAKE, b"other", &sig),
                Err(Error::Local(Alert::DecryptError))
            );
            assert_eq!(
                verify(&cert, scheme.code(), &[], &content, &sig),
                Err(Error::Local(Alert::IllegalParameter))
            );
            // A scheme of another key type.
            let other = if scheme == SignatureScheme::Ed25519 {
                SignatureScheme::EcdsaP256Sha256
            } else {
                SignatureScheme::Ed25519
            };
            assert_eq!(
                verify(&cert, other.code(), &SignatureScheme::HANDSHAKE, &content, &sig),
                Err(Error::Local(Alert::IllegalParameter))
            );
            assert_eq!(
                sign(key, SignatureScheme::RsaPkcs1Sha256, &content, &[]),
                Err(Error::Local(Alert::InternalError))
            );
        }
        // The curve of the scheme must be the key's curve.
        let p384 = &keys[2];
        let cert = generate::self_signed(
            &CertificateParams::node([1; 16], Name::default(), 0, 1 << 40),
            p384,
        );
        let cert = cert.expect("the value is present");
        assert_eq!(
            verify(
                &cert,
                SignatureScheme::EcdsaP256Sha256.code(),
                &SignatureScheme::HANDSHAKE,
                b"m",
                &[0; 8]
            ),
            Err(Error::Local(Alert::IllegalParameter))
        );
        assert_eq!(choose_scheme(&keys[0], &[0x0403]), None);
        assert_eq!(
            verify(
                &cert,
                SignatureScheme::RsaPkcs1Sha256.code(),
                &[SignatureScheme::RsaPkcs1Sha256],
                b"m",
                &[]
            ),
            Err(Error::Local(Alert::IllegalParameter))
        );
    }
}
