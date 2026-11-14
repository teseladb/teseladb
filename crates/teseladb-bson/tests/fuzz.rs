//! Reading random and mutated bytes: never a panic nor a read out of bounds,
//! and whatever is accepted can be walked whole and written back to bytes that read the same.
//! Values are compared as bytes, because a double that is NaN is not equal to itself.

use teseladb_bson::{Document, RawDoc};
use teseladb_types::Rng;

/// Reads `bytes`; if accepted, walks it, writes it back and checks that writing again changes
/// nothing.
fn exercise(bytes: &[u8]) -> bool {
    let Ok(raw) = RawDoc::from_bytes(bytes) else {
        return false;
    };
    for (_, v) in &raw {
        let _ = v.element_type();
    }
    let once = Document::from(raw).to_bytes().expect("an accepted document can be written");
    let again = Document::from(RawDoc::from_bytes(&once).expect("what was written reads"))
        .to_bytes()
        .expect("to_bytes succeeds");
    assert_eq!(once, again);
    true
}

#[test]
fn random_bytes_are_refused_without_panicking() {
    let mut rng = Rng::new(3);
    let mut accepted = 0;
    for _ in 0..200_000 {
        let len = rng.below(48);
        let mut b = vec![0; len];
        rng.fill(&mut b);
        // Half of them with a right length and terminator, to get past the first check.
        if len >= 5 && rng.chance(0.5) {
            b[..4].copy_from_slice(&i32::try_from(len).expect("a test length").to_le_bytes());
            b[len - 1] = 0;
        }
        accepted += usize::from(exercise(&b));
    }
    // An empty document or one with a few simple fields comes up now and then.
    assert!(accepted > 0);
}
