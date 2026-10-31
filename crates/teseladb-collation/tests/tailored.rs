//! Mappings in context and tailorings over a base: the longest context wins, a context table
//! without a real match falls back to a shorter one, the tailoring's contexts join the base's,
//! and suppressed contractions take the tailoring's own with them.

use teseladb_collation::{Ce, Mappings, Root, Tables, Tailored, elements};

fn ce(p: u32) -> Ce {
    Ce { primary: p, secondary: 0x0500, tertiary: 0x0500 }
}

/// A root with a, b, c, x and two strings in the context `a`.
fn root() -> Root {
    Root::new(Tables {
        ces: vec![
            ce(0x1000_0000),
            ce(0x2000_0000),
            ce(0x3000_0000),
            ce(0x4000_0000),
            ce(0x5000_0000),
            ce(0x6000_0000),
        ],
        singles: vec![(0x61, 0, 1), (0x62, 1, 1), (0x63, 2, 1), (0x78, 3, 1)],
        contexts: vec![(vec![0x61], vec![0x62], 4, 1), (vec![0x61], vec![0x62, 0x63], 5, 1)],
        ..Tables::default()
    })
}

fn primaries<M: Mappings<Element = Ce>>(m: &M, s: &str) -> Vec<u32> {
    elements(m, &s.chars().map(u32::from).collect::<Vec<_>>())
        .iter()
        .map(|c| c.primary >> 28)
        .collect()
}

#[test]
fn contexts_of_the_root() {
    let r = root();
    assert_eq!(r.contexts(0x62), [&[0x61][..]]);
    assert_eq!(primaries(&r, "ab"), [1, 5]);
    assert_eq!(primaries(&r, "abc"), [1, 6]);
    assert_eq!(primaries(&r, "bc"), [2, 3]);
}

#[test]
fn tailored_contexts_join_the_base_and_suppression_removes_them() {
    let r = root();
    let mut t = Tailored::new(&r);
    t.insert_context(&[0x78, 0x61], &[0x62], vec![ce(0x7000_0000)]);
    t.insert_context(&[0x61], &[0x62], vec![ce(0x8000_0000)]);
    t.insert_context(&[0x61], &[0x62], vec![ce(0x9000_0000)]);
    assert_eq!(t.contexts(0x62), [&[0x78, 0x61][..], &[0x61][..]]);
    assert_eq!(primaries(&t, "xab"), [4, 1, 7]);
    assert_eq!(primaries(&t, "ab"), [1, 9]);
    assert_eq!(t.own().count(), 2);
    t.suppress_contractions(0x62, 0x62);
    assert_eq!(t.contexts(0x62), Vec::<&[u32]>::new());
    assert_eq!(primaries(&t, "ab"), [1, 2]);
    assert!(!t.starts(&[0x61], &[0x62]));
}
