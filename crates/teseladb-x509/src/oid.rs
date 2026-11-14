//! Object identifiers as the contents octets of their DER encoding.

/// rsaEncryption, 1.2.840.113549.1.1.1.
pub const RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
/// sha1WithRSAEncryption, 1.2.840.113549.1.1.5.
pub const SHA1_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05];
/// id-RSASSA-PSS, 1.2.840.113549.1.1.10.
pub const RSASSA_PSS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0a];
/// sha256WithRSAEncryption, 1.2.840.113549.1.1.11.
pub const SHA256_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
/// sha384WithRSAEncryption, 1.2.840.113549.1.1.12.
pub const SHA384_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
/// sha512WithRSAEncryption, 1.2.840.113549.1.1.13.
pub const SHA512_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
/// sha224WithRSAEncryption, 1.2.840.113549.1.1.14.
pub const SHA224_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0e];
/// id-mgf1, 1.2.840.113549.1.1.8.
pub const MGF1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x08];
/// id-ecPublicKey, 1.2.840.10045.2.1.
pub const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
/// prime256v1 (P-256), 1.2.840.10045.3.1.7.
pub const P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
/// secp384r1 (P-384), 1.3.132.0.34.
pub const P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
/// secp521r1 (P-521), 1.3.132.0.35.
pub const P521: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];
/// ecdsa-with-SHA1, 1.2.840.10045.4.1.
pub const ECDSA_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x01];
/// ecdsa-with-SHA224, 1.2.840.10045.4.3.1.
pub const ECDSA_SHA224: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x01];
/// ecdsa-with-SHA256, 1.2.840.10045.4.3.2.
pub const ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
/// ecdsa-with-SHA384, 1.2.840.10045.4.3.3.
pub const ECDSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
/// ecdsa-with-SHA512, 1.2.840.10045.4.3.4.
pub const ECDSA_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04];
/// id-Ed25519, 1.3.101.112.
pub const ED25519: &[u8] = &[0x2b, 0x65, 0x70];
/// id-sha1, 1.3.14.3.2.26.
pub const SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
/// id-sha256, 2.16.840.1.101.3.4.2.1.
pub const SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
/// id-sha384, 2.16.840.1.101.3.4.2.2.
pub const SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
/// id-sha512, 2.16.840.1.101.3.4.2.3.
pub const SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
/// id-sha224, 2.16.840.1.101.3.4.2.4.
pub const SHA224: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x04];
/// id-dsa-with-sha1, 1.2.840.10040.4.3 (recognized only to report it as unsupported).
pub const DSA_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x03];

/// id-PBES2, 1.2.840.113549.1.5.13.
pub const PBES2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0d];
/// id-PBKDF2, 1.2.840.113549.1.5.12.
pub const PBKDF2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0c];
/// hmacWithSHA1, 1.2.840.113549.2.7.
pub const HMAC_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x07];
/// hmacWithSHA256, 1.2.840.113549.2.9.
pub const HMAC_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x09];
/// hmacWithSHA384, 1.2.840.113549.2.10.
pub const HMAC_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0a];
/// hmacWithSHA512, 1.2.840.113549.2.11.
pub const HMAC_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0b];
/// aes128-CBC-PAD, 2.16.840.1.101.3.4.1.2.
pub const AES128_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x02];
/// aes192-CBC-PAD, 2.16.840.1.101.3.4.1.22.
pub const AES192_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x16];
/// aes256-CBC-PAD, 2.16.840.1.101.3.4.1.42.
pub const AES256_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x2a];
/// pkcs-9-at-extensionRequest, 1.2.840.113549.1.9.14.
pub const EXTENSION_REQUEST: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x0e];

/// commonName, 2.5.4.3.
pub const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
/// serialNumber, 2.5.4.5.
pub const SERIAL_NUMBER: &[u8] = &[0x55, 0x04, 0x05];
/// countryName, 2.5.4.6.
pub const COUNTRY: &[u8] = &[0x55, 0x04, 0x06];
/// localityName, 2.5.4.7.
pub const LOCALITY: &[u8] = &[0x55, 0x04, 0x07];
/// stateOrProvinceName, 2.5.4.8.
pub const STATE: &[u8] = &[0x55, 0x04, 0x08];
/// streetAddress, 2.5.4.9.
pub const STREET: &[u8] = &[0x55, 0x04, 0x09];
/// organizationName, 2.5.4.10.
pub const ORGANIZATION: &[u8] = &[0x55, 0x04, 0x0a];
/// organizationalUnitName, 2.5.4.11.
pub const ORGANIZATIONAL_UNIT: &[u8] = &[0x55, 0x04, 0x0b];
/// domainComponent, 0.9.2342.19200300.100.1.25.
pub const DOMAIN_COMPONENT: &[u8] = &[0x09, 0x92, 0x26, 0x89, 0x93, 0xf2, 0x2c, 0x64, 0x01, 0x19];
/// userId, 0.9.2342.19200300.100.1.1.
pub const USER_ID: &[u8] = &[0x09, 0x92, 0x26, 0x89, 0x93, 0xf2, 0x2c, 0x64, 0x01, 0x01];
/// emailAddress, 1.2.840.113549.1.9.1.
pub const EMAIL_ADDRESS: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x01];

/// subjectKeyIdentifier, 2.5.29.14.
pub const SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1d, 0x0e];
/// keyUsage, 2.5.29.15.
pub const KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
/// subjectAltName, 2.5.29.17.
pub const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
/// issuerAltName, 2.5.29.18.
pub const ISSUER_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x12];
/// basicConstraints, 2.5.29.19.
pub const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
/// cRLNumber, 2.5.29.20.
pub const CRL_NUMBER: &[u8] = &[0x55, 0x1d, 0x14];
/// reasonCode, 2.5.29.21.
pub const REASON_CODE: &[u8] = &[0x55, 0x1d, 0x15];
/// invalidityDate, 2.5.29.24.
pub const INVALIDITY_DATE: &[u8] = &[0x55, 0x1d, 0x18];
/// deltaCRLIndicator, 2.5.29.27.
pub const DELTA_CRL_INDICATOR: &[u8] = &[0x55, 0x1d, 0x1b];
/// issuingDistributionPoint, 2.5.29.28.
pub const ISSUING_DISTRIBUTION_POINT: &[u8] = &[0x55, 0x1d, 0x1c];
/// certificateIssuer, 2.5.29.29.
pub const CERTIFICATE_ISSUER: &[u8] = &[0x55, 0x1d, 0x1d];
/// nameConstraints, 2.5.29.30.
pub const NAME_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x1e];
/// cRLDistributionPoints, 2.5.29.31.
pub const CRL_DISTRIBUTION_POINTS: &[u8] = &[0x55, 0x1d, 0x1f];
/// certificatePolicies, 2.5.29.32.
pub const CERTIFICATE_POLICIES: &[u8] = &[0x55, 0x1d, 0x20];
/// anyPolicy, 2.5.29.32.0.
pub const ANY_POLICY: &[u8] = &[0x55, 0x1d, 0x20, 0x00];
/// policyMappings, 2.5.29.33.
pub const POLICY_MAPPINGS: &[u8] = &[0x55, 0x1d, 0x21];
/// authorityKeyIdentifier, 2.5.29.35.
pub const AUTHORITY_KEY_ID: &[u8] = &[0x55, 0x1d, 0x23];
/// policyConstraints, 2.5.29.36.
pub const POLICY_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x24];
/// extKeyUsage, 2.5.29.37.
pub const EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
/// freshestCRL, 2.5.29.46.
pub const FRESHEST_CRL: &[u8] = &[0x55, 0x1d, 0x2e];
/// inhibitAnyPolicy, 2.5.29.54.
pub const INHIBIT_ANY_POLICY: &[u8] = &[0x55, 0x1d, 0x36];
/// anyExtendedKeyUsage, 2.5.29.37.0.
pub const ANY_EXTENDED_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25, 0x00];
/// id-kp-serverAuth, 1.3.6.1.5.5.7.3.1.
pub const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
/// id-kp-clientAuth, 1.3.6.1.5.5.7.3.2.
pub const CLIENT_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];

/// The dotted form of an OID's contents, such as `2.5.4.3`.
#[must_use]
pub fn to_dotted(content: &[u8]) -> String {
    let mut arcs: Vec<u128> = Vec::new();
    let mut acc: u128 = 0;
    for &b in content {
        acc = (acc << 7) | u128::from(b & 0x7f);
        if b & 0x80 == 0 {
            arcs.push(acc);
            acc = 0;
        }
    }
    let mut out = String::new();
    if let Some(&first) = arcs.first() {
        let (a, b) = match first {
            0..=39 => (0, first),
            40..=79 => (1, first - 40),
            _ => (2, first - 80),
        };
        out = format!("{a}.{b}");
    }
    for arc in arcs.iter().skip(1) {
        out.push('.');
        out.push_str(&arc.to_string());
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn dotted_forms() {
        assert_eq!(to_dotted(COMMON_NAME), "2.5.4.3");
        assert_eq!(to_dotted(SHA256_WITH_RSA), "1.2.840.113549.1.1.11");
        assert_eq!(to_dotted(DOMAIN_COMPONENT), "0.9.2342.19200300.100.1.25");
        assert_eq!(to_dotted(&[0x81, 0x00]), "2.48");
        assert_eq!(to_dotted(&[]), "");
    }
}
