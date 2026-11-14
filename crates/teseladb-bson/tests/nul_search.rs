//! The vector search of the 0 byte that ends a name (003 FR-023) gives what the byte-by-byte
//! search gives, at every position and length around the vector widths, and documents read the
//! same with it.

use teseladb_bson::{BsonError, Document, Limits, RawDoc, find_nul, nul_search, validate_message};

fn bytewise(b: &[u8]) -> Option<usize> {
    b.iter().position(|&c| c == 0)
}

/// A small deterministic generator (xorshift), so a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn every_position_of_a_single_zero_matches_the_bytewise_search() {
    for len in 0..=130 {
        let mut b = vec![0x61u8; len];
        assert_eq!(find_nul(&b), bytewise(&b), "len {len} without a zero");
        for at in 0..len {
            b[at] = 0;
            assert_eq!(find_nul(&b), Some(at), "len {len}, zero at {at}");
            b[at] = 0x61;
        }
    }
}

#[test]
fn the_first_of_several_zeros_and_bytes_with_the_high_bit_match() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let len = usize::try_from(rng.next() % 200).expect("small");
        let density = rng.next() % 64 + 1;
        let b: Vec<u8> = (0..len)
            .map(|_| {
                let r = rng.next();
                if r.is_multiple_of(density) {
                    0
                } else {
                    u8::try_from(r >> 56).expect("byte").max(1)
                }
            })
            .collect();
        for start in [0, 1, 3, 15, 17] {
            let s = b.get(start..).unwrap_or(&[]);
            assert_eq!(find_nul(s), bytewise(s), "{s:?}");
        }
    }
}

#[test]
fn the_search_in_use_is_a_vector_one_on_the_architectures_that_have_it() {
    let path = nul_search();
    if cfg!(target_arch = "aarch64") {
        assert_eq!(path, "neon");
    } else if cfg!(target_arch = "x86_64") {
        assert!(path == "avx2" || path == "sse2", "{path}");
    } else {
        assert_eq!(path, "bytewise");
    }
    // With `TESELADB_EXPECT_ACCEL`, the CPU of the run has to give the paths it names.
    if let Ok(expected) = std::env::var("TESELADB_EXPECT_ACCEL")
        && expected.split(',').any(|n| n == "bson-avx2")
    {
        assert_eq!(path, "avx2");
    }
}

#[test]
fn documents_with_long_and_short_names_read_and_validate_as_before() {
    let long = "n".repeat(70);
    let names = ["", "a", "abcdefghijklmno", "abcdefghijklmnop", long.as_str(), "ñandú"];
    let mut doc = Document::new();
    for (i, n) in names.iter().enumerate() {
        doc = doc.with(format!("{n}{i}"), i64::try_from(i).expect("small"));
    }
    let bytes = doc.to_bytes().expect("encodes");
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let keys: Vec<&str> = raw.iter().map(|(k, _)| k).collect();
    let want: Vec<String> = names.iter().enumerate().map(|(i, n)| format!("{n}{i}")).collect();
    assert_eq!(keys, want);
    validate_message(&bytes, Limits::default()).expect("valid message");
}

#[test]
fn a_name_that_runs_into_the_end_of_the_document_is_refused_where_its_value_is_cut() {
    // An `int32` element whose 40-byte name ends at the document's last byte: the search finds that
    // byte, and the value after it is missing.
    let mut bytes = vec![0, 0, 0, 0, 0x10];
    bytes.extend(std::iter::repeat_n(b'x', 40));
    bytes.push(0);
    let n = i32::try_from(bytes.len()).expect("small");
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    let err = RawDoc::from_bytes(&bytes).expect_err("cut short");
    assert!(matches!(err, BsonError::InvalidBson { offset: 46, .. }), "{err:?}");
}
