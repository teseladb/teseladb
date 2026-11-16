//! Tests of the small core modules: internal error reporting without aborting, the printing
//! helpers, structures with a trailing array, size units, unaligned values, two-pass
//! relocation, histogram arithmetic, enum hashing, formatting into an `Sstring`, the
//! fixed-size array map, the timeout error, temporary buffers from a custom allocator, aligned
//! buffers, the default reactor configuration, and the file-system AIO check.
#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::time::Duration;

use teseladb::engine::core::aligned_buffer::{FreeDeleter, allocate_aligned_buffer};
use teseladb::engine::core::array_map::ArrayMap;
use teseladb::engine::core::enum_::{EnumHash, UnderlyingEnum};
use teseladb::engine::core::format::format;
use teseladb::engine::core::fsqual::filesystem_has_good_aio_support;
use teseladb::engine::core::future::{ExceptionPtr, OutOfRange, RuntimeError};
use teseladb::engine::core::memory::MALLOC_ALLOCATOR;
use teseladb::engine::core::metrics_types::{Histogram, HistogramBucket};
use teseladb::engine::core::on_internal_error::{
    on_internal_error, on_internal_error_ex, on_internal_error_noexcept,
    set_abort_on_internal_error,
};
use teseladb::engine::core::polymorphic_temporary_buffer::{
    PolymorphicAllocator, make_temporary_buffer,
};
use teseladb::engine::core::print::{log, usecfmt};
use teseladb::engine::core::reactor::engine_logger;
use teseladb::engine::core::reactor_config::ReactorConfig;
use teseladb::engine::core::timed_out_error::{DefaultTimeoutExceptionFactory, TimedOutError};
use teseladb::engine::core::transfer::{transfer_pass1, transfer_pass2};
use teseladb::engine::core::unaligned::Unaligned;
use teseladb::engine::core::units::{gib, kib, mib, tib};
use teseladb::engine::core::vla::make_struct_with_vla;
use teseladb::format_sstring;

#[test]
fn internal_errors_are_reported_without_aborting() {
    let before = set_abort_on_internal_error(false);
    let e = on_internal_error(engine_logger(), "cov internal error");
    assert!(e.to_string().contains("cov internal error"), "{e}");
    let passed = ExceptionPtr::new(RuntimeError("passed through".into()));
    assert!(on_internal_error_ex(engine_logger(), passed.clone()) == passed);
    on_internal_error_noexcept(engine_logger(), "cov noexcept error");
    assert!(!set_abort_on_internal_error(before), "the switch was off");
}

#[test]
#[allow(deprecated)]
fn printing_helpers_join_and_time_stamp() {
    use teseladb::engine::core::print::{fmt_print, format_separated};
    assert_eq!(format_separated(["a", "b", "c"], ", "), "a, b, c");
    assert_eq!(format_separated(Vec::<String>::new(), ","), "");
    assert_eq!(format_separated(["only"], "-"), "only");
    assert_eq!(usecfmt(Duration::from_micros(3_000_042)).to_string(), "           3.000042");
    let first = usecfmt(std::time::Instant::now()).to_string();
    assert!(first.trim_start().contains('.'), "{first}");
    log(format_args!("cov log line {}\n", 1));
    let mut out = Vec::new();
    fmt_print(&mut out, format_args!("{}-{}", 1, 2)).extend_from_slice(b"!");
    assert_eq!(out, b"1-2!");
}

#[derive(Default)]
#[repr(C)]
struct Header {
    len: u32,
}

#[test]
fn trailing_arrays_units_and_unaligned_values() {
    let mut v = make_struct_with_vla::<Header, u64>(std::mem::size_of::<Header>(), 4);
    assert_eq!(v.len, 0, "the header is default-initialized");
    v.len = 4;
    assert!(!v.get().is_null());
    assert_eq!(v.len, 4);

    assert_eq!(kib(2), 2048);
    assert_eq!(mib(3), 3 << 20);
    assert_eq!(gib(1), 1 << 30);
    assert_eq!(tib(1), 1 << 40);

    let mut u = Unaligned::new(5u32);
    assert_eq!(u.get(), 5);
    u.set(9);
    assert_eq!(u.raw, 9);
    let from: Unaligned<u16> = 7u16.into();
    assert_eq!(from.get(), 7);
    let mut bytes = [0u8; 9];
    #[allow(deprecated)]
    let p = teseladb::engine::core::unaligned::unaligned_cast::<u64, u8>(
        bytes.as_mut_ptr().wrapping_add(1),
    );
    // SAFETY: the cast keeps the pointer into `bytes`, which has 8 bytes after the first;
    // unaligned writes and reads are used.
    unsafe {
        std::ptr::write_unaligned(p, Unaligned::new(u64::MAX));
        assert_eq!(std::ptr::read_unaligned(p).get(), u64::MAX);
    }
    #[allow(deprecated)]
    let c = teseladb::engine::core::unaligned::unaligned_cast_const::<u64, u8>(
        bytes.as_ptr().wrapping_add(1),
    );
    // SAFETY: as above.
    assert_eq!(unsafe { std::ptr::read_unaligned(c) }.get(), u64::MAX);

    let mut from = std::mem::MaybeUninit::new(String::from("moved"));
    let mut to = std::mem::MaybeUninit::<String>::uninit();
    // SAFETY: `from` is initialized, `to` is writable, and they do not overlap; `from` is not
    // read again.
    unsafe {
        transfer_pass1(from.as_mut_ptr(), to.as_mut_ptr());
        transfer_pass2(from.as_mut_ptr(), to.as_mut_ptr());
        assert_eq!(to.assume_init(), "moved");
    }
}

fn hist(counts: &[u64], bounds: &[f64], sum: f64) -> Histogram {
    Histogram {
        sample_count: counts.iter().sum(),
        sample_sum: sum,
        buckets: counts
            .iter()
            .zip(bounds)
            .map(|(&count, &upper_bound)| HistogramBucket { count, upper_bound })
            .collect(),
        native_histogram: None,
    }
}

#[test]
fn histograms_add_when_their_buckets_match() {
    let a = hist(&[1, 2], &[1.0, 2.0], 3.0);
    let b = hist(&[3, 4, 5], &[1.0, 2.0, 4.0], 30.0);
    let sum = a.add(&b).unwrap();
    assert_eq!(sum.sample_count, 15);
    assert_eq!(sum.sample_sum, 33.0);
    assert_eq!(sum.buckets.iter().map(|b| b.count).collect::<Vec<_>>(), [4, 6, 5]);
    let owned = a.add_owned(b.clone()).unwrap();
    assert_eq!(owned, sum.clone().add(&Histogram::default()).unwrap(), "adding nothing");
    let other = hist(&[1], &[9.0], 1.0);
    let e = a.add(&other).unwrap_err();
    assert!(e.is::<OutOfRange>());
    let mut acc = Histogram::default();
    acc.add_assign(&a).unwrap().add_assign(&a).unwrap();
    assert_eq!(acc.sample_count, 6);
}

#[derive(Clone, Copy, Default)]
enum Color {
    #[default]
    Red = 1,
    Blue = 2,
}

impl UnderlyingEnum for Color {
    type Underlying = u8;
    fn to_underlying(self) -> u8 {
        self as u8
    }
}

#[test]
fn enums_hash_by_their_value_and_maps_check_keys() {
    let h = EnumHash::<Color>::default();
    assert_eq!(h.hash(&Color::Red), h.hash(&Color::Red));
    assert_ne!(h.hash(&Color::Red), h.hash(&Color::Blue));

    let s = format(format_args!("{}+{}", 1, 2));
    assert_eq!(s.as_str(), "1+2");
    assert_eq!(format_sstring!("{:>3}", 7).as_str(), "  7");

    let mut m = ArrayMap::<u32, 4>::new([(1, 10), (3, 30)]);
    assert_eq!((m[0], m[1], m[3]), (0, 10, 30));
    m[2] = 20;
    *m.at(0).unwrap() = 5;
    assert_eq!((m[0], m[2]), (5, 20));
    let e = m.at(4).unwrap_err();
    assert!(e.to_string().contains("4 >= 4"), "{e}");

    assert_eq!(TimedOutError::new().what(), "timedout");
    assert_eq!(DefaultTimeoutExceptionFactory::timeout().to_string(), "timedout");
}

/// An allocator that counts its calls.
struct Counting {
    allocs: Cell<u32>,
    frees: Cell<u32>,
}

// SAFETY: the tests use the allocator from one thread only.
unsafe impl Sync for Counting {}

impl PolymorphicAllocator for Counting {
    fn allocate(&self, n: usize) -> *mut u8 {
        self.allocs.set(self.allocs.get() + 1);
        MALLOC_ALLOCATOR.allocate(n)
    }
    unsafe fn deallocate(&self, p: *mut u8, n: usize) {
        self.frees.set(self.frees.get() + 1);
        // SAFETY: `p` came from `allocate(n)` above.
        unsafe { MALLOC_ALLOCATOR.deallocate(p, n) }
    }
}

static COUNTING: Counting = Counting { allocs: Cell::new(0), frees: Cell::new(0) };

#[test]
fn temporary_and_aligned_buffers_from_allocators() {
    let plain = make_temporary_buffer(MALLOC_ALLOCATOR, 32);
    assert_eq!(plain.size(), 32);
    {
        let mut b = make_temporary_buffer(&COUNTING, 100);
        assert_eq!(b.size(), 100);
        // SAFETY: the buffer holds 100 writable bytes.
        unsafe { std::ptr::write_bytes(b.get_write(), 1, 100) };
        let share = b.share();
        drop(b);
        assert_eq!(COUNTING.frees.get(), 0, "a share keeps the memory");
        drop(share);
    }
    assert_eq!((COUNTING.allocs.get(), COUNTING.frees.get()), (1, 1));

    let a = allocate_aligned_buffer(1000, 512).unwrap();
    assert_eq!(a.get() as usize % 512, 0);
    drop(a);
    let raw = allocate_aligned_buffer(64, 64).unwrap().release();
    assert!(!raw.is_null());
    // SAFETY: the released buffer came from the malloc family and is freed once.
    unsafe { FreeDeleter.call(raw.cast()) };

    let cfg = ReactorConfig::default();
    assert!(cfg.handle_sigint && cfg.strict_o_direct && !cfg.bypass_fsync);
    assert_eq!(cfg.max_task_backlog, 1000);
    assert_eq!(cfg.max_networking_aio_io_control_blocks, 10000);
}

#[test]
fn filesystem_aio_check_runs_in_a_directory() {
    let dir = std::env::temp_dir().join(format!("cov_fsqual_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The answer depends on the file system; the check itself must complete.
    let r = filesystem_has_good_aio_support(&dir.display().to_string(), true);
    std::fs::remove_dir_all(&dir).unwrap();
    // A file system without direct I/O fails the check with the reason.
    if let Err(e) = r {
        assert!(!e.to_string().is_empty());
    }
    assert!(filesystem_has_good_aio_support("/no/such/cov/dir", false).is_err());
}
