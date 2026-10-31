//! The encrypted audit log (013 T042, FR-031) against files RefDb Enterprise 9.0.4 wrote with
//! `--auditLocalKeyFile` and a test key (`tests/fixtures/audit_encrypted/`, measured on
//! 31-10-2026): TeselaDB writes their header and their messages byte for byte from the same keys.

use std::path::Path;

use teseladb_auth::audit_crypt::{AuditEncryption, Compression, NONCE_LEN, TAG_LEN, to_json};
use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_crypto::aes::Aes;
use teseladb_crypto::modes::{AesGcm, cbc_decrypt};
use teseladb_x509::pem::base64_decode;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/audit_encrypted").join(name),
    )
    .unwrap()
}

fn master() -> [u8; 32] {
    base64_decode(fixture("test.key").trim().as_bytes()).unwrap().try_into().unwrap()
}

fn string(d: &Document, key: &str) -> String {
    match d.get(key) {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{key}: {other:?}"),
    }
}

fn millis(d: &Document) -> i64 {
    match d.get("ts") {
        Some(Value::DateTime(ms)) => *ms,
        other => panic!("{other:?}"),
    }
}

/// The log's key and the IV that wrapped it, from a header.
fn unwrap(header: &Document) -> ([u8; 32], [u8; 16]) {
    let Some(Value::Binary { bytes, .. }) = header.get("encryptedKey") else { panic!() };
    let iv: [u8; 16] = bytes[..16].try_into().unwrap();
    let aes = Aes::new(&master()).unwrap();
    let key = cbc_decrypt(&aes, &iv, &bytes[16..]).unwrap();
    (key.try_into().unwrap(), iv)
}

/// Opens `tag ‖ nonce ‖ ciphertext`, checking the 12-byte tag.
fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Vec<u8> {
    let (tag, rest) = sealed.split_at(TAG_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    let gcm = AesGcm::new(Aes::new(key).unwrap());
    // GCM's keystream gives the plaintext; sealing it again gives the tag over the ciphertext.
    let mut plain = ct.to_vec();
    gcm.seal_in_place(nonce, aad, &mut plain);
    let mut again = plain.clone();
    let expected = gcm.seal_in_place(nonce, aad, &mut again);
    assert_eq!(&expected[..TAG_LEN], tag, "tag");
    assert_eq!(again, ct);
    plain
}

#[test]
fn refdbs_encrypted_logs_open_and_are_rewritten_byte_for_byte() {
    for (file, compression) in [("none.json", Compression::None), ("zstd.json", Compression::Zstd)]
    {
        let text = fixture(file);
        let lines: Vec<&str> = text.lines().collect();
        let header = extjson::parse(lines[0]).unwrap();
        assert_eq!(string(&header, "compressionMode"), compression.name());
        let (log_key, iv) = unwrap(&header);
        let enc = AuditEncryption::new(&master(), log_key, iv, "/tmp/k", compression);
        // The same header, MAC included.
        assert_eq!(to_json(&enc.header(millis(&header))), lines[0], "{file}");
        for line in &lines[1..] {
            let record = extjson::parse(line).unwrap();
            let ts = millis(&record);
            let sealed = base64_decode(string(&record, "log").as_bytes()).unwrap();
            let plain = open(&log_key, &ts.to_le_bytes(), &sealed);
            let bson = match compression {
                Compression::None => plain.clone(),
                Compression::Zstd => teseladb_compress::zstd::decompress(&plain, 1 << 20).unwrap(),
            };
            let doc = Document::from(RawDoc::from_bytes(&bson).unwrap());
            assert!(matches!(doc.get("atype"), Some(Value::String(_))), "{doc:?}");
            // Sealed again by TeselaDB with the next counter: the same line.
            if compression == Compression::None {
                assert_eq!(to_json(&enc.record(ts, &bson)), *line, "{file}");
            }
        }
    }
}

#[test]
fn new_logs_seal_with_a_rising_counter_and_compress_with_zstd() {
    let enc =
        AuditEncryption::new(&[7; 32], [9; 32], [3; 16], "/keys/audit.key", Compression::Zstd);
    let header = enc.header(1_793_413_741_694);
    let line = to_json(&header);
    assert!(line.contains(r#""subType":"0"}"#) && line.contains(r#""filename":"/keys/audit.key""#));
    assert!(!format!("{enc:?}").contains("[9"), "the key never shows");
    let message = Document::new().with("atype", "applicationMessage").to_bytes().unwrap();
    for n in 1u64..=2 {
        let r = enc.record(1_793_413_741_700, &message);
        let sealed = base64_decode(string(&r, "log").as_bytes()).unwrap();
        assert_eq!(&sealed[TAG_LEN + 4..TAG_LEN + NONCE_LEN], &n.to_le_bytes());
        let plain = open(&[9; 32], &1_793_413_741_700i64.to_le_bytes(), &sealed);
        assert_eq!(teseladb_compress::zstd::decompress(&plain, 1 << 20).unwrap(), message);
    }
    assert_eq!(Compression::ALL.map(Compression::name), ["none", "zstd"]);
}

#[test]
fn the_options_and_the_auditor_encrypt_only_a_file() {
    use teseladb_auth::audit::{AuditType, Auditor, Client, Config, Event, Options};
    let options =
        |dest: &'static str, key: Option<&'static str>, mode: Option<&'static str>| Options {
            destination: Some(dest),
            format: (dest == "file").then_some("JSON"),
            path: (dest == "file").then_some("/tmp/a.json"),
            local_key_file: key,
            compression_mode: mode,
            ..Options::default()
        };
    let err = |o: Options<'_>| Config::from_options(&o).unwrap_err().0;
    assert_eq!(
        err(options("file", None, Some("zstd"))),
        "auditLog.compressionMode is only allowed if audit log encryption is enabled"
    );
    assert_eq!(
        err(options("file", Some("/k"), Some("lz4"))),
        "auditLog.compressionMode must be set as zstd"
    );
    let alone = Options { local_key_file: Some("/k"), ..Options::default() };
    assert_eq!(
        err(alone),
        "auditLog.destination is required when auditLog.localAuditKeyFile is specified"
    );
    let config = Config::from_options(&options("file", Some("/k"), Some("zstd"))).unwrap().unwrap();
    assert_eq!(
        (config.local_key_file.as_deref(), config.compression),
        (Some("/k"), Compression::Zstd)
    );
    // A file: the header, then sealed messages; the console keeps its plain lines.
    let event = Event { atype: AuditType::Shutdown, ts_ms: 5, param: Document::new(), result: 0 };
    let system = Client::system([1; 16]);
    for (dest, encrypted) in [("file", true), ("console", false)] {
        let mut config = Config::from_options(&options(dest, Some("/k"), None)).unwrap().unwrap();
        config.encryption =
            Some(AuditEncryption::new(&[7; 32], [9; 32], [3; 16], "/k", Compression::None));
        let a = Auditor::new(config);
        let line = String::from_utf8(a.render(&event, &system).unwrap()).unwrap();
        assert_eq!(
            line.starts_with(r#"{"ts":{"$date":{"$numberLong":"5"}},"log":""#),
            encrypted,
            "{line}"
        );
        assert!(a.encryption_header(5).is_some());
    }
    let mut bson = Config::from_options(&Options {
        format: Some("BSON"),
        ..options("file", Some("/k"), None)
    })
    .unwrap()
    .unwrap();
    bson.encryption =
        Some(AuditEncryption::new(&[7; 32], [9; 32], [3; 16], "/k", Compression::None));
    let a = Auditor::new(bson);
    let record = Document::from(RawDoc::from_bytes(&a.render(&event, &system).unwrap()).unwrap());
    assert!(matches!(record.get("log"), Some(Value::String(_))));
    let header = Document::from(RawDoc::from_bytes(&a.encryption_header(5).unwrap()).unwrap());
    assert_eq!(string(&header, "auditRecordType"), "header");
    let plain = Auditor::new(Config::from_options(&options("file", None, None)).unwrap().unwrap());
    assert!(plain.encryption_header(5).is_none());
}

/// The header of a log whose key a KMIP server wrapped (measured with a responder that gave the
/// data back without an IV).
#[test]
fn a_kmip_wrapped_key_goes_in_the_header_as_refdb_writes_it() {
    let enc = AuditEncryption::with_kmip(
        [9; 32],
        "1",
        &["127.0.0.1".to_owned()],
        5698,
        (&[4; 32], &[]),
        Compression::None,
    );
    let line = to_json(&enc.header(1_793_414_387_798));
    assert!(
        line.starts_with(r#"{"ts":{"$date":{"$numberLong":"1793414387798"}},"version":"0.0","compressionMode":"none","keyStoreIdentifier":{"provider":"kmip","uid":"1","kmipServerName":["127.0.0.1"],"kmipPort":{"$numberInt":"5698"},"keyWrapMethod":"encrypt"},"encryptedKey":{"$binary":{"base64":"OAAAAAVpdgAAAAAAAAVrZXkAIAAAAAAE"#),
        "{line}"
    );
}

#[test]
fn a_kmip_key_needs_a_kmip_server_and_no_key_file() {
    use teseladb_auth::audit::{Config, Options};
    let base = Options {
        destination: Some("file"),
        format: Some("JSON"),
        path: Some("/a"),
        encryption_key_uid: Some("1"),
        ..Options::default()
    };
    let err = |o: Options<'_>| Config::from_options(&o).unwrap_err().0;
    assert_eq!(
        err(base),
        "security.kmip.serverName is required when auditLog.auditEncryptionKeyIdentifier is specified"
    );
    let both = Options { kmip_server_set: true, local_key_file: Some("/k"), ..base };
    assert_eq!(
        err(both),
        "auditLog.auditEncryptionKeyIdentifier is not allowed when auditLog.localAuditKeyFile is specified"
    );
    let ok = Options { kmip_server_set: true, compression_mode: Some("zstd"), ..base };
    let config = Config::from_options(&ok).unwrap().unwrap();
    assert_eq!(
        (config.encryption_key_uid.as_deref(), config.compression),
        (Some("1"), Compression::Zstd)
    );
}
