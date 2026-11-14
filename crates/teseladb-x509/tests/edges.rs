//! Malformed and unusual inputs that the vector suites do not reach: DER
//! rules of each structure, extension decoding, key formats, name constraints of every form,
//! policy corner cases and path building limits.

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::ec::NamedCurve;
use teseladb_x509::der::{self, tag};
use teseladb_x509::ext::{self, DistributionPointName, Extension, GeneralName};
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::key::Pbes2Params;
use teseladb_x509::{
    Certificate, Error, Name, PolicySettings, PrivateKey, PublicKey, SignatureAlgorithm,
    VerifyOptions, oid, verify_chain,
};

const NOW: i64 = 1_800_000_000;
const YEAR: i64 = 365 * 86_400;
/// A test policy OID (2.16.840.1.101.3.2.1.48.1).
const P1: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x02, 0x01, 0x30, 0x01];
const P2: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x02, 0x01, 0x30, 0x02];

fn ed_key(seed: u8) -> PrivateKey {
    PrivateKey::ed25519(&[seed; 32])
}

fn cn(name: &str) -> Name {
    Name::build(&[(oid::COMMON_NAME, name)])
}

fn ca_params(name: &str) -> CertificateParams {
    let mut p = CertificateParams::node([2; 16], cn(name), NOW - 60, NOW + YEAR);
    p.ca = Some(None);
    p.ext_key_usage.clear();
    p
}

fn signed(tbs: &[u8], key: &PrivateKey) -> Vec<u8> {
    der::sequence(&[
        tbs,
        &key.default_algorithm().to_der(),
        &der::bits(&key.sign(tbs).expect("sign succeeds")),
    ])
}

/// A self-issued `TBSCertificate` with an optional version field and `tail` after the key.
fn raw_tbs(version: Option<u64>, serial: &[u8], tail: &[Vec<u8>]) -> Vec<u8> {
    let key = ed_key(1);
    let mut parts: Vec<Vec<u8>> = Vec::new();
    if let Some(v) = version {
        parts.push(der::tlv(tag::context(0), &der::small_integer(v)));
    }
    parts.push(der::tlv(tag::INTEGER, serial));
    parts.push(key.default_algorithm().to_der());
    parts.push(cn("raw").der().to_vec());
    parts.push(der::sequence(&[
        &teseladb_x509::time::encode(NOW - 60),
        &teseladb_x509::time::encode(NOW + YEAR),
    ]));
    parts.push(cn("raw").der().to_vec());
    parts.push(key.public_key().to_spki());
    parts.extend(tail.iter().cloned());
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    der::sequence(&refs)
}

fn raw_cert(version: Option<u64>, tail: &[Vec<u8>]) -> Result<Certificate, Error> {
    Certificate::from_der(&signed(&raw_tbs(version, &[1], tail), &ed_key(1)))
}

fn ext_block(exts: &[Vec<u8>]) -> Vec<u8> {
    let refs: Vec<&[u8]> = exts.iter().map(Vec::as_slice).collect();
    der::tlv(tag::context(3), &der::sequence(&refs))
}

fn ext(o: &[u8], critical: bool, value: Vec<u8>) -> Vec<u8> {
    Extension { oid: o.to_vec(), critical, value }.to_der()
}

#[test]
fn certificate_structure_rules() {
    let uid = der::tlv(tag::context_primitive(1), &[0]);
    assert_eq!(raw_cert(None, std::slice::from_ref(&uid)).err(), Some(Error::Der));
    let v3 = raw_cert(Some(2), &[uid]).expect("raw_cert succeeds");
    assert_eq!(v3.version, 3);
    assert!(v3.extensions.is_empty());
    let ku = ext(oid::KEY_USAGE, true, der::named_bits(ext::key_usage::DIGITAL_SIGNATURE));
    assert_eq!(raw_cert(None, &[ext_block(std::slice::from_ref(&ku))]).err(), Some(Error::Der));
    assert_eq!(
        raw_cert(Some(1), &[ext_block(std::slice::from_ref(&ku))]).map(|c| c.version),
        Err(Error::Der)
    );
    assert_eq!(raw_cert(Some(2), &[ext_block(&[])]).err(), Some(Error::Der));
    assert_eq!(raw_cert(Some(3), &[]).err(), Some(Error::Der));
    assert_eq!(raw_cert(Some(0), &[]).err(), Some(Error::Der));
    assert_eq!(raw_cert(Some(1), &[]).map(|c| c.version), Ok(2));
    assert_eq!(
        raw_cert(Some(2), &[ext_block(&[ku.clone(), ku.clone()])]).err(),
        Some(Error::DuplicateExtension)
    );
    let zero_ku = ext(oid::KEY_USAGE, true, der::named_bits(0));
    assert_eq!(raw_cert(Some(2), &[ext_block(&[zero_ku])]).err(), Some(Error::Der));
    let explicit_false = der::sequence(&[
        &der::object_id(oid::KEY_USAGE),
        &der::boolean_value(false),
        &der::octets(&[]),
    ]);
    assert_eq!(raw_cert(Some(2), &[ext_block(&[explicit_false])]).err(), Some(Error::Der));
    let negative = signed(&raw_tbs(None, &[0x80], &[]), &ed_key(1));
    assert_eq!(Certificate::from_der(&negative).err(), Some(Error::Der));
    // The inner and outer algorithms must agree.
    let tbs = raw_tbs(None, &[1], &[]);
    let other = SignatureAlgorithm::Ecdsa(HashAlgorithm::Sha256).to_der();
    let mismatched = der::sequence(&[&tbs, &other, &der::bits(&[0; 64])]);
    assert_eq!(Certificate::from_der(&mismatched).err(), Some(Error::Der));
    // An unknown critical extension makes the certificate unusable in a path.
    let unknown = ext(&[0x2a, 0x03, 0x04], true, der::null());
    let issuer_alt =
        ext(oid::ISSUER_ALT_NAME, false, der::sequence(&[&GeneralName::Dns("x".into()).to_der()]));
    let c = raw_cert(Some(2), &[ext_block(&[unknown, issuer_alt])]).expect("raw_cert succeeds");
    assert!(c.unknown_critical);
    assert_eq!(c.fingerprint_sha256(), teseladb_crypto::sha256(c.der()));
    assert_eq!(c.subject.rdns().len(), 1);
    let key = ed_key(2);
    let leaf = generate::issue(
        &CertificateParams::node([3; 16], cn("l"), NOW - 60, NOW + YEAR),
        &key.public_key(),
        &c,
        &ed_key(1),
    );
    // `c` is not a CA (no basicConstraints): it cannot be an anchor that issues.
    assert_eq!(
        verify_chain(
            &leaf.expect("the value is present"),
            &[],
            std::slice::from_ref(&c),
            &VerifyOptions::new(NOW)
        )
        .err(),
        Some(Error::NotCa)
    );
}

#[test]
fn extension_decoders_reject_malformed_values() {
    let seq = |parts: &[Vec<u8>]| {
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        der::sequence(&refs)
    };
    assert_eq!(ext::parse_general_names(&[]), Err(Error::Der));
    assert_eq!(ext::parse_general_names(&[0x89, 0x00]), Err(Error::Der));
    assert_eq!(ext::parse_general_names(&[0x82, 0x01, 0xc3]), Err(Error::Der));
    let other = ext::parse_general_names(&[0xa0, 0x00, 0x88, 0x01, 0x2a])
        .expect("parse_general_names succeeds");
    assert_eq!(other, vec![GeneralName::Other(0xa0, vec![]), GeneralName::Other(0x88, vec![0x2a])]);
    for g in
        other.iter().chain([GeneralName::Uri("u".into()), GeneralName::Directory(cn("d"))].iter())
    {
        assert_eq!(
            ext::parse_general_names(&g.to_der()).expect("parse_general_names succeeds"),
            vec![g.clone()]
        );
    }
    assert_eq!(ext::parse_basic_constraints(&seq(&[der::boolean_value(false)])), Err(Error::Der));
    let dns = GeneralName::Dns("a".into()).to_der();
    let subtree = |extra: Vec<u8>| seq(&[dns.clone(), extra]);
    let nc = |subtrees: Vec<u8>| seq(&[der::tlv(tag::context(0), &subtrees)]);
    let min1 = subtree(der::tlv(tag::context_primitive(0), &[1]));
    assert_eq!(ext::parse_name_constraints(&nc(min1)), Err(Error::UnknownCriticalExtension));
    let max = subtree(der::tlv(tag::context_primitive(1), &[1]));
    assert_eq!(ext::parse_name_constraints(&nc(max)), Err(Error::UnknownCriticalExtension));
    let min0 = subtree(der::tlv(tag::context_primitive(0), &[0]));
    assert!(ext::parse_name_constraints(&nc(min0)).is_ok());
    assert_eq!(ext::parse_name_constraints(&nc(Vec::new())), Err(Error::Der));
    assert_eq!(ext::parse_name_constraints(&seq(&[])), Err(Error::Der));
    let pol = |o: &[u8]| seq(&[der::object_id(o)]);
    assert_eq!(ext::parse_policies(&seq(&[pol(P1), pol(P1)])), Err(Error::Der));
    assert_eq!(ext::parse_policies(&seq(&[])), Err(Error::Der));
    assert_eq!(ext::parse_policy_mappings(&seq(&[])), Err(Error::Der));
    assert_eq!(ext::parse_policy_constraints(&seq(&[])), Err(Error::Der));
    assert_eq!(ext::parse_ext_key_usage(&seq(&[])), Err(Error::Der));
    // Distribution points.
    let dp = |inner: Vec<u8>| seq(&[seq(&[der::tlv(tag::context(0), &inner)])]);
    assert_eq!(ext::parse_distribution_points(&dp(der::tlv(0xa1, &[]))), Err(Error::Der));
    assert_eq!(ext::parse_distribution_points(&dp(der::tlv(0xa2, &[]))), Err(Error::Der));
    assert_eq!(ext::parse_distribution_points(&seq(&[seq(&[])])), Err(Error::Der));
    assert_eq!(ext::parse_distribution_points(&seq(&[])), Err(Error::Der));
    let rdn =
        der::sequence(&[&der::object_id(oid::COMMON_NAME), &der::tlv(tag::UTF8_STRING, b"CRL1")]);
    let rel = ext::parse_distribution_points(&dp(der::tlv(0xa1, &rdn)))
        .expect("parse_distribution_points succeeds");
    let issuer = Name::build(&[(oid::COUNTRY, "ES")]);
    let resolved =
        rel[0].name.as_ref().expect("as_ref succeeds").resolve(&issuer).expect("resolve succeeds");
    assert!(matches!(&resolved[0], GeneralName::Directory(n) if n.to_rfc4514() == "CN=CRL1,C=ES"));
    assert!(matches!(rel[0].name, Some(DistributionPointName::Relative(_))));
    // Issuing distribution points.
    let flag = |n: u8, v: bool| der::tlv(tag::context_primitive(n), &[if v { 0xff } else { 0 }]);
    assert_eq!(ext::parse_idp(&seq(&[flag(1, false)])), Err(Error::Der));
    assert_eq!(ext::parse_idp(&seq(&[flag(1, true), flag(2, true)])), Err(Error::Der));
    assert!(ext::parse_idp(&seq(&[flag(4, true)])).expect("parse_idp succeeds").indirect);
    let aki = seq(&[
        der::tlv(tag::context_primitive(0), &[1]),
        der::tlv(tag::context(1), &dns),
        der::tlv(tag::context_primitive(2), &[5]),
    ]);
    assert_eq!(ext::parse_authority_key_id(&aki), Ok(Some(vec![1])));
}

fn alg_id(o: &[u8], params: &[Vec<u8>]) -> Vec<u8> {
    let mut parts = vec![der::object_id(o)];
    parts.extend(params.iter().cloned());
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    der::read_single(&der::sequence(&refs), tag::SEQUENCE)
        .expect("read_single succeeds")
        .value
        .to_vec()
}

#[test]
fn signature_algorithms_round_trip_and_reject_bad_parameters() {
    let hashes = [
        HashAlgorithm::Sha1,
        HashAlgorithm::Sha224,
        HashAlgorithm::Sha256,
        HashAlgorithm::Sha384,
        HashAlgorithm::Sha512,
    ];
    let mut algs = vec![SignatureAlgorithm::Ed25519];
    for h in hashes {
        algs.extend([
            SignatureAlgorithm::RsaPkcs1(h),
            SignatureAlgorithm::Ecdsa(h),
            SignatureAlgorithm::RsaPss { hash: h, salt_len: 20 },
        ]);
    }
    for a in algs {
        let enc = a.to_der();
        assert_eq!(
            SignatureAlgorithm::from_der(
                der::read_single(&enc, tag::SEQUENCE).expect("read_single succeeds").value
            ),
            Ok(a)
        );
    }
    let absent = alg_id(oid::SHA256_WITH_RSA, &[]);
    assert_eq!(
        SignatureAlgorithm::from_der(&absent),
        Ok(SignatureAlgorithm::RsaPkcs1(HashAlgorithm::Sha256))
    );
    let bad_null = alg_id(oid::SHA256_WITH_RSA, &[der::tlv(tag::NULL, &[0])]);
    assert_eq!(SignatureAlgorithm::from_der(&bad_null), Err(Error::Der));
    assert_eq!(
        SignatureAlgorithm::from_der(&alg_id(oid::DSA_SHA1, &[])),
        Err(Error::UnsupportedAlgorithm)
    );
    let pss = |fields: &[Vec<u8>]| {
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        SignatureAlgorithm::from_der(&alg_id(oid::RSASSA_PSS, &[der::sequence(&refs)]))
    };
    let h256 = der::sequence(&[&der::object_id(oid::SHA256), &der::null()]);
    let h384 = der::sequence(&[&der::object_id(oid::SHA384)]);
    let mgf =
        |h: &[u8]| der::tlv(tag::context(1), &der::sequence(&[&der::object_id(oid::MGF1), h]));
    assert_eq!(
        pss(&[]),
        Ok(SignatureAlgorithm::RsaPss { hash: HashAlgorithm::Sha1, salt_len: 20 })
    );
    assert_eq!(pss(&[der::tlv(tag::context(0), &h256)]), Err(Error::UnsupportedAlgorithm));
    assert_eq!(
        pss(&[der::tlv(tag::context(0), &h256), mgf(&h384)]),
        Err(Error::UnsupportedAlgorithm)
    );
    let not_mgf1 = der::tlv(tag::context(1), &der::sequence(&[&der::object_id(oid::SHA1), &h256]));
    assert_eq!(pss(&[not_mgf1]), Err(Error::UnsupportedAlgorithm));
    let trailer = |v: u64| der::tlv(tag::context(3), &der::small_integer(v));
    assert_eq!(pss(&[trailer(2)]), Err(Error::UnsupportedAlgorithm));
    assert!(pss(&[trailer(1)]).is_ok());
    let bad_hash = der::sequence(&[&der::object_id(oid::SHA256), &der::tlv(tag::NULL, &[1])]);
    assert_eq!(pss(&[der::tlv(tag::context(0), &bad_hash)]), Err(Error::Der));
    let unknown_hash = der::sequence(&[&der::object_id(&[0x2a, 0x03])]);
    assert_eq!(pss(&[der::tlv(tag::context(0), &unknown_hash)]), Err(Error::UnsupportedAlgorithm));
}

fn spki(alg: &[Vec<u8>], key: &[u8]) -> Vec<u8> {
    let refs: Vec<&[u8]> = alg.iter().map(Vec::as_slice).collect();
    der::sequence(&[&der::sequence(&refs), &der::bits(key)])
}

#[test]
#[expect(clippy::too_many_lines, reason = "every key format and its failures, listed in turn")]
fn public_and_private_key_formats() {
    let ec = PrivateKey::generate_ec(NamedCurve::P256, &[7; 40]).expect("generate_ec succeeds");
    let PublicKey::Ec { point, .. } = ec.public_key() else { panic!("EC key") };
    let ec_alg = vec![der::object_id(oid::EC_PUBLIC_KEY), der::object_id(oid::P256)];
    assert_eq!(PublicKey::from_spki(&spki(&ec_alg, &point[..33])).err(), Some(Error::InvalidKey));
    let mut compressed = point.clone();
    compressed[0] = 0x02;
    assert_eq!(PublicKey::from_spki(&spki(&ec_alg, &compressed)).err(), Some(Error::InvalidKey));
    // A P-256 point under the P-521 OID has the wrong length; secp256k1 is not supported.
    let p521 = vec![der::object_id(oid::EC_PUBLIC_KEY), der::object_id(oid::P521)];
    assert_eq!(PublicKey::from_spki(&spki(&p521, &point)).err(), Some(Error::InvalidKey));
    let k256 =
        vec![der::object_id(oid::EC_PUBLIC_KEY), der::object_id(&[0x2b, 0x81, 0x04, 0x00, 0x0a])];
    assert_eq!(PublicKey::from_spki(&spki(&k256, &point)).err(), Some(Error::UnsupportedAlgorithm));
    assert_eq!(
        PublicKey::from_spki(&spki(&[der::object_id(oid::ED25519)], &[1; 31])).err(),
        Some(Error::InvalidKey)
    );
    assert_eq!(
        PublicKey::from_spki(&spki(&[der::object_id(oid::DSA_SHA1)], &[1])).err(),
        Some(Error::UnsupportedAlgorithm)
    );
    let rsa = PrivateKey::from_pem(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/openssl/rsa-pkcs1.pem"
        ))
        .expect("read_to_string succeeds"),
        None,
    )
    .expect("from_pem succeeds");
    let PublicKey::Rsa(rsa_pub) = rsa.public_key() else { panic!("RSA key") };
    let rsa_body = der::sequence(&[
        &der::unsigned_integer(&rsa_pub.modulus()),
        &der::unsigned_integer(&rsa_pub.exponent()),
    ]);
    let pss_key = PublicKey::from_spki(&spki(&[der::object_id(oid::RSASSA_PSS)], &rsa_body))
        .expect("from_spki succeeds");
    assert_eq!(pss_key, rsa.public_key());
    assert_eq!(
        PublicKey::from_spki(&spki(&[der::object_id(oid::RSA_ENCRYPTION)], &[0x30, 0x00])).err(),
        Some(Error::Der)
    );
    // A key and an algorithm of different families.
    assert_eq!(
        ec.public_key().verify(SignatureAlgorithm::Ed25519, b"m", &[0; 64]),
        Err(Error::InvalidKey)
    );
    assert_eq!(
        format!("{:?} {:?} {:?}", rsa, ec, ed_key(1)),
        "PrivateKey(RSA) PrivateKey(P256) PrivateKey(Ed25519)"
    );
    assert_eq!(rsa.default_algorithm(), SignatureAlgorithm::RsaPkcs1(HashAlgorithm::Sha256));

    // SEC 1: the curve inside and outside must agree, the public key must match, and a key
    // without either curve is malformed.
    let d = ec.to_pkcs8();
    let inner = |fields: &[Vec<u8>]| {
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        der::sequence(&refs)
    };
    let PrivateKey::Ec(k) = &ec else { panic!("EC key") };
    let scalar = der::octets(&k.to_bytes());
    let one = der::small_integer(1);
    assert!(
        PrivateKey::from_sec1(&inner(&[
            one.clone(),
            scalar.clone(),
            der::tlv(tag::context(0), &der::object_id(oid::P256))
        ]))
        .is_ok()
    );
    assert_eq!(
        PrivateKey::from_sec1(&inner(&[one.clone(), scalar.clone()])).err(),
        Some(Error::Der)
    );
    assert_eq!(
        PrivateKey::from_sec1(&inner(&[der::small_integer(2), scalar.clone()])).err(),
        Some(Error::Der)
    );
    let wrong_pub = der::tlv(tag::context(1), &der::bits(&[4; 65]));
    let with_curve = |extra: Vec<u8>| {
        inner(&[
            one.clone(),
            scalar.clone(),
            der::tlv(tag::context(0), &der::object_id(oid::P256)),
            extra,
        ])
    };
    assert_eq!(PrivateKey::from_sec1(&with_curve(wrong_pub)).err(), Some(Error::InvalidKey));
    let p8 = |alg: Vec<u8>, key: Vec<u8>, version: u64| {
        der::sequence(&[&der::small_integer(version), &alg, &der::octets(&key)])
    };
    let p384_alg =
        der::sequence(&[&der::object_id(oid::EC_PUBLIC_KEY), &der::object_id(oid::P384)]);
    let sec1_p256 = inner(&[
        one.clone(),
        scalar.clone(),
        der::tlv(tag::context(0), &der::object_id(oid::P256)),
    ]);
    assert_eq!(PrivateKey::from_pkcs8(&p8(p384_alg, sec1_p256, 0)).err(), Some(Error::InvalidKey));
    assert_eq!(
        PrivateKey::from_pkcs8(&p8(der::sequence(&[&der::object_id(oid::DSA_SHA1)]), vec![], 0))
            .err(),
        Some(Error::UnsupportedAlgorithm)
    );
    let v2 = der::read_single(&d, tag::SEQUENCE).expect("read_single succeeds").value.to_vec();
    let mut v2 = v2;
    v2[2] = 2;
    assert_eq!(PrivateKey::from_pkcs8(&der::tlv(tag::SEQUENCE, &v2)).err(), Some(Error::Der));
    let pkcs1 = der::sequence(&[&der::small_integer(1)]);
    assert_eq!(PrivateKey::from_pkcs1(&pkcs1).err(), Some(Error::UnsupportedAlgorithm));
    assert_eq!(PrivateKey::generate_ec(NamedCurve::P256, &[1; 8]).err(), Some(Error::InvalidKey));
}

fn pbes2(
    prf: Option<&[u8]>,
    iterations: u64,
    key_len: Option<u64>,
    cipher: &[u8],
    kdf: &[u8],
    scheme: &[u8],
) -> Vec<u8> {
    let mut kp = vec![der::octets(b"salt"), der::small_integer(iterations)];
    if let Some(l) = key_len {
        kp.push(der::small_integer(l));
    }
    if let Some(p) = prf {
        kp.push(der::sequence(&[&der::object_id(p), &der::null()]));
    }
    let kp_refs: Vec<&[u8]> = kp.iter().map(Vec::as_slice).collect();
    let kdf = der::sequence(&[&der::object_id(kdf), &der::sequence(&kp_refs)]);
    let enc = der::sequence(&[&der::object_id(cipher), &der::octets(&[0; 16])]);
    let alg = der::sequence(&[&der::object_id(scheme), &der::sequence(&[&kdf, &enc])]);
    der::sequence(&[&alg, &der::octets(&[0; 32])])
}

#[test]
fn encrypted_keys_reject_bad_parameters() {
    let pw = b"pw";
    let case = |prf, it, len, cipher, kdf, scheme| {
        PrivateKey::from_encrypted_pkcs8(&pbes2(prf, it, len, cipher, kdf, scheme), pw).err()
    };
    let (aes, kdf, scheme) = (oid::AES256_CBC, oid::PBKDF2, oid::PBES2);
    assert_eq!(case(None, 0, None, aes, kdf, scheme), Some(Error::UnsupportedAlgorithm));
    assert_eq!(case(None, 20_000_000, None, aes, kdf, scheme), Some(Error::UnsupportedAlgorithm));
    assert_eq!(case(None, 1, Some(16), aes, kdf, scheme), Some(Error::UnsupportedAlgorithm));
    assert_eq!(
        case(Some(&[0x2a, 0x03]), 1, None, aes, kdf, scheme),
        Some(Error::UnsupportedAlgorithm)
    );
    assert_eq!(case(None, 1, None, oid::SHA1, kdf, scheme), Some(Error::UnsupportedAlgorithm));
    assert_eq!(case(None, 1, None, aes, oid::SHA1, scheme), Some(Error::UnsupportedAlgorithm));
    assert_eq!(case(None, 1, None, aes, kdf, oid::SHA1), Some(Error::UnsupportedAlgorithm));
    // Valid parameters with garbage data: the padding or the inner key is wrong.
    for prf in [oid::HMAC_SHA1, oid::HMAC_SHA256, oid::HMAC_SHA384, oid::HMAC_SHA512] {
        assert_eq!(
            case(Some(prf), 1, Some(16), oid::AES128_CBC, kdf, scheme),
            Some(Error::Decrypt)
        );
        assert_eq!(
            case(Some(prf), 1, Some(24), oid::AES192_CBC, kdf, scheme),
            Some(Error::Decrypt)
        );
    }
    let k = ed_key(4);
    for it in [0, 20_000_000] {
        let params = Pbes2Params { salt: b"s", iterations: it, iv: [0; 16] };
        assert_eq!(k.to_encrypted_pkcs8(pw, params).err(), Some(Error::UnsupportedAlgorithm));
    }
}

fn policies_ext(list: &[&[u8]]) -> Extension {
    let infos: Vec<Vec<u8>> = list.iter().map(|o| der::sequence(&[&der::object_id(o)])).collect();
    let refs: Vec<&[u8]> = infos.iter().map(Vec::as_slice).collect();
    Extension {
        oid: oid::CERTIFICATE_POLICIES.to_vec(),
        critical: false,
        value: der::sequence(&refs),
    }
}

#[test]
fn policy_trees_with_any_policy_beside_explicit_ones() {
    let root_key = ed_key(10);
    let root = generate::self_signed(&ca_params("root"), &root_key).expect("self_signed succeeds");
    let ca_key = ed_key(11);
    let mut cap = ca_params("ca");
    cap.extra = vec![policies_ext(&[oid::ANY_POLICY])];
    let ca = generate::issue(&cap, &ca_key.public_key(), &root, &root_key).expect("issue succeeds");
    let mut lp = CertificateParams::node([5; 16], cn("leaf"), NOW - 60, NOW + YEAR);
    lp.extra = vec![policies_ext(&[P1, oid::ANY_POLICY])];
    let leaf =
        generate::issue(&lp, &ed_key(12).public_key(), &ca, &ca_key).expect("issue succeeds");
    let anchors = [root.clone()];
    let pool = [ca.clone()];
    let opts = |policies: Vec<&[u8]>, explicit| VerifyOptions {
        policy: PolicySettings {
            initial_policies: Some(policies.iter().map(|p| p.to_vec()).collect()),
            explicit,
            ..PolicySettings::default()
        },
        ..VerifyOptions::new(NOW)
    };
    assert!(verify_chain(&leaf, &pool, &anchors, &opts(vec![P1, P2], true)).is_ok());
    // A CA asserting P1 above a leaf asserting P1 and anyPolicy.
    let mut ca1_params = ca_params("ca1");
    ca1_params.extra = vec![policies_ext(&[P1])];
    let ca1 = generate::issue(&ca1_params, &ca_key.public_key(), &root, &root_key)
        .expect("issue succeeds");
    let leaf1 =
        generate::issue(&lp, &ed_key(12).public_key(), &ca1, &ca_key).expect("issue succeeds");
    assert!(
        verify_chain(&leaf1, std::slice::from_ref(&ca1), &anchors, &opts(vec![P1], true)).is_ok()
    );
    assert_eq!(
        verify_chain(&leaf1, &[ca1], &anchors, &opts(vec![P2], true)).err(),
        Some(Error::Policy)
    );
}

#[test]
fn path_building_limits_and_loops() {
    let root_key = ed_key(20);
    let root = generate::self_signed(&ca_params("root"), &root_key).expect("self_signed succeeds");
    let a_key = ed_key(21);
    let a = generate::issue(&ca_params("a"), &a_key.public_key(), &root, &root_key)
        .expect("issue succeeds");
    let leaf = generate::issue(
        &CertificateParams::node([6; 16], cn("leaf"), NOW - 60, NOW + YEAR),
        &ed_key(22).public_key(),
        &a,
        &a_key,
    )
    .expect("issue succeeds");
    let anchors = [root.clone()];
    let short = VerifyOptions { max_depth: 2, ..VerifyOptions::new(NOW) };
    assert_eq!(
        verify_chain(&leaf, std::slice::from_ref(&a), &anchors, &short).err(),
        Some(Error::PathLength)
    );
    // Two CAs that certify each other and no anchor above them: the search ends.
    let b_key = ed_key(23);
    let mut a_params = ca_params("a");
    a_params.serial = [9; 16];
    let b =
        generate::issue(&ca_params("b"), &b_key.public_key(), &a, &a_key).expect("issue succeeds");
    let a_by_b =
        generate::issue(&a_params, &a_key.public_key(), &b, &b_key).expect("issue succeeds");
    let other_root =
        generate::self_signed(&ca_params("other"), &ed_key(24)).expect("self_signed succeeds");
    let pool = [b, a_by_b];
    assert_eq!(
        verify_chain(&leaf, &pool, &[other_root], &VerifyOptions::new(NOW)).err(),
        Some(Error::UnknownIssuer)
    );
    // An anchor whose keyUsage lacks keyCertSign.
    let mut bad = ca_params("bad");
    bad.key_usage = Some(ext::key_usage::CRL_SIGN);
    let bad_root = generate::self_signed(&bad, &root_key).expect("self_signed succeeds");
    let leaf2 = generate::issue(
        &CertificateParams::node([7; 16], cn("l2"), NOW - 60, NOW + YEAR),
        &ed_key(25).public_key(),
        &bad_root,
        &root_key,
    )
    .expect("issue succeeds");
    assert_eq!(
        verify_chain(&leaf2, &[], &[bad_root], &VerifyOptions::new(NOW)).err(),
        Some(Error::KeyUsage)
    );
    // An issuer without a subject key identifier: the AKI is computed from its key.
    let mut no_ski = root.clone();
    no_ski.subject_key_id = None;
    let leaf3 = generate::issue(
        &CertificateParams::node([8; 16], cn("l3"), NOW - 60, NOW + YEAR),
        &ed_key(26).public_key(),
        &no_ski,
        &root_key,
    )
    .expect("issue succeeds");
    assert_eq!(leaf3.authority_key_id, root.subject_key_id);
}

fn nc_ext(permitted: &[GeneralName], excluded: &[GeneralName]) -> Extension {
    let subtrees = |names: &[GeneralName]| {
        let s: Vec<Vec<u8>> = names.iter().map(|g| der::sequence(&[&g.to_der()])).collect();
        let refs: Vec<&[u8]> = s.iter().map(Vec::as_slice).collect();
        refs.concat()
    };
    let mut parts = Vec::new();
    if !permitted.is_empty() {
        parts.push(der::tlv(tag::context(0), &subtrees(permitted)));
    }
    if !excluded.is_empty() {
        parts.push(der::tlv(tag::context(1), &subtrees(excluded)));
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Extension { oid: oid::NAME_CONSTRAINTS.to_vec(), critical: true, value: der::sequence(&refs) }
}

#[test]
fn name_constraints_of_every_form() {
    let root_key = ed_key(30);
    let mut rp = ca_params("root");
    rp.extra = vec![nc_ext(
        &[
            GeneralName::Dns("example.com".into()),
            GeneralName::Email(".example.com".into()),
            GeneralName::Email("boss@corp.com".into()),
            GeneralName::Email("corp.org".into()),
            GeneralName::Uri(".example.com".into()),
            GeneralName::Ip(vec![10, 0, 0, 0, 255, 0, 0, 0]),
            GeneralName::Other(0x88, vec![0x2a]),
        ],
        &[
            GeneralName::Dns("bad.example.com".into()),
            GeneralName::Directory(Name::build(&[(oid::COMMON_NAME, "evil")])),
        ],
    )];
    let root = generate::self_signed(&rp, &root_key).expect("self_signed succeeds");
    let check = |subject: Name, sans: Vec<GeneralName>| {
        let mut p = CertificateParams::node([4; 16], subject, NOW - 60, NOW + YEAR);
        p.alt_names = sans;
        let leaf = generate::issue(&p, &ed_key(31).public_key(), &root, &root_key)
            .expect("issue succeeds");
        verify_chain(&leaf, &[], std::slice::from_ref(&root), &VerifyOptions::new(NOW)).map(|_| ())
    };
    let ok = |s: Vec<GeneralName>| check(Name::default(), s);
    assert_eq!(
        ok(vec![GeneralName::Dns("a.example.com".into()), GeneralName::Dns("Example.COM.".into())]),
        Ok(())
    );
    assert_eq!(ok(vec![GeneralName::Dns("example.org".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Dns("x.bad.example.com".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Email("u@mail.example.com".into())]), Ok(()));
    assert_eq!(ok(vec![GeneralName::Email("u@example.com".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Email("boss@CORP.com".into())]), Ok(()));
    assert_eq!(ok(vec![GeneralName::Email("other@corp.com".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Email("u@corp.org".into())]), Ok(()));
    assert_eq!(ok(vec![GeneralName::Email("no-at-sign".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Uri("https://user@www.example.com:8443/p?q".into())]), Ok(()));
    assert_eq!(
        ok(vec![GeneralName::Uri("https://example.com/".into())]),
        Err(Error::NameConstraints)
    );
    assert_eq!(ok(vec![GeneralName::Uri("https://[::1]/".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Uri("urn:x".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Uri("https:///path".into())]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Ip(vec![10, 1, 2, 3])]), Ok(()));
    assert_eq!(ok(vec![GeneralName::Ip(vec![11, 1, 2, 3])]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Ip(vec![0; 16])]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Other(0x88, vec![0x2a])]), Err(Error::NameConstraints));
    assert_eq!(ok(vec![GeneralName::Other(0xa0, vec![])]), Ok(()));
    assert_eq!(
        check(Name::build(&[(oid::COMMON_NAME, "evil")]), vec![]),
        Err(Error::NameConstraints)
    );
    let email_subject =
        Name::build(&[(oid::COMMON_NAME, "x"), (oid::EMAIL_ADDRESS, "u@elsewhere.net")]);
    assert_eq!(check(email_subject.clone(), vec![]), Err(Error::NameConstraints));
    // With an email in the SAN, the subject's emailAddress is not checked.
    assert_eq!(check(email_subject, vec![GeneralName::Email("u@a.example.com".into())]), Ok(()));
    // An empty DNS constraint permits every name.
    let mut ep = ca_params("e");
    ep.extra = vec![nc_ext(&[GeneralName::Dns(String::new())], &[])];
    let e_root = generate::self_signed(&ep, &root_key).expect("self_signed succeeds");
    let mut p = CertificateParams::node([4; 16], Name::default(), NOW - 60, NOW + YEAR);
    p.alt_names = vec![GeneralName::Dns("any.where".into())];
    let leaf =
        generate::issue(&p, &ed_key(31).public_key(), &e_root, &root_key).expect("issue succeeds");
    assert!(verify_chain(&leaf, &[], &[e_root], &VerifyOptions::new(NOW)).is_ok());
}

#[test]
fn host_name_patterns() {
    let key = ed_key(40);
    let mut p = CertificateParams::node([4; 16], cn("h"), NOW - 60, NOW + YEAR);
    p.alt_names = vec![
        GeneralName::Dns("*.com".into()),
        GeneralName::Dns("a*.example.org".into()),
        GeneralName::Dns(String::new()),
        GeneralName::Email("x@y.z".into()),
    ];
    let c = generate::self_signed(&p, &key).expect("self_signed succeeds");
    for host in ["x.com", "ab.example.org", ".example.org", "a"] {
        assert_eq!(
            teseladb_x509::verify_hostname(&c, host),
            Err(Error::HostnameMismatch),
            "{host}"
        );
    }
}
