//! The record layer of TLS 1.3 (RFC 8446, 5): framing, record protection with a per-record nonce,
//! inner content type and padding, and the reassembly of handshake messages from records; and the
//! record protection of TLS 1.2 (RFC 5246, 6.2.3.3; RFC 5288, 3; RFC 7905, 2).

use crate::codec::Reader;
use crate::keyschedule::TrafficKeys;
use crate::suites::{Aead, CipherSuite, NONCE_LEN, TAG_LEN};
use crate::tls12::{EXPLICIT_NONCE_LEN, Keys12, Tls12Suite};
use crate::{Alert, Error};

/// Largest plaintext fragment (2^14).
pub const MAX_PLAINTEXT: usize = 1 << 14;
/// Largest protected fragment (2^14 + 256).
pub const MAX_CIPHERTEXT: usize = MAX_PLAINTEXT + 256;
/// Record header length.
pub const HEADER_LEN: usize = 5;
/// Largest handshake message accepted (certificate chains included).
pub const MAX_HANDSHAKE_MESSAGE: usize = 1 << 17;
/// `legacy_record_version` of protected records and of every record after the first flight.
pub const LEGACY_VERSION: u16 = 0x0303;

/// A record content type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ContentType {
    /// `change_cipher_spec` (only the compatibility record of TLS 1.3).
    ChangeCipherSpec,
    /// alert.
    Alert,
    /// handshake.
    Handshake,
    /// `application_data`.
    ApplicationData,
}

impl ContentType {
    /// The code point.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            ContentType::ChangeCipherSpec => 20,
            ContentType::Alert => 21,
            ContentType::Handshake => 22,
            ContentType::ApplicationData => 23,
        }
    }

    /// The type of a code point.
    #[must_use]
    pub fn from_code(code: u8) -> Option<ContentType> {
        match code {
            20 => Some(ContentType::ChangeCipherSpec),
            21 => Some(ContentType::Alert),
            22 => Some(ContentType::Handshake),
            23 => Some(ContentType::ApplicationData),
            _ => None,
        }
    }
}

/// How protected records are framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// TLS 1.3: the real type inside, the outer one `application_data`.
    Tls13,
    /// TLS 1.2 with AES-GCM: an 8-byte explicit nonce before the ciphertext.
    Tls12Explicit,
    /// TLS 1.2 with ChaCha20-Poly1305: the nonce built as in TLS 1.3.
    Tls12Implicit,
}

/// One direction's record protection.
#[derive(Debug)]
pub struct Protection {
    aead: Aead,
    iv: [u8; NONCE_LEN],
    seq: u64,
    framing: Framing,
}

impl Protection {
    /// Protection with the keys of a traffic secret.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::InternalError`] for keys of the wrong length.
    pub fn new(suite: CipherSuite, keys: &TrafficKeys) -> Result<Protection, Error> {
        Ok(Protection {
            aead: suite.aead(&keys.key.0)?,
            iv: keys.iv,
            seq: 0,
            framing: Framing::Tls13,
        })
    }

    /// TLS 1.2 protection with one direction of the key block. With AES-GCM the IV is the 4-byte
    /// salt followed by zeros, so the nonce is the salt and the sequence number, which this side
    /// also sends as the explicit nonce.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::InternalError`] for keys of the wrong length.
    pub fn tls12(suite: Tls12Suite, keys: &Keys12) -> Result<Protection, Error> {
        let mut iv = [0u8; NONCE_LEN];
        iv.get_mut(..keys.iv.len())
            .ok_or(Error::Local(Alert::InternalError))?
            .copy_from_slice(&keys.iv);
        let framing = if suite.fixed_iv_len() == NONCE_LEN {
            Framing::Tls12Implicit
        } else {
            Framing::Tls12Explicit
        };
        Ok(Protection { aead: suite.aead(&keys.key.0)?, iv, seq: 0, framing })
    }

    /// Whether this is TLS 1.2 protection.
    #[must_use]
    pub fn is_tls12(&self) -> bool {
        self.framing != Framing::Tls13
    }

    /// The additional data of a TLS 1.2 record (RFC 5246, 6.2.3.3).
    fn aad12(&self, typ: ContentType, len: usize) -> [u8; 13] {
        let mut aad = [0u8; 13];
        aad[..8].copy_from_slice(&self.seq.to_be_bytes());
        aad[8..].copy_from_slice(&header(typ, len));
        aad
    }

    /// Protects one TLS 1.2 fragment of type `typ`.
    fn seal12(
        &mut self,
        typ: ContentType,
        fragment: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        let explicit = if self.framing == Framing::Tls12Explicit { EXPLICIT_NONCE_LEN } else { 0 };
        let aad = self.aad12(typ, fragment.len());
        let nonce = self.nonce();
        let mut body = fragment.to_vec();
        let tag = self.aead.seal(&nonce, &aad, &mut body);
        out.extend_from_slice(&header(typ, explicit + body.len() + TAG_LEN));
        out.extend_from_slice(&nonce[NONCE_LEN - explicit..]);
        out.extend_from_slice(&body);
        out.extend_from_slice(&tag);
        self.bump()
    }

    /// Removes the protection of a TLS 1.2 record of type `typ`.
    fn open12(&mut self, typ: ContentType, body: &[u8]) -> Result<Vec<u8>, Error> {
        let explicit = if self.framing == Framing::Tls12Explicit { EXPLICIT_NONCE_LEN } else { 0 };
        if body.len() < explicit + TAG_LEN {
            return Err(Error::Local(Alert::BadRecordMac));
        }
        let mut nonce = self.nonce();
        nonce[NONCE_LEN - explicit..].copy_from_slice(&body[..explicit]);
        let (data, tag) = body[explicit..].split_at(body.len() - explicit - TAG_LEN);
        if data.len() > MAX_PLAINTEXT {
            return Err(Error::Local(Alert::RecordOverflow));
        }
        let aad = self.aad12(typ, data.len());
        let mut plain = data.to_vec();
        self.aead.open(&nonce, &aad, &mut plain, tag)?;
        self.bump()?;
        Ok(plain)
    }

    fn nonce(&self) -> [u8; NONCE_LEN] {
        let mut n = self.iv;
        for (b, s) in n[NONCE_LEN - 8..].iter_mut().zip(self.seq.to_be_bytes()) {
            *b ^= s;
        }
        n
    }

    fn bump(&mut self) -> Result<(), Error> {
        self.seq = self.seq.checked_add(1).ok_or(Error::Local(Alert::InternalError))?;
        Ok(())
    }

    /// Protects one fragment: `TLSInnerPlaintext` with no padding, as an `application_data` record.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::InternalError`] when the sequence number runs out.
    pub fn seal(
        &mut self,
        typ: ContentType,
        fragment: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        if self.is_tls12() {
            return self.seal12(typ, fragment, out);
        }
        let len = fragment.len() + 1 + TAG_LEN;
        let header = header(ContentType::ApplicationData, len);
        let mut body = Vec::with_capacity(len);
        body.extend_from_slice(fragment);
        body.push(typ.code());
        let tag = self.aead.seal(&self.nonce(), &header, &mut body);
        out.extend_from_slice(&header);
        out.extend_from_slice(&body);
        out.extend_from_slice(&tag);
        self.bump()
    }

    /// Removes the protection of a record and returns its real type and content.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::RecordOverflow`], [`Alert::BadRecordMac`], [`Alert::DecodeError`] or
    /// [`Alert::UnexpectedMessage`] for a record that is too long, does not authenticate, is malformed or comes
    /// when it may not.
    pub fn open(&mut self, header: &[u8], body: &[u8]) -> Result<(ContentType, Vec<u8>), Error> {
        if body.len() < TAG_LEN + 1 {
            return Err(Error::Local(Alert::BadRecordMac));
        }
        let (data, tag) = body.split_at(body.len() - TAG_LEN);
        let mut plain = data.to_vec();
        self.aead.open(&self.nonce(), header, &mut plain, tag)?;
        self.bump()?;
        let end =
            plain.iter().rposition(|&b| b != 0).ok_or(Error::Local(Alert::UnexpectedMessage))?;
        if end > MAX_PLAINTEXT {
            return Err(Error::Local(Alert::RecordOverflow));
        }
        let typ =
            ContentType::from_code(plain[end]).ok_or(Error::Local(Alert::UnexpectedMessage))?;
        plain.truncate(end);
        Ok((typ, plain))
    }
}

/// A record header.
///
/// # Panics
///
/// When `len` is above 65 535: the callers fragment records to at most 2¹⁴ + 256 bytes.
#[must_use]
pub fn header(typ: ContentType, len: usize) -> [u8; HEADER_LEN] {
    let v = LEGACY_VERSION.to_be_bytes();
    let l = u16::try_from(len).expect("a record holds at most 2^14 + 256 bytes").to_be_bytes();
    [typ.code(), v[0], v[1], l[0], l[1]]
}

/// Records in and out of one connection.
#[derive(Debug, Default)]
pub struct RecordLayer {
    /// Protection of records read, once keys exist.
    pub read: Option<Protection>,
    /// Protection of records written, once keys exist.
    pub write: Option<Protection>,
    /// Bytes received; those before `read_at` are records already read.
    input: Vec<u8>,
    read_at: usize,
}

/// One record as read, without protection.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    /// The (inner) content type.
    pub typ: ContentType,
    /// The content.
    pub data: Vec<u8>,
    /// Whether it came protected.
    pub protected: bool,
}

impl RecordLayer {
    /// Appends bytes received from the peer.
    pub fn push_input(&mut self, data: &[u8]) {
        // The records read leave once they are at least half the buffer, so each byte moves a
        // bounded number of times however many records arrive at once.
        if self.read_at > 0 && self.read_at * 2 >= self.input.len() {
            self.input.drain(..self.read_at);
            self.read_at = 0;
        }
        self.input.extend_from_slice(data);
    }

    /// Whether received bytes are waiting.
    #[must_use]
    pub fn has_input(&self) -> bool {
        self.read_at < self.input.len()
    }

    /// The next complete record, if the input holds one.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::RecordOverflow`], [`Alert::BadRecordMac`], [`Alert::DecodeError`] or
    /// [`Alert::UnexpectedMessage`] for a record that is too long, does not authenticate, is malformed or comes
    /// when it may not.
    pub fn next_record(&mut self) -> Result<Option<Record>, Error> {
        let pending = &self.input[self.read_at..];
        if pending.len() < HEADER_LEN {
            return Ok(None);
        }
        let mut r = Reader::new(&pending[..HEADER_LEN]);
        let code = r.u8()?;
        let version = r.u16()?;
        let len = usize::from(r.u16()?);
        let typ = ContentType::from_code(code).ok_or(Error::Local(Alert::UnexpectedMessage))?;
        if version >> 8 != 3 {
            return Err(Error::Local(Alert::DecodeError));
        }
        if len > MAX_CIPHERTEXT || (self.read.is_none() && len > MAX_PLAINTEXT) {
            return Err(Error::Local(Alert::RecordOverflow));
        }
        if pending.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let start = self.read_at;
        self.read_at += HEADER_LEN + len;
        let (head, body) = self.input[start..self.read_at].split_at(HEADER_LEN);
        let record = match (typ, self.read.as_mut()) {
            // The compatibility change_cipher_spec stays unprotected (RFC 8446, 5), and TLS 1.2's
            // is never protected; without keys nothing is.
            (ContentType::ChangeCipherSpec, _) | (_, None) => {
                Record { typ, data: body.to_vec(), protected: false }
            }
            (_, Some(p)) if p.is_tls12() => {
                Record { typ, data: p.open12(typ, body)?, protected: true }
            }
            (ContentType::ApplicationData, Some(p)) => {
                let (inner, data) = p.open(head, body)?;
                Record { typ: inner, data, protected: true }
            }
            (_, Some(_)) => return Err(Error::Local(Alert::UnexpectedMessage)),
        };
        if record.data.is_empty() && record.typ != ContentType::ApplicationData {
            return Err(Error::Local(Alert::UnexpectedMessage));
        }
        Ok(Some(record))
    }

    /// Writes `data` as records of type `typ`, protected if keys exist.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::InternalError`] when the sequence number runs out.
    pub fn write(&mut self, typ: ContentType, data: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
        for frag in data.chunks(MAX_PLAINTEXT) {
            if let Some(p) = self.write.as_mut() {
                p.seal(typ, frag, out)?;
            } else {
                out.extend_from_slice(&header(typ, frag.len()));
                out.extend_from_slice(frag);
            }
        }
        Ok(())
    }
}

/// Handshake messages reassembled from handshake records.
#[derive(Debug, Default)]
pub struct HandshakeBuffer {
    data: Vec<u8>,
}

impl HandshakeBuffer {
    /// Appends a handshake record's content.
    pub fn push(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
    }

    /// Whether a partial message is waiting (forbidden across a key change).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The next complete message, header included.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] for a handshake message cut at the end of a record that is not continued.
    pub fn next_message(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if self.data.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([0, self.data[1], self.data[2], self.data[3]]) as usize;
        if len > MAX_HANDSHAKE_MESSAGE {
            return Err(Error::Local(Alert::DecodeError));
        }
        if self.data.len() < 4 + len {
            return Ok(None);
        }
        Ok(Some(self.data.drain(..4 + len).collect()))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {

    use super::*;
    use crate::keyschedule::Secret;

    fn keys() -> TrafficKeys {
        TrafficKeys { key: Secret(vec![1; 16]), iv: [2; 12] }
    }

    #[test]
    fn protected_records_round_trip_and_fail_closed() {
        let suite = CipherSuite::Aes128GcmSha256;
        let mut w = RecordLayer {
            write: Some(Protection::new(suite, &keys()).expect("new succeeds")),
            ..Default::default()
        };
        let mut r = RecordLayer {
            read: Some(Protection::new(suite, &keys()).expect("new succeeds")),
            ..Default::default()
        };
        let mut out = Vec::new();
        let big = vec![7u8; MAX_PLAINTEXT + 10];
        w.write(ContentType::ApplicationData, &big, &mut out).expect("write succeeds");
        w.write(ContentType::Alert, &[1, 0], &mut out).expect("write succeeds");
        r.push_input(&out[..3]);
        assert_eq!(r.next_record(), Ok(None));
        r.push_input(&out[3..]);
        assert_eq!(
            r.next_record()
                .expect("next_record succeeds")
                .expect("the value is present")
                .data
                .len(),
            MAX_PLAINTEXT
        );
        assert_eq!(
            r.next_record()
                .expect("next_record succeeds")
                .expect("the value is present")
                .data
                .len(),
            10
        );
        let alert = r.next_record().expect("next_record succeeds").expect("the value is present");
        assert_eq!((alert.typ, alert.protected), (ContentType::Alert, true));
        assert!(!r.has_input());
        // A tampered record, a plaintext handshake record after keys, and a short body.
        let mut bad = Vec::new();
        w.write(ContentType::Handshake, &[1, 2, 3], &mut bad).expect("write succeeds");
        let last = bad.len() - 1;
        bad[last] ^= 1;
        r.push_input(&bad);
        assert_eq!(r.next_record(), Err(Error::Local(Alert::BadRecordMac)));
        let mut r2 = RecordLayer {
            read: Some(Protection::new(suite, &keys()).expect("new succeeds")),
            ..Default::default()
        };
        r2.push_input(&[22, 3, 3, 0, 1, 1]);
        assert_eq!(r2.next_record(), Err(Error::Local(Alert::UnexpectedMessage)));
        let mut r3 = RecordLayer {
            read: Some(Protection::new(suite, &keys()).expect("new succeeds")),
            ..Default::default()
        };
        r3.push_input(&[23, 3, 3, 0, 4, 1, 2, 3, 4]);
        assert_eq!(r3.next_record(), Err(Error::Local(Alert::BadRecordMac)));
        // All-zero inner plaintext has no content type.
        let q = Protection::new(suite, &keys()).expect("new succeeds");
        let mut z = Vec::new();
        let mut body = vec![0u8];
        let hdr = header(ContentType::ApplicationData, 1 + TAG_LEN);
        let tag = q.aead.seal(&q.nonce(), &hdr, &mut body);
        z.extend_from_slice(&hdr);
        z.extend_from_slice(&body);
        z.extend_from_slice(&tag);
        let mut r4 = RecordLayer {
            read: Some(Protection::new(suite, &keys()).expect("new succeeds")),
            ..Default::default()
        };
        r4.push_input(&z);
        assert_eq!(r4.next_record(), Err(Error::Local(Alert::UnexpectedMessage)));
    }

    #[test]
    fn plaintext_records_and_limits() {
        let mut r = RecordLayer::default();
        r.push_input(&[24, 3, 3, 0, 0]);
        assert_eq!(r.next_record(), Err(Error::Local(Alert::UnexpectedMessage)));
        let mut r = RecordLayer::default();
        r.push_input(&[22, 2, 0, 0, 1, 0]);
        assert_eq!(r.next_record(), Err(Error::Local(Alert::DecodeError)));
        let mut r = RecordLayer::default();
        r.push_input(&[22, 3, 1, 0x40, 0x01]);
        assert_eq!(r.next_record(), Err(Error::Local(Alert::RecordOverflow)));
        let mut r = RecordLayer::default();
        r.push_input(&[22, 3, 1, 0, 0]);
        assert_eq!(r.next_record(), Err(Error::Local(Alert::UnexpectedMessage)));
        let mut r = RecordLayer::default();
        r.push_input(&[20, 3, 3, 0, 1, 1]);
        assert_eq!(
            r.next_record().expect("next_record succeeds").expect("the value is present").typ,
            ContentType::ChangeCipherSpec
        );
        let mut out = Vec::new();
        r.write(ContentType::Handshake, &[1], &mut out).expect("write succeeds");
        assert_eq!(out, vec![22, 3, 3, 0, 1, 1]);
        for code in [20u8, 21, 22, 23] {
            assert_eq!(ContentType::from_code(code).expect("from_code succeeds").code(), code);
        }
        let mut hb = HandshakeBuffer::default();
        hb.push(&[1, 0, 0]);
        assert_eq!(hb.next_message(), Ok(None));
        hb.push(&[2, 9]);
        assert_eq!(hb.next_message(), Ok(None));
        hb.push(&[9]);
        assert_eq!(hb.next_message(), Ok(Some(vec![1, 0, 0, 2, 9, 9])));
        assert!(hb.is_empty());
        hb.push(&[1, 0xff, 0xff, 0xff]);
        assert_eq!(hb.next_message(), Err(Error::Local(Alert::DecodeError)));
    }
}
