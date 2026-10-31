//! KMIP messages (013 FR-028) against the bytes RefDb Enterprise 9.0.4 and a KMIP server
//! exchanged on 31-10-2026 (`tests/fixtures/kmip/`): the requests come out byte for byte as
//! `refnode` sends them, the answers read as the server gave them, and damaged messages are
//! refused.

use teseladb_auth::kmip::{
    ACTIVE, Answer, HEADER_LEN, Item, KmipError, Request, V1_0, V1_2, Value, message_len, tag,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/kmip/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    let text = text.trim();
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

/// The identifier the server gave the key.
const KEY_ID: &str = "1";

#[test]
fn requests_are_the_bytes_refnode_sends() {
    let id = KEY_ID.to_owned();
    assert_eq!(Request::CreateKey.encode(V1_2), fixture("create-request"));
    assert_eq!(Request::Get(id.clone()).encode(V1_2), fixture("get-request"));
    assert_eq!(Request::Activate(id.clone()).encode(V1_2), fixture("activate-request"));
    assert_eq!(Request::State(id).encode(V1_2), fixture("state-request"));
    // The legacy protocol changes only the version.
    let legacy = Request::CreateKey.encode(V1_0);
    let (msg, _) = Item::decode(&legacy).unwrap();
    let version = msg.child(tag::REQUEST_HEADER).unwrap().child(tag::PROTOCOL_VERSION).unwrap();
    assert_eq!(
        version.child(tag::PROTOCOL_VERSION_MINOR).unwrap().value,
        Value::Integer(0),
        "{version:?}"
    );
}

#[test]
fn answers_read_as_the_server_gave_them() {
    let id = KEY_ID.to_owned();
    let created = Request::CreateKey.answer(&fixture("create-response")).unwrap();
    assert_eq!(created, Answer::Created(id.clone()));
    let key = Request::Get(id.clone()).answer(&fixture("get-response")).unwrap();
    assert!(matches!(&key, Answer::Key(k) if k.len() == 32));
    assert_eq!(format!("{key:?}"), "Key");
    let activated = Request::Activate(id.clone()).answer(&fixture("activate-response")).unwrap();
    assert_eq!(format!("{activated:?}"), format!("Activated({id:?})"));
    let state = Request::State(id.clone()).answer(&fixture("state-response")).unwrap();
    assert_eq!(state, Answer::State(ACTIVE));
    assert_eq!(format!("{state:?} {created:?}"), format!("State(2) Created({id:?})"));
    // The whole length shows in the first eight bytes.
    let get = fixture("get-response");
    assert_eq!(message_len(&get[..HEADER_LEN]), Some(get.len()));
    assert_eq!(message_len(&get[..HEADER_LEN - 1]), None);
    // An answer to another operation, or with bytes after it, is not this one's.
    let other = Request::Activate(id.clone()).answer(&fixture("create-response"));
    assert_eq!(other, Err(KmipError::Malformed("the answer of another operation")));
    let mut longer = fixture("create-response");
    longer.extend_from_slice(&[0; 8]);
    assert!(Request::CreateKey.answer(&longer).is_err());
    assert!(Request::CreateKey.answer(&fixture("create-request")).is_err());
}

fn header() -> Item {
    Item::new(tag::RESPONSE_HEADER, Value::Structure(Vec::new()))
}

fn response(items: Vec<Item>) -> Vec<u8> {
    let item = Item::new(tag::BATCH_ITEM, Value::Structure(items));
    Item::new(tag::RESPONSE_MESSAGE, Value::Structure(vec![header(), item])).encode()
}

fn op(n: u32) -> Item {
    Item::new(tag::OPERATION, Value::Enumeration(n))
}

fn ok() -> Item {
    Item::new(tag::RESULT_STATUS, Value::Enumeration(0))
}

fn text(t: u32, s: &str) -> Item {
    Item::new(t, Value::TextString(s.to_owned()))
}

fn structure(t: u32, items: Vec<Item>) -> Item {
    Item::new(t, Value::Structure(items))
}

#[test]
fn a_refusal_carries_the_servers_reason_and_message() {
    let refused = response(vec![
        op(10),
        Item::new(tag::RESULT_STATUS, Value::Enumeration(1)),
        Item::new(tag::RESULT_REASON, Value::Enumeration(1)),
        text(tag::RESULT_MESSAGE, "Could not locate object: 99"),
    ]);
    let e = Request::Get("99".into()).answer(&refused).unwrap_err();
    assert_eq!(e.to_string(), "code: 1 error: Could not locate object: 99");
    // Without a reason or a message.
    let bare = response(vec![op(10), Item::new(tag::RESULT_STATUS, Value::Enumeration(1))]);
    let e = Request::Get("99".into()).answer(&bare).unwrap_err();
    assert_eq!(e, KmipError::Failed { reason: 0, message: String::new() });
    assert_eq!(KmipError::Malformed("x").to_string(), "malformed KMIP response: x");
}

#[test]
fn key_material_in_transparent_form_and_bad_keys() {
    let key = |format: u32, material: Item| {
        let block = structure(
            tag::KEY_BLOCK,
            vec![
                Item::new(tag::KEY_FORMAT_TYPE, Value::Enumeration(format)),
                structure(tag::KEY_VALUE, vec![material]),
            ],
        );
        response(vec![
            op(10),
            ok(),
            structure(
                tag::RESPONSE_PAYLOAD,
                vec![text(tag::UNIQUE_IDENTIFIER, "1"), structure(tag::SYMMETRIC_KEY, vec![block])],
            ),
        ])
    };
    let get = Request::Get("1".into());
    let transparent =
        structure(tag::KEY_MATERIAL, vec![Item::new(tag::KEY, Value::ByteString(vec![5; 32]))]);
    assert_eq!(get.answer(&key(7, transparent)).unwrap(), Answer::Key(vec![5; 32]));
    let not_bytes = structure(tag::KEY_MATERIAL, vec![Item::new(tag::KEY, Value::Integer(1))]);
    assert!(get.answer(&key(7, not_bytes)).is_err());
    let short = Item::new(tag::KEY_MATERIAL, Value::ByteString(vec![5; 16]));
    assert_eq!(get.answer(&key(1, short)), Err(KmipError::Malformed("a key that is not 256 bits")));
    let wrapped = Item::new(tag::KEY_MATERIAL, Value::ByteString(vec![5; 32]));
    assert!(get.answer(&key(2, wrapped)).is_err());
    // A state answer without the state, and an identifier that is not text.
    let state = Request::State("1".into());
    let no_state = response(vec![
        op(11),
        ok(),
        structure(tag::RESPONSE_PAYLOAD, vec![text(tag::UNIQUE_IDENTIFIER, "1")]),
    ]);
    assert!(state.answer(&no_state).is_err());
    let other_attr = response(vec![
        op(11),
        ok(),
        structure(
            tag::RESPONSE_PAYLOAD,
            vec![
                text(tag::UNIQUE_IDENTIFIER, "1"),
                structure(
                    tag::ATTRIBUTE,
                    vec![
                        text(tag::ATTRIBUTE_NAME, "Name"),
                        Item::new(tag::ATTRIBUTE_VALUE, Value::Enumeration(2)),
                    ],
                ),
            ],
        ),
    ]);
    assert!(state.answer(&other_attr).is_err());
    let number_id = response(vec![
        op(1),
        ok(),
        structure(
            tag::RESPONSE_PAYLOAD,
            vec![Item::new(tag::UNIQUE_IDENTIFIER, Value::Integer(1))],
        ),
    ]);
    assert!(Request::CreateKey.answer(&number_id).is_err());
    // Missing pieces.
    assert!(Request::CreateKey.answer(&response(vec![op(1), ok()])).is_err());
    assert!(Request::CreateKey.answer(&response(vec![op(1)])).is_err());
    let no_item = Item::new(tag::RESPONSE_MESSAGE, Value::Structure(vec![header()])).encode();
    assert!(Request::CreateKey.answer(&no_item).is_err());
}

#[test]
fn every_type_round_trips_and_damage_is_refused() {
    let all = Item::new(
        tag::REQUEST_PAYLOAD,
        Value::Structure(vec![
            Item::new(1, Value::Integer(-7)),
            Item::new(2, Value::LongInteger(-1 << 40)),
            Item::new(3, Value::BigInteger(vec![1; 16])),
            Item::new(4, Value::Enumeration(9)),
            Item::new(5, Value::Boolean(true)),
            Item::new(6, Value::Boolean(false)),
            Item::new(7, Value::TextString("ñ".into())),
            Item::new(8, Value::ByteString(vec![1, 2, 3])),
            Item::new(9, Value::DateTime(1_793_405_271)),
            Item::new(10, Value::Interval(60)),
        ]),
    );
    let bytes = all.encode();
    assert_eq!(bytes.len() % 8, 0);
    assert_eq!(Item::decode(&bytes), Ok((all.clone(), bytes.len())));
    assert_eq!(Item::new(1, Value::Integer(1)).items(), &[]);
    // Every cut is refused.
    for n in 0..bytes.len() {
        assert!(Item::decode(&bytes[..n]).is_err(), "cut at {n}");
    }
    // A wrong length for a fixed-size type, a bad boolean, bad UTF-8, an unknown type, a big
    // integer that is not a multiple of eight, padding that is not zero.
    let raw = |kind: u8, body: &[u8]| {
        let mut b = vec![0x42, 0, 1, kind];
        b.extend_from_slice(&(body.len() as u32).to_be_bytes());
        b.extend_from_slice(body);
        b.resize(b.len() + (8 - body.len() % 8) % 8, 0);
        b
    };
    for (kind, body) in [
        (2u8, &[0u8; 8][..]),
        (6, &[0, 0, 0, 0, 0, 0, 0, 2][..]),
        (7, &[0xff][..]),
        (11, &[0][..]),
        (4, &[1; 4][..]),
    ] {
        assert!(Item::decode(&raw(kind, body)).is_err(), "type {kind}");
    }
    let mut padded = raw(8, &[1]);
    padded[HEADER_LEN + 3] = 1;
    assert_eq!(Item::decode(&padded), Err(KmipError::Malformed("padding that is not zero")));
    // A structure whose item claims more than the structure holds.
    let mut inner = raw(8, &[1; 8]);
    inner[7] = 9;
    let mut outer = vec![0x42, 0, 2, 1];
    outer.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    outer.extend_from_slice(&inner);
    assert!(Item::decode(&outer).is_err());
    // Nesting deeper than any answer.
    let mut deep = Item::new(1, Value::Integer(1));
    for _ in 0..20 {
        deep = Item::new(2, Value::Structure(vec![deep]));
    }
    assert_eq!(Item::decode(&deep.encode()), Err(KmipError::Malformed("too deeply nested")));
}

/// The audit log's key wrapped by the KMIP server (013 T042): Encrypt with the identifier and the
/// data alone, as `refnode` 9.0.4 sent it (measured with a responder that logs the request).
#[test]
fn the_audit_key_goes_to_encrypt_as_refnode_sends_it() {
    let req = fixture("encrypt-request");
    let (msg, _) = Item::decode(&req).unwrap();
    let payload = msg.child(tag::BATCH_ITEM).unwrap().child(tag::REQUEST_PAYLOAD).unwrap();
    let Value::ByteString(data) = &payload.child(tag::DATA).unwrap().value else { panic!() };
    let encrypt = Request::Encrypt(KEY_ID.to_owned(), data.clone());
    assert_eq!(encrypt.encode(V1_2), req);
    // An answer with the data and an IV, and one with the data alone.
    let answer = |items: Vec<Item>| {
        let ok = vec![
            Item::new(tag::OPERATION, Value::Enumeration(31)),
            Item::new(tag::RESULT_STATUS, Value::Enumeration(0)),
            Item::new(tag::RESPONSE_PAYLOAD, Value::Structure(items)),
        ];
        Item::new(
            tag::RESPONSE_MESSAGE,
            Value::Structure(vec![Item::new(tag::BATCH_ITEM, Value::Structure(ok))]),
        )
        .encode()
    };
    let id = Item::new(tag::UNIQUE_IDENTIFIER, Value::TextString(KEY_ID.to_owned()));
    let with_iv = answer(vec![
        id.clone(),
        Item::new(tag::DATA, Value::ByteString(vec![1; 32])),
        Item::new(tag::IV_COUNTER_NONCE, Value::ByteString(vec![2; 12])),
    ]);
    assert_eq!(
        encrypt.answer(&with_iv),
        Ok(Answer::Encrypted { data: vec![1; 32], iv: vec![2; 12] })
    );
    let alone = answer(vec![id.clone(), Item::new(tag::DATA, Value::ByteString(vec![1; 32]))]);
    assert_eq!(encrypt.answer(&alone), Ok(Answer::Encrypted { data: vec![1; 32], iv: Vec::new() }));
    let none = answer(vec![id]);
    assert!(matches!(encrypt.answer(&none), Err(KmipError::Malformed(_))));
    assert_eq!(format!("{:?}", encrypt.answer(&alone).unwrap()), "Encrypted");
}
