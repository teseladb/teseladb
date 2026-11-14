//! Packed documents (003 FR-024): the data files store the documents of their shapes as the
//! shape's number and the values, and every read sees each document byte for byte as written,
//! after a flush, after a restart and after compactions that mix files with shapes and files
//! written whole, as before 003 FR-024. Documents stored with their table of offsets after the
//! BSON (003 FR-021) pack and come back with it.

mod common;

use std::collections::BTreeMap;

use common::{get, open, scan, small, ts};
use teseladb_keys::KeyBuilder;
use teseladb_lsm::{DOCUMENT_VALUE, Kind, Lsm, LsmConfig};
use teseladb_bson::{Document, RawValue};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

/// Documents written per collection in a round.
const DOCS: u32 = 300;

/// What each key holds at the last timestamp written.
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// The stored value of document `i` of a round: three shapes, one of them with a nested
/// document, an array and a binary.
fn document(i: u32, round: u64) -> Vec<u8> {
    let d = Document::new().with("_id", i64::from(i)).with("round", round as i64);
    let d = match i % 3 {
        0 => d.with("name", format!("user {i}")).with("city", "Bilbao"),
        1 => d.with("name", format!("user {i}")).with("age", (i % 90) as i32),
        _ => d
            .with("address", Document::new().with("street", "Gran Vía").with("n", i as i32))
            .with("tags", Document::new().with("0", "a").with("1", 2.5)),
    };
    [&[DOCUMENT_VALUE][..], &d.to_bytes().unwrap()].concat()
}

/// The stored value of document `i` of a round wide enough for its table, which goes after the
/// BSON (003 FR-021); one in two has a nested document, and one in three has the tags of a
/// collection's dictionary of field names, some of its names unnumbered (FR-022).
fn tabled(i: u32, round: u64) -> Vec<u8> {
    let mut d = Document::new().with("_id", i64::from(i)).with("round", round as i64);
    for f in 0..teseladb_keys::stored::MIN_TABLE_FIELDS {
        d.push(format!("f{f}"), format!("value {i} {f}"));
    }
    if i.is_multiple_of(2) {
        d.push("nested", Document::new().with("n", i as i32));
    }
    let names = [&b"_id"[..], b"round", b"f1", b"f4"];
    let numbers = match i % 3 {
        0 => teseladb_keys::fields::FieldNumbers::from_names(0xD1C7, names),
        _ => teseladb_keys::fields::FieldNumbers::default(),
    };
    let stored = teseladb_keys::stored::encode_numbered(d.to_bytes().unwrap(), &numbers, None);
    let table = teseladb_keys::stored::split(&stored).unwrap().table().expect("a table");
    assert_eq!(table.dictionary(), i.is_multiple_of(3).then_some(0xD1C7));
    [&[DOCUMENT_VALUE][..], &stored].concat()
}

/// Writes a round of documents of collections 7 and 8 at timestamp `round`, a value that is not
/// a document and an index entry, and flushes them into a file.
fn round(lsm: &mut Lsm, disk: &mut MemDisk, model: &mut Model, index: &mut u64, round: u64) {
    round_of(document, (lsm, disk), model, index, round);
}

/// [`round`] with the documents `doc` gives.
fn round_of(
    doc: fn(u32, u64) -> Vec<u8>,
    (lsm, disk): (&mut Lsm, &mut MemDisk),
    model: &mut Model,
    index: &mut u64,
    round: u64,
) {
    for coll in [7, 8] {
        for i in 0..DOCS {
            let key = KeyBuilder::document(1, coll, RawValue::Int32(i as i32)).as_bytes().to_vec();
            let value = doc(i, round);
            *index += 1;
            lsm.put(G, *index, &key, ts(round, 0), &value).unwrap();
            model.insert(key, value);
        }
    }
    let others = [
        (KeyBuilder::index_prefix(1, 7, 2).as_bytes().to_vec(), vec![round as u8; 3]),
        (vec![teseladb_keys::space::SYSTEM, 9], vec![DOCUMENT_VALUE, 1, 2]),
    ];
    for (key, value) in others {
        *index += 1;
        lsm.put(G, *index, &key, ts(round, 0), &value).unwrap();
        model.insert(key, value);
    }
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

/// Checks every key of `model` at `at` with a read of each and with a scan of all.
fn check(lsm: &mut Lsm, disk: &mut MemDisk, model: &Model, at: u64) {
    let view = lsm.view();
    for (key, value) in model {
        let found = get(lsm, disk, &view, key, ts(at, 0)).map(|r| (r.2, r.3));
        assert_eq!(found, Some((Kind::Put, value.clone())), "{key:?}");
    }
    let all: Vec<(Vec<u8>, Vec<u8>)> = scan(lsm, disk, &view, b"", &[0xFF; 4], ts(at, 0))
        .into_iter()
        .map(|r| (r.0, r.3))
        .collect();
    assert_eq!(all, model.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
}

/// Bytes of the data blocks of the files as stored, without the pages of the filter, index,
/// properties and footer, which hold the same in both kinds of file.
fn files_bytes(lsm: &Lsm) -> u64 {
    lsm.space().data.stored
}

fn config(pack_documents: bool) -> LsmConfig {
    LsmConfig { memtable_bytes: 4 << 20, pack_documents, ..small() }
}

#[test]
fn documents_read_back_whole_after_a_flush_and_a_restart_in_smaller_files() {
    let mut sizes = Vec::new();
    for pack in [true, false] {
        let mut disk = MemDisk::new();
        let mut lsm = open(&config(pack), &mut disk);
        let (mut model, mut index) = (Model::new(), 0);
        round(&mut lsm, &mut disk, &mut model, &mut index, 10);
        check(&mut lsm, &mut disk, &model, 10);
        sizes.push(files_bytes(&lsm));
        drop(lsm);
        let mut lsm = open(&config(pack), &mut disk);
        check(&mut lsm, &mut disk, &model, 10);
    }
    // The names and types of every document of the packed file are in its shapes alone.
    assert!(sizes[0] * 10 < sizes[1] * 8, "{sizes:?}");
}

/// Writes a round into files written whole, as before packed documents, then two more over
/// the same keys with `then_pack`, compacts everything and checks every round's reads before
/// and after a restart. Gives the bytes of the files after the compaction.
fn mixed(then_pack: bool) -> u64 {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(false), &mut disk);
    let (mut model, mut index) = (Model::new(), 0);
    let mut models = Vec::new();
    round(&mut lsm, &mut disk, &mut model, &mut index, 10);
    models.push((10, model.clone()));
    drop(lsm);
    let mut lsm = open(&config(then_pack), &mut disk);
    for r in [20, 30] {
        round(&mut lsm, &mut disk, &mut model, &mut index, r);
        models.push((r, model.clone()));
    }
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    for (at, m) in &models {
        check(&mut lsm, &mut disk, m, *at);
    }
    let bytes = files_bytes(&lsm);
    drop(lsm);
    let mut lsm = open(&config(then_pack), &mut disk);
    for (at, m) in &models {
        check(&mut lsm, &mut disk, m, *at);
    }
    bytes
}

#[test]
fn compactions_mix_files_with_and_without_shapes() {
    // The compaction packs the newest versions again, those from the file written whole too.
    let (packed, whole) = (mixed(true), mixed(false));
    assert!(packed < whole, "{packed} and {whole}");
}

/// Writes two rounds of documents with their tables, the first into a file written whole, and
/// compacts them with `then_pack`, checking every read before and after a restart. Gives the
/// bytes of the files after the compaction.
fn with_tables(then_pack: bool) -> u64 {
    let mut disk = MemDisk::new();
    let mut lsm = open(&config(false), &mut disk);
    let (mut model, mut index) = (Model::new(), 0);
    round_of(tabled, (&mut lsm, &mut disk), &mut model, &mut index, 10);
    let first = model.clone();
    drop(lsm);
    let mut lsm = open(&config(then_pack), &mut disk);
    check(&mut lsm, &mut disk, &first, 10);
    round_of(tabled, (&mut lsm, &mut disk), &mut model, &mut index, 20);
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    check(&mut lsm, &mut disk, &first, 10);
    check(&mut lsm, &mut disk, &model, 20);
    let bytes = files_bytes(&lsm);
    drop(lsm);
    let mut lsm = open(&config(then_pack), &mut disk);
    check(&mut lsm, &mut disk, &first, 10);
    check(&mut lsm, &mut disk, &model, 20);
    bytes
}

#[test]
fn documents_with_their_tables_pack_and_come_back_after_a_restart_and_a_compaction() {
    // Their names and types go in the shapes, and their tables after the values.
    let (packed, whole) = (with_tables(true), with_tables(false));
    assert!(packed < whole, "{packed} and {whole}");
}
