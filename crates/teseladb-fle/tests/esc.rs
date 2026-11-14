//! The server cipher and the encrypted state collection (017 FR-005, FR-009): AES-256-CTR against
//! OpenSSL's output, and the search of an ESC's state over an in-memory collection built the way
//! writes, compaction and cleanup leave it.

use std::collections::HashMap;

use teseladb_fle::cipher;
use teseladb_fle::esc::{self, EscKeys, EscReader, EscState, Overlay};
use teseladb_fle::tokens::Token;

fn key() -> Token {
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::try_from(i).expect("index under 32");
    }
    Token(bytes)
}

const IV: [u8; 16] = [
    0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff,
];

#[test]
fn the_cipher_is_aes_256_ctr_with_its_iv_in_front() {
    // `openssl enc -aes-256-ctr -K 000102…1f -iv f0f1…ff` of "fourteen bytes".
    let expected =
        [0xf4, 0x6f, 0xb8, 0xff, 0x57, 0xf3, 0xe5, 0xa5, 0x7a, 0x0b, 0x9f, 0x20, 0x25, 0x41];
    let sealed = cipher::encrypt(&key(), IV, b"fourteen bytes");
    assert_eq!(&sealed[..16], &IV);
    assert_eq!(&sealed[16..], &expected);
    assert_eq!(cipher::decrypt(&key(), &sealed).expect("sealed"), b"fourteen bytes");
}

#[test]
fn decrypting_needs_a_whole_iv_and_an_empty_body_is_empty() {
    assert_eq!(cipher::decrypt(&key(), &[0u8; 15]), None);
    assert_eq!(cipher::decrypt(&key(), &IV), Some(Vec::new()));
}

/// An ESC in memory that counts what is read and read ahead.
#[derive(Default)]
struct Mem {
    docs: HashMap<Token, Vec<u8>>,
    gets: usize,
    rounds: usize,
    ahead: usize,
}

impl EscReader for Mem {
    type Error = ();
    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, ()> {
        self.gets += 1;
        Ok(self.docs.get(id).cloned())
    }
    fn read_ahead(&mut self, ids: &[Token]) -> Result<(), ()> {
        if !ids.is_empty() {
            self.rounds += 1;
            self.ahead += ids.len();
        }
        Ok(())
    }
}

impl Mem {
    fn write_positions(&mut self, keys: &EscKeys, from: u64, to: u64) {
        for p in from..=to {
            self.docs.insert(keys.position_id(p), Vec::new());
        }
    }
    fn anchor(&mut self, keys: &EscKeys, apos: u64, cpos: u64) {
        self.docs.insert(keys.anchor_id(apos), keys.anchor_value(0, cpos, IV));
    }
    fn null_anchor(&mut self, keys: &EscKeys, apos: u64, cpos: u64) {
        self.docs.insert(keys.anchor_id(0), keys.anchor_value(apos, cpos, IV));
    }
}

fn keys() -> EscKeys {
    EscKeys::of(&key().derive_u64(42))
}

#[test]
fn an_anchor_value_holds_its_two_numbers_and_rejects_short_values() {
    let k = keys();
    let sealed = k.anchor_value(3, 77, IV);
    assert_eq!(k.read_anchor(&sealed), Some((3, 77)));
    assert_eq!(k.read_anchor(&sealed[..20]), None);
    assert_eq!(k.read_anchor(&[1, 2, 3]), None);
    assert_ne!(k.anchor_id(0), k.anchor_id(1));
    assert_ne!(k.anchor_id(1), k.position_id(1));
}

#[test]
fn an_empty_esc_has_no_writes() {
    let mut mem = Mem::default();
    let state = esc::state(&mut mem, &keys()).expect("in memory");
    assert_eq!(state, EscState::default());
}

#[test]
fn positions_alone_give_their_count_for_every_count_up_to_a_hundred() {
    let k = keys();
    for n in 0..=100u64 {
        let mut mem = Mem::default();
        mem.write_positions(&k, 1, n);
        let state = esc::state(&mut mem, &k).expect("in memory");
        assert_eq!(state, EscState { null: None, apos: 0, anchored: 0, count: n }, "{n} writes");
        if n <= 16 {
            assert!(mem.rounds <= 2, "{n} writes took {} rounds", mem.rounds);
        }
    }
}

#[test]
fn an_anchor_moves_the_count_past_the_positions_it_recorded() {
    let k = keys();
    let mut mem = Mem::default();
    mem.anchor(&k, 1, 10);
    mem.anchor(&k, 2, 25);
    mem.write_positions(&k, 26, 31);
    let state = esc::state(&mut mem, &k).expect("in memory");
    assert_eq!(state, EscState { null: None, apos: 2, anchored: 25, count: 31 });
}

#[test]
fn a_cleanup_null_anchor_starts_the_anchors_after_its_number() {
    let k = keys();
    let mut mem = Mem::default();
    mem.null_anchor(&k, 4, 40);
    // Anchors 1 to 4 were removed by the cleanup; a later compaction adds anchor 5.
    let state = esc::state(&mut mem, &k).expect("in memory");
    assert_eq!(state, EscState { null: Some((4, 40)), apos: 4, anchored: 40, count: 40 });
    mem.anchor(&k, 5, 47);
    mem.write_positions(&k, 48, 50);
    let state = esc::state(&mut mem, &k).expect("in memory");
    assert_eq!(state, EscState { null: Some((4, 40)), apos: 5, anchored: 47, count: 50 });
}

#[test]
fn emu_binary_finds_the_end_of_any_prefix() {
    for from in [1u64, 5, 100] {
        for len in 0..200u64 {
            let mut probes = 0;
            let found = esc::emu_binary::<()>(from, |n| {
                probes += 1;
                Ok(n < from + len)
            })
            .expect("no error");
            assert_eq!(found, from + len - 1, "from {from}, {len} present");
            assert!(probes <= 2 * (64 - len.leading_zeros()) + 2, "{probes} probes for {len}");
        }
    }
}

#[test]
fn emu_binary_passes_the_reader_error_on() {
    let r = esc::emu_binary(1, |n| if n > 3 { Err("down") } else { Ok(true) });
    assert_eq!(r, Err("down"));
}

#[test]
fn the_overlay_sees_its_own_writes_and_reads_ahead_only_the_rest() {
    let k = keys();
    let mut mem = Mem::default();
    mem.write_positions(&k, 1, 3);
    let mut overlay = Overlay::new(&mut mem);
    overlay.write(k.position_id(4));
    overlay.write(k.position_id(5));
    assert_eq!(overlay.written(), &[k.position_id(4), k.position_id(5)]);
    assert_eq!(overlay.get(&k.position_id(5)).expect("in memory"), Some(Vec::new()));
    assert_eq!(overlay.get(&k.position_id(2)).expect("in memory"), Some(Vec::new()));
    assert_eq!(overlay.get(&k.position_id(6)).expect("in memory"), None);
    let state = esc::state(&mut overlay, &k).expect("in memory");
    assert_eq!(state.count, 5);
    overlay.read_ahead(&[k.position_id(4), k.position_id(9)]).expect("in memory");
    let written = overlay.into_written();
    assert_eq!(written.len(), 2);
    // The ids the overlay wrote never reach the inner reader's read-ahead.
    assert!(mem.ahead > 0);
}

/// A reader that fails every read: the state search passes the error on.
struct Down;

impl EscReader for Down {
    type Error = &'static str;
    fn get(&mut self, _id: &Token) -> Result<Option<Vec<u8>>, &'static str> {
        Err("down")
    }
}

#[test]
fn a_failing_reader_fails_the_state_search_and_reads_ahead_by_default() {
    assert_eq!(esc::state(&mut Down, &keys()), Err("down"));
    assert_eq!(Down.read_ahead(&[key()]), Ok(()));
}
