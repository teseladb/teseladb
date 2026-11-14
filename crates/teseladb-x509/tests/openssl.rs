//! Interoperability with OpenSSL 3.6.3: keys in PKCS #1, SEC 1, PKCS #8 and
//! encrypted PKCS #8, a chain signed with RSA-PSS and ECDSA P-384 over P-256 and Ed25519 keys,
//! a CSR and a CRL, all written by `tests/data/openssl/generate.sh`.

use std::path::Path;

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::ec::NamedCurve;
use teseladb_x509::chain::verify_hostname;
use teseladb_x509::csr::CertificateRequest;
use teseladb_x509::ext::GeneralName;
use teseladb_x509::pem;
use teseladb_x509::{
    Certificate, Crl, CrlCheck, Error, PrivateKey, PublicKey, Purpose, SignatureAlgorithm,
    VerifyOptions, verify_chain,
};

/// 2027-01-01T00:00:00Z, inside the validity of every fixture.
const NOW: i64 = 1_798_761_600;
const PASSWORD: &[u8] = b"teseladb-test";

fn read(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/openssl").join(name),
    )
    .expect("read_to_string succeeds")
}

fn cert(name: &str) -> Certificate {
    Certificate::from_pem_all(&read(name)).expect("from_pem_all succeeds").remove(0)
}

fn key(name: &str, password: Option<&[u8]>) -> PrivateKey {
    PrivateKey::from_pem(&read(name), password).expect("from_pem succeeds")
}

#[test]
fn every_key_format_reads_and_matches_its_certificate() {
    let rsa = key("rsa-pkcs8.pem", None);
    assert_eq!(key("rsa-pkcs1.pem", None).public_key(), rsa.public_key());
    assert_eq!(key("rsa-enc.pem", Some(PASSWORD)).public_key(), rsa.public_key());
    assert_eq!(cert("root.pem").public_key, rsa.public_key());
    let ec256 = key("ec256-pkcs8.pem", None);
    assert_eq!(key("ec256-sec1.pem", None).public_key(), ec256.public_key());
    assert_eq!(cert("leaf.pem").public_key, ec256.public_key());
    let ec384 = key("ec384-pkcs8.pem", None);
    assert_eq!(key("ec384-enc.pem", Some(PASSWORD)).public_key(), ec384.public_key());
    assert_eq!(cert("inter.pem").public_key, ec384.public_key());
    assert_eq!(cert("client.pem").public_key, key("ed25519.pem", None).public_key());
    // Legacy OpenSSL encryption: AES-128/192/256-CBC and DES-EDE3-CBC.
    for (file, plain) in [
        ("rsa-legacy-aes256.pem", &rsa),
        ("rsa-legacy-des3.pem", &rsa),
        ("ec256-legacy-aes128.pem", &ec256),
        ("ec384-legacy-aes192.pem", &ec384),
    ] {
        assert_eq!(key(file, Some(PASSWORD)).public_key(), plain.public_key(), "{file}");
        assert_eq!(
            PrivateKey::from_pem(&read(file), Some(b"wrong")).err(),
            Some(Error::Decrypt),
            "{file}"
        );
        assert_eq!(PrivateKey::from_pem(&read(file), None).err(), Some(Error::Decrypt), "{file}");
    }
    // Wrong or missing password.
    assert_eq!(
        PrivateKey::from_pem(&read("rsa-enc.pem"), Some(b"wrong")).err(),
        Some(Error::Decrypt)
    );
    assert_eq!(PrivateKey::from_pem(&read("rsa-enc.pem"), None).err(), Some(Error::Decrypt));
    assert_eq!(PrivateKey::from_pem(&read("root.pem"), None).err(), Some(Error::Pem));
    // PKCS #8 written back reads the same key; an encrypted one too.
    for k in [&rsa, &ec256, &ec384, &key("ed25519.pem", None)] {
        let again = PrivateKey::from_pkcs8(&k.to_pkcs8()).expect("from_pkcs8 succeeds");
        assert_eq!(again.public_key(), k.public_key());
        let params =
            teseladb_x509::key::Pbes2Params { salt: b"saltsalt", iterations: 1000, iv: [7; 16] };
        let enc = k.to_encrypted_pkcs8(PASSWORD, params).expect("to_encrypted_pkcs8 succeeds");
        let text = pem::encode(pem::label::ENCRYPTED_PRIVATE_KEY, &enc);
        assert_eq!(
            PrivateKey::from_pem(&text, Some(PASSWORD)).expect("from_pem succeeds").public_key(),
            k.public_key()
        );
        // What `sign` produces verifies with the matching algorithm.
        let sig = k.sign(b"message").expect("sign succeeds");
        assert_eq!(k.public_key().verify(k.default_algorithm(), b"message", &sig), Ok(()));
        assert_eq!(
            k.public_key().verify(k.default_algorithm(), b"other", &sig),
            Err(Error::BadSignature)
        );
    }
    assert!(matches!(ec384.public_key(), PublicKey::Ec { curve: NamedCurve::P384, .. }));
}

#[test]
fn the_mixed_algorithm_chain_validates() {
    let root = cert("root.pem");
    let inter = cert("inter.pem");
    let leaf = cert("leaf.pem");
    assert!(matches!(
        root.signature_algorithm,
        SignatureAlgorithm::RsaPss { hash: HashAlgorithm::Sha256, salt_len: 32 }
    ));
    assert_eq!(leaf.signature_algorithm, SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha384));
    let anchors = [root.clone()];
    let pool = [inter.clone()];
    let server = VerifyOptions { purpose: Purpose::ServerAuth, ..VerifyOptions::new(NOW) };
    let path = verify_chain(&leaf, &pool, &anchors, &server).expect("verify_chain succeeds");
    assert_eq!(path.len(), 3);
    // The leaf is for servers only.
    let client = VerifyOptions { purpose: Purpose::ClientAuth, ..VerifyOptions::new(NOW) };
    assert_eq!(verify_chain(&leaf, &pool, &anchors, &client).err(), Some(Error::ExtKeyUsage));
    assert_eq!(verify_chain(&cert("client.pem"), &pool, &anchors, &client).map(|p| p.len()), Ok(3));
    // Without the intermediate, or with another anchor, there is no path.
    assert_eq!(verify_chain(&leaf, &[], &anchors, &server).err(), Some(Error::UnknownIssuer));
    assert_eq!(
        verify_chain(&leaf, &pool, std::slice::from_ref(&inter), &server).map(|p| p.len()),
        Ok(2)
    );
    // Dates.
    let early = VerifyOptions::new(root.not_before - 1);
    assert_eq!(verify_chain(&leaf, &pool, &anchors, &early).err(), Some(Error::NotYetValid));
    let late = VerifyOptions::new(root.not_after + 1);
    assert_eq!(verify_chain(&leaf, &pool, &anchors, &late).err(), Some(Error::Expired));
    // Host names and addresses of the SAN.
    for host in [
        "node1.teseladb.test",
        "NODE1.teseladb.test.",
        "a.nodes.teseladb.test",
        "127.0.0.1",
        "::1",
        "[::1]",
    ] {
        assert_eq!(verify_hostname(&leaf, host), Ok(()), "{host}");
    }
    for host in [
        "node2.teseladb.test",
        "nodes.teseladb.test",
        "a.b.nodes.teseladb.test",
        "127.0.0.2",
        "node1",
        "",
    ] {
        assert_eq!(verify_hostname(&leaf, host), Err(Error::HostnameMismatch), "{host}");
    }
    assert_eq!(leaf.subject.to_rfc4514(), "CN=node1,O=TeselaDB Test");
    assert_eq!(cert("client.pem").subject.to_rfc4514(), "CN=alice,OU=clients,O=TeselaDB Test");
}

#[test]
fn the_openssl_crl_revokes_the_client() {
    let crl = Crl::from_pem_all(&read("inter.crl")).expect("from_pem_all succeeds").remove(0);
    let anchors = [cert("root.pem")];
    let pool = [cert("inter.pem")];
    let crls = [crl];
    let opts = VerifyOptions { crls: &crls, crl_check: CrlCheck::Leaf, ..VerifyOptions::new(NOW) };
    assert_eq!(
        verify_chain(&cert("client.pem"), &pool, &anchors, &opts).err(),
        Some(Error::Revoked)
    );
    assert_eq!(verify_chain(&cert("leaf.pem"), &pool, &anchors, &opts).map(|p| p.len()), Ok(3));
    // Every certificate needs a CRL: the root has issued none for the intermediate.
    let all = VerifyOptions { crl_check: CrlCheck::All, ..opts.clone() };
    assert_eq!(
        verify_chain(&cert("leaf.pem"), &pool, &anchors, &all).err(),
        Some(Error::RevocationUnknown)
    );
}

#[test]
fn the_openssl_csr_verifies_and_carries_its_names() {
    let der = pem::parse_label(&read("leaf.csr"), pem::label::CERTIFICATE_REQUEST)
        .expect("parse_label succeeds")
        .remove(0);
    let csr = CertificateRequest::from_der(&der).expect("from_der succeeds");
    assert_eq!(csr.public_key, key("ec256-pkcs8.pem", None).public_key());
    assert_eq!(csr.alt_names.len(), 4);
    assert_eq!(csr.alt_names[0], GeneralName::Dns("node1.teseladb.test".into()));
    assert_eq!(csr.alt_names[2], GeneralName::Ip(vec![127, 0, 0, 1]));
    // A flipped signature byte is rejected.
    let mut bad = der.clone();
    let last = bad.len() - 3;
    bad[last] ^= 1;
    assert!(CertificateRequest::from_der(&bad).is_err());
}

#[test]
fn a_p521_root_with_ecdsa_sha512_validates_and_its_key_reads() {
    let root = cert("ec521-root.pem");
    let leaf = cert("ec521-leaf.pem");
    assert_eq!(root.signature_algorithm, SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha512));
    assert!(matches!(root.public_key, PublicKey::Ec { curve: NamedCurve::P521, .. }));
    let k = key("ec521-pkcs8.pem", None);
    assert_eq!(k.public_key(), root.public_key);
    let server = VerifyOptions { purpose: Purpose::ServerAuth, ..VerifyOptions::new(NOW) };
    let path = verify_chain(&leaf, &[], std::slice::from_ref(&root), &server)
        .expect("verify_chain succeeds");
    assert_eq!(path.len(), 2);
    // What this crate signs with the P-521 key verifies, and the key writes back to PKCS #8.
    let sig = k.sign(b"message").expect("sign succeeds");
    assert_eq!(k.public_key().verify(k.default_algorithm(), b"message", &sig), Ok(()));
    let again = PrivateKey::from_pem(&pem::encode(pem::label::PRIVATE_KEY, &k.to_pkcs8()), None);
    assert_eq!(again.expect("the value is present").public_key(), root.public_key);
}

/// A certificate signed with SHA-1 is refused as OpenSSL 3 refuses it at security level 1 (`CA
/// signature digest algorithm too weak`, measured on the 9.0.4 image of the reference server), unless
/// the caller allows weak digests (the NIST PKITS vectors are signed with SHA-1).
#[test]
fn a_certificate_signed_with_sha1_is_refused() {
    let leaf = cert("sha1-leaf.pem");
    let root = cert("root.pem");
    let opts = VerifyOptions::new(NOW);
    let err = verify_chain(&leaf, &[], std::slice::from_ref(&root), &opts).err();
    assert_eq!(err, Some(Error::WeakDigest));
    assert_eq!(Error::WeakDigest.to_string(), "CA signature digest algorithm too weak");
    let allowed = VerifyOptions { weak_digests: true, ..VerifyOptions::new(NOW) };
    assert!(verify_chain(&leaf, &[], std::slice::from_ref(&root), &allowed).is_ok());
}
