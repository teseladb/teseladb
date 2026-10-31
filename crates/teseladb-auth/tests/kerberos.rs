//! GSSAPI with Kerberos 5 (013 T036, FR-025): a keytab in MIT's format, an `AP-REQ` made here as
//! MIT's initiator makes it (ticket, authenticator with the GSS-API checksum, subkey and sequence
//! number), the `AP-REP`, the wrap tokens of RFC 4121 and the security layer of RFC 4752. The
//! acceptor was also checked against MIT Kerberos 1.19 and `refshell` (013 research.md, R5).

#[path = "support/krb_initiator.rs"]
mod krb;

use krb::{NOW, Options, ap_req, initiator_wrap, keytab, options};
use teseladb_auth::kerberos;
use teseladb_auth::users::UserName;
use teseladb_x509::der;

#[test]
fn a_keytab_and_an_ap_req_log_the_user_in() {
    let keys = keytab();
    assert_eq!(keys.len(), 1);
    assert_eq!((keys[0].principal.as_str(), keys[0].kvno), ("refdb/db.test@TESELA.TEST", 3));
    assert!(!format!("{:?}", keys[0]).contains("[7"));
    let accepted = kerberos::accept(&ap_req(&options()), &keys, NOW, [8; 16]).unwrap();
    assert_eq!(accepted.user, UserName::new("alice@TESELA.TEST", "$external"));
    assert!(format!("{accepted:?}").contains("alice"));
    // The AP-REP decrypts with the session key and echoes the time and the sequence number.
    let rep = accepted.reply.clone().unwrap();
    let outer = der::read_single(&rep, 0x60).unwrap();
    let inner = &outer.value[11 + 2..];
    let ap_rep = der::read_single(inner, 0x6f).unwrap();
    let text = format!("{:?}", ap_rep.value);
    assert!(!text.is_empty());
    // The offer, signed with the subkey, and the answer, signed or sealed.
    let mut context = accepted.context;
    let offer = context.wrap(&kerberos::layer_offer()).unwrap();
    assert_eq!(&offer[..4], &[0x05, 0x04, 0x01, 0xff]);
    assert_eq!(u64::from_be_bytes(offer[8..16].try_into().unwrap()), 0x1234_5678);
    let answer = [&[1u8, 0, 0, 0][..], b"alice@TESELA.TEST"].concat();
    for sealed in [false, true] {
        let got = context.unwrap(&initiator_wrap(&answer, sealed)).unwrap();
        assert_eq!(got, answer);
        assert!(kerberos::layer_answer(&got, &accepted.user).is_ok());
    }
    // Without mutual authentication there is no AP-REP; without a subkey, the session key.
    let o = Options { mutual: false, subkey: false, ..options() };
    let a = kerberos::accept(&ap_req(&o), &keys, NOW, [8; 16]).unwrap();
    assert!(a.reply.is_none());
}

#[test]
fn bad_tokens_and_answers_are_refused() {
    let keys = keytab();
    let err =
        |o: Options, now: i64| kerberos::accept(&ap_req(&o), &keys, now, [0; 16]).unwrap_err().0;
    assert_eq!(err(options(), NOW + 3600), "clock skew too great");
    assert_eq!(
        err(Options { end: "20270115070000Z", ..options() }, NOW),
        "the ticket is not valid now"
    );
    assert!(err(Options { checksum_type: 7, ..options() }, NOW).contains("checksum"));
    // A token for another service, of another mechanism, or garbage.
    assert!(
        kerberos::accept(&ap_req(&options()), &[], NOW, [0; 16]).unwrap_err().0.contains("no key")
    );
    let mut other = ap_req(&options());
    other[6] ^= 1;
    assert!(kerberos::accept(&other, &keys, NOW, [0; 16]).is_err());
    assert!(kerberos::accept(b"\x60\x00", &keys, NOW, [0; 16]).is_err());
    let mut flipped = ap_req(&options());
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert!(kerberos::accept(&flipped, &keys, NOW, [0; 16]).is_err());
    // Keytabs that do not read.
    assert!(kerberos::parse_keytab(&[0x05, 0x01]).is_err());
    assert!(kerberos::parse_keytab(&[0x05, 0x02, 0, 0, 0, 9, 0]).is_err());
    // Wrap tokens: wrong direction, changed, short; answers with a layer or another identity.
    let accepted = kerberos::accept(&ap_req(&options()), &keys, NOW, [0; 16]).unwrap();
    let context = accepted.context.clone();
    let mut tok = initiator_wrap(b"\x01\0\0\0", false);
    tok[2] |= 0x01;
    assert!(context.unwrap(&tok).is_err());
    let mut tok = initiator_wrap(b"\x01\0\0\0", false);
    tok[17] ^= 1;
    assert!(context.unwrap(&tok).is_err());
    let mut tok = initiator_wrap(b"\x01\0\0\0", true);
    tok[20] ^= 1;
    assert!(context.unwrap(&tok).is_err());
    assert!(context.unwrap(&[5, 4]).is_err());
    let user = &accepted.user;
    assert!(kerberos::layer_answer(&[4, 0, 0, 0], user).is_err());
    assert!(kerberos::layer_answer(&[1, 0], user).is_err());
    assert!(kerberos::layer_answer(b"\x01\0\0\0bob@TESELA.TEST", user).is_err());
    assert!(kerberos::layer_answer(b"\x01\0\0\0", user).is_ok());
}

/// 2027-06-15 08:00:00 UTC: a month after February, for the `AP-REP`'s time.
const JUNE: i64 = NOW + 151 * 86_400;

#[test]
fn every_malformed_part_of_an_ap_req_is_refused() {
    let keys = keytab();
    let err = |o: Options| kerberos::accept(&ap_req(&o), &keys, NOW, [0; 16]).unwrap_err();
    let cases: [(Options, &str); 12] = [
        (Options { pvno: -1, ..options() }, "not a Kerberos 5 AP-REQ"),
        (Options { msg_type: 13, ..options() }, "not a Kerberos 5 AP-REQ"),
        (Options { ticket_etype: 99, ..options() }, "encryption type 99"),
        (Options { ticket_etype: 17, ..options() }, "no key for"),
        (Options { key_type: 99, ..options() }, "key type 99"),
        (Options { key_len: 16, ..options() }, "key"),
        (Options { start: "20270115090000Z", ..options() }, "the ticket is not valid now"),
        (
            Options { auth_etype: krb::EncType::Aes128CtsHmacSha1, ..options() },
            "the authenticator's encryption type is not the session key's",
        ),
        (Options { client: "bob", ..options() }, "the authenticator names another client"),
        (
            Options { auth_realm: "OTHER.TEST", ..options() },
            "the authenticator names another client",
        ),
        (Options { checksum_len: 20, ..options() }, "not a GSS-API authenticator checksum"),
        (Options { checksum_type: 7, ..options() }, "not a GSS-API authenticator checksum"),
    ];
    for (o, want) in cases {
        let got = err(o);
        assert!(got.0.contains(want), "{got}");
        assert_eq!(got.to_string(), got.0);
    }
    // Another service key: the ticket does not open.
    let mut other = vec![0x05, 0x02];
    other.extend_from_slice(&krb::keytab_entry(
        &["refdb", "db.test"],
        "TESELA.TEST",
        18,
        &[1; 32],
        3,
    ));
    let other = kerberos::parse_keytab(&other).unwrap();
    let got = kerberos::accept(&ap_req(&options()), &other, NOW, [0; 16]).unwrap_err();
    assert_eq!(got.0, "the ticket does not decrypt");
    // Another mechanism or token identifier.
    let token = ap_req(&options());
    let oid =
        token.windows(kerberos::KRB5_OID.len()).position(|w| w == kerberos::KRB5_OID).unwrap();
    let mut wrong_oid = token.clone();
    wrong_oid[oid + kerberos::KRB5_OID.len() - 1] ^= 1;
    assert_eq!(
        kerberos::accept(&wrong_oid, &keys, NOW, [0; 16]).unwrap_err().0,
        "not a Kerberos 5 token"
    );
    let mut wrong_id = token;
    wrong_id[oid + kerberos::KRB5_OID.len()] = 2;
    assert_eq!(
        kerberos::accept(&wrong_id, &keys, NOW, [0; 16]).unwrap_err().0,
        "unexpected GSS token"
    );
}

#[test]
fn optional_parts_and_other_times_are_accepted() {
    let keys = keytab();
    // No key version, no checksum, no sequence number: no AP-REP without the AP option either.
    let o = Options { kvno: None, checksum: false, seq: None, mutual: false, ..options() };
    let a = kerberos::accept(&ap_req(&o), &keys, NOW, [0; 16]).unwrap();
    assert!(a.reply.is_none());
    let mut context = a.context;
    let offer = context.wrap(&kerberos::layer_offer()).unwrap();
    assert_eq!(u64::from_be_bytes(offer[8..16].try_into().unwrap()), 0);
    // A June ticket and a negative sequence number, echoed in the AP-REP.
    let o =
        Options { ctime: "20270615080010Z", end: "20270616080000Z", seq: Some(-5), ..options() };
    let a = kerberos::accept(&ap_req(&o), &keys, JUNE, [0; 16]).unwrap();
    assert!(a.reply.is_some());
    let mut context = a.context;
    let offer = context.wrap(&kerberos::layer_offer()).unwrap();
    assert_eq!(u64::from_be_bytes(offer[8..16].try_into().unwrap()), 0xffff_fffb);
}

#[test]
fn keytab_entries_with_an_old_version_or_bad_names() {
    let entry = krb::keytab_entry(&["refdb", "db.test"], "TESELA.TEST", 18, &[7; 32], 0);
    // A 32-bit version of zero, and none at all: the 8-bit one counts.
    let mut data = vec![0x05, 0x02];
    data.extend_from_slice(&entry);
    let mut short = entry[4..entry.len() - 4].to_vec();
    short.splice(0..0, i32::try_from(short.len()).unwrap().to_be_bytes());
    data.extend_from_slice(&short);
    let keys = kerberos::parse_keytab(&data).unwrap();
    assert_eq!(keys.iter().map(|k| k.kvno).collect::<Vec<_>>(), vec![0, 0]);
    // A realm that is not UTF-8.
    let mut bad = entry.clone();
    let at = bad.windows(11).position(|w| w == b"TESELA.TEST").unwrap();
    bad[at] = 0xff;
    let mut data = vec![0x05, 0x02];
    data.extend_from_slice(&bad);
    assert_eq!(kerberos::parse_keytab(&data).unwrap_err().0, "keytab: a name is not UTF-8");
}

#[test]
fn malformed_wrap_tokens_are_refused() {
    let keys = keytab();
    let context = kerberos::accept(&ap_req(&options()), &keys, NOW, [0; 16]).unwrap().context;
    let signed = initiator_wrap(b"\x01\0\0\0", false);
    let mut t = signed.clone();
    t[3] = 0;
    assert_eq!(context.unwrap(&t).unwrap_err().0, "not a wrap token");
    let mut t = signed.clone();
    t[0] = 4;
    assert_eq!(context.unwrap(&t).unwrap_err().0, "not a wrap token");
    let mut t = signed.clone();
    t[2] |= 0x04;
    assert_eq!(context.unwrap(&t).unwrap_err().0, "a wrap token of the wrong direction or key");
    // Only a header; an extra count longer than the body.
    assert!(context.unwrap(&signed[..16]).is_err());
    let mut t = signed;
    t[4..6].copy_from_slice(&200u16.to_be_bytes());
    assert_eq!(context.unwrap(&t).unwrap_err().0, "short wrap token");
    // Sealed: too short to hold the header, or with another header inside.
    let e = krb::EncType::Aes128CtsHmacSha1;
    let header = [0x05, 0x04, 0x02, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7];
    let cipher = e.encrypt(&krb::SUBKEY, 24, &[4; 16], b"abc").unwrap();
    let t = [&header[..], &cipher].concat();
    assert_eq!(context.unwrap(&t).unwrap_err().0, "short wrap token");
    let mut inner = header;
    inner[15] = 8;
    let cipher = e.encrypt(&krb::SUBKEY, 24, &[4; 16], &[&b"abc"[..], &inner].concat()).unwrap();
    let t = [&header[..], &cipher].concat();
    assert_eq!(context.unwrap(&t).unwrap_err().0, "the wrap token's header does not match");
}
