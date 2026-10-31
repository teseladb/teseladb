//! A Kerberos 5 initiator for the GSSAPI tests (013 T036): a keytab in MIT's format and an
//! `AP-REQ` made as MIT's initiator makes it (ticket, authenticator with the GSS-API checksum,
//! subkey and sequence number), and the initiator's wrap tokens of RFC 4121. Shared by the
//! acceptor's tests here and the `saslStart` tests of `teseladb-wire`.

use teseladb_auth::kerberos::{self, ServiceKey};
pub use teseladb_crypto::kerberos::EncType;
use teseladb_x509::der;

pub const NOW: i64 = 1_800_000_000;
pub const SERVICE_KEY: [u8; 32] = [7; 32];
pub const SESSION_KEY: [u8; 32] = [9; 32];
pub const SUBKEY: [u8; 16] = [5; 16];
pub const SEQ: i64 = 0x1234_5678;

pub fn keytab_entry(
    components: &[&str],
    realm: &str,
    etype: u16,
    key: &[u8],
    kvno: u32,
) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&u16::try_from(components.len()).unwrap().to_be_bytes());
    let mut s = |t: &str| {
        e.extend_from_slice(&u16::try_from(t.len()).unwrap().to_be_bytes());
        e.extend_from_slice(t.as_bytes());
    };
    s(realm);
    for c in components {
        s(c);
    }
    e.extend_from_slice(&1u32.to_be_bytes());
    e.extend_from_slice(&0u32.to_be_bytes());
    e.push(u8::try_from(kvno.min(255)).unwrap());
    e.extend_from_slice(&etype.to_be_bytes());
    e.extend_from_slice(&u16::try_from(key.len()).unwrap().to_be_bytes());
    e.extend_from_slice(key);
    e.extend_from_slice(&kvno.to_be_bytes());
    let mut out = i32::try_from(e.len()).unwrap().to_be_bytes().to_vec();
    out.extend_from_slice(&e);
    out
}

pub fn keytab() -> Vec<ServiceKey> {
    let mut data = vec![0x05, 0x02];
    data.extend_from_slice(&keytab_entry(
        &["refdb", "db.test"],
        "TESELA.TEST",
        18,
        &SERVICE_KEY,
        3,
    ));
    // An entry of another type, and a deleted one: left out.
    data.extend_from_slice(&keytab_entry(&["refdb", "db.test"], "TESELA.TEST", 23, &[1; 16], 3));
    data.extend_from_slice(&(-4i32).to_be_bytes());
    data.extend_from_slice(&[0; 4]);
    kerberos::parse_keytab(&data).unwrap()
}

pub fn ctx(n: u8, inner: &[u8]) -> Vec<u8> {
    der::tlv(0xa0 | n, inner)
}

pub fn int(n: i64) -> Vec<u8> {
    let b = n.to_be_bytes();
    let mut i = 0;
    while i < 7 && ((b[i] == 0 && b[i + 1] & 0x80 == 0) || (b[i] == 0xff && b[i + 1] & 0x80 != 0)) {
        i += 1;
    }
    der::tlv(0x02, &b[i..])
}

pub fn gstr(s: &str) -> Vec<u8> {
    der::tlv(0x1b, s.as_bytes())
}

pub fn time(s: &str) -> Vec<u8> {
    der::tlv(0x18, s.as_bytes())
}

pub fn name(parts: &[&str]) -> Vec<u8> {
    let names: Vec<Vec<u8>> = parts.iter().map(|p| gstr(p)).collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    der::sequence(&[&ctx(0, &int(1)), &ctx(1, &der::sequence(&refs))])
}

pub fn enc_data(
    etype: EncType,
    kvno: Option<i64>,
    key: &[u8],
    usage: u32,
    plain: &[u8],
) -> Vec<u8> {
    let cipher = etype.encrypt(key, usage, &[3; 16], plain).unwrap();
    let mut parts = vec![ctx(0, &int(i64::from(etype.number())))];
    if let Some(k) = kvno {
        parts.push(ctx(1, &int(k)));
    }
    parts.push(ctx(2, &der::tlv(0x04, &cipher)));
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    der::sequence(&refs)
}

/// `2027-01-15 08:00:00 UTC` and the other times the tokens name.
pub const AUTH_TIME: &str = "20270115080000Z";
pub const END_TIME: &str = "20270116080000Z";
/// NOW is 2027-01-15 08:00:00 UTC.
pub const CTIME: &str = "20270115080010Z";

pub struct Options {
    pub mutual: bool,
    pub subkey: bool,
    pub client: &'static str,
    pub ctime: &'static str,
    pub end: &'static str,
    pub checksum_type: i64,
    /// The length of the checksum's value (24 is the shortest RFC 4121 allows).
    pub checksum_len: usize,
    /// Whether the authenticator carries the GSS-API checksum at all.
    pub checksum: bool,
    /// The key version the ticket names, if any.
    pub kvno: Option<i64>,
    /// The encryption type number the ticket's encrypted part claims.
    pub ticket_etype: u8,
    /// The session key's type and length.
    pub key_type: i64,
    pub key_len: usize,
    /// The ticket's start time.
    pub start: &'static str,
    /// The authenticator's encryption type and realm.
    pub auth_etype: EncType,
    pub auth_realm: &'static str,
    /// The initiator's sequence number, if it sends one.
    pub seq: Option<i64>,
    /// The `AP-REQ`'s protocol version and message type.
    pub pvno: i64,
    pub msg_type: i64,
}

pub fn options() -> Options {
    Options {
        mutual: true,
        subkey: true,
        client: "alice",
        ctime: CTIME,
        end: END_TIME,
        checksum_type: 0x8003,
        checksum_len: 24,
        checksum: true,
        kvno: Some(3),
        ticket_etype: 18,
        key_type: 18,
        key_len: 32,
        start: AUTH_TIME,
        auth_etype: EncType::Aes256CtsHmacSha1,
        auth_realm: "TESELA.TEST",
        seq: Some(SEQ),
        pvno: 5,
        msg_type: 14,
    }
}

pub fn ap_req(o: &Options) -> Vec<u8> {
    let session = der::sequence(&[
        &ctx(0, &int(o.key_type)),
        &ctx(1, &der::tlv(0x04, &[9; 64][..o.key_len])),
    ]);
    let part = der::sequence(&[
        &ctx(0, &der::tlv(0x03, &[0, 0, 0, 0, 0])),
        &ctx(1, &session),
        &ctx(2, &gstr("TESELA.TEST")),
        &ctx(3, &name(&["alice"])),
        &ctx(4, &der::sequence(&[&ctx(0, &int(1)), &ctx(1, &der::tlv(0x04, b""))])),
        &ctx(5, &time(AUTH_TIME)),
        &ctx(6, &time(o.start)),
        &ctx(7, &time(o.end)),
    ]);
    let mut enc_part =
        enc_data(EncType::Aes256CtsHmacSha1, o.kvno, &SERVICE_KEY, 2, &der::tlv(0x63, &part));
    // The etype number is the first INTEGER: a0 03 02 01 <n>.
    let at = enc_part.windows(4).position(|w| w == [0xa0, 0x03, 0x02, 0x01]).unwrap() + 4;
    enc_part[at] = o.ticket_etype;
    let ticket = der::tlv(
        0x61,
        &der::sequence(&[
            &ctx(0, &int(5)),
            &ctx(1, &gstr("TESELA.TEST")),
            &ctx(2, &name(&["refdb", "db.test"])),
            &ctx(3, &enc_part),
        ]),
    );
    let mut flags = vec![0u8; 24];
    flags[16..20].copy_from_slice(&16u32.to_le_bytes());
    flags[20..24].copy_from_slice(&(if o.mutual { 0x3eu32 } else { 0x3c }).to_le_bytes());
    flags.truncate(o.checksum_len);
    let cksum = der::sequence(&[&ctx(0, &int(o.checksum_type)), &ctx(1, &der::tlv(0x04, &flags))]);
    let mut auth = vec![ctx(0, &int(5)), ctx(1, &gstr(o.auth_realm)), ctx(2, &name(&[o.client]))];
    if o.checksum {
        auth.push(ctx(3, &cksum));
    }
    auth.push(ctx(4, &int(123)));
    auth.push(ctx(5, &time(o.ctime)));
    if o.subkey {
        auth.push(ctx(6, &der::sequence(&[&ctx(0, &int(17)), &ctx(1, &der::tlv(0x04, &SUBKEY))])));
    }
    if let Some(seq) = o.seq {
        auth.push(ctx(7, &int(seq)));
    }
    let refs: Vec<&[u8]> = auth.iter().map(Vec::as_slice).collect();
    let authenticator = der::tlv(0x62, &der::sequence(&refs));
    let key = &SESSION_KEY[..o.auth_etype.key_len()];
    let enc_auth = enc_data(o.auth_etype, None, key, 11, &authenticator);
    let options = if o.mutual { [0, 0x20, 0, 0, 0] } else { [0, 0, 0, 0, 0] };
    let req = der::tlv(
        0x6e,
        &der::sequence(&[
            &ctx(0, &int(o.pvno)),
            &ctx(1, &int(o.msg_type)),
            &ctx(2, &der::tlv(0x03, &options)),
            &ctx(3, &ticket),
            &ctx(4, &enc_auth),
        ]),
    );
    let body = [&der::tlv(0x06, kerberos::KRB5_OID)[..], &[1, 0], &req].concat();
    der::tlv(0x60, &body)
}

/// A wrap token of the initiator with `data`, sealed or only signed, with the context key.
pub fn initiator_wrap(data: &[u8], sealed: bool) -> Vec<u8> {
    let e = EncType::Aes128CtsHmacSha1;
    let mut header = vec![0x05, 0x04, if sealed { 0x02 } else { 0 }, 0xff, 0, 0, 0, 0];
    header.extend_from_slice(&7u64.to_be_bytes());
    if sealed {
        let plain = [data, &header].concat();
        let mut cipher = e.encrypt(&SUBKEY, 24, &[4; 16], &plain).unwrap();
        // Rotated right by RRC = 28 bytes, as MIT does.
        cipher.rotate_right(28);
        header[6..8].copy_from_slice(&28u16.to_be_bytes());
        return [header, cipher].concat();
    }
    let kc = e.checksum_key(&SUBKEY, 24).unwrap();
    let tag = teseladb_crypto::HashAlgorithm::Sha1.hmac(&kc, &[data, &header].concat());
    header[4..6].copy_from_slice(&12u16.to_be_bytes());
    [&header[..], data, &tag.as_ref()[..12]].concat()
}
