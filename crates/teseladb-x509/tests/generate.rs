//! The cluster PKI built with this crate (013 T007, FR-002, FR-004, FR-005): a CA, node and
//! client certificates issued from CSRs, a CRL, and their validation. The PEM files are also
//! written to `CARGO_TARGET_TMPDIR/pki` so that `openssl verify` can check them.

use teseladb_crypto::ec::NamedCurve;
use teseladb_x509::chain::verify_hostname;
use teseladb_x509::csr::CertificateRequest;
use teseladb_x509::ext::{GeneralName, key_usage};
use teseladb_x509::generate::{self, CertificateParams, Revocation};
use teseladb_x509::{
    Certificate, CrlCheck, Error, Name, PrivateKey, Purpose, VerifyOptions, oid, verify_chain,
};

const NOW: i64 = 1_800_000_000;
const YEAR: i64 = 365 * 86_400;

fn ec_key(curve: NamedCurve, seed: u8) -> PrivateKey {
    let random: Vec<u8> =
        (0..curve.scalar_len() + 8).map(|i| seed.wrapping_mul(31).wrapping_add(i as u8)).collect();
    PrivateKey::generate_ec(curve, &random).unwrap()
}

fn ca(key: &PrivateKey) -> Certificate {
    let mut params = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::ORGANIZATION, "TeselaDB"), (oid::COMMON_NAME, "TeselaDB CA")]),
        NOW - 60,
        NOW + 10 * YEAR,
    );
    params.ca = Some(Some(0));
    params.ext_key_usage.clear();
    generate::self_signed(&params, key).unwrap()
}

#[test]
fn ca_issues_node_and_client_certificates_from_csrs() {
    let ca_key = ec_key(NamedCurve::P256, 1);
    let ca = ca(&ca_key);
    assert!(ca.is_ca());
    assert_eq!(ca.key_usage, Some(key_usage::KEY_CERT_SIGN | key_usage::CRL_SIGN));
    assert_eq!(ca.subject_key_id, ca.authority_key_id);
    let node_key = ec_key(NamedCurve::P384, 2);
    let names = [GeneralName::Dns("node1.cluster".into()), GeneralName::Ip(vec![10, 0, 0, 1])];
    let subject = Name::build(&[(oid::ORGANIZATION, "TeselaDB"), (oid::COMMON_NAME, "node1")]);
    let csr = CertificateRequest::build(&subject, &node_key, &names).unwrap();
    let csr = CertificateRequest::from_der(csr.der()).unwrap();
    let mut params = CertificateParams::node([0x80; 16], csr.subject.clone(), NOW - 60, NOW + YEAR);
    params.alt_names = csr.alt_names.clone();
    let node = generate::issue(&params, &csr.public_key, &ca, &ca_key).unwrap();
    assert_eq!(node.serial[0] & 0x80, 0);
    assert_eq!(node.authority_key_id, ca.subject_key_id);
    let anchors = [ca.clone()];
    for purpose in [Purpose::ServerAuth, Purpose::ClientAuth] {
        let opts = VerifyOptions { purpose, ..VerifyOptions::new(NOW) };
        assert_eq!(verify_chain(&node, &[], &anchors, &opts).map(|p| p.len()), Ok(2));
    }
    assert_eq!(verify_hostname(&node, "node1.cluster"), Ok(()));
    assert_eq!(verify_hostname(&node, "10.0.0.1"), Ok(()));

    // An Ed25519 client and an empty subject with a critical SAN.
    let client_key = PrivateKey::ed25519(&[9; 32]);
    let mut cp = CertificateParams::node([0; 16], Name::default(), NOW - 60, NOW + YEAR);
    cp.alt_names = vec![GeneralName::Email("alice@example.com".into())];
    let client = generate::issue(&cp, &client_key.public_key(), &ca, &ca_key).unwrap();
    assert_eq!(client.serial, vec![1]);
    assert!(client.extension(oid::SUBJECT_ALT_NAME).unwrap().critical);
    assert_eq!(
        verify_chain(&client, &[], &anchors, &VerifyOptions::new(NOW)).map(|p| p.len()),
        Ok(2)
    );

    // A CA with pathLenConstraint 0 cannot sign another CA's certificate that signs a leaf.
    let sub_key = ec_key(NamedCurve::P256, 3);
    let mut sp = CertificateParams::node(
        [3; 16],
        Name::build(&[(oid::COMMON_NAME, "sub")]),
        NOW - 60,
        NOW + YEAR,
    );
    sp.ca = Some(None);
    let sub = generate::issue(&sp, &sub_key.public_key(), &ca, &ca_key).unwrap();
    let leaf = generate::issue(&params, &node_key.public_key(), &sub, &sub_key).unwrap();
    assert_eq!(
        verify_chain(&leaf, std::slice::from_ref(&sub), &anchors, &VerifyOptions::new(NOW)).err(),
        Some(Error::PathLength)
    );
    // A key that is not the issuer's is refused.
    assert_eq!(
        generate::issue(&params, &node_key.public_key(), &ca, &sub_key).err(),
        Some(Error::InvalidKey)
    );
    let mut backwards = params.clone();
    backwards.not_after = backwards.not_before - 1;
    assert_eq!(
        generate::issue(&backwards, &node_key.public_key(), &ca, &ca_key).err(),
        Some(Error::Expired)
    );

    // Revocation through a CRL of the CA.
    let crl = generate::issue_crl(
        &ca,
        &ca_key,
        NOW - 60,
        NOW + YEAR,
        7,
        &[Revocation { serial: node.serial.clone(), date: NOW - 30 }],
    )
    .unwrap();
    assert_eq!(crl.crl_number, Some(vec![7]));
    let empty = generate::issue_crl(&ca, &ca_key, NOW - 60, NOW + YEAR, 8, &[]).unwrap();
    assert!(empty.entries.is_empty());
    assert_eq!(
        generate::issue_crl(&ca, &sub_key, NOW, NOW + 1, 1, &[]).err(),
        Some(Error::InvalidKey)
    );
    let crls = [crl.clone()];
    let opts = VerifyOptions { crls: &crls, crl_check: CrlCheck::Leaf, ..VerifyOptions::new(NOW) };
    assert_eq!(verify_chain(&node, &[], &anchors, &opts).err(), Some(Error::Revoked));
    assert_eq!(verify_chain(&client, &[], &anchors, &opts).map(|p| p.len()), Ok(2));
    // A stale CRL proves nothing.
    let stale = VerifyOptions {
        crls: &crls,
        crl_check: CrlCheck::Leaf,
        ..VerifyOptions::new(NOW + 2 * YEAR)
    };
    assert!(verify_chain(&client, &[], std::slice::from_ref(&ca), &stale).is_err());

    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("pki");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ca.pem"), ca.to_pem()).unwrap();
    std::fs::write(dir.join("node.pem"), node.to_pem()).unwrap();
    std::fs::write(dir.join("client.pem"), client.to_pem()).unwrap();
    std::fs::write(dir.join("node.csr"), csr.to_pem()).unwrap();
    std::fs::write(
        dir.join("ca.crl"),
        teseladb_x509::pem::encode(teseladb_x509::pem::label::CRL, crl.der()),
    )
    .unwrap();
}
