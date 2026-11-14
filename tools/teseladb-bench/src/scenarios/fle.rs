//! `fle-query` scenario (017 SC-002, T011): an equality query on a field of Queryable Encryption
//! over the wire protocol against `--target` or a local cluster of three nodes, one connection,
//! each `find` timed alone, over `--records` documents `{_id, ssn}` where `ssn` is encrypted with
//! equality queries and contention 0 and each value has 10 documents. The payloads are built here
//! from a fixed index key, as a driver builds them with the reference encryption library (the same
//! token derivations, checked against its vectors in `teseladb-fle`); the server never needs the
//! key. The collection is created with its metadata collections and the index of `__safeContent__`,
//! as a driver's helper creates it. Target: under 5 ms at p50 with three nodes; `encrypted_insert`
//! is the rate of the load.

use teseladb_bson::{Document, Value};
use teseladb_fle::tokens::{self, Collection, Token};

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, Target, check, doc};
use crate::report::Report;

/// Database and collection of the documents.
const DB: &str = "bench";
const COLLECTION: &str = "fle";
/// The metadata collections of the collection, by their default names.
const ESC: &str = "enxcol_.fle.esc";
const ECOC: &str = "enxcol_.fle.ecoc";
/// The array of tags of an encrypted document.
const SAFE_CONTENT: &str = "__safeContent__";
/// The encrypted field.
const FIELD: &str = "ssn";
/// Documents of each value.
const PER_VALUE: i64 = 10;
/// SC-002: the equality query under 5 ms at p50 with three nodes.
const EQUALITY_P50_NS: f64 = 5_000_000.0;
/// Documents written by each `insert`.
const BATCH: i64 = 500;
/// The index key's id (`keyId` of the field), a UUID.
const KEY_ID: [u8; 16] = [0x17; 16];
/// The index key (its root token).
const ROOT: Token = Token([0x5a; 32]);
/// FLE2InsertUpdatePayloadV2 and FLE2FindEqualityPayloadV2, and a stored equality value.
const INSERT_SUBTYPE: u8 = 11;
const FIND_SUBTYPE: u8 = 12;
/// The BSON type of the clear values: string.
const STRING: i32 = 2;
/// The length of the value the client would have encrypted (`v`): key id and ciphertext.
const CIPHERTEXT: usize = 64;

fn coll() -> Value {
    Value::String(COLLECTION.into())
}

fn binary(subtype: u8, bytes: Vec<u8>) -> Value {
    Value::Binary { subtype, bytes }
}

fn token(t: &Token) -> Value {
    binary(0, t.0.to_vec())
}

/// The tokens of value number `n` that depend only on the value: EDC, ESC and server derived.
fn value_tokens(n: i64) -> (Token, Token, Token) {
    let level1 = tokens::collections_level1(&ROOT);
    let clear = format!("value-{n}");
    let edc = tokens::collection_token(&level1, Collection::Edc).derive(clear.as_bytes());
    let esc = tokens::collection_token(&level1, Collection::Esc).derive(clear.as_bytes());
    let server = tokens::server_token_derivation_level1(&ROOT).derive(clear.as_bytes());
    (edc, esc, server)
}

/// A payload: the subtype byte, then the BSON document.
fn payload(subtype: u8, body: &Document) -> Result<Value, String> {
    let mut bytes = vec![subtype];
    bytes.extend(body.to_bytes().map_err(super::msg)?);
    Ok(binary(6, bytes))
}

/// The insert payload of value number `n`, with contention factor 0.
fn insert_payload(n: i64) -> Result<Value, String> {
    let (edc, esc, server) = value_tokens(n);
    let mut value = KEY_ID.to_vec();
    value.resize(CIPHERTEXT, 0x33);
    let body = doc(vec![
        ("d", token(&edc.derive_u64(0))),
        ("s", token(&esc.derive_u64(0))),
        ("p", binary(0, vec![0x44; 48])),
        ("u", binary(4, KEY_ID.to_vec())),
        ("t", Value::Int32(STRING)),
        ("v", binary(0, value)),
        ("e", token(&tokens::server_data_encryption_level1(&ROOT))),
        ("l", token(&server)),
        ("k", Value::Int64(0)),
    ]);
    payload(INSERT_SUBTYPE, &body)
}

/// The find payload of value number `n`.
fn find_payload(n: i64) -> Result<Value, String> {
    let (edc, esc, server) = value_tokens(n);
    let body = doc(vec![
        ("d", token(&edc)),
        ("s", token(&esc)),
        ("l", token(&server)),
        ("cm", Value::Int64(0)),
    ]);
    payload(FIND_SUBTYPE, &body)
}

/// The collection's `encryptedFields`.
fn encrypted_fields() -> Value {
    let field = doc(vec![
        ("path", Value::String(FIELD.into())),
        ("bsonType", Value::String("string".into())),
        ("keyId", binary(4, KEY_ID.to_vec())),
        (
            "queries",
            Value::Document(doc(vec![
                ("queryType", Value::String("equality".into())),
                ("contention", Value::Int64(0)),
            ])),
        ),
    ]);
    Value::Document(doc(vec![("fields", Value::Array(vec![Value::Document(field)]))]))
}

/// What a command of a driver with automatic encryption carries.
fn encryption_information() -> Value {
    let ns = format!("{DB}.{COLLECTION}");
    Value::Document(doc(vec![
        ("type", Value::Int32(1)),
        ("schema", Value::Document(doc(vec![(ns.as_str(), encrypted_fields())]))),
    ]))
}

/// Runs `command`; its error names the command and its first argument.
fn run_on(c: &mut Client, command: Document) -> Result<Vec<u8>, String> {
    let name = match command.iter().next() {
        Some((k, Value::String(v))) => format!("{k} {v}"),
        Some((k, _)) => k.to_owned(),
        None => String::new(),
    };
    let reply = c.command(DB, command).map_err(crate::client::context(&name))?;
    check(&reply).map_err(crate::client::context(&name))?;
    Ok(reply)
}

/// What the scenario sends before the load, in order: the metadata collections, the collection
/// and the index of `__safeContent__`, which the server does not create (a driver's helper does,
/// contract `fle-server.md`); without it each query reads the whole collection.
fn setup() -> Vec<Document> {
    let index = doc(vec![
        ("key", Value::Document(doc(vec![(SAFE_CONTENT, Value::Int32(1))]))),
        ("name", Value::String(format!("{SAFE_CONTENT}_1"))),
    ]);
    vec![
        doc(vec![("create", Value::String(ESC.into()))]),
        doc(vec![("create", Value::String(ECOC.into()))]),
        doc(vec![("create", coll()), ("encryptedFields", encrypted_fields())]),
        doc(vec![
            ("createIndexes", coll()),
            ("indexes", Value::Array(vec![Value::Document(index)])),
        ]),
    ]
}

/// Creates the collection and writes `records` documents; the number of values.
fn load(c: &mut Client, records: u64) -> Result<i64, String> {
    // The collection may not exist yet.
    for name in [COLLECTION, ESC, ECOC] {
        let _ = c.command(DB, doc(vec![("drop", Value::String(name.into()))]));
    }
    for command in setup() {
        run_on(c, command)?;
    }
    let records = i64::try_from(records).unwrap_or(i64::MAX).max(PER_VALUE);
    let mut next = 0;
    while next < records {
        let mut docs = Vec::new();
        for i in next..(next + BATCH).min(records) {
            let d = doc(vec![("_id", Value::Int64(i)), (FIELD, insert_payload(i / PER_VALUE)?)]);
            docs.push(Value::Document(d));
        }
        next += BATCH;
        run_on(
            c,
            doc(vec![
                ("insert", coll()),
                ("documents", Value::Array(docs)),
                ("encryptionInformation", encryption_information()),
            ]),
        )?;
    }
    Ok(records / PER_VALUE)
}

/// The documents of the first batch of a `find` reply.
fn found(reply: &[u8]) -> usize {
    let Ok(raw) = teseladb_bson::RawDoc::from_bytes(reply) else { return 0 };
    let d = Document::from(raw);
    let Some(Value::Document(cursor)) = d.get("cursor") else { return 0 };
    match cursor.get("firstBatch") {
        Some(Value::Array(b)) => b.len(),
        _ => 0,
    }
}

/// The `fle-query` scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let started = std::time::Instant::now();
    let values = load(&mut c, args.records)?;
    let loaded = started.elapsed().as_secs_f64();
    let mut k = 0i64;
    let equality = each(args.warmup, args.duration, || {
        k = (k + 1) % values;
        let filter = Value::Document(doc(vec![(FIELD, find_payload(k)?)]));
        let reply = run_on(
            &mut c,
            doc(vec![
                ("find", coll()),
                ("filter", filter),
                ("encryptionInformation", encryption_information()),
            ]),
        )?;
        let n = found(&reply);
        if n != PER_VALUE as usize {
            return Err(format!("the query of value {k} found {n} documents, not {PER_VALUE}"));
        }
        Ok(())
    })?;
    let mut report = Report::new("fle-query");
    report.latency("encrypted_equality_10", &equality, Some(EQUALITY_P50_NS), None);
    // Each encrypted insert runs in its own transaction of the node (D-120).
    let records = (values * PER_VALUE) as f64;
    report.push("encrypted_insert", "docs/s", records / loaded.max(f64::MIN_POSITIVE), None);
    report.notes.push(format!(
        "{} documents, 10 per encrypted value, contention 0, one connection, target {}",
        args.records,
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn payloads_carry_the_subtype_and_the_tokens_of_their_value() {
        let Value::Binary { subtype: 6, bytes } = insert_payload(3).unwrap() else { panic!() };
        assert_eq!(bytes[0], INSERT_SUBTYPE);
        let parsed = teseladb_fle::payloads::InsertUpdatePayload::parse(&bytes).unwrap();
        let (edc, esc, _) = value_tokens(3);
        assert_eq!((parsed.tokens.edc, parsed.tokens.esc), (edc.derive_u64(0), esc.derive_u64(0)));
        let Value::Binary { bytes, .. } = find_payload(3).unwrap() else { panic!() };
        let find = teseladb_fle::payloads::FindEqualityPayload::parse(&bytes).unwrap();
        assert_eq!((find.tokens.edc, find.max_contention), (edc, 0));
        assert_ne!(value_tokens(4).0, edc);
        assert_eq!(found(b"not bson"), 0);
        let bytes = |d: Document| d.to_bytes().unwrap();
        assert_eq!(found(&bytes(Document::new().with("ok", 1.0))), 0, "no cursor");
        assert_eq!(found(&bytes(Document::new().with("cursor", Document::new()))), 0, "no batch");
        let names: Vec<String> =
            setup().iter().map(|d| format!("{:?}", d.iter().next().unwrap().1)).collect();
        assert_eq!(names.len(), 4);
        assert!(names[0].contains(ESC) && names[1].contains(ECOC), "{names:?}");
        assert!(format!("{:?}", setup()[3]).contains("__safeContent___1"));
    }
}
