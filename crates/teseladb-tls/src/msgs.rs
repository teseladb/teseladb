//! Handshake messages and extensions (RFC 8446, 4 and appendix B.3).

use crate::codec::{Reader, put_u16, put_u24, put_vec8, put_vec16, put_vec24};
use crate::{Alert, Error};

/// Handshake message types.
pub mod hs {
    /// client_hello.
    pub const CLIENT_HELLO: u8 = 1;
    /// server_hello.
    pub const SERVER_HELLO: u8 = 2;
    /// new_session_ticket.
    pub const NEW_SESSION_TICKET: u8 = 4;
    /// encrypted_extensions.
    pub const ENCRYPTED_EXTENSIONS: u8 = 8;
    /// certificate.
    pub const CERTIFICATE: u8 = 11;
    /// certificate_request.
    pub const CERTIFICATE_REQUEST: u8 = 13;
    /// certificate_verify.
    pub const CERTIFICATE_VERIFY: u8 = 15;
    /// finished.
    pub const FINISHED: u8 = 20;
    /// key_update.
    pub const KEY_UPDATE: u8 = 24;
    /// message_hash (the synthetic message after a HelloRetryRequest).
    pub const MESSAGE_HASH: u8 = 254;
}

/// Extension types.
pub mod ext {
    /// server_name.
    pub const SERVER_NAME: u16 = 0;
    /// status_request (RFC 6066, 8; RFC 8446, 4.4.2.1).
    pub const STATUS_REQUEST: u16 = 5;
    /// supported_groups.
    pub const SUPPORTED_GROUPS: u16 = 10;
    /// signature_algorithms.
    pub const SIGNATURE_ALGORITHMS: u16 = 13;
    /// application_layer_protocol_negotiation.
    pub const ALPN: u16 = 16;
    /// pre_shared_key.
    pub const PRE_SHARED_KEY: u16 = 41;
    /// early_data.
    pub const EARLY_DATA: u16 = 42;
    /// supported_versions.
    pub const SUPPORTED_VERSIONS: u16 = 43;
    /// cookie.
    pub const COOKIE: u16 = 44;
    /// psk_key_exchange_modes.
    pub const PSK_KEY_EXCHANGE_MODES: u16 = 45;
    /// signature_algorithms_cert.
    pub const SIGNATURE_ALGORITHMS_CERT: u16 = 50;
    /// key_share.
    pub const KEY_SHARE: u16 = 51;
}

/// TLS 1.3.
pub const TLS13: u16 = 0x0304;
/// TLS 1.2.
pub const TLS12: u16 = 0x0303;
/// `psk_dhe_ke`, the only PSK mode TeselaDB uses.
pub const PSK_DHE_KE: u8 = 1;
/// The `random` of a HelloRetryRequest (SHA-256 of "HelloRetryRequest").
pub const HRR_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

fn decode() -> Error {
    Error::Local(Alert::DecodeError)
}

fn illegal() -> Error {
    Error::Local(Alert::IllegalParameter)
}

fn array32(r: &mut Reader<'_>) -> Result<[u8; 32], Error> {
    let mut out = [0u8; 32];
    out.copy_from_slice(r.bytes(32)?);
    Ok(out)
}

/// A list of extensions, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extensions(pub Vec<(u16, Vec<u8>)>);

impl Extensions {
    /// Reads an extension block, rejecting duplicates.
    pub fn read(r: &mut Reader<'_>) -> Result<Extensions, Error> {
        let mut e = Reader::new(r.vec16()?);
        let mut out: Vec<(u16, Vec<u8>)> = Vec::new();
        while !e.is_empty() {
            let typ = e.u16()?;
            let data = e.vec16()?.to_vec();
            if out.iter().any(|(t, _)| *t == typ) {
                return Err(illegal());
            }
            out.push((typ, data));
        }
        Ok(Extensions(out))
    }

    /// Appends the extension block.
    pub fn write(&self, out: &mut Vec<u8>) {
        let mut body = Vec::new();
        for (t, d) in &self.0 {
            put_u16(&mut body, *t);
            put_vec16(&mut body, d);
        }
        put_vec16(out, &body);
    }

    /// The data of extension `typ`.
    pub fn get(&self, typ: u16) -> Option<&[u8]> {
        self.0.iter().find(|(t, _)| *t == typ).map(|(_, d)| d.as_slice())
    }

    /// Adds an extension.
    pub fn push(&mut self, typ: u16, data: Vec<u8>) {
        self.0.push((typ, data));
    }

    /// Fails with `unsupported_extension` if an extension is not in `allowed`.
    pub fn only(&self, allowed: &[u16]) -> Result<(), Error> {
        if self.0.iter().all(|(t, _)| allowed.contains(t)) {
            Ok(())
        } else {
            Err(Error::Local(Alert::UnsupportedExtension))
        }
    }
}

/// Frames a handshake message: type, 3-byte length, body.
pub fn frame(typ: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(typ);
    put_u24(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

/// Splits a framed message into type and body.
pub fn unframe(msg: &[u8]) -> Result<(u8, &[u8]), Error> {
    let mut r = Reader::new(msg);
    let typ = r.u8()?;
    let body = r.vec24()?;
    r.finish()?;
    Ok((typ, body))
}

/// A list of `u16` values with a 2-byte length (suites, groups, schemes).
pub fn read_u16_list(data: &[u8]) -> Result<Vec<u16>, Error> {
    let mut r = Reader::new(data);
    let list = r.vec16()?;
    r.finish()?;
    if list.is_empty() || list.len() % 2 != 0 {
        return Err(decode());
    }
    Ok(list.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect())
}

/// Encodes a list of `u16` values with a 2-byte length.
pub fn u16_list(values: &[u16]) -> Vec<u8> {
    let body: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
    let mut out = Vec::new();
    put_vec16(&mut out, &body);
    out
}

/// ClientHello.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientHello {
    /// legacy_version.
    pub legacy_version: u16,
    /// random.
    pub random: [u8; 32],
    /// legacy_session_id.
    pub session_id: Vec<u8>,
    /// cipher_suites.
    pub suites: Vec<u16>,
    /// legacy_compression_methods.
    pub compression: Vec<u8>,
    /// extensions.
    pub extensions: Extensions,
}

impl ClientHello {
    /// Parses the body.
    pub fn parse(body: &[u8]) -> Result<ClientHello, Error> {
        let mut r = Reader::new(body);
        let legacy_version = r.u16()?;
        let random = array32(&mut r)?;
        let session_id = r.vec8()?.to_vec();
        if session_id.len() > 32 {
            return Err(decode());
        }
        let suites_raw = r.vec16()?;
        if suites_raw.is_empty() || suites_raw.len() % 2 != 0 {
            return Err(decode());
        }
        let suites = suites_raw.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        let compression = r.vec8()?.to_vec();
        if compression.is_empty() {
            return Err(decode());
        }
        // A TLS 1.2 or earlier hello may have no extensions at all.
        let extensions =
            if r.is_empty() { Extensions::default() } else { Extensions::read(&mut r)? };
        r.finish()?;
        // pre_shared_key must be the last extension (RFC 8446, 4.2.11).
        let psk_pos = extensions.0.iter().position(|(t, _)| *t == ext::PRE_SHARED_KEY);
        if psk_pos.is_some_and(|p| p + 1 != extensions.0.len()) {
            return Err(illegal());
        }
        Ok(ClientHello { legacy_version, random, session_id, suites, compression, extensions })
    }

    /// Encodes the framed message.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_u16(&mut b, self.legacy_version);
        b.extend_from_slice(&self.random);
        put_vec8(&mut b, &self.session_id);
        let suites: Vec<u8> = self.suites.iter().flat_map(|s| s.to_be_bytes()).collect();
        put_vec16(&mut b, &suites);
        put_vec8(&mut b, &self.compression);
        self.extensions.write(&mut b);
        frame(hs::CLIENT_HELLO, &b)
    }
}

/// ServerHello (also HelloRetryRequest, with `HRR_RANDOM`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerHello {
    /// random.
    pub random: [u8; 32],
    /// legacy_session_id_echo.
    pub session_id: Vec<u8>,
    /// cipher_suite.
    pub suite: u16,
    /// extensions.
    pub extensions: Extensions,
}

impl ServerHello {
    /// Parses the body.
    pub fn parse(body: &[u8]) -> Result<ServerHello, Error> {
        let mut r = Reader::new(body);
        let _legacy_version = r.u16()?;
        let random = array32(&mut r)?;
        let session_id = r.vec8()?.to_vec();
        let suite = r.u16()?;
        if r.u8()? != 0 {
            return Err(illegal());
        }
        let extensions = Extensions::read(&mut r)?;
        r.finish()?;
        Ok(ServerHello { random, session_id, suite, extensions })
    }

    /// Encodes the framed message.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_u16(&mut b, TLS12);
        b.extend_from_slice(&self.random);
        put_vec8(&mut b, &self.session_id);
        put_u16(&mut b, self.suite);
        b.push(0);
        self.extensions.write(&mut b);
        frame(hs::SERVER_HELLO, &b)
    }

    /// Whether this is a HelloRetryRequest.
    pub fn is_retry(&self) -> bool {
        self.random == HRR_RANDOM
    }
}

/// One entry of a Certificate message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertEntry {
    /// The DER certificate.
    pub der: Vec<u8>,
    /// Its extensions.
    pub extensions: Extensions,
}

/// Certificate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateMsg {
    /// certificate_request_context.
    pub context: Vec<u8>,
    /// The chain, leaf first.
    pub entries: Vec<CertEntry>,
}

impl CertificateMsg {
    /// Parses the body.
    pub fn parse(body: &[u8]) -> Result<CertificateMsg, Error> {
        let mut r = Reader::new(body);
        let context = r.vec8()?.to_vec();
        let mut list = Reader::new(r.vec24()?);
        r.finish()?;
        let mut entries = Vec::new();
        while !list.is_empty() {
            let der = list.vec24()?.to_vec();
            if der.is_empty() {
                return Err(decode());
            }
            let extensions = Extensions::read(&mut list)?;
            entries.push(CertEntry { der, extensions });
        }
        Ok(CertificateMsg { context, entries })
    }

    /// Encodes the framed message.
    pub fn encode(&self) -> Vec<u8> {
        let mut list = Vec::new();
        for e in &self.entries {
            put_vec24(&mut list, &e.der);
            e.extensions.write(&mut list);
        }
        let mut b = Vec::new();
        put_vec8(&mut b, &self.context);
        put_vec24(&mut b, &list);
        frame(hs::CERTIFICATE, &b)
    }
}

/// CertificateVerify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateVerify {
    /// The signature scheme's code point.
    pub scheme: u16,
    /// The signature.
    pub signature: Vec<u8>,
}

impl CertificateVerify {
    /// Parses the body.
    pub fn parse(body: &[u8]) -> Result<CertificateVerify, Error> {
        let mut r = Reader::new(body);
        let scheme = r.u16()?;
        let signature = r.vec16()?.to_vec();
        r.finish()?;
        Ok(CertificateVerify { scheme, signature })
    }

    /// Encodes the framed message.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_u16(&mut b, self.scheme);
        put_vec16(&mut b, &self.signature);
        frame(hs::CERTIFICATE_VERIFY, &b)
    }
}

/// CertificateRequest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateRequest {
    /// certificate_request_context.
    pub context: Vec<u8>,
    /// extensions (signature_algorithms is mandatory).
    pub extensions: Extensions,
}

impl CertificateRequest {
    /// Parses the body.
    pub fn parse(body: &[u8]) -> Result<CertificateRequest, Error> {
        let mut r = Reader::new(body);
        let context = r.vec8()?.to_vec();
        let extensions = Extensions::read(&mut r)?;
        r.finish()?;
        Ok(CertificateRequest { context, extensions })
    }

    /// Encodes the framed message.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_vec8(&mut b, &self.context);
        self.extensions.write(&mut b);
        frame(hs::CERTIFICATE_REQUEST, &b)
    }
}

/// NewSessionTicket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSessionTicket {
    /// ticket_lifetime, seconds.
    pub lifetime: u32,
    /// ticket_age_add.
    pub age_add: u32,
    /// ticket_nonce.
    pub nonce: Vec<u8>,
    /// ticket.
    pub ticket: Vec<u8>,
    /// extensions.
    pub extensions: Extensions,
}

impl NewSessionTicket {
    /// Parses the body.
    pub fn parse(body: &[u8]) -> Result<NewSessionTicket, Error> {
        let mut r = Reader::new(body);
        let lifetime = r.u32()?;
        let age_add = r.u32()?;
        let nonce = r.vec8()?.to_vec();
        let ticket = r.vec16()?.to_vec();
        if ticket.is_empty() {
            return Err(decode());
        }
        let extensions = Extensions::read(&mut r)?;
        r.finish()?;
        Ok(NewSessionTicket { lifetime, age_add, nonce, ticket, extensions })
    }

    /// Encodes the framed message.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.lifetime.to_be_bytes());
        b.extend_from_slice(&self.age_add.to_be_bytes());
        put_vec8(&mut b, &self.nonce);
        put_vec16(&mut b, &self.ticket);
        self.extensions.write(&mut b);
        frame(hs::NEW_SESSION_TICKET, &b)
    }
}

/// A key share: group and key exchange bytes.
pub fn read_key_share_entry(r: &mut Reader<'_>) -> Result<(u16, Vec<u8>), Error> {
    let group = r.u16()?;
    let key = r.vec16()?.to_vec();
    if key.is_empty() {
        return Err(decode());
    }
    Ok((group, key))
}

/// Encodes one key share entry.
pub fn key_share_entry(group: u16, key: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    put_u16(&mut out, group);
    put_vec16(&mut out, key);
    out
}

/// The identities and binders of a ClientHello's pre_shared_key extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferedPsks {
    /// (identity, obfuscated_ticket_age).
    pub identities: Vec<(Vec<u8>, u32)>,
    /// The binders, one per identity.
    pub binders: Vec<Vec<u8>>,
}

impl OfferedPsks {
    /// Parses the extension data.
    pub fn parse(data: &[u8]) -> Result<OfferedPsks, Error> {
        let mut r = Reader::new(data);
        let mut ids = Reader::new(r.vec16()?);
        let mut binders_r = Reader::new(r.vec16()?);
        r.finish()?;
        let mut identities = Vec::new();
        while !ids.is_empty() {
            let id = ids.vec16()?.to_vec();
            identities.push((id, ids.u32()?));
        }
        let mut binders = Vec::new();
        while !binders_r.is_empty() {
            binders.push(binders_r.vec8()?.to_vec());
        }
        if identities.is_empty() || identities.len() != binders.len() {
            return Err(illegal());
        }
        Ok(OfferedPsks { identities, binders })
    }

    /// Encodes the extension data.
    pub fn encode(&self) -> Vec<u8> {
        let mut ids = Vec::new();
        for (id, age) in &self.identities {
            put_vec16(&mut ids, id);
            ids.extend_from_slice(&age.to_be_bytes());
        }
        let mut binders = Vec::new();
        for b in &self.binders {
            put_vec8(&mut binders, b);
        }
        let mut out = Vec::new();
        put_vec16(&mut out, &ids);
        put_vec16(&mut out, &binders);
        out
    }

    /// Length of the encoded binders list, with its 2-byte length: what the binder's
    /// transcript leaves out of the ClientHello.
    pub fn binders_len(&self) -> usize {
        2 + self.binders.iter().map(|b| 1 + b.len()).sum::<usize>()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let mut e = Extensions::default();
        e.push(ext::SERVER_NAME, vec![1]);
        let ch = ClientHello {
            legacy_version: TLS12,
            random: [1; 32],
            session_id: vec![2; 32],
            suites: vec![0x1301],
            compression: vec![0],
            extensions: e.clone(),
        };
        let enc = ch.encode();
        let (t, body) = unframe(&enc).unwrap();
        assert_eq!(t, hs::CLIENT_HELLO);
        assert_eq!(ClientHello::parse(body).unwrap(), ch);
        let sh = ServerHello {
            random: HRR_RANDOM,
            session_id: vec![],
            suite: 0x1301,
            extensions: e.clone(),
        };
        assert!(ServerHello::parse(unframe(&sh.encode()).unwrap().1).unwrap().is_retry());
        let cm = CertificateMsg {
            context: vec![],
            entries: vec![CertEntry { der: vec![1], extensions: e.clone() }],
        };
        assert_eq!(CertificateMsg::parse(unframe(&cm.encode()).unwrap().1).unwrap(), cm);
        let cv = CertificateVerify { scheme: 0x0403, signature: vec![1, 2] };
        assert_eq!(CertificateVerify::parse(unframe(&cv.encode()).unwrap().1).unwrap(), cv);
        let cr = CertificateRequest { context: vec![], extensions: e.clone() };
        assert_eq!(CertificateRequest::parse(unframe(&cr.encode()).unwrap().1).unwrap(), cr);
        let nst = NewSessionTicket {
            lifetime: 1,
            age_add: 2,
            nonce: vec![3],
            ticket: vec![4],
            extensions: e.clone(),
        };
        assert_eq!(NewSessionTicket::parse(unframe(&nst.encode()).unwrap().1).unwrap(), nst);
        let psk = OfferedPsks { identities: vec![(vec![1], 7)], binders: vec![vec![9; 32]] };
        assert_eq!(OfferedPsks::parse(&psk.encode()).unwrap(), psk);
        assert_eq!(psk.binders_len(), 35);
        assert_eq!(read_u16_list(&u16_list(&[1, 2])).unwrap(), vec![1, 2]);
        assert!(e.only(&[ext::SERVER_NAME]).is_ok());
        assert_eq!(e.only(&[]), Err(Error::Local(Alert::UnsupportedExtension)));
        let mut r = Reader::new(&[0, 0x1d, 0, 1, 5]);
        assert_eq!(read_key_share_entry(&mut r).unwrap(), (0x1d, vec![5]));
        assert_eq!(key_share_entry(0x1d, &[5]), vec![0, 0x1d, 0, 1, 5]);
    }

    #[test]
    fn malformed_messages() {
        let dup = [0, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(Extensions::read(&mut Reader::new(&dup)), Err(illegal()));
        let mut base = ClientHello {
            legacy_version: TLS12,
            random: [1; 32],
            session_id: vec![],
            suites: vec![0x1301],
            compression: vec![0],
            extensions: Extensions(vec![(ext::PRE_SHARED_KEY, vec![]), (ext::SERVER_NAME, vec![])]),
        };
        assert_eq!(ClientHello::parse(unframe(&base.encode()).unwrap().1), Err(illegal()));
        base.extensions = Extensions::default();
        base.session_id = vec![0; 33];
        assert_eq!(ClientHello::parse(unframe(&base.encode()).unwrap().1), Err(decode()));
        base.session_id = vec![];
        base.compression = vec![];
        assert_eq!(ClientHello::parse(unframe(&base.encode()).unwrap().1), Err(decode()));
        base.compression = vec![0];
        base.suites = vec![];
        assert_eq!(ClientHello::parse(unframe(&base.encode()).unwrap().1), Err(decode()));
        // Without an extensions block.
        let mut legacy = vec![3, 3];
        legacy.extend_from_slice(&[0; 32]);
        legacy.extend_from_slice(&[0, 0, 2, 0, 0x2f, 1, 0]);
        assert!(ClientHello::parse(&legacy).unwrap().extensions.0.is_empty());
        let mut sh = ServerHello {
            random: [0; 32],
            session_id: vec![],
            suite: 1,
            extensions: Extensions::default(),
        }
        .encode();
        let n = sh.len();
        sh[n - 3] = 1;
        assert_eq!(ServerHello::parse(unframe(&sh).unwrap().1), Err(illegal()));
        assert_eq!(CertificateMsg::parse(&[0, 0, 0, 5, 0, 0, 0, 0, 0]), Err(decode()));
        let empty_ticket = NewSessionTicket {
            lifetime: 0,
            age_add: 0,
            nonce: vec![],
            ticket: vec![],
            extensions: Extensions::default(),
        };
        assert_eq!(
            NewSessionTicket::parse(unframe(&empty_ticket.encode()).unwrap().1),
            Err(decode())
        );
        assert_eq!(read_u16_list(&[0, 1, 0]), Err(decode()));
        assert_eq!(read_key_share_entry(&mut Reader::new(&[0, 0x1d, 0, 0])), Err(decode()));
        let mismatched = OfferedPsks { identities: vec![(vec![1], 7)], binders: vec![] };
        assert_eq!(OfferedPsks::parse(&mismatched.encode()), Err(illegal()));
        assert_eq!(unframe(&[1, 0, 0, 2, 0]), Err(decode()));
    }
}
