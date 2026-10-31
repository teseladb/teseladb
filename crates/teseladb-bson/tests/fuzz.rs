//! Reading random and mutated bytes (003 FR-002, T008): never a panic nor a read out of bounds,
//! and whatever is accepted can be walked whole and written back to bytes that read the same.
//! Values are compared as bytes, because a double that is NaN is not equal to itself.

mod common;

use teseladb_bson::{Document, RawDoc};
use teseladb_types::Rng;

/// Reads `bytes`; if accepted, walks it, writes it back and checks that writing again changes
/// nothing.
fn exercise(bytes: &[u8]) -> bool {
    let Ok(raw) = RawDoc::from_bytes(bytes) else { return false };
    for (_, v) in raw.iter() {
        let _ = v.element_type();
    }
    let once = Document::from(raw).to_bytes().expect("an accepted document can be written");
    let again = Document::from(RawDoc::from_bytes(&once).expect("what was written reads"))
        .to_bytes()
        .unwrap();
    assert_eq!(once, again);
    true
}

#[test]
fn random_bytes_are_refused_without_panicking() {
    let mut rng = Rng::new(3);
    let mut accepted = 0;
    for _ in 0..200_000 {
        let len = rng.range(0, 48) as usize;
        let mut b = vec![0; len];
        rng.fill(&mut b);
        // Half of them with a right length and terminator, to get past the first check.
        if len >= 5 && rng.chance(0.5) {
            b[..4].copy_from_slice(&(len as i32).to_le_bytes());
            b[len - 1] = 0;
        }
        accepted += usize::from(exercise(&b));
    }
    // An empty document or one with a few simple fields comes up now and then.
    assert!(accepted > 0);
}

#[test]
fn mutated_corpus_documents_are_read_or_refused_without_panicking() {
    let docs = common::valid_documents();
    let mut rng = Rng::new(4);
    let (mut accepted, mut refused) = (0, 0);
    for doc in &docs {
        for _ in 0..300 {
            let mut b = doc.clone();
            match rng.range(0, 4) {
                0 => {
                    let i = rng.range(0, b.len() as u64) as usize;
                    b[i] ^= 1 << rng.range(0, 8);
                }
                1 => {
                    let i = rng.range(0, b.len() as u64) as usize;
                    b[i] = rng.next_u32() as u8;
                }
                2 => b.truncate(rng.range(0, b.len() as u64) as usize),
                _ => {
                    let i = rng.range(0, b.len() as u64 + 1) as usize;
                    b.insert(i, rng.next_u32() as u8);
                }
            }
            if exercise(&b) { accepted += 1 } else { refused += 1 }
        }
    }
    assert!(accepted > 0 && refused > 0, "{accepted} accepted, {refused} refused");
}
