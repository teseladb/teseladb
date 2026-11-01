//! Segments from outside the crate (011 T010, FR-009, FR-012): build, write, reopen, check the
//! checksums, merge with the GC threshold and record the result in the manifest.

use std::collections::BTreeMap;

use teseladb_keys::Hlc;
use teseladb_search::query::bm25::{Bm25, FieldStats};
use teseladb_search::query::wand::{Cursor, top_k};
use teseladb_search::segment::data::{
    DocEntry, FieldData, Occurrence, PostingEntry, SegmentData, VectorData,
};
use teseladb_search::segment::file::{Segment, build};
use teseladb_search::segment::manifest::{Op, encode, replay};
use teseladb_search::segment::merge::merge;
use teseladb_search::segment::norms;
use teseladb_search::vector::{Quantization, Similarity};
use teseladb_types::GroupId;

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

/// A segment of `texts`, one document each, tokenised on spaces into field `body`.
fn segment(prefix: &str, texts: &[&str], applied_index: u64) -> SegmentData {
    let mut data = SegmentData::new(GroupId(4), "default", 1);
    data.applied_index = applied_index;
    let mut body = FieldData::default();
    for (doc, text) in texts.iter().enumerate() {
        let doc = doc as u32;
        data.docs.push(DocEntry {
            key: format!("{prefix}{doc}").into_bytes(),
            ts: ts(10 + u64::from(doc)),
        });
        data.stored.push(Some(text.as_bytes().to_vec()));
        let mut start = 0;
        let mut terms: BTreeMap<Vec<u8>, Vec<Occurrence>> = BTreeMap::new();
        let words: Vec<&str> = text.split(' ').collect();
        for (position, word) in words.iter().enumerate() {
            let end = start + word.len() as u32;
            terms.entry(word.as_bytes().to_vec()).or_default().push(Occurrence {
                position: position as u32,
                start,
                end,
            });
            start = end + 1;
        }
        body.lengths.push(words.len() as u32);
        for (term, occurrences) in terms {
            body.terms.entry(term).or_default().push(PostingEntry { doc, occurrences });
        }
    }
    data.fields.insert("body".to_owned(), body);
    data
}

fn search(seg: &Segment<'_>, terms: &[&str], k: usize) -> Vec<String> {
    let body = seg.field("body").expect("field");
    let docs = seg.docs() as u64;
    let total: u64 = (0..seg.docs() as u32).map(|d| u64::from(body.length(d))).sum();
    let stats = FieldStats { docs, total_len: total };
    let mut cursors: Vec<_> = terms
        .iter()
        .filter_map(|t| {
            let df = body.cursor(t.as_bytes(), Bm25::DEFAULT.term(1.0, stats, 1))?.ok()?.doc_freq();
            body.cursor(t.as_bytes(), Bm25::DEFAULT.term(1.0, stats, df))?.ok()
        })
        .collect();
    top_k(&mut cursors, k)
        .into_iter()
        .map(|h| String::from_utf8(seg.doc(h.doc).key).expect("utf-8"))
        .collect()
}

#[test]
fn a_written_segment_reopens_and_answers_queries() {
    let data = segment(
        "a",
        &["the quick brown fox", "the lazy dog", "quick quick fox jumps", "a fox"],
        42,
    );
    let bytes = build(&data);
    let seg = Segment::open(&bytes).expect("valid segment");
    assert_eq!(seg.to_data().expect("decodes"), data);
    assert_eq!((seg.range, seg.applied_index, seg.docs()), (GroupId(4), 42, 4));
    assert_eq!(seg.stored(1), Some(&b"the lazy dog"[..]));
    let body = seg.field("body").expect("field");
    assert_eq!(body.norm(2), norms::encode(4));
    // "quick" twice in a document as long as the others ranks it first; "a fox" is shortest.
    assert_eq!(search(&seg, &["quick"], 2), ["a2", "a0"]);
    assert_eq!(search(&seg, &["fox"], 1), ["a3"]);
    assert_eq!(search(&seg, &["dog", "lazy"], 5), ["a1"]);
    assert!(search(&seg, &["absent"], 5).is_empty());
    // The dictionary runs automata: every term, in order.
    let terms: Vec<Vec<u8>> = body.dictionary().entries().into_iter().map(|e| e.0).collect();
    assert_eq!(terms.first().map(Vec::as_slice), Some(&b"a"[..]));
    assert_eq!(terms.len() as u64, body.terms());
    let mut c = body
        .cursor(b"the", Bm25::DEFAULT.term(1.0, FieldStats::default(), 2))
        .expect("term")
        .expect("ok");
    assert_eq!(c.doc(), 0);
    c.advance(1);
    assert_eq!(c.doc(), 1);
}

#[test]
fn every_flipped_bit_of_a_section_or_the_footer_is_caught() {
    let bytes = build(&segment("a", &["one two", "two three", "three four five"], 1));
    let ok = Segment::open(&bytes).expect("valid").to_data().expect("decodes");
    for at in 0..bytes.len() {
        let mut bad = bytes.clone();
        bad[at] ^= 0x10;
        // Only the zero padding between sections lies outside every checksum.
        if let Ok(seg) = Segment::open(&bad) {
            assert_eq!(seg.to_data().expect("decodes"), ok, "byte {at}");
        }
    }
}

#[test]
fn a_merge_drops_deletions_below_the_threshold_and_the_manifest_records_it() {
    let a = segment("a", &["red apple", "green apple", "red cherry"], 10);
    let b = segment("b", &["red wine", "white wine"], 20);
    let a_bytes = build(&a);
    let b_bytes = build(&b);
    let mut manifest = Vec::new();
    for (number, applied_index) in [(1, 10), (2, 20)] {
        manifest.extend(encode(&[Op::AddSegment {
            number,
            range: GroupId(4),
            index: "default".into(),
            applied_index,
        }]));
    }
    // a1 deleted at 5 (below the threshold of 8), b0 replaced at 30 (above it).
    manifest.extend(encode(&[
        Op::Delete { number: 1, doc: 1, ts: ts(5) },
        Op::Delete { number: 2, doc: 0, ts: ts(30) },
    ]));
    let (state, _) = replay(&manifest).expect("valid manifest");
    let inputs: Vec<_> = [(1, &a_bytes), (2, &b_bytes)]
        .into_iter()
        .map(|(n, bytes)| {
            let data = Segment::open(bytes).expect("valid").to_data().expect("decodes");
            (data, state.segments[&n].deletes.clone())
        })
        .collect();
    let (merged, deletes) = merge(&inputs, ts(8));
    let merged_bytes = build(&merged);
    let seg = Segment::open(&merged_bytes).expect("valid merged segment");
    let keys: Vec<String> =
        (0..seg.docs() as u32).map(|d| String::from_utf8(seg.doc(d).key).expect("utf-8")).collect();
    assert_eq!(keys, ["a0", "a2", "b0", "b1"]);
    assert_eq!(deletes, BTreeMap::from([(2, ts(30))]));
    assert_eq!(seg.applied_index, 20);
    assert!(seg.field("body").expect("field").postings(b"green").is_none());
    assert_eq!(search(&seg, &["red"], 10).len(), 3);
    // The merge goes into the manifest as one edit.
    let mut edit = vec![Op::RemoveSegment { number: 1 }, Op::RemoveSegment { number: 2 }];
    edit.push(Op::AddSegment {
        number: 3,
        range: GroupId(4),
        index: "default".into(),
        applied_index: 20,
    });
    edit.extend(deletes.iter().map(|(&doc, &ts)| Op::Delete { number: 3, doc, ts }));
    manifest.extend(encode(&edit));
    let (state, end) = replay(&manifest).expect("valid manifest");
    assert_eq!(end, manifest.len() as u64);
    assert_eq!(state.segments.keys().copied().collect::<Vec<_>>(), vec![3]);
    assert_eq!(state.segments[&3].deletes, deletes);
    assert_eq!(state.next_number, 4);
}

#[test]
fn a_sealed_segment_hides_its_text_and_opens_with_its_master_key() {
    use teseladb_crypto::at_rest::{CipherMode, KeyRing};
    use teseladb_search::segment::file::{build_sealed, open_sealed};
    let data = segment("secret-key-", &["confidential salary figures", "more private words"], 9);
    for mode in [CipherMode::Cbc, CipherMode::Gcm] {
        let ring = KeyRing::new(&[4; 32], mode, |b: &mut [u8]| {
            b.fill(2);
            Ok(())
        });
        let sealed = build_sealed(&data, &ring).unwrap();
        for leak in [&b"confidential"[..], b"salary", b"secret-key-"] {
            assert!(!sealed.windows(leak.len()).any(|w| w == leak), "{mode:?}");
        }
        assert!(Segment::open(&sealed).is_err(), "a sealed segment needs opening first");
        let clear = open_sealed(&sealed, Some(&ring)).unwrap();
        assert_eq!(clear, build(&data));
        assert_eq!(Segment::open(&clear).unwrap().to_data().unwrap(), data);
        // Without the ring, with another master key, or changed: refused.
        assert!(open_sealed(&sealed, None).is_err());
        let other = KeyRing::new(&[5; 32], mode, |b: &mut [u8]| {
            b.fill(2);
            Ok(())
        });
        assert!(open_sealed(&sealed, Some(&other)).is_err());
        for at in [4096 + 40, sealed.len() - 10, 4100] {
            let mut bad = sealed.clone();
            bad[at] ^= 1;
            assert!(open_sealed(&bad, Some(&ring)).is_err(), "{mode:?} byte {at}");
        }
        assert!(open_sealed(&sealed[..10], Some(&ring)).is_err());
    }
    // A clear segment comes back as it is.
    let clear = build(&data);
    assert_eq!(open_sealed(&clear, None).unwrap(), clear);
}

#[test]
fn vector_fields_are_written_reopened_and_merged() {
    let with_vectors = |prefix: &str, values: Vec<Option<Vec<f32>>>| {
        let texts: Vec<&str> = values.iter().map(|_| "a").collect();
        let mut s = segment(prefix, &texts, 1);
        s.vectors.insert(
            "v".to_owned(),
            VectorData {
                dims: 2,
                similarity: Similarity::Euclidean,
                quantization: Quantization::None,
                values,
            },
        );
        s
    };
    let a = with_vectors("a", vec![Some(vec![0.0, 0.0]), None, Some(vec![1.0, 0.0])]);
    let bytes = build(&a);
    let seg = Segment::open(&bytes).expect("open");
    let g = seg.vectors("v").expect("graph");
    let top: Vec<u32> = g.search(&[0.9, 0.0], 10, 2, |_| true).iter().map(|s| s.doc).collect();
    assert_eq!(top, [2, 0]);
    assert!(seg.vectors("w").is_none());
    assert_eq!(seg.to_data().expect("data").vectors, a.vectors);
    // A merge with a segment without the field keeps every document's place.
    let b = segment("b", &["a", "a"], 2);
    let mut deletes = BTreeMap::new();
    deletes.insert(0u32, ts(5));
    let (m, _) = merge(&[(a, deletes), (b, BTreeMap::new())], ts(9));
    assert_eq!(m.vectors["v"].values, [None, Some(vec![1.0, 0.0]), None, None]);
    let bytes = build(&m);
    let seg = Segment::open(&bytes).expect("open merged");
    let g = seg.vectors("v").expect("graph");
    assert_eq!(
        g.search(&[0.0, 0.0], 10, 10, |_| true).iter().map(|s| s.doc).collect::<Vec<_>>(),
        [1]
    );
}
