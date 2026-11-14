//! The key schedule against every derivation of the RFC 8448 traces: each
//! HKDF-Extract, each HKDF-Expand-Label with its `HkdfLabel`, the traffic keys, the PSK binder
//! and the x25519 shared secrets.

mod common;

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::curve25519::x25519;
use teseladb_crypto::ec::{EcPrivateKey, NamedCurve};
use teseladb_tls::keyschedule::{expand, extract, label_info};

const H: HashAlgorithm = HashAlgorithm::Sha256;

#[test]
fn every_derivation_of_every_trace_matches() {
    let all = common::load();
    let mut checked = 0;
    for sec in 3..=7 {
        let values = common::section(&all, sec);
        let mut i = 0;
        while i < values.len() {
            let v = &values[i];
            let step: Vec<&common::Value> =
                values[i..].iter().take_while(|w| w.step == v.step && w.side == v.side).collect();
            let get = |label: &str| step.iter().find(|w| w.label == label).map(|w| w.bytes.clone());
            if let (Some(prk), Some(info), Some(expanded)) =
                (get("PRK"), get("info"), get("expanded"))
            {
                assert_eq!(expand(H, &prk, &info, expanded.len()).0, expanded, "{sec} {}", v.step);
                checked += 1;
                if let Some(fin) = get("finished") {
                    let hash = get("binder hash").unwrap_or_default();
                    if !hash.is_empty() {
                        assert_eq!(H.hmac(&expanded, &hash).as_ref(), fin.as_slice());
                        assert_eq!(
                            H.hash(&get("ClientHello prefix").expect("get succeeds")).as_ref(),
                            hash.as_slice()
                        );
                        checked += 1;
                    }
                }
            }
            for (inf, exp) in [("key info", "key expanded"), ("iv info", "iv expanded")] {
                if let (Some(prk), Some(info), Some(expanded)) = (get("PRK"), get(inf), get(exp)) {
                    assert_eq!(
                        expand(H, &prk, &info, expanded.len()).0,
                        expanded,
                        "{sec} {}",
                        v.step
                    );
                    let label = if inf == "key info" { &b"key"[..] } else { b"iv" };
                    assert_eq!(label_info(label, &[], expanded.len()), info);
                    checked += 1;
                }
            }
            if let (Some(ikm), Some(secret)) = (get("IKM"), get("secret")) {
                let salt = get("salt").unwrap_or_else(|| vec![0; 32]);
                assert_eq!(extract(H, &salt, &ikm).0, secret, "{sec} {}", v.step);
                checked += 1;
            }
            i += step.len();
        }
        // The (EC)DHE input of the handshake secret is x25519 of the two ephemeral keys.
        let ikm = common::find(&values, "server", "extract secret \"handshake\"", "IKM", 0);
        let n_client = values
            .iter()
            .filter(|v| {
                v.side == "client"
                    && v.step.starts_with("create an ephemeral")
                    && v.label == "private key"
            })
            .count();
        let client_priv =
            common::find(&values, "client", "create an ephemeral", "private key", n_client - 1);
        let server_pub = common::find(&values, "server", "create an ephemeral", "public key", 0);
        let shared = if server_pub.len() == 32 {
            x25519(
                &client_priv.clone().try_into().expect("try_into succeeds"),
                &server_pub.clone().try_into().expect("try_into succeeds"),
            )
            .to_vec()
        } else {
            EcPrivateKey::from_bytes(NamedCurve::P256, &client_priv)
                .expect("from_bytes succeeds")
                .agree(&server_pub)
                .expect("agree succeeds")
        };
        assert_eq!(shared, ikm, "section {sec}");
    }
    assert_eq!(checked, 117);
}
