//! Messages of the wire protocol (006 T003, FR-001): header, `OP_MSG` with body and document
//! sequences, checksum, `moreToCome`, `exhaustAllowed`, `OP_QUERY`/`OP_REPLY` of the handshake
//! and `OP_COMPRESSED`. The expected answers are the ones a `refrouter` gives (measured against
//! RefDb 7.0, the newest that starts on the development machine; see the report of 006).

use teseladb_wire::compressed::{self, Compressor};
use teseladb_wire::connection::{Connection, Kind};
use teseladb_wire::error;
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg, flags, opcode};
use teseladb_wire::query::{AWAIT_CAPABLE, OpQuery, encode_reply};
use teseladb_wire::{MAX_MESSAGE_SIZE, WireError};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_hash::crc32c;

fn bson(d: &Document) -> Vec<u8> {
    d.to_bytes().unwrap()
}

fn ping() -> Document {
    Document::new().with("ping", 1).with("$db", "admin")
}

/// An `OP_MSG` with `flags` and `sections` already encoded.
fn op_msg(request_id: i32, flag_bits: u32, sections: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = HEADER_LEN + 4 + sections.len();
    Header { length: len as i32, request_id, response_to: 0, op_code: opcode::OP_MSG }
        .write(&mut out);
    out.extend_from_slice(&flag_bits.to_le_bytes());
    out.extend_from_slice(sections);
    out
}

fn body(d: &Document) -> Vec<u8> {
    let mut s = vec![0u8];
    s.extend_from_slice(&bson(d));
    s
}

fn sequence(id: &str, docs: &[Document]) -> Vec<u8> {
    let mut payload = id.as_bytes().to_vec();
    payload.push(0);
    for d in docs {
        payload.extend_from_slice(&bson(d));
    }
    let mut s = vec![1u8];
    s.extend_from_slice(&((payload.len() + 4) as i32).to_le_bytes());
    s.extend_from_slice(&payload);
    s
}

fn op_query(ns: &str, q: &Document) -> Vec<u8> {
    let mut b = 0i32.to_le_bytes().to_vec();
    b.extend_from_slice(ns.as_bytes());
    b.push(0);
    b.extend_from_slice(&0i32.to_le_bytes());
    b.extend_from_slice(&(-1i32).to_le_bytes());
    b.extend_from_slice(&bson(q));
    let mut out = Vec::new();
    let len = (HEADER_LEN + b.len()) as i32;
    Header { length: len, request_id: 3, response_to: 0, op_code: opcode::OP_QUERY }
        .write(&mut out);
    out.extend_from_slice(&b);
    out
}

fn first(
    conn: &mut Connection,
    bytes: &[u8],
) -> Result<teseladb_wire::connection::Request, WireError> {
    conn.receive(bytes);
    conn.next_request().map(|r| r.expect("a whole message"))
}

#[test]
fn header_round_trip() {
    let h = Header { length: 100, request_id: 7, response_to: -3, op_code: opcode::OP_MSG };
    let mut out = Vec::new();
    h.write(&mut out);
    assert_eq!(out.len(), HEADER_LEN);
    assert_eq!(Header::parse(&out), h);
}

#[test]
fn op_msg_body_and_sequences() {
    let insert = Document::new().with("insert", "items").with("$db", "test");
    let docs = [Document::new().with("a", 1), Document::new().with("b", 2)];
    let mut sections = body(&insert);
    sections.extend_from_slice(&sequence("documents", &docs));
    let m = OpMsg::parse(&op_msg(1, 0, &sections)).unwrap();
    assert_eq!(m.body.as_deref(), Some(&bson(&insert)[..]));
    assert_eq!(m.sequences.len(), 1);
    assert_eq!(m.sequences[0].id, "documents");
    assert_eq!(m.sequences[0].docs.len(), 2);
    assert_eq!(m.problem, None);

    // The connection merges the sequence into the body as an array field.
    let mut c = Connection::new(1);
    let req = first(&mut c, &op_msg(1, 0, &sections)).unwrap();
    assert_eq!(req.db, "test");
    assert_eq!(req.name(), "insert");
    let want: Vec<Value> = docs.iter().cloned().map(Value::Document).collect();
    assert_eq!(req.command.get("documents"), Some(&Value::Array(want)));
}

#[test]
fn op_msg_errors_answered_like_refrouter() {
    // A field both in the body and as a sequence, two bodies (also trailing bytes after the
    // body) and no body close the connection, as a `refrouter` 9.0 does (measured).
    let dup = Document::new()
        .with("insert", "items")
        .with("documents", Vec::<Value>::new())
        .with("$db", "t");
    let mut s = body(&dup);
    s.extend_from_slice(&sequence("documents", &[Document::new()]));
    let mut c = Connection::new(1);
    assert!(first(&mut c, &op_msg(1, 0, &s)).is_err());
    let mut s = body(&ping());
    s.extend_from_slice(&[0, 0]);
    assert!(OpMsg::parse(&op_msg(1, 0, &s)).is_err());
    assert!(OpMsg::parse(&op_msg(1, 0, &sequence("documents", &[]))).is_err());
    // Security token section: 13. Unknown section: 40432.
    let mut s = body(&ping());
    s.extend_from_slice(&[2, b'a', b'b', b'c']);
    let m = OpMsg::parse(&op_msg(1, 0, &s)).unwrap();
    assert_eq!(
        m.problem.map(|p| (p.code, p.errmsg)),
        Some((error::UNAUTHORIZED, "Unsupported Security Token provided".into()))
    );
    let mut s = body(&ping());
    s.push(9);
    let m = OpMsg::parse(&op_msg(1, 0, &s)).unwrap();
    assert_eq!(m.problem.map(|p| p.code), Some(error::UNKNOWN_SECTION));
    // Two sequences with the same name: 40431.
    let mut s = body(&ping());
    s.extend_from_slice(&sequence("d", &[]));
    s.extend_from_slice(&sequence("d", &[]));
    let m = OpMsg::parse(&op_msg(1, 0, &s)).unwrap();
    assert_eq!(m.problem.map(|p| p.code), Some(error::DUPLICATE_SEQUENCE));
}

#[test]
fn structural_faults_close_the_connection() {
    let s = body(&ping());
    // A required flag bit nobody knows (bit 5).
    assert_eq!(OpMsg::parse(&op_msg(1, 1 << 5, &s)), Err(WireError::IllegalFlags(1 << 5)));
    // Optional bits (16 to 31) are fine.
    assert!(OpMsg::parse(&op_msg(1, 1 << 20, &s)).is_ok());
    // Cut sections and invalid BSON.
    assert!(OpMsg::parse(&op_msg(1, 0, &s[..s.len() - 1])).is_err());
    assert!(OpMsg::parse(&op_msg(1, 0, &[1, 3, 0, 0, 0])).is_err());
    assert!(OpMsg::parse(&op_msg(1, 0, &[1, 0xff, 0xff, 0xff, 0xff])).is_err());
    assert!(OpMsg::parse(&op_msg(1, 0, &[1, 6, 0, 0, 0, b'x'])).is_err());
    let mut bad = s.clone();
    bad[5] = 0x7f;
    assert!(OpMsg::parse(&op_msg(1, 0, &bad)).is_err());
    let mut short = op_msg(1, 0, &[]);
    short.truncate(HEADER_LEN + 2);
    assert!(OpMsg::parse(&short).is_err());
}

#[test]
fn checksum() {
    let mut whole = op_msg(9, flags::CHECKSUM_PRESENT, &body(&ping()));
    let len = whole.len() + 4;
    whole[..4].copy_from_slice(&(len as i32).to_le_bytes());
    let crc = crc32c(&whole);
    whole.extend_from_slice(&crc.to_le_bytes());
    let m = OpMsg::parse(&whole).unwrap();
    assert_eq!(m.flags, flags::CHECKSUM_PRESENT);
    // The reply to a request with checksum carries one too.
    let mut c = Connection::new(1);
    let req = first(&mut c, &whole).unwrap();
    let reply = c.reply(&req, &Document::new().with("ok", 1.0), false);
    let n = reply.len();
    assert_eq!(u32::from_le_bytes(reply[16..20].try_into().unwrap()), flags::CHECKSUM_PRESENT);
    assert_eq!(crc32c(&reply[..n - 4]).to_le_bytes(), reply[n - 4..]);
    // A wrong checksum closes the connection.
    let n = whole.len();
    whole[n - 1] ^= 1;
    assert_eq!(OpMsg::parse(&whole), Err(WireError::Checksum));
    let mut tiny = op_msg(1, flags::CHECKSUM_PRESENT, &[]);
    tiny.push(0);
    assert!(OpMsg::parse(&tiny).is_err());
}

#[test]
fn more_to_come_and_exhaust_flags() {
    let mut c = Connection::new(1);
    let req = first(&mut c, &op_msg(5, flags::MORE_TO_COME, &body(&ping()))).unwrap();
    assert!(req.more_to_come());
    let req = first(&mut c, &op_msg(6, flags::EXHAUST_ALLOWED, &body(&ping()))).unwrap();
    assert!(req.exhaust_allowed() && !req.more_to_come());
    // A streamed reply sets moreToCome and answers the previous reply.
    let (bytes, id) = c.reply_to(77, &req, &Document::new().with("ok", 1.0), true);
    let h = Header::parse(&bytes);
    assert_eq!((h.response_to, h.request_id), (77, id));
    assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), flags::MORE_TO_COME);
}

#[test]
fn messages_arrive_in_pieces() {
    let mut c = Connection::new(1);
    let mut stream = op_msg(1, 0, &body(&ping()));
    stream.extend_from_slice(&op_msg(2, 0, &body(&ping())));
    for b in &stream[..stream.len() - 1] {
        c.receive(std::slice::from_ref(b));
    }
    assert_eq!(c.next_request().unwrap().map(|r| r.header.request_id), Some(1));
    assert_eq!(c.next_request().unwrap().map(|r| r.header.request_id), None);
    c.receive(&stream[stream.len() - 1..]);
    assert_eq!(c.next_request().unwrap().map(|r| r.header.request_id), Some(2));
    assert!(c.next_request().unwrap().is_none());
}

/// With authentication on, a connection not yet authenticated takes messages of at most 16 KiB
/// and closes at the header of a longer one, without an answer (measured on 04-11-2026 on a
/// `refrouter` and a `refnode` 9.0.4: 16,384 bytes answered, 16,385 closed). Once authenticated, or
/// with authentication off, the limit is RefDb's.
#[test]
fn an_unauthenticated_connection_takes_messages_of_16_kib() {
    use teseladb_wire::connection::UNAUTHENTICATED_MAX_MESSAGE;
    let header = |len: i32| {
        let mut h = Vec::new();
        Header { length: len, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }
            .write(&mut h);
        h
    };
    let over = UNAUTHENTICATED_MAX_MESSAGE as i32 + 1;
    let mut c = Connection::new(1);
    c.auth.required = true;
    c.receive(&header(over));
    assert_eq!(c.next_request().err(), Some(WireError::UnauthenticatedLength(over)));
    // At the limit the message is read as any other.
    let mut c = Connection::new(1);
    c.auth.required = true;
    c.receive(&header(UNAUTHENTICATED_MAX_MESSAGE as i32));
    assert_eq!(c.next_request(), Ok(None), "waits for the rest");
    // Without authentication required, the same header waits for its body.
    let mut c = Connection::new(1);
    c.receive(&header(over));
    assert_eq!(c.next_request(), Ok(None));
}

#[test]
fn message_length_limits() {
    for len in [15i32, -1, MAX_MESSAGE_SIZE as i32 + 1] {
        let mut c = Connection::new(1);
        let mut h = Vec::new();
        Header { length: len, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }
            .write(&mut h);
        c.receive(&h);
        assert_eq!(c.next_request().err(), Some(WireError::MessageLength(len)), "{len}");
    }
    // Removed and unknown opcodes close the connection.
    for op in [2002, 2005, 9999] {
        let mut c = Connection::new(1);
        let mut m = Vec::new();
        Header { length: 20, request_id: 1, response_to: 0, op_code: op }.write(&mut m);
        m.extend_from_slice(&[0; 4]);
        assert_eq!(first(&mut c, &m).err(), Some(WireError::UnsupportedOpcode(op)));
    }
}

#[test]
fn op_query_handshake() {
    let q = Document::new().with("isMaster", 1).with("client", Document::new());
    let whole = op_query("admin.$cmd", &q);
    let parsed = OpQuery::parse(&whole).unwrap();
    assert_eq!(parsed.namespace, "admin.$cmd");
    assert_eq!(parsed.command_db(), Some("admin"));
    let mut c = Connection::new(1);
    let req = first(&mut c, &whole).unwrap();
    assert_eq!(req.kind, Kind::Query);
    assert_eq!(req.db, "admin");
    assert_eq!(req.name(), "isMaster");
    // The reply is an OP_REPLY with AwaitCapable and one document.
    let reply = c.reply(&req, &Document::new().with("ok", 1.0), false);
    let h = Header::parse(&reply);
    assert_eq!((h.op_code, h.response_to), (opcode::OP_REPLY, 3));
    assert_eq!(i32::from_le_bytes(reply[16..20].try_into().unwrap()), AWAIT_CAPABLE);
    assert_eq!(i32::from_le_bytes(reply[32..36].try_into().unwrap()), 1);
    assert!(RawDoc::from_bytes(&reply[36..]).is_ok());
    // A query on a collection is the legacy kind.
    let req = first(&mut c, &op_query("test.items", &Document::new())).unwrap();
    assert_eq!(req.kind, Kind::LegacyQuery);
    let r = encode_reply(1, 2, 0, &bson(&Document::new()));
    assert_eq!(Header::parse(&r).length as usize, r.len());
    // Faults.
    let mut cut = whole.clone();
    cut.truncate(HEADER_LEN + 2);
    assert!(OpQuery::parse(&cut).is_err());
    let mut unterminated = whole[..HEADER_LEN + 4].to_vec();
    unterminated.extend_from_slice(b"admin");
    assert!(OpQuery::parse(&unterminated).is_err());
    let mut no_doc = whole[..HEADER_LEN + 4 + 11].to_vec();
    no_doc.extend_from_slice(&[0; 4]);
    assert!(OpQuery::parse(&no_doc).is_err());
}

#[test]
fn op_compressed_with_each_compressor() {
    let plain = op_msg(4, 0, &body(&ping()));
    for c in Compressor::ALL {
        let packed = compressed::compress(&plain, c);
        assert_eq!(Header::parse(&packed).op_code, opcode::OP_COMPRESSED);
        assert_eq!(Header::parse(&packed).request_id, 4);
        assert_eq!(compressed::decompress(&packed).unwrap(), (plain.clone(), c));
        assert_eq!(Compressor::from_name(c.name()), Some(c));
        // A compressed request gets a reply compressed the same way.
        let mut conn = Connection::new(1);
        let req = first(&mut conn, &packed).unwrap();
        assert_eq!(req.compressor, Some(c));
        let reply = conn.reply(&req, &Document::new().with("ok", 1.0), false);
        assert_eq!(compressed::decompress(&reply).map(|(_, x)| x), Ok(c));
    }
    assert_eq!(Compressor::from_name("noop"), None);
}

#[test]
fn op_compressed_faults() {
    let plain = op_msg(4, 0, &body(&ping()));
    let good = compressed::compress(&plain, Compressor::Zlib);
    let with = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut m = good.clone();
        f(&mut m);
        compressed::decompress(&m)
    };
    // noop and unknown ids, a wrapped OP_QUERY, sizes that do not match, a corrupt payload.
    assert_eq!(with(&|m| m[24] = 0), Err(WireError::UnknownCompressor(0)));
    assert_eq!(with(&|m| m[24] = 9), Err(WireError::UnknownCompressor(9)));
    assert_eq!(
        with(&|m| m[16..20].copy_from_slice(&2004i32.to_le_bytes())),
        Err(WireError::UnsupportedOpcode(2004))
    );
    assert!(with(&|m| m[20] += 1).is_err());
    assert!(with(&|m| m[20..24].copy_from_slice(&(-5i32).to_le_bytes())).is_err());
    assert!(
        with(&|m| {
            m.pop();
        })
        .is_err()
    );
    assert!(with(&|m| m.push(0)).is_err());
    assert!(with(&|m| m[26] ^= 0xff).is_err());
    assert!(compressed::decompress(&good[..HEADER_LEN + 5]).is_err());
    for c in [Compressor::Snappy, Compressor::Zstd] {
        let mut m = compressed::compress(&plain, c);
        m[20] += 1;
        assert!(compressed::decompress(&m).is_err());
    }
}

#[test]
fn disabled_compressors_close_the_connection() {
    let plain = op_msg(4, 0, &body(&ping()));
    let mut c = Connection::with_compressors(1, &[Compressor::Zlib]);
    c.receive(&compressed::compress(&plain, Compressor::Zlib));
    assert!(c.next_request().unwrap().is_some());
    c.receive(&compressed::compress(&plain, Compressor::Zstd));
    assert_eq!(c.next_request().err(), Some(WireError::UnknownCompressor(Compressor::Zstd.id())));
    let mut none = Connection::with_compressors(1, &[]);
    none.receive(&compressed::compress(&plain, Compressor::Snappy));
    assert!(none.next_request().is_err());
    assert_eq!(c.id(), 1);
    assert_eq!(c.buffered(), 0);
}

#[test]
fn unencodable_reply_answers_the_bson_error() {
    let mut c = Connection::new(1);
    let req = first(&mut c, &op_msg(1, 0, &body(&ping()))).unwrap();
    let bad = Document::new().with("a\u{0}b", 1);
    let bytes = c.reply(&req, &bad, false);
    let doc = RawDoc::from_bytes(&bytes[21..]).unwrap();
    assert_eq!(doc.get("code"), Some(teseladb_bson::RawValue::Int32(22)));
}

/// `levels` nested documents `{a: {a: … {x: 1}}}`, the outer one included.
fn nested(levels: usize) -> Document {
    (1..levels).fold(Document::new().with("x", 1), |inner, _| Document::new().with("a", inner))
}

/// A body written within a command's limits, deeper than a stored document may be.
fn deep_body(d: &Document) -> Vec<u8> {
    let limits = teseladb_bson::Limits { max_size: 1 << 20, max_depth: 400 };
    let mut s = vec![0u8];
    s.extend_from_slice(&d.to_bytes_with(limits).unwrap());
    s
}

#[test]
fn nesting_is_answered_as_a_refrouter_answers_it() {
    // A `refrouter` 9.0.4 takes objects 200 levels below the command and answers `Overflow` past
    // them, naming the element and the command's `_id` when it comes first (measured, 003 T009).
    let find = |filter: Document| Document::new().with("find", "c").with("filter", filter);
    let ok = OpMsg::parse(&op_msg(1, 0, &deep_body(&find(nested(200)).with("$db", "t")))).unwrap();
    assert_eq!(ok.problem, None);
    let m = OpMsg::parse(&op_msg(1, 0, &deep_body(&find(nested(201))))).unwrap();
    let p = m.problem.expect("Overflow");
    assert_eq!(p.code, error::OVERFLOW);
    let path = format!("filter{}", ".a".repeat(200));
    assert_eq!(
        p.errmsg,
        format!(
            "BSONObj exceeds maximum nested object depth in element with field name '{path}' in \
             object with unknown _id"
        )
    );
    let with_id = Document::new().with("_id", 7).with("find", "c").with("filter", nested(201));
    let p = OpMsg::parse(&op_msg(1, 0, &deep_body(&with_id))).unwrap().problem.unwrap();
    assert!(p.errmsg.ends_with("' in object with _id: 7"), "{}", p.errmsg);
    let with_id = Document::new().with("_id", "k\\").with("find", "c").with("filter", nested(201));
    let p = OpMsg::parse(&op_msg(1, 0, &deep_body(&with_id))).unwrap().problem.unwrap();
    assert!(p.errmsg.ends_with(r#"' in object with _id: \"k\\\""#), "{}", p.errmsg);
    // The depth found first wins over a fault further on, and a fault found first closes.
    let mut late = deep_body(&find(nested(250)).with("b", true));
    let last = late.len() - 2;
    late[last] = 2;
    assert_eq!(
        OpMsg::parse(&op_msg(1, 0, &late)).unwrap().problem.map(|p| p.code),
        Some(error::OVERFLOW)
    );
    let mut early = deep_body(&Document::new().with("b", true).with("filter", nested(250)));
    early[8] = 2;
    assert!(matches!(OpMsg::parse(&op_msg(1, 0, &early)), Err(WireError::Bson(_))));
    // A document of a sequence counts its levels on its own, named by its own `_id`.
    let insert = Document::new().with("insert", "c").with("$db", "t");
    let mut s = body(&insert);
    let stored = |levels: usize| Document::new().with("_id", 202).with("a", nested(levels - 1));
    let mut seq = vec![1u8];
    let limits = teseladb_bson::Limits { max_size: 1 << 20, max_depth: 400 };
    let doc = stored(201).to_bytes_with(limits).unwrap();
    let mut payload = b"documents\0".to_vec();
    payload.extend_from_slice(&doc);
    seq.extend_from_slice(&((payload.len() + 4) as i32).to_le_bytes());
    seq.extend_from_slice(&payload);
    s.extend_from_slice(&seq);
    assert_eq!(OpMsg::parse(&op_msg(1, 0, &s)).unwrap().problem, None);
    let mut s = body(&insert);
    let doc = stored(202).to_bytes_with(limits).unwrap();
    let mut payload = b"documents\0".to_vec();
    payload.extend_from_slice(&doc);
    s.push(1);
    s.extend_from_slice(&((payload.len() + 4) as i32).to_le_bytes());
    s.extend_from_slice(&payload);
    let p = OpMsg::parse(&op_msg(1, 0, &s)).unwrap().problem.unwrap();
    assert_eq!(p.code, error::OVERFLOW);
    assert!(p.errmsg.contains(&format!("'{}' in object with _id: 202", vec!["a"; 201].join("."))));
}

#[test]
fn what_the_bson_specification_refuses_is_answered() {
    // A `refrouter` 9.0.4 stores a string that is not UTF-8; TeselaDB answers `InvalidBSON` and keeps
    // the connection (003 T009, a divergence).
    let good = Document::new().with("insert", "c").with("s", "\u{e9}").with("$db", "t");
    let mut s = body(&good);
    let at = s.iter().position(|&c| c == 0xC3).unwrap();
    s[at] = 0xFF;
    let mut c = Connection::new(1);
    let req = first(&mut c, &op_msg(1, 0, &s)).unwrap();
    let p = req.problem.expect("InvalidBSON");
    assert_eq!((p.code, p.errmsg.as_str()), (error::INVALID_BSON, "invalid UTF-8 at 25"));
    // And in a sequence.
    let mut s = body(&Document::new().with("insert", "c").with("$db", "t"));
    let mut seq = sequence("documents", &[Document::new().with("s", "\u{e9}")]);
    let at = seq.iter().position(|&c| c == 0xC3).unwrap();
    seq[at] = 0xFF;
    s.extend_from_slice(&seq);
    let m = OpMsg::parse(&op_msg(1, 0, &s)).unwrap();
    assert_eq!(m.problem.map(|p| p.code), Some(error::INVALID_BSON));
}

#[test]
fn sequence_faults() {
    // A sequence whose size field is cut, and three sequences of one name (one error).
    let mut s = body(&ping());
    s.extend_from_slice(&[1, 9, 0]);
    assert!(OpMsg::parse(&op_msg(1, 0, &s)).is_err());
    let mut s = body(&ping());
    for _ in 0..3 {
        s.extend_from_slice(&sequence("d", &[]));
    }
    let m = OpMsg::parse(&op_msg(1, 0, &s)).unwrap();
    assert_eq!(m.problem.map(|p| p.code), Some(error::DUPLICATE_SEQUENCE));
    // A sequence of size under 4 or past the message.
    let mut s = body(&ping());
    s.extend_from_slice(&[1, 2, 0, 0, 0]);
    assert!(OpMsg::parse(&op_msg(1, 0, &s)).is_err());
    let mut s = body(&ping());
    s.extend_from_slice(&[1, 0xff, 0xff, 0xff, 0x7f]);
    assert!(OpMsg::parse(&op_msg(1, 0, &s)).is_err());
}

#[test]
fn bson_columns_are_answered_as_a_refrouter_answers_them() {
    // A `refrouter` 9.0.4 answers 378 for a BSON column whose bytes do not follow the column layout,
    // in any document of the message, naming the element and the document's `_id` (measured,
    // 003 T029); a valid column passes.
    let bad = Value::Binary { subtype: 7, bytes: vec![7] };
    let good = Value::Binary { subtype: 7, bytes: vec![0x10, 0, 5, 0, 0, 0, 0] };
    let find = |v: Value| {
        Document::new()
            .with("find", "c")
            .with("filter", Document::new().with("c", v))
            .with("$db", "t")
    };
    assert_eq!(OpMsg::parse(&op_msg(1, 0, &body(&find(good)))).unwrap().problem, None);
    let p = OpMsg::parse(&op_msg(1, 0, &body(&find(bad.clone())))).unwrap().problem.unwrap();
    assert_eq!(p.code, error::NON_CONFORMANT_BSON);
    assert_eq!(
        p.errmsg,
        "Invalid BSON column in element with field name 'filter.c' in object with unknown _id"
    );
    let insert = Document::new().with("insert", "c").with("$db", "t");
    let stored =
        Document::new().with("_id", "n1").with("a", Value::Array(vec![Value::Int32(1), bad]));
    let mut s = body(&insert);
    s.extend_from_slice(&sequence("documents", &[stored]));
    let p = OpMsg::parse(&op_msg(1, 0, &s)).unwrap().problem.unwrap();
    assert_eq!(
        p.errmsg,
        r#"Invalid BSON column in element with field name 'a.1' in object with _id: \"n1\""#
    );
}

#[test]
fn a_body_written_behind_the_header_is_the_message_written_apart() {
    // H-009 of the native tests: the reply's document is written in the message's buffer.
    let doc = teseladb_bson::Document::new().with("ok", 1.0).with(
        "cursor",
        teseladb_bson::Document::new()
            .with("id", 0i64)
            .with("firstBatch", Vec::<teseladb_bson::Value>::new()),
    );
    let limits = teseladb_bson::Limits::default();
    for bits in
        [0, teseladb_wire::msg::flags::CHECKSUM_PRESENT, teseladb_wire::msg::flags::MORE_TO_COME]
    {
        let apart = teseladb_wire::msg::OpMsg::encode(7, 3, bits, &doc.to_bytes().unwrap());
        let together =
            teseladb_wire::msg::OpMsg::encode_document(7, 3, bits, &doc, limits).unwrap();
        assert_eq!(together, apart, "flags {bits}");
    }
    // A document past the limits is the error of that fault.
    let tight = teseladb_bson::Limits { max_size: 10, max_depth: 10 };
    assert!(teseladb_wire::msg::OpMsg::encode_document(7, 3, 0, &doc, tight).is_err());
}
