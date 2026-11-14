//! The credentials the reference server 9.0.4 (its 9.0.4 image, 2026-10-30)
//! stored in `admin.system.users` for two users created with `createUser` and both mechanisms,
//! derived again here from the same password, salt and count (013 T044, FR-032, FR-013). The second
//! user checks the escaping of `,` and `=` that does not enter the derivation, and SASLprep of the
//! SCRAM-SHA-256 password.

use teseladb_auth::scram::{Credentials, Mechanism};
use teseladb_x509::pem::{base64_decode, base64_encode};

/// (user, password, mechanism, iterationCount, salt, storedKey, serverKey), as the reference server
/// wrote them.
const OBSERVED: [(&str, &str, &str, u32, &str, &str, &str); 4] = [
    (
        "alice",
        "pencil",
        "SCRAM-SHA-1",
        10000,
        "OlKjQcfVYNN9clHCBwFZ+g==",
        "52cRfEQ6urnc11B168GZQxEJpPU=",
        "8qYa4KYB+SLcOqiyR+NrUBM+WwU=",
    ),
    (
        "alice",
        "pencil",
        "SCRAM-SHA-256",
        15000,
        "z4duC3lRDS/pFx3DB64YiX3L63cMgkj4h7L90w==",
        "PhNBdkTpVTcCJbPFBZP12pFEcZ7ddZHT30HyoFpfYFM=",
        "Fh6VMbc7ZZknO9eWfW7aW9pv8frvi7siSIx0yWz+h8Q=",
    ),
    (
        "bob,=x",
        "I\u{00AD}X\u{2168}",
        "SCRAM-SHA-1",
        10000,
        "x4r9LBd9E9+0flnECiE8AQ==",
        "N37FM028briq8Dv4++/JVWFgqw0=",
        "gBhX1TeHQZyVK3D9C11H/pIQ7H8=",
    ),
    (
        "bob,=x",
        "I\u{00AD}X\u{2168}",
        "SCRAM-SHA-256",
        15000,
        "pf5vDU02hPP1krHGtljZ9ru+g9V+AuofWj8yyg==",
        "U5MLoaO5MmVWBFmfsviEcbDFJVhXjv3okxcrLhd0h2A=",
        "NtM6OF3ccAw4VBOAZeRkQGon2BqL0eMu0j/jg1VFlX8=",
    ),
];

#[test]
fn derivation_matches_the_reference() {
    for (user, password, mech, iterations, salt, stored, server) in OBSERVED {
        let m = Mechanism::from_name(mech).unwrap();
        assert_eq!(iterations, m.default_iterations());
        let salt = base64_decode(salt.as_bytes()).unwrap();
        assert_eq!(salt.len(), m.salt_len());
        let prepared = m.prepare_password(user, password, true).unwrap();
        let c = Credentials::derive(m, &prepared, &salt, iterations);
        assert_eq!(base64_encode(&c.stored_key), stored, "{user} {mech}");
        assert_eq!(base64_encode(&c.server_key), server, "{user} {mech}");
    }
}
