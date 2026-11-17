#![allow(unsafe_code)]
//! The support of the table-driven parsers (the original's ragel.hh): a token split across
//! buffers is collected by the builder and its guards, the state stack grows by doubling from
//! 16 entries, and `StateMachineParser::call` hands back the unparsed rest of a buffer, or
//! nothing when the parser wants more input; an empty buffer is the end of input.
#![cfg(target_os = "linux")]

use teseladb::engine::core::parser_support::{
    ParserBase, SstringBuilder, SstringBuilderGuard, StateMachineParser, UnconsumedRemainder,
    trim_trailing_spaces_and_tabs,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;

/// Feeds `blocks` to a builder the way a parser does: the token starts at the first `<` and
/// ends at the first `>`; returns the collected token.
fn collect(blocks: &[&[u8]]) -> Option<String> {
    let mut builder = SstringBuilder::default();
    let mut out = None;
    for block in blocks {
        let range = block.as_ptr_range();
        // SAFETY: every pointer given to the guard lies in `block`, which outlives it.
        unsafe {
            let mut g = SstringBuilderGuard::new(&mut builder, range.start, range.end);
            for (i, &c) in block.iter().enumerate() {
                let p = range.start.add(i);
                if c == b'<' {
                    g.mark_start(p.add(1));
                } else if c == b'>' {
                    let s = g.get_str(p);
                    out = Some(String::from_utf8(s.as_bytes().to_vec()).unwrap());
                }
            }
        }
    }
    out
}

#[test]
fn a_token_inside_one_block() {
    assert_eq!(collect(&[b"xx<name>yy"]).as_deref(), Some("name"));
    assert_eq!(collect(&[b"<>"]).as_deref(), Some(""));
}

#[test]
fn a_token_split_across_blocks_is_joined() {
    assert_eq!(collect(&[b"..<na", b"m", b"e>.."]).as_deref(), Some("name"));
    // The pending piece continues from the very start of the next block.
    assert_eq!(collect(&[b"<ab", b">"]).as_deref(), Some("ab"));
}

#[test]
fn builder_get_reset_and_guard_access() {
    let mut builder = SstringBuilder::default();
    let data = b"hello world";
    let r = data.as_ptr_range();
    // SAFETY: the pointers lie in `data`.
    unsafe {
        let mut g = SstringBuilderGuard::new(&mut builder, r.start, r.end);
        g.mark_start(r.start);
        g.mark_end(r.start.add(5));
        // The string is in the builder until taken.
        assert_eq!(g.builder().get().as_bytes(), b"hello");
        assert!(g.builder().get().empty(), "get takes the string");
    }
    // A token open when the block ends is kept by the guard's drop, and reset forgets it.
    // SAFETY: as above.
    unsafe {
        let mut g = SstringBuilderGuard::new(&mut builder, r.start, r.end);
        g.mark_start(r.start.add(6));
    }
    let mut copy = SstringBuilder::default();
    std::mem::swap(&mut copy, &mut builder);
    assert_eq!(copy.get().as_bytes(), b"world");
    let mut b2 = SstringBuilder::default();
    // SAFETY: as above.
    unsafe {
        let mut g = SstringBuilderGuard::new(&mut b2, r.start, r.end);
        g.mark_start(r.start);
    }
    b2.reset();
    assert!(b2.get().empty());
}

#[test]
fn parser_base_stack_grows_by_doubling() {
    let mut base = ParserBase::default();
    assert_eq!(base._fsm_stack_size, 0);
    base.prepush();
    assert_eq!(base._fsm_stack_size, 16);
    assert_eq!(base._fsm_stack.len(), 16);
    base.prepush();
    assert_eq!(base._fsm_stack_size, 16, "room left: no growth");
    base._fsm_top = 16;
    base.prepush();
    assert_eq!(base._fsm_stack_size, 32);
    assert_eq!(base._fsm_stack.len(), 32);
    base.postpop();
    base.init_base();
    assert!(base.get_str().empty());
}

/// Stops right after the first `;` and remembers whether the end of input was seen.
struct UntilSemicolon {
    base: ParserBase,
    eof_seen: bool,
}

impl StateMachineParser for UntilSemicolon {
    fn base(&mut self) -> &mut ParserBase {
        &mut self.base
    }
    fn parse(&mut self, p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8 {
        self.eof_seen = !eof.is_null();
        let mut q = p;
        while q != pe {
            // SAFETY: `q` is in `[p, pe)`, the buffer being parsed.
            unsafe {
                if *q == b';' {
                    return q.add(1);
                }
                q = q.add(1);
            }
        }
        std::ptr::null_mut()
    }
}

fn feed_buf(p: &mut UntilSemicolon, buf: TemporaryBuffer<u8>) -> UnconsumedRemainder {
    p.call(buf).get().unwrap()
}

fn feed(p: &mut UntilSemicolon, s: &[u8]) -> UnconsumedRemainder {
    feed_buf(p, TemporaryBuffer::copy_from(s))
}

#[test]
fn call_returns_the_rest_or_asks_for_more() {
    let mut p = UntilSemicolon { base: ParserBase::default(), eof_seen: false };
    assert!(p.base()._builder.get().empty());
    let rest = feed(&mut p, b"ab;cd").expect("stopped inside the buffer");
    assert_eq!(rest.as_slice(), b"cd");
    assert!(!p.eof_seen);
    assert!(feed(&mut p, b"abcd").is_none(), "no ';': needs more data");
    // An empty buffer over real storage marks the end of input: eof = pe.
    let mut drained = TemporaryBuffer::copy_from(b"x");
    drained.trim(0);
    assert!(feed_buf(&mut p, drained).is_none());
    assert!(p.eof_seen, "an empty buffer marks the end of input");
    // A default empty buffer has no storage, so its end pointer, and with it eof, is null:
    // the original computes `eof = buf.empty() ? pe : nullptr` from the same null pointer.
    assert!(feed_buf(&mut p, TemporaryBuffer::empty_buffer()).is_none());
    assert!(!p.eof_seen);
    let rest = feed(&mut p, b";").expect("stopped at the end");
    assert!(rest.empty());
}

#[test]
fn trailing_spaces_and_tabs_are_trimmed() {
    for (input, want) in [("a \t ", "a"), ("", ""), (" \t", ""), ("a b", "a b"), ("x\n ", "x\n")] {
        let mut s = Sstring::from(input);
        trim_trailing_spaces_and_tabs(&mut s);
        assert_eq!(s.as_bytes(), want.as_bytes(), "{input:?}");
    }
}
