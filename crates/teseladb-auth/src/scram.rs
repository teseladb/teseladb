//! SCRAM-SHA-1 and SCRAM-SHA-256 (RFC 5802, RFC 7677) as the reference server uses them (013
//! FR-008, FR-013, FR-032): the stored credentials, the server and client sides of the exchange,
//! and the passwords each mechanism hashes: for SCRAM-SHA-1, the hex MD5 of
//! `user:<middle>:password` (the reference server's digest, `<middle>` being
//! [`teseladb_names::SCRAM_SHA1_DIGEST_MIDDLE`]); for SCRAM-SHA-256, the SASLprep'd password.
//! Channel binding is not offered, as in the reference server.

use teseladb_crypto::pbkdf2::pbkdf2;
use teseladb_crypto::{HashAlgorithm, Sha1, Sha256, ct};
use teseladb_x509::pem::{base64_decode_strict as base64_decode, base64_encode};

use crate::saslprep::{SaslprepError, saslprep};

/// GS2 header without channel binding or authorization identity.
const GS2_HEADER: &str = "n,,";
/// Fewest iterations a server accepts in a stored credential (RFC 5802 recommends 4096).
pub const MIN_ITERATIONS: u32 = 4096;
/// Most iterations a client accepts from a server, bounding the work it can be asked for.
pub const MAX_ITERATIONS: u32 = 10_000_000;

/// A SCRAM mechanism.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mechanism {
    /// SCRAM-SHA-1.
    ScramSha1,
    /// SCRAM-SHA-256.
    ScramSha256,
}

impl Mechanism {
    /// The SASL name.
    pub fn name(self) -> &'static str {
        match self {
            Mechanism::ScramSha1 => "SCRAM-SHA-1",
            Mechanism::ScramSha256 => "SCRAM-SHA-256",
        }
    }

    /// The mechanism of a SASL name.
    pub fn from_name(name: &str) -> Option<Mechanism> {
        [Mechanism::ScramSha1, Mechanism::ScramSha256].into_iter().find(|m| m.name() == name)
    }

    /// The hash.
    pub fn hash(self) -> HashAlgorithm {
        match self {
            Mechanism::ScramSha1 => HashAlgorithm::Sha1,
            Mechanism::ScramSha256 => HashAlgorithm::Sha256,
        }
    }

    /// The reference server's default iteration count (`scramIterationCount`,
    /// `scramSHA256IterationCount`).
    pub fn default_iterations(self) -> u32 {
        match self {
            Mechanism::ScramSha1 => 10_000,
            Mechanism::ScramSha256 => 15_000,
        }
    }

    /// The salt length the reference server uses: 16 bytes for SCRAM-SHA-1, 28 for SCRAM-SHA-256.
    pub fn salt_len(self) -> usize {
        match self {
            Mechanism::ScramSha1 => 16,
            Mechanism::ScramSha256 => 28,
        }
    }

    /// The length of its stored and server keys: that of its hash.
    pub fn key_len(self) -> usize {
        match self {
            Mechanism::ScramSha1 => 20,
            Mechanism::ScramSha256 => 32,
        }
    }

    /// The password the mechanism derives from: the hex MD5 digest of `user:<middle>:password` for
    /// SCRAM-SHA-1 (D-022, FR-032), the SASLprep'd password for SCRAM-SHA-256 (`stored` for a
    /// password being set, which refuses unassigned code points).
    pub fn prepare_password(
        self,
        user: &str,
        password: &str,
        stored: bool,
    ) -> Result<Vec<u8>, ScramError> {
        match self {
            Mechanism::ScramSha1 => {
                let digest = teseladb_crypto::md5(
                    format!("{user}:{}:{password}", teseladb_names::SCRAM_SHA1_DIGEST_MIDDLE)
                        .as_bytes(),
                );
                Ok(digest.iter().map(|b| format!("{b:02x}")).collect::<String>().into_bytes())
            }
            Mechanism::ScramSha256 => {
                Ok(saslprep(password, stored).map_err(ScramError::Saslprep)?.into_bytes())
            }
        }
    }

    fn hi(self, password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
        let mut out = vec![0u8; self.hash().output_len()];
        match self {
            Mechanism::ScramSha1 => pbkdf2::<Sha1>(password, salt, iterations, &mut out),
            Mechanism::ScramSha256 => pbkdf2::<Sha256>(password, salt, iterations, &mut out),
        }
        out
    }

    fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        self.hash().hmac(key, data).as_ref().to_vec()
    }
}

/// Why an exchange failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScramError {
    /// A message that does not follow the grammar of RFC 5802, section 7.
    Malformed(&'static str),
    /// The proof or the server signature does not verify, or the nonce changed.
    AuthenticationFailed,
    /// The password does not pass SASLprep.
    Saslprep(SaslprepError),
    /// A message in the wrong state.
    OutOfOrder,
}

impl std::fmt::Display for ScramError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScramError::Malformed(what) => write!(f, "malformed SCRAM message: {what}"),
            ScramError::AuthenticationFailed => f.write_str("Authentication failed."),
            ScramError::Saslprep(e) => write!(f, "password: {e}"),
            ScramError::OutOfOrder => f.write_str("SCRAM message out of order"),
        }
    }
}

impl std::error::Error for ScramError {}

/// The credentials a server stores for one mechanism (FR-013): no password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    /// `iterationCount`.
    pub iterations: u32,
    /// `salt`.
    pub salt: Vec<u8>,
    /// `storedKey` = H(ClientKey).
    pub stored_key: Vec<u8>,
    /// `serverKey` = HMAC(SaltedPassword, "Server Key").
    pub server_key: Vec<u8>,
}

impl Credentials {
    /// Derives the credentials of an already prepared password.
    pub fn derive(mech: Mechanism, prepared: &[u8], salt: &[u8], iterations: u32) -> Credentials {
        let salted = mech.hi(prepared, salt, iterations);
        let client_key = mech.hmac(&salted, b"Client Key");
        Credentials {
            iterations,
            salt: salt.to_vec(),
            stored_key: mech.hash().hash(&client_key).as_ref().to_vec(),
            server_key: mech.hmac(&salted, b"Server Key"),
        }
    }
}

/// The attributes `a=value` of a message, in order.
fn attributes(msg: &str) -> Result<Vec<(char, &str)>, ScramError> {
    msg.split(',')
        .map(|part| {
            let mut chars = part.chars();
            match (chars.next(), chars.next()) {
                (Some(k), Some('=')) if k.is_ascii_alphabetic() => Ok((k, &part[2..])),
                _ => Err(ScramError::Malformed("attribute")),
            }
        })
        .collect()
}

/// `saslname` decoding: `=2C` is a comma, `=3D` an equals sign, any other `=` is an error.
fn decode_name(s: &str) -> Result<String, ScramError> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('=') {
        out.push_str(&rest[..i]);
        match rest.get(i..i + 3) {
            Some("=2C") => out.push(','),
            Some("=3D") => out.push('='),
            _ => return Err(ScramError::Malformed("user name")),
        }
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `saslname` encoding.
fn encode_name(s: &str) -> String {
    s.replace('=', "=3D").replace(',', "=2C")
}

fn printable_nonce(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b',')
}

/// What the client sent first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientFirst {
    /// The user name, decoded.
    pub user: String,
    /// The GS2 header, `n,,`, `y,,` or `n,a=<authzid>,`.
    gs2: String,
    bare: String,
    nonce: String,
}

impl ClientFirst {
    /// Parses `client-first-message`.
    pub fn parse(msg: &[u8]) -> Result<ClientFirst, ScramError> {
        let msg = std::str::from_utf8(msg).map_err(|_| ScramError::Malformed("not UTF-8"))?;
        // `gs2-cbind-flag "," [authzid] ","`: channel binding is never offered, so `p=` fails.
        let rest = msg
            .strip_prefix("n,")
            .or_else(|| msg.strip_prefix("y,"))
            .ok_or(ScramError::Malformed("GS2 header"))?;
        let (authzid, bare) = rest.split_once(',').ok_or(ScramError::Malformed("GS2 header"))?;
        let authzid = match authzid {
            "" => None,
            a => Some(decode_name(a.strip_prefix("a=").ok_or(ScramError::Malformed("authzid"))?)?),
        };
        let gs2 = &msg[..msg.len() - bare.len()];
        let attrs = attributes(bare)?;
        let (user, nonce) = match attrs.as_slice() {
            [('n', u), ('r', r)] => (decode_name(u)?, *r),
            [('m', _), ..] => return Err(ScramError::Malformed("mandatory extension")),
            _ => return Err(ScramError::Malformed("client-first-message")),
        };
        if user.is_empty() || !printable_nonce(nonce) {
            return Err(ScramError::Malformed("client-first-message"));
        }
        // The reference server takes an authorization identity only when it names the same user
        // (measured).
        if authzid.is_some_and(|a| a != user) {
            return Err(ScramError::AuthenticationFailed);
        }
        Ok(ClientFirst {
            user,
            gs2: gs2.to_owned(),
            bare: bare.to_owned(),
            nonce: nonce.to_owned(),
        })
    }
}

/// The server side of one exchange, after the client's first message.
#[derive(Debug)]
pub struct ServerSession {
    mech: Mechanism,
    creds: Credentials,
    gs2: String,
    auth_prefix: String,
    nonce: String,
    done: bool,
}

impl ServerSession {
    /// Answers `first` for a user with `creds`: `server_nonce` is appended to the client's nonce.
    /// Returns the session and `server-first-message`.
    pub fn start(
        mech: Mechanism,
        first: &ClientFirst,
        creds: Credentials,
        server_nonce: &str,
    ) -> (ServerSession, Vec<u8>) {
        let nonce = format!("{}{server_nonce}", first.nonce);
        let server_first =
            format!("r={nonce},s={},i={}", base64_encode(&creds.salt), creds.iterations);
        let auth_prefix = format!("{},{server_first}", first.bare);
        let gs2 = first.gs2.clone();
        let s = ServerSession { mech, creds, gs2, auth_prefix, nonce, done: false };
        (s, server_first.into_bytes())
    }

    /// Checks `client-final-message` and returns `server-final-message`.
    pub fn finish(&mut self, msg: &[u8]) -> Result<Vec<u8>, ScramError> {
        if self.done {
            return Err(ScramError::OutOfOrder);
        }
        self.done = true;
        let msg = std::str::from_utf8(msg).map_err(|_| ScramError::Malformed("not UTF-8"))?;
        let (without_proof, proof_b64) =
            msg.rsplit_once(",p=").ok_or(ScramError::Malformed("proof"))?;
        let proof =
            base64_decode(proof_b64.as_bytes()).map_err(|_| ScramError::Malformed("proof"))?;
        let attrs = attributes(without_proof)?;
        let (cbind, nonce) = match attrs.as_slice() {
            [('c', c), ('r', r)] => (*c, *r),
            _ => return Err(ScramError::Malformed("client-final-message")),
        };
        let cbind = base64_decode(cbind.as_bytes())
            .map_err(|_| ScramError::Malformed("channel binding"))?;
        // The header the client sent first, or `n,,` or `y,,`: the reference server takes any of
        // them as "no channel binding" (measured), whatever the first header was.
        if cbind != b"n,," && cbind != b"y,," && cbind != self.gs2.as_bytes() {
            return Err(ScramError::Malformed("channel binding"));
        }
        if nonce != self.nonce {
            return Err(ScramError::AuthenticationFailed);
        }
        let auth_message = format!("{},{without_proof}", self.auth_prefix);
        let signature = self.mech.hmac(&self.creds.stored_key, auth_message.as_bytes());
        if proof.len() != signature.len() {
            return Err(ScramError::AuthenticationFailed);
        }
        let client_key: Vec<u8> = proof.iter().zip(&signature).map(|(a, b)| a ^ b).collect();
        let stored = self.mech.hash().hash(&client_key);
        if !ct::ct_eq(stored.as_ref(), &self.creds.stored_key) {
            return Err(ScramError::AuthenticationFailed);
        }
        let server_signature = self.mech.hmac(&self.creds.server_key, auth_message.as_bytes());
        Ok(format!("v={}", base64_encode(&server_signature)).into_bytes())
    }
}

/// The client side of one exchange.
#[derive(Debug)]
pub struct ClientSession {
    mech: Mechanism,
    prepared: Vec<u8>,
    bare: String,
    nonce: String,
    expected_signature: Option<Vec<u8>>,
}

impl ClientSession {
    /// Starts an exchange for `user` with an already prepared password and `nonce`; returns
    /// the session and `client-first-message`.
    pub fn start(
        mech: Mechanism,
        user: &str,
        prepared: &[u8],
        nonce: &str,
    ) -> (ClientSession, Vec<u8>) {
        let bare = format!("n={},r={nonce}", encode_name(user));
        let first = format!("{GS2_HEADER}{bare}");
        let s = ClientSession {
            mech,
            prepared: prepared.to_vec(),
            bare,
            nonce: nonce.to_owned(),
            expected_signature: None,
        };
        (s, first.into_bytes())
    }

    /// Answers `server-first-message` with `client-final-message`.
    pub fn respond(&mut self, server_first: &[u8]) -> Result<Vec<u8>, ScramError> {
        if self.expected_signature.is_some() {
            return Err(ScramError::OutOfOrder);
        }
        let sf =
            std::str::from_utf8(server_first).map_err(|_| ScramError::Malformed("not UTF-8"))?;
        let attrs = attributes(sf)?;
        let (nonce, salt, iterations) = match attrs.as_slice() {
            [('r', r), ('s', s), ('i', i), ..] => (*r, *s, *i),
            _ => return Err(ScramError::Malformed("server-first-message")),
        };
        if !nonce.starts_with(&self.nonce)
            || nonce.len() == self.nonce.len()
            || !printable_nonce(nonce)
        {
            return Err(ScramError::AuthenticationFailed);
        }
        let salt = base64_decode(salt.as_bytes()).map_err(|_| ScramError::Malformed("salt"))?;
        let iterations: u32 =
            iterations.parse().map_err(|_| ScramError::Malformed("iteration count"))?;
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
            return Err(ScramError::Malformed("iteration count"));
        }
        let salted = self.mech.hi(&self.prepared, &salt, iterations);
        let client_key = self.mech.hmac(&salted, b"Client Key");
        let stored_key = self.mech.hash().hash(&client_key);
        let without_proof = format!("c={},r={nonce}", base64_encode(GS2_HEADER.as_bytes()));
        let auth_message = format!("{},{sf},{without_proof}", self.bare);
        let signature = self.mech.hmac(stored_key.as_ref(), auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&signature).map(|(a, b)| a ^ b).collect();
        let server_key = self.mech.hmac(&salted, b"Server Key");
        self.expected_signature = Some(self.mech.hmac(&server_key, auth_message.as_bytes()));
        Ok(format!("{without_proof},p={}", base64_encode(&proof)).into_bytes())
    }

    /// Checks `server-final-message`: the server proved it knows the password.
    pub fn verify(&self, server_final: &[u8]) -> Result<(), ScramError> {
        let expected = self.expected_signature.as_ref().ok_or(ScramError::OutOfOrder)?;
        let sf =
            std::str::from_utf8(server_final).map_err(|_| ScramError::Malformed("not UTF-8"))?;
        let v = sf.strip_prefix("v=").ok_or(ScramError::AuthenticationFailed)?;
        let got = base64_decode(v.as_bytes()).map_err(|_| ScramError::Malformed("verifier"))?;
        if ct::ct_eq(&got, expected) { Ok(()) } else { Err(ScramError::AuthenticationFailed) }
    }
}
