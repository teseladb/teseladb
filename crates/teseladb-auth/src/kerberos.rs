//! GSSAPI with Kerberos 5 (013 FR-025), the acceptor's side without I/O: the service keys of a
//! keytab (MIT format), the client's `AP-REQ` inside the GSS-API initial token (RFC 4120,
//! RFC 1964 framing), the mutual authentication `AP-REP`, the per-message tokens of RFC 4121
//! and the security layer negotiation of the SASL mechanism (RFC 4752). Encryption types are
//! the AES ones of `teseladb-crypto` (RFC 3962 and RFC 8009). The user is `<cname>@<crealm>`
//! of `$external`.

use std::fmt;

use teseladb_crypto::HashAlgorithm;
use teseladb_crypto::kerberos::EncType;
use teseladb_x509::der::{self, Reader};

use crate::users::{EXTERNAL_DB, UserName};

/// Why a step failed; the client only learns `Authentication failed.`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KerberosError(pub String);

impl fmt::Display for KerberosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KerberosError {}

fn fail<T>(m: impl Into<String>) -> Result<T, KerberosError> {
    Err(KerberosError(m.into()))
}

fn bad(what: &str) -> impl Fn(teseladb_x509::Error) -> KerberosError + '_ {
    move |e| KerberosError(format!("{what}: {e:?}"))
}

/// The Kerberos 5 mechanism of GSS-API, 1.2.840.113554.1.2.2, DER contents.
pub const KRB5_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
/// How far the client's clock may be from the node's (RFC 4120's usual five minutes).
pub const CLOCK_SKEW_SECS: i64 = 300;
/// Key usages (RFC 4120, section 7.5.1; RFC 4121, section 2: a Wrap token uses the SEAL usage
/// whether it is sealed or only signed).
const USAGE_TICKET: u32 = 2;
const USAGE_AUTHENTICATOR: u32 = 11;
const USAGE_AP_REP: u32 = 12;
const USAGE_ACCEPTOR_SEAL: u32 = 22;
const USAGE_INITIATOR_SEAL: u32 = 24;
/// GSS token identifiers.
const TOK_AP_REQ: [u8; 2] = [0x01, 0x00];
const TOK_AP_REP: [u8; 2] = [0x02, 0x00];
const TOK_WRAP: [u8; 2] = [0x05, 0x04];
/// Wrap token flags.
const FLAG_SENT_BY_ACCEPTOR: u8 = 0x01;
const FLAG_SEALED: u8 = 0x02;
const FLAG_ACCEPTOR_SUBKEY: u8 = 0x04;
/// `ap-options` bit of mutual authentication (bit 2, the third of the first byte).
const MUTUAL_REQUIRED: u8 = 0x20;
/// The authenticator checksum type of GSS-API (RFC 4121, section 4.1.1).
const GSS_CHECKSUM: i64 = 0x8003;
/// Its flag of mutual authentication.
const GSS_MUTUAL: u32 = 0x02;
/// SASL GSSAPI's "no security layer" (RFC 4752, section 3.3).
const LAYER_NONE: u8 = 0x01;
/// The largest message the node takes in the security layer: none, as Cyrus SASL sends.
const MAX_MESSAGE: [u8; 3] = [0, 0, 0];
/// The keytab format MIT writes.
const KEYTAB_VERSION: [u8; 2] = [0x05, 0x02];

fn etype(n: i64) -> Option<EncType> {
    [
        EncType::Aes128CtsHmacSha1,
        EncType::Aes256CtsHmacSha1,
        EncType::Aes128CtsHmacSha256,
        EncType::Aes256CtsHmacSha384,
    ]
    .into_iter()
    .find(|e| i64::from(e.number()) == n)
}

fn checksum_hash(e: EncType) -> HashAlgorithm {
    match e {
        EncType::Aes128CtsHmacSha1 | EncType::Aes256CtsHmacSha1 => HashAlgorithm::Sha1,
        EncType::Aes128CtsHmacSha256 => HashAlgorithm::Sha256,
        EncType::Aes256CtsHmacSha384 => HashAlgorithm::Sha384,
    }
}

/// A key of the keytab.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceKey {
    /// `service/host@REALM`.
    pub principal: String,
    /// Key version.
    pub kvno: u32,
    /// Encryption type.
    pub etype: EncType,
    key: Vec<u8>,
}

impl fmt::Debug for ServiceKey {
    /// Without the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceKey")
            .field("principal", &self.principal)
            .field("kvno", &self.kvno)
            .field("etype", &self.etype)
            .finish_non_exhaustive()
    }
}

impl Drop for ServiceKey {
    fn drop(&mut self) {
        self.key.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.key);
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], KerberosError> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.data.len());
        let end = end.ok_or_else(|| KerberosError("keytab: truncated".into()))?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, KerberosError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, KerberosError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn text(&mut self) -> Result<String, KerberosError> {
        let n = usize::from(self.u16()?);
        String::from_utf8(self.take(n)?.to_vec())
            .map_err(|_| KerberosError("keytab: a name is not UTF-8".into()))
    }
}

/// The AES keys of a keytab (MIT format 0x0502); entries of other encryption types are left out.
pub fn parse_keytab(data: &[u8]) -> Result<Vec<ServiceKey>, KerberosError> {
    if data.get(..2) != Some(&KEYTAB_VERSION[..]) {
        return fail("keytab: not a version 0x0502 keytab");
    }
    let mut c = Cursor { data, pos: 2 };
    let mut out = Vec::new();
    while c.pos < data.len() {
        let size = i32::from_be_bytes(c.take(4)?.try_into().unwrap_or([0; 4]));
        let Ok(len) = usize::try_from(size) else {
            // A deleted entry: its absolute size is a hole to skip.
            c.take(usize::try_from(size.unsigned_abs()).unwrap_or(usize::MAX))?;
            continue;
        };
        let entry = c.take(len)?;
        let mut e = Cursor { data: entry, pos: 0 };
        let components = e.u16()?;
        let realm = e.text()?;
        let names = (0..components).map(|_| e.text()).collect::<Result<Vec<_>, _>>()?;
        let _name_type = e.u32()?;
        let _timestamp = e.u32()?;
        let vno8 = e.take(1)?[0];
        let key_type = e.u16()?;
        let key_len = usize::from(e.u16()?);
        let key = e.take(key_len)?.to_vec();
        // A 32-bit version after the key replaces the 8-bit one when present and non-zero.
        let kvno = match e.take(4) {
            Ok(v) if v != [0; 4] => u32::from_be_bytes(v.try_into().unwrap_or([0; 4])),
            _ => u32::from(vno8),
        };
        if let Some(etype) = etype(i64::from(key_type)).filter(|t| t.key_len() == key.len()) {
            let principal = format!("{}@{realm}", names.join("/"));
            out.push(ServiceKey { principal, kvno, etype, key });
        }
    }
    Ok(out)
}

/// `[n]` of a context-specific explicit tag.
fn ctx(n: u8) -> u8 {
    0xa0 | n
}

/// `[APPLICATION n]`, constructed.
fn app(n: u8) -> u8 {
    0x60 | n
}

const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OCTET_STRING: u8 = 0x04;
const OID: u8 = 0x06;
const SEQUENCE: u8 = 0x30;
const GENERAL_STRING: u8 = 0x1b;
const GENERALIZED_TIME: u8 = 0x18;

fn int(value: &[u8]) -> Result<i64, KerberosError> {
    let v = der::integer(value).map_err(bad("integer"))?;
    if v.len() > 8 {
        return fail("integer too large");
    }
    let neg = v.first().is_some_and(|b| b & 0x80 != 0);
    let mut out: i64 = if neg { -1 } else { 0 };
    for b in v {
        out = (out << 8) | i64::from(*b);
    }
    Ok(out)
}

fn explicit<'a>(r: &mut Reader<'a>, n: u8, tag: u8) -> Result<&'a [u8], KerberosError> {
    let inner = r.read(ctx(n)).map_err(bad("field"))?;
    der::read_single(inner, tag).map(|t| t.value).map_err(bad("field"))
}

fn optional<'a>(r: &mut Reader<'a>, n: u8, tag: u8) -> Result<Option<&'a [u8]>, KerberosError> {
    match r.read_optional(ctx(n)).map_err(bad("field"))? {
        None => Ok(None),
        Some(inner) => der::read_single(inner, tag).map(|t| Some(t.value)).map_err(bad("field")),
    }
}

fn text(v: &[u8]) -> Result<String, KerberosError> {
    String::from_utf8(v.to_vec()).map_err(|_| KerberosError("a name is not UTF-8".into()))
}

/// A `PrincipalName`: its components joined with `/`.
fn principal_name(v: &[u8]) -> Result<String, KerberosError> {
    let mut r = Reader::new(v);
    explicit(&mut r, 0, INTEGER)?;
    let names = explicit(&mut r, 1, SEQUENCE)?;
    let mut n = Reader::new(names);
    let mut parts = Vec::new();
    while !n.is_empty() {
        parts.push(text(n.read(GENERAL_STRING).map_err(bad("name"))?)?);
    }
    Ok(parts.join("/"))
}

fn time(v: &[u8]) -> Result<i64, KerberosError> {
    teseladb_x509::time::parse(GENERALIZED_TIME, v).map_err(bad("time"))
}

/// `EncryptedData`: its encryption type, key version and cipher text.
fn encrypted(v: &[u8]) -> Result<(EncType, Option<u32>, Vec<u8>), KerberosError> {
    let mut r = Reader::new(v);
    let n = int(explicit(&mut r, 0, INTEGER)?)?;
    let etype = etype(n).ok_or_else(|| KerberosError(format!("encryption type {n}")))?;
    let kvno = optional(&mut r, 1, INTEGER)?.map(int).transpose()?;
    let cipher = explicit(&mut r, 2, OCTET_STRING)?.to_vec();
    Ok((etype, kvno.and_then(|k| u32::try_from(k).ok()), cipher))
}

/// The key a token carries: `EncryptionKey`.
fn encryption_key(v: &[u8]) -> Result<(EncType, Vec<u8>), KerberosError> {
    let mut r = Reader::new(v);
    let n = int(explicit(&mut r, 0, INTEGER)?)?;
    let e = etype(n).ok_or_else(|| KerberosError(format!("key type {n}")))?;
    let key = explicit(&mut r, 1, OCTET_STRING)?.to_vec();
    if key.len() != e.key_len() {
        return fail("key of the wrong length");
    }
    Ok((e, key))
}

fn encode_time(secs: i64) -> Vec<u8> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let s = format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60);
    der::tlv(GENERALIZED_TIME, s.as_bytes())
}

fn explicit_out(n: u8, inner: &[u8]) -> Vec<u8> {
    der::tlv(ctx(n), inner)
}

fn small_int(n: i64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let mut i = 0;
    while i < 7
        && ((bytes[i] == 0 && bytes[i + 1] & 0x80 == 0)
            || (bytes[i] == 0xff && bytes[i + 1] & 0x80 != 0))
    {
        i += 1;
    }
    der::tlv(INTEGER, &bytes[i..])
}

/// The GSS-API framing of RFC 1964: `[APPLICATION 0]` with the mechanism, the token
/// identifier and the inner token.
fn frame(tok_id: [u8; 2], inner: &[u8]) -> Vec<u8> {
    let body = [&der::tlv(OID, KRB5_OID)[..], &tok_id, inner].concat();
    der::tlv(app(0), &body)
}

fn unframe(token: &[u8], tok_id: [u8; 2]) -> Result<&[u8], KerberosError> {
    let t = der::read_single(token, app(0)).map_err(bad("GSS token"))?;
    let mech = Reader::new(t.value).read_any().map_err(bad("GSS token"))?;
    if mech.tag != OID || mech.value != KRB5_OID {
        return fail("not a Kerberos 5 token");
    }
    let rest = &t.value[mech.raw.len()..];
    if rest.get(..2) != Some(&tok_id[..]) {
        return fail("unexpected GSS token");
    }
    Ok(&rest[2..])
}

/// What the acceptor learnt from the client's `AP-REQ`.
#[derive(Clone)]
pub struct Accepted {
    /// The user: `<cname>@<crealm>` of `$external`.
    pub user: UserName,
    /// The token to send back: the `AP-REP` when the client asked for mutual authentication.
    pub reply: Option<Vec<u8>>,
    /// The context for the per-message tokens.
    pub context: Context,
}

impl fmt::Debug for Accepted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Accepted").field("user", &self.user).finish_non_exhaustive()
    }
}

/// The keys and sequence numbers of an established context (RFC 4121).
#[derive(Clone)]
pub struct Context {
    etype: EncType,
    key: Vec<u8>,
    send_seq: u64,
}

impl Drop for Context {
    fn drop(&mut self) {
        self.key.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.key);
    }
}

/// The parts of an `AP-REQ` the acceptor reads.
struct ApReq<'a> {
    mutual: bool,
    ticket: &'a [u8],
    authenticator: &'a [u8],
}

fn read_ap_req(token: &[u8]) -> Result<ApReq<'_>, KerberosError> {
    let ap_req = unframe(token, TOK_AP_REQ)?;
    let req = der::read_single(ap_req, app(14)).map_err(bad("AP-REQ"))?;
    let req = der::read_single(req.value, SEQUENCE).map_err(bad("AP-REQ"))?;
    let mut r = Reader::new(req.value);
    if int(explicit(&mut r, 0, INTEGER)?)? != 5 || int(explicit(&mut r, 1, INTEGER)?)? != 14 {
        return fail("not a Kerberos 5 AP-REQ");
    }
    let options = explicit(&mut r, 2, BIT_STRING)?;
    let mutual = options.get(1).is_some_and(|b| b & MUTUAL_REQUIRED != 0);
    let ticket = r.read(ctx(3)).map_err(bad("ticket"))?;
    let authenticator = explicit(&mut r, 4, SEQUENCE)?;
    Ok(ApReq { mutual, ticket, authenticator })
}

/// What the ticket's encrypted part says: the session key and the client.
struct TicketPart {
    session_type: EncType,
    session: Vec<u8>,
    crealm: String,
    cname: String,
}

impl Drop for TicketPart {
    fn drop(&mut self) {
        self.session.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.session);
    }
}

/// Opens the ticket with the service key it names and checks it is valid at `now`.
fn open_ticket(ticket: &[u8], keys: &[ServiceKey], now: i64) -> Result<TicketPart, KerberosError> {
    let ticket = der::read_single(ticket, app(1)).map_err(bad("ticket"))?;
    let ticket = der::read_single(ticket.value, SEQUENCE).map_err(bad("ticket"))?;
    let mut t = Reader::new(ticket.value);
    explicit(&mut t, 0, INTEGER)?;
    let realm = text(explicit(&mut t, 1, GENERAL_STRING)?)?;
    let sname = principal_name(explicit(&mut t, 2, SEQUENCE)?)?;
    let (etype, kvno, cipher) = encrypted(explicit(&mut t, 3, SEQUENCE)?)?;
    let service = format!("{sname}@{realm}");
    let key = keys
        .iter()
        .find(|k| k.principal == service && k.etype == etype && kvno.is_none_or(|v| v == k.kvno))
        .ok_or_else(|| KerberosError(format!("no key for {service} in the keytab")))?;
    let plain = etype
        .decrypt(&key.key, USAGE_TICKET, &cipher)
        .map_err(|_| KerberosError("the ticket does not decrypt".into()))?;
    let part = der::read_single(&plain, app(3)).map_err(bad("EncTicketPart"))?;
    let part = der::read_single(part.value, SEQUENCE).map_err(bad("EncTicketPart"))?;
    let mut p = Reader::new(part.value);
    explicit(&mut p, 0, BIT_STRING)?;
    let (session_type, session) = encryption_key(explicit(&mut p, 1, SEQUENCE)?)?;
    let crealm = text(explicit(&mut p, 2, GENERAL_STRING)?)?;
    let cname = principal_name(explicit(&mut p, 3, SEQUENCE)?)?;
    explicit(&mut p, 4, SEQUENCE)?;
    explicit(&mut p, 5, GENERALIZED_TIME)?;
    let start = optional(&mut p, 6, GENERALIZED_TIME)?.map(time).transpose()?;
    let end = time(explicit(&mut p, 7, GENERALIZED_TIME)?)?;
    if now >= end + CLOCK_SKEW_SECS || start.is_some_and(|s| now + CLOCK_SKEW_SECS < s) {
        return fail("the ticket is not valid now");
    }
    Ok(TicketPart { session_type, session, crealm, cname })
}

/// What the authenticator says, beyond the client it must repeat.
struct AuthenticatorPart {
    cusec: i64,
    ctime: i64,
    subkey: Option<(EncType, Vec<u8>)>,
    seq: Option<i64>,
    gss_mutual: bool,
}

impl Drop for AuthenticatorPart {
    fn drop(&mut self) {
        if let Some((_, k)) = self.subkey.as_mut() {
            k.iter_mut().for_each(|b| *b = 0);
            std::hint::black_box(&k);
        }
    }
}

/// Opens the authenticator with the ticket's session key and checks it names the ticket's
/// client at a time close to `now`.
fn open_authenticator(
    authenticator: &[u8],
    ticket: &TicketPart,
    now: i64,
) -> Result<AuthenticatorPart, KerberosError> {
    let (a_type, _, a_cipher) = encrypted(authenticator)?;
    if a_type != ticket.session_type {
        return fail("the authenticator's encryption type is not the session key's");
    }
    let plain = a_type
        .decrypt(&ticket.session, USAGE_AUTHENTICATOR, &a_cipher)
        .map_err(|_| KerberosError("the authenticator does not decrypt".into()))?;
    let auth = der::read_single(&plain, app(2)).map_err(bad("Authenticator"))?;
    let auth = der::read_single(auth.value, SEQUENCE).map_err(bad("Authenticator"))?;
    let mut a = Reader::new(auth.value);
    explicit(&mut a, 0, INTEGER)?;
    let a_realm = text(explicit(&mut a, 1, GENERAL_STRING)?)?;
    let a_name = principal_name(explicit(&mut a, 2, SEQUENCE)?)?;
    if a_realm != ticket.crealm || a_name != ticket.cname {
        return fail("the authenticator names another client");
    }
    let checksum = optional(&mut a, 3, SEQUENCE)?;
    let cusec = int(explicit(&mut a, 4, INTEGER)?)?;
    let ctime = time(explicit(&mut a, 5, GENERALIZED_TIME)?)?;
    if (now - ctime).abs() > CLOCK_SKEW_SECS {
        return fail("clock skew too great");
    }
    let subkey = optional(&mut a, 6, SEQUENCE)?.map(encryption_key).transpose()?;
    let seq = optional(&mut a, 7, INTEGER)?.map(int).transpose()?;
    let gss_mutual = checksum.map(gss_wants_mutual).transpose()?.unwrap_or(false);
    Ok(AuthenticatorPart { cusec, ctime, subkey, seq, gss_mutual })
}

/// Whether the GSS-API authenticator checksum (RFC 4121 4.1.1) asks for mutual authentication.
fn gss_wants_mutual(checksum: &[u8]) -> Result<bool, KerberosError> {
    let mut cr = Reader::new(checksum);
    let kind = int(explicit(&mut cr, 0, INTEGER)?)?;
    let value = explicit(&mut cr, 1, OCTET_STRING)?;
    if kind != GSS_CHECKSUM || value.len() < 24 {
        return fail("not a GSS-API authenticator checksum");
    }
    let flags = u32::from_le_bytes([value[20], value[21], value[22], value[23]]);
    Ok(flags & GSS_MUTUAL != 0)
}

/// The GSS-API token with the `AP-REP` that answers `auth`, sealed with the ticket's session key.
fn ap_rep(
    ticket: &TicketPart,
    auth: &AuthenticatorPart,
    confounder: &[u8; 16],
) -> Result<Vec<u8>, KerberosError> {
    let mut enc = explicit_out(0, &encode_time(auth.ctime));
    enc.extend_from_slice(&explicit_out(1, &small_int(auth.cusec)));
    if let Some(s) = auth.seq {
        enc.extend_from_slice(&explicit_out(3, &small_int(s)));
    }
    let part = der::tlv(app(27), &der::sequence(&[&enc]));
    let cipher = ticket
        .session_type
        .encrypt(&ticket.session, USAGE_AP_REP, confounder, &part)
        .map_err(|_| KerberosError("the AP-REP does not encrypt".into()))?;
    let data = der::sequence(&[
        &explicit_out(0, &small_int(i64::from(ticket.session_type.number()))),
        &explicit_out(2, &der::tlv(OCTET_STRING, &cipher)),
    ]);
    let rep = der::sequence(&[
        &explicit_out(0, &small_int(5)),
        &explicit_out(1, &small_int(15)),
        &explicit_out(2, &data),
    ]);
    Ok(frame(TOK_AP_REP, &der::tlv(app(15), &rep)))
}

/// Accepts the GSS-API initial token `token` with the keys `keys` at `now` (seconds): the user,
/// and the `AP-REP` to send, made with `confounder`.
pub fn accept(
    token: &[u8],
    keys: &[ServiceKey],
    now: i64,
    confounder: [u8; 16],
) -> Result<Accepted, KerberosError> {
    let req = read_ap_req(token)?;
    let ticket = open_ticket(req.ticket, keys, now)?;
    let auth = open_authenticator(req.authenticator, &ticket, now)?;
    let (etype, key) = auth.subkey.clone().unwrap_or((ticket.session_type, ticket.session.clone()));
    // The acceptor's sequence numbers start at the one its AP-REP names: the initiator's,
    // echoed (MIT and RFC 4121 count from it without an AP-REP too).
    let send_seq = auth.seq.and_then(|s| u64::try_from(s & 0xffff_ffff).ok()).unwrap_or(0);
    let context = Context { etype, key, send_seq };
    let reply = if req.mutual || auth.gss_mutual {
        Some(ap_rep(&ticket, &auth, &confounder)?)
    } else {
        None
    };
    let user = UserName::new(format!("{}@{}", ticket.cname, ticket.crealm), EXTERNAL_DB);
    Ok(Accepted { user, reply, context })
}

impl Context {
    fn checksum(&self, usage: u32, data: &[u8]) -> Result<Vec<u8>, KerberosError> {
        let kc = self
            .etype
            .checksum_key(&self.key, usage)
            .map_err(|_| KerberosError("bad context key".into()))?;
        let full = checksum_hash(self.etype).hmac(&kc, data);
        Ok(full.as_ref()[..self.etype.mac_len()].to_vec())
    }

    /// A wrap token of the acceptor with `data`, signed but not sealed, as Cyrus SASL sends the
    /// security layer offer.
    pub fn wrap(&mut self, data: &[u8]) -> Result<Vec<u8>, KerberosError> {
        let ec = u16::try_from(self.etype.mac_len()).unwrap_or(u16::MAX);
        let mut header = TOK_WRAP.to_vec();
        header.extend_from_slice(&[FLAG_SENT_BY_ACCEPTOR, 0xff, 0, 0, 0, 0]);
        header.extend_from_slice(&self.send_seq.to_be_bytes());
        self.send_seq += 1;
        let tag = self.checksum(USAGE_ACCEPTOR_SEAL, &[data, &header].concat())?;
        header[4..6].copy_from_slice(&ec.to_be_bytes());
        Ok([&header[..], data, &tag].concat())
    }

    /// Opens a wrap token of the initiator: its data, sealed or only signed.
    pub fn unwrap(&self, token: &[u8]) -> Result<Vec<u8>, KerberosError> {
        if token.len() < 16 || token[..2] != TOK_WRAP || token[3] != 0xff {
            return fail("not a wrap token");
        }
        let flags = token[2];
        if flags & FLAG_SENT_BY_ACCEPTOR != 0 || flags & FLAG_ACCEPTOR_SUBKEY != 0 {
            return fail("a wrap token of the wrong direction or key");
        }
        let ec = usize::from(u16::from_be_bytes([token[4], token[5]]));
        let rrc = usize::from(u16::from_be_bytes([token[6], token[7]]));
        let mut body = token[16..].to_vec();
        if !body.is_empty() {
            let n = rrc % body.len();
            body.rotate_left(n);
        }
        let mut header = token[..16].to_vec();
        header[6..8].copy_from_slice(&[0, 0]);
        if flags & FLAG_SEALED != 0 {
            let plain = self
                .etype
                .decrypt(&self.key, USAGE_INITIATOR_SEAL, &body)
                .map_err(|_| KerberosError("the wrap token does not decrypt".into()))?;
            let cut = plain
                .len()
                .checked_sub(ec + 16)
                .ok_or_else(|| KerberosError("short wrap token".into()))?;
            if plain[cut + ec..] != header[..] {
                return fail("the wrap token's header does not match");
            }
            return Ok(plain[..cut].to_vec());
        }
        let cut =
            body.len().checked_sub(ec).ok_or_else(|| KerberosError("short wrap token".into()))?;
        let (data, tag) = body.split_at(cut);
        header[4..6].copy_from_slice(&[0, 0]);
        let want = self.checksum(USAGE_INITIATOR_SEAL, &[data, &header].concat())?;
        if !teseladb_crypto::ct::ct_eq(&want, tag) {
            return fail("the wrap token's checksum does not match");
        }
        Ok(data.to_vec())
    }
}

/// The acceptor's security layer offer of SASL GSSAPI: no layer, no message size.
pub fn layer_offer() -> Vec<u8> {
    [&[LAYER_NONE][..], &MAX_MESSAGE].concat()
}

/// Reads the client's answer to the offer: it must choose no layer; an authorization identity,
/// if given, must be the user itself.
pub fn layer_answer(data: &[u8], user: &UserName) -> Result<(), KerberosError> {
    let (Some(&layer), Some(authzid)) = (data.first(), data.get(4..)) else {
        return fail("short security layer answer");
    };
    if layer != LAYER_NONE {
        return fail("a security layer the node does not offer");
    }
    if !authzid.is_empty() && authzid != user.user.as_bytes() {
        return fail("SASL authorization identity must match authentication identity");
    }
    Ok(())
}
