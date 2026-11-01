//! A segment dumped as an immutable file and read back is the same segment (011 FR-012,
//! FR-013): same contents, same scores bit for bit, the applied mark it covers, and a damaged
//! file or one of other analyzers is refused instead of served.

use teseladb_bson::Document;
use teseladb_keys::Hlc;
use teseladb_search::definition::Definition;
use teseladb_search::index::MemIndex;
use teseladb_search::indexer::{DumpError, Found, Indexer};
use teseladb_search::segment::file::{self, Segment};

const TEXTS: [&str; 7] = [
    "gato perro",
    "gato gato perro raton",
    "",
    "el gato y el perro y el raton",
    "Wi-Fi WiFi wifi wi fi",
    "casa casa casa",
    "perro",
];

/// Dynamic mappings with a custom analyzer whose shingles stack tokens on one position, so the
/// scoring length and the number of tokens of a document differ.
fn definition() -> Definition {
    let shingles = Document::new()
        .with("name", "sh")
        .with("tokenizer", Document::new().with("type", "standard"))
        .with(
            "tokenFilters",
            vec![
                teseladb_bson::Value::Document(Document::new().with("type", "lowercase")),
                teseladb_bson::Value::Document(
                    Document::new()
                        .with("type", "shingle")
                        .with("minShingleSize", 2)
                        .with("maxShingleSize", 3),
                ),
            ],
        );
    let def = Document::new()
        .with(
            "mappings",
            Document::new().with("dynamic", true).with(
                "fields",
                Document::new()
                    .with("s", Document::new().with("type", "string").with("analyzer", "sh")),
            ),
        )
        .with("analyzers", vec![teseladb_bson::Value::Document(shingles)]);
    Definition::parse(&def).expect("valid definition")
}

fn doc(i: usize) -> Document {
    Document::new().with("_id", i as i32).with("t", TEXTS[i]).with("s", TEXTS[i])
}

fn bounds() -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![(Vec::new(), vec![3]), (vec![3], Vec::new())]
}

fn filled() -> Indexer {
    let mut ix = Indexer::new(definition());
    ix.reshape(&bounds()).expect("reshaped");
    ix.apply((0..TEXTS.len()).map(|i| (vec![i as u8], Some(doc(i))))).expect("indexed");
    // A replacement and a deletion leave holes the dump does not keep.
    ix.apply([(vec![1], Some(doc(5))), (vec![6], None)]).expect("changed");
    ix
}

fn query(op: &str, path: &str, q: &str) -> Document {
    Document::new().with(op, Document::new().with("query", q).with("path", path))
}

fn scores(found: &[Found]) -> Vec<(Vec<u8>, u32, Document)> {
    found.iter().map(|f| (f.key.clone(), f.score.to_bits(), f.doc.clone())).collect()
}

const MARK: Hlc = Hlc { physical: 1_700_000_000_000, logical: 7 };

#[test]
fn a_dumped_index_reads_back_whole() {
    let ix = filled();
    let files = ix.dump(MARK);
    assert_eq!(files.len(), 2, "one file per segment");
    let back = Indexer::load(definition(), &files).expect("read back");
    assert_eq!(back.contents(), ix.contents());
    assert_eq!((back.len(), back.segments()), (ix.len(), ix.segments()));
    for (op, path, q) in [
        ("text", "t", "gato perro"),
        ("text", "s", "gato perro raton"),
        ("phrase", "t", "el gato"),
        ("text", "t", "wifi"),
        ("phrase", "s", "casa casa"),
    ] {
        let spec = query(op, path, q);
        assert_eq!(
            scores(&back.search(&spec).unwrap()),
            scores(&ix.search(&spec).unwrap()),
            "{op} {q}"
        );
        assert_eq!(back.meta(&spec).unwrap(), ix.meta(&spec).unwrap());
    }
    for f in &files {
        assert_eq!(Indexer::applied_of(&f.bytes), Ok(MARK), "the mark the file covers");
    }
}

#[test]
fn a_loaded_index_keeps_indexing() {
    let mut a = filled();
    let mut b = Indexer::load(definition(), &a.dump(MARK)).expect("read back");
    let more = [(vec![2], Some(doc(3))), (vec![9], Some(doc(4))), (vec![0], None)];
    a.apply(more.clone()).unwrap();
    b.apply(more).unwrap();
    assert_eq!(b.contents(), a.contents());
    let spec = query("text", "t", "el gato wifi");
    assert_eq!(scores(&b.search(&spec).unwrap()), scores(&a.search(&spec).unwrap()));
    // A segment read back splits and merges like one built from the documents.
    let whole: Vec<(Vec<u8>, Vec<u8>)> = vec![(Vec::new(), Vec::new())];
    a.reshape(&whole).unwrap();
    b.reshape(&whole).unwrap();
    assert_eq!(b.contents(), a.contents());
}

#[test]
fn an_empty_index_dumps_and_reads_back() {
    let ix = Indexer::new(definition());
    let files = ix.dump(MARK);
    let back = Indexer::load(definition(), &files).expect("read back");
    assert!(back.is_empty());
    assert_eq!(back.segments(), 1);
    assert_eq!(Indexer::load(definition(), &[]).map(|i| i.segments()), Ok(1));
}

#[test]
fn a_damaged_or_foreign_file_is_refused() {
    let ix = filled();
    let files = ix.dump(MARK);
    let mut bad = files.clone();
    let n = bad[0].bytes.len();
    bad[0].bytes[n - 5] ^= 0x40;
    assert!(matches!(Indexer::load(definition(), &bad), Err(DumpError::Corrupt(_))));
    assert!(matches!(Indexer::applied_of(&bad[0].bytes[..10]), Err(DumpError::Corrupt(_))));
    // Overlapping or unordered ranges do not make an index.
    let swapped = vec![files[1].clone(), files[0].clone()];
    assert_eq!(Indexer::load(definition(), &swapped).err(), Some(DumpError::Ranges));
    // A file whose terms other analyzers made is rebuilt, not served.
    let mut data = Segment::open(&files[0].bytes).unwrap().to_data().unwrap();
    data.analyzer_version += 1;
    let mut other = files.clone();
    other[0].bytes = file::build(&data);
    assert_eq!(Indexer::load(definition(), &other).err(), Some(DumpError::Analyzers));
    // A segment file that is not a search index's dump has no mark.
    data.analyzer_version -= 1;
    data.index = "plain".into();
    other[0].bytes = file::build(&data);
    assert!(matches!(Indexer::load(definition(), &other), Err(DumpError::Corrupt(_))));
    assert!(matches!(Indexer::applied_of(&other[0].bytes), Err(DumpError::Corrupt(_))));
    // MemIndex itself round-trips through the file of one segment.
    let mut one = MemIndex::new(definition());
    one.put(b"k", &doc(4)).unwrap();
    let bytes = one.dump(MARK);
    assert_eq!(MemIndex::load(definition(), &bytes).map(|m| m.contents()), Ok(one.contents()));
}
