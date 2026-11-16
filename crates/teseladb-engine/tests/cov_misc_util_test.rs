//! Tests of the small utility modules that need no reactor: reference wrappers, lazy values,
//! transforming iterators, memory-size parsing, file-system errors, source locations, the
//! signal-safe printers, indirect comparisons, deferred actions, iovec lengths, the
//! case-insensitive hasher, the spinlock, critical allocation sections, tuple helpers, the
//! capped token bucket and the deferred close and stop guards.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::ffi::c_void;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::util::closeable::{Closeable, DeferredClose, DeferredStop, Stoppable};
use teseladb::engine::util::conversions::{BadLexicalCast, parse_memory_size, string2vector};
use teseladb::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection;
use teseladb::engine::util::defer::{DeferredAction, defer};
use teseladb::engine::util::exceptions::{make_filesystem_error, make_filesystem_error2};
use teseladb::engine::util::indirect::{
    IndirectEqualTo, IndirectHash, IndirectLess, default_indirect_equal_to, default_indirect_less,
};
use teseladb::engine::util::internal::iovec_utils::{Iovec, iovec_len, iovec_len_ptr};
use teseladb::engine::util::lazy::{lazy_deref, value_of};
use teseladb::engine::util::print_safe::{
    convert_decimal_safe, convert_hex_safe, convert_zero_padded_hex_safe, print_decimal_safe,
    print_safe, print_zero_padded_hex_safe,
};
use teseladb::engine::util::reference_wrapper::{cref, ref_};
use teseladb::engine::util::shared_token_bucket::internal::{
    CappedRelease, Milli, RateResolution, Rovers, RoversCapped, SharedTokenBucket,
};
use teseladb::engine::util::source_location_compat::internal::SourceLocation;
use teseladb::engine::util::spinlock::util::Spinlock;
use teseladb::engine::util::string_utils::internal::{
    CaseInsensitiveHasher, std_hash_bytes_lowered,
};
use teseladb::engine::util::transform_iterator::make_transform_iterator;
use teseladb::engine::util::tuple_utils::internal::untuple;
use teseladb::engine::util::tuple_utils::{
    FalseType, FilterClass, TrueType, tuple_filter_by_type_ref,
};
use teseladb::engine::util::used_size::internal::used_size;

#[test]
fn reference_wrappers_reach_the_object() {
    let mut x = 5;
    let mut w = ref_(&mut x);
    assert_eq!(*w.get(), 5);
    *w.get_mut() += 1;
    assert_eq!(*w.get(), 6);
    let inner = w.into_inner();
    *inner *= 2;
    assert_eq!(x, 12);

    let y = String::from("text");
    let c = cref(&y);
    let copy = c;
    #[allow(clippy::clone_on_copy)]
    let cloned = c.clone();
    assert_eq!(copy.get(), "text");
    assert!(std::ptr::eq(cloned.get(), &y));
}

#[test]
fn lazy_values_are_computed_when_printed() {
    let calls = Cell::new(0);
    let v = value_of(|| {
        calls.set(calls.get() + 1);
        40 + 2
    });
    assert_eq!(calls.get(), 0);
    assert_eq!(v.call(), 42);
    assert_eq!(format!("{v}"), "42");
    assert_eq!(calls.get(), 2);

    let some = Some(Box::new(7));
    assert_eq!(lazy_deref(&some).to_string(), "7");
    let none: Option<Box<i32>> = None;
    assert_eq!(lazy_deref(&none).to_string(), "null");
}

#[test]
fn transform_iterators_map_and_compare_by_position() {
    let f: fn(i32) -> i32 = |x| x * 10;
    let it = make_transform_iterator(0..4, f);
    assert_eq!(it.size_hint(), (4, Some(4)));
    let same = make_transform_iterator(0..4, f);
    let other = make_transform_iterator(1..4, f);
    assert!(it == same);
    assert!(it != other);
    let cloned = it.clone();
    assert_eq!(cloned.collect::<Vec<_>>(), vec![0, 10, 20, 30]);
    assert_eq!(it.skip(3).collect::<Vec<_>>(), vec![30]);
}

#[test]
fn memory_sizes_parse_with_units_and_suffixes() {
    assert_eq!(parse_memory_size("512").unwrap(), 512);
    assert_eq!(parse_memory_size("4k").unwrap(), 4 << 10);
    assert_eq!(parse_memory_size("4K").unwrap(), 4 << 10);
    assert_eq!(parse_memory_size("1KiB").unwrap(), 1 << 10);
    assert_eq!(parse_memory_size("2Mi").unwrap(), 2 << 20);
    assert_eq!(parse_memory_size("3G").unwrap(), 3 << 30);
    assert_eq!(parse_memory_size("1T").unwrap(), 1 << 40);
    assert_eq!(parse_memory_size("7B").unwrap(), 7);
    for bad in ["", "abc", "4X", "k", "-1"] {
        let e = parse_memory_size(bad).unwrap_err();
        assert!(e.is::<BadLexicalCast>(), "{bad}");
    }
    assert!(BadLexicalCast.to_string().contains("cannot parse value"));
    assert_eq!(string2vector("ab"), b"ab\0".to_vec());
    assert_eq!(string2vector(""), vec![0u8]);
}

#[test]
fn filesystem_errors_carry_their_paths() {
    let e = make_filesystem_error("open", PathBuf::from("/no/such"), 2);
    assert_eq!(e.code(), 2);
    assert_eq!(e.path1(), Path::new("/no/such"));
    assert!(e.path2().is_none());
    let text = e.to_string();
    assert!(text.starts_with("filesystem error: open: "), "{text}");
    assert!(text.ends_with("[/no/such]"), "{text}");

    // The two-path form records the first path twice and ignores the second.
    let e2 = make_filesystem_error2("rename", PathBuf::from("/a"), PathBuf::from("/b"), 17);
    assert_eq!(e2.code(), 17);
    assert_eq!(e2.path1(), Path::new("/a"));
    assert_eq!(e2.path2(), Some(Path::new("/a")));
    assert!(e2.to_string().ends_with("[/a] [/a]"), "{e2}");
}

#[test]
fn used_size_is_the_type_size() {
    assert_eq!(used_size::<u64>(), 8);
    assert_eq!(used_size::<[u16; 3]>(), 6);
    assert_eq!(used_size::<()>(), 0);
}

#[test]
fn source_locations_report_where_they_were_taken() {
    let unknown = SourceLocation::new();
    assert_eq!(unknown.file_name(), "unknown");
    assert_eq!(unknown.function_name(), "unknown");
    assert_eq!(unknown.line(), 0);
    assert_eq!(unknown.column(), 0);
    let d = SourceLocation::default();
    assert_eq!(d.file_name(), "unknown");

    let here = line!() + 1;
    let c = SourceLocation::current();
    assert!(c.file_name().ends_with("cov_misc_util_test.rs"), "{}", c.file_name());
    assert_eq!(c.line(), here);
    assert_eq!(c.function_name(), "");
    assert_eq!(c.column(), 0);

    let here = line!() + 1;
    let m = teseladb::source_location_current!();
    assert_eq!(m.line(), here);
    assert!(m.file_name().ends_with("cov_misc_util_test.rs"));
    assert!(
        m.function_name().ends_with("source_locations_report_where_they_were_taken"),
        "{}",
        m.function_name()
    );

    let at = SourceLocation::current_at("f.rs", "g", 3, 4);
    assert_eq!((at.file_name(), at.function_name(), at.line(), at.column()), ("f.rs", "g", 3, 4));
}

#[test]
fn signal_safe_conversions_write_digits() {
    let mut buf = [0u8; 8];
    let start = convert_hex_safe(&mut buf, 0xabcu32);
    assert_eq!(start, 5);
    assert_eq!(&buf, b"     abc");
    let start = convert_hex_safe(&mut buf, 0u32);
    assert_eq!(start, 7);
    assert_eq!(&buf, b"       0");

    let mut buf = [0u8; 8];
    convert_zero_padded_hex_safe(&mut buf, 0x1fu32);
    assert_eq!(&buf, b"0000001f");
    let mut buf = [0u8; 4];
    convert_zero_padded_hex_safe(&mut buf, 0xbeefu16);
    assert_eq!(&buf, b"beef");

    // The size argument is the type's digit capacity (three per byte).
    let mut buf = [0u8; 48];
    let n = convert_decimal_safe(&mut buf, 4 * 3, 1234567u32);
    assert_eq!(&buf[..n], b"1234567");
    let n = convert_decimal_safe(&mut buf, 8 * 3, 0u64);
    assert_eq!(&buf[..n], b"0");
    let n = convert_decimal_safe(&mut buf, 3, 255u8);
    assert_eq!(&buf[..n], b"255");

    // The printers write to standard error; they only need not fail.
    print_safe(b"");
    print_safe(b"cov_misc print_safe\n");
    print_zero_padded_hex_safe(0x2au8);
    print_zero_padded_hex_safe(0xdead_beefu64);
    print_decimal_safe(42u32);
    print_decimal_safe(0usize);
    print_safe(b"\n");
}

#[test]
fn indirect_comparisons_handle_null_pointers() {
    let one = Some(Box::new(1));
    let one_again = Some(Box::new(1));
    let two = Some(Box::new(2));
    let none: Option<Box<i32>> = None;

    let eq = IndirectEqualTo::new(|a: &i32, b: &i32| a == b);
    assert!(eq.call(&one, &one_again));
    assert!(!eq.call(&one, &two));
    assert!(eq.call(&none, &None));
    assert!(!eq.call(&one, &none));
    assert!(!eq.call(&none, &one));
    let deq = default_indirect_equal_to::<i32>();
    assert!(deq.call(&two, &Some(Box::new(2))));
    assert!(!deq.call(&none, &two));

    let less = IndirectLess::new(|a: &i32, b: &i32| a < b);
    assert!(less.call(&one, &two));
    assert!(!less.call(&two, &one));
    assert!(less.call(&none, &one));
    assert!(!less.call(&one, &none));
    assert!(!less.call(&none, &None));
    let dless = default_indirect_less::<i32>();
    assert!(dless.call(&one, &two));
    assert!(!dless.call(&two, &none));

    let h = IndirectHash::new(BuildHasherDefault::<DefaultHasher>::default());
    assert_eq!(h.call(&none), 0);
    let expected = BuildHasherDefault::<DefaultHasher>::default().hash_one(1i32);
    assert_eq!(h.call(&one), expected);
    assert_eq!(h.call(&one), h.call(&one_again));
}

#[test]
fn deferred_actions_run_once_unless_cancelled() {
    let log = Rc::new(std::cell::RefCell::new(Vec::new()));
    {
        let l = log.clone();
        let _a = defer(move || l.borrow_mut().push("plain"));
    }
    {
        let l = log.clone();
        let mut a = defer(move || l.borrow_mut().push("cancelled"));
        a.cancel();
    }
    assert_eq!(*log.borrow(), vec!["plain"]);

    type Boxed = Box<dyn FnOnce()>;
    let mk = |tag: &'static str| -> DeferredAction<Boxed> {
        let l = log.clone();
        DeferredAction::new(Box::new(move || l.borrow_mut().push(tag)) as Boxed)
    };
    {
        // Assigning runs the current action first and takes over the new one.
        let mut a = mk("first");
        a.assign(mk("second"));
        assert_eq!(log.borrow().last(), Some(&"first"));
        // Assigning over a cancelled action does not run it.
        let mut b = mk("never");
        b.cancel();
        b.assign(mk("third"));
        // Assigning a cancelled action keeps it cancelled.
        let mut c = mk("fourth");
        let mut cancelled = mk("dropped");
        cancelled.cancel();
        c.assign(cancelled);
    }
    assert_eq!(*log.borrow(), vec!["plain", "first", "fourth", "third", "second"]);
}

#[test]
fn iovec_lengths_add_up() {
    let mut a = [0u8; 3];
    let mut b = [0u8; 5];
    let iov = [
        Iovec { iov_base: a.as_mut_ptr() as *mut c_void, iov_len: a.len() },
        Iovec { iov_base: b.as_mut_ptr() as *mut c_void, iov_len: b.len() },
    ];
    assert_eq!(iovec_len(&iov), 8);
    // SAFETY: `iov` is a live array of two initialised elements.
    assert_eq!(unsafe { iovec_len_ptr(iov.as_ptr(), iov.len()) }, 8);
    // SAFETY: a zero-length range reads nothing.
    assert_eq!(unsafe { iovec_len_ptr(iov.as_ptr(), 0) }, 0);
}

#[test]
fn case_insensitive_hasher_hashes_lowered_bytes() {
    let mut h = CaseInsensitiveHasher::default();
    assert_eq!(h.finish(), 0);
    h.write(b"HeLLo");
    assert_eq!(h.finish(), std_hash_bytes_lowered(b"hello"));
    let mut h2 = CaseInsensitiveHasher::default();
    h2.write(b"hello");
    assert_eq!(h.finish(), h2.finish());
    h2.write_u64(99);
    assert_eq!(h2.finish(), 99);
}

#[test]
fn spinlock_try_lock_fails_while_held() {
    let l = Spinlock::new();
    assert!(l.try_lock());
    assert!(!l.try_lock());
    l.unlock();
    assert!(l.try_lock());
    l.unlock();
    {
        let _g = l.lock_guard();
        assert!(!l.try_lock());
    }
    assert!(l.try_lock());
    l.unlock();
}

#[test]
fn critical_alloc_sections_nest() {
    let _outer = ScopedCriticalAllocSection::default();
    {
        let _inner = ScopedCriticalAllocSection::new();
    }
}

struct KeepInts;
impl FilterClass<i32> for KeepInts {
    type Value = TrueType;
}
impl FilterClass<f64> for KeepInts {
    type Value = FalseType;
}

#[test]
fn tuple_helpers_untuple_and_filter_by_reference() {
    assert_eq!(untuple((5u8,)), 5);
    assert_eq!(untuple((String::from("x"),)), "x");
    let t = (1i32, 2.5f64, 3i32);
    let f = tuple_filter_by_type_ref::<KeepInts, _>(&t);
    assert_eq!(f, (1, 3));
    // The source tuple is untouched.
    assert_eq!(t, (1, 2.5, 3));
}

#[test]
fn capped_token_bucket_release_raises_the_ceiling() {
    type Tb = SharedTokenBucket<Milli, RoversCapped>;
    assert_eq!(Tb::IS_CAPPED, CappedRelease::Yes);
    let tb = Tb::new(1000, 100, 1, false);
    let r = RoversCapped::new(10);
    assert_eq!(r.head().load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(r.max_extra(0), 10);
    r.release(5);
    assert_eq!(r.max_extra(0), 15);

    // Grabbing past the head leaves a deficiency that releases and refunds pay back.
    let want = tb.grab(30);
    assert_eq!(want, 30);
    assert_eq!(tb.deficiency(want), 30);
    tb.release(30);
    tb.refund(30);
    assert_eq!(tb.deficiency(want), 0);

    tb.update_rate(5);
    assert_eq!(tb.rate(), 5);
    tb.update_rate(u64::MAX);
    assert_eq!(tb.rate(), Tb::max_rate());

    let a = RateResolution::<Milli>::new(2.0);
    #[allow(clippy::clone_on_copy)]
    let b = a.clone();
    assert!(a == b);
    assert!(a != RateResolution::<Milli>::new(3.0));
    assert!(a < RateResolution::<Milli>::new(3.0));
    assert_eq!(Tb::rate_cast(Duration::from_millis(7)).count().round(), 7.0);
}

struct Counted {
    closes: Rc<Cell<u32>>,
}

impl Closeable for Counted {
    fn close(&mut self) -> Future<()> {
        self.closes.set(self.closes.get() + 1);
        make_ready_future(())
    }
}

impl Stoppable for Counted {
    fn stop(&mut self) -> Future<()> {
        self.closes.set(self.closes.get() + 10);
        make_ready_future(())
    }
}

#[test]
fn deferred_close_assignment_closes_the_previous_object() {
    let a_count = Rc::new(Cell::new(0));
    let b_count = Rc::new(Cell::new(0));
    let mut a = Counted { closes: a_count.clone() };
    let mut b = Counted { closes: b_count.clone() };
    {
        let mut g = DeferredClose::new(&mut a);
        g.assign(DeferredClose::new(&mut b));
        assert_eq!(a_count.get(), 1);
        assert_eq!(b_count.get(), 0);
    }
    assert_eq!(a_count.get(), 1);
    assert_eq!(b_count.get(), 1);

    {
        // A closed guard is not closed again by assignment.
        let mut g = DeferredClose::new(&mut a);
        g.close_now();
        g.assign(DeferredClose::new(&mut b));
    }
    assert_eq!(a_count.get(), 2);
    assert_eq!(b_count.get(), 2);

    {
        let mut g = DeferredStop::new(&mut a);
        g.assign(DeferredStop::new(&mut b));
        assert_eq!(a_count.get(), 12);
    }
    assert_eq!(b_count.get(), 12);

    {
        let mut g = DeferredStop::new(&mut a);
        g.cancel();
        let mut other = DeferredStop::new(&mut b);
        other.cancel();
        g.assign(other);
    }
    assert_eq!((a_count.get(), b_count.get()), (12, 12));
}

#[test]
fn no_abort_scopes_restore_the_previous_setting() {
    use teseladb::engine::core::on_internal_error::set_abort_on_internal_error;
    use teseladb::engine::testing::on_internal_error::testing::ScopedNoAbortOnInternalError;

    let initial = set_abort_on_internal_error(true);
    {
        let _g = ScopedNoAbortOnInternalError::new();
        // Inside the scope internal errors do not abort.
        assert!(!set_abort_on_internal_error(false));
        {
            let _inner = ScopedNoAbortOnInternalError::default();
            assert!(!set_abort_on_internal_error(false));
        }
        assert!(!set_abort_on_internal_error(false));
    }
    assert!(set_abort_on_internal_error(initial));
}

#[test]
fn test_random_engines_are_deterministic() {
    use teseladb::engine::testing::random::testing::{
        DefaultRandomEngine, local_random, seed_local_random_engine,
    };

    let mut e = DefaultRandomEngine::default();
    assert_eq!(e, DefaultRandomEngine::new(DefaultRandomEngine::DEFAULT_SEED));
    assert_eq!(e.next(), 16807);
    assert_eq!(e.next(), 282_475_249);
    e.seed(1);
    assert_eq!(e.next(), 16807);
    // A zero seed (or a multiple of the modulus) is replaced by one.
    assert_eq!(DefaultRandomEngine::new(0), DefaultRandomEngine::new(1));
    assert_eq!(DefaultRandomEngine::new(2_147_483_647), DefaultRandomEngine::new(1));

    seed_local_random_engine(1);
    assert_eq!(local_random(), 16807);
    assert_eq!(local_random(), 282_475_249);
    seed_local_random_engine(1);
    assert_eq!(local_random(), 16807);
}
