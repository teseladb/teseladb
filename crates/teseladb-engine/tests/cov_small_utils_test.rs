//! Tests of small helpers that need no reactor: the 64-bit Mersenne Twister and the
//! allocation sampler, the bit operations, the boolean class, the optimized optional, the
//! function input iterator, the printing helpers, the estimated histogram and the prefetch
//! hints.
#![cfg(target_os = "linux")]
#![allow(deprecated)]

use std::fmt;
use std::time::Duration;

use teseladb::engine::core::bitops::{
    count_leading_zeros, count_leading_zeros_ul, count_leading_zeros_ull, count_trailing_zeros,
    count_trailing_zeros_ul, count_trailing_zeros_ull, log2ceil, log2ceil_u32, log2ceil_u64,
    log2ceil_usize, log2floor, log2floor_u32, log2floor_u64, log2floor_usize,
};
use teseladb::engine::core::internal::estimated_histogram::{
    ApproximateExponentialHistogram, TimeEstimatedHistogram, base_estimated_histogram_merge,
    num_buckets, time_estimated_histogram_merge,
};
use teseladb::engine::core::prefetch::{
    builtin_prefetch, prefetch, prefetch_lines_n, prefetch_lines_n_loc, prefetch_loc, prefetch_n,
    prefetch_n_loc, prefetch_object, prefetch_object_loc, prefetch_objects, prefetch_objects_loc,
    prefetch_objects_n, prefetch_objects_n_loc, prefetcher, prefetchw, prefetchw_lines_n,
    prefetchw_lines_n_loc, prefetchw_loc, prefetchw_n, prefetchw_n_loc, prefetchw_object,
    prefetchw_object_loc, prefetchw_objects, prefetchw_objects_loc, prefetchw_objects_n,
    prefetchw_objects_n_loc,
};
use teseladb::engine::core::print::{fmt_print, format_separated, log, usecfmt};
use teseladb::engine::util::bool_class::BoolClass;
use teseladb::engine::util::function_input_iterator::{
    FunctionInputRange, make_function_input_iterator, make_function_input_iterator_default,
};
use teseladb::engine::util::optimized_optional::{OptimizableOptional, OptimizedOptional};
use teseladb::engine::util::sampler::{ExponentialDistribution, Mt19937_64, RandomDevice, Sampler};

#[test]
fn mersenne_twister_matches_the_reference_sequence() {
    // The 10000th output of the 64-bit Mersenne Twister with the default seed is fixed by the
    // generator's definition.
    let mut g = Mt19937_64::new(Mt19937_64::DEFAULT_SEED);
    let mut last = 0;
    for _ in 0..10000 {
        last = g.next_u64();
    }
    assert_eq!(last, 9_981_545_732_273_789_042);

    // Reseeding restarts the sequence.
    let first = Mt19937_64::new(42).next_u64();
    g.seed(42);
    assert_eq!(g.next_u64(), first);

    // Canonical values are in [0, 1).
    for _ in 0..1000 {
        let c = g.generate_canonical();
        assert!((0.0..1.0).contains(&c));
    }
}

#[test]
fn exponential_distribution_has_the_expected_mean() {
    let mut g = Mt19937_64::new(7);
    let d = ExponentialDistribution::new(0.01);
    let n = 20000;
    let mean = (0..n).map(|_| d.sample(&mut g)).sum::<f64>() / f64::from(n);
    assert!((mean - 100.0).abs() < 5.0, "mean {mean}");
    assert!((0..100).all(|_| d.sample(&mut g) >= 0.0));
}

#[test]
fn random_device_reads_the_kernel_generator() {
    let mut rd = RandomDevice::default();
    // Sixteen 32-bit draws are not all equal unless the device is broken.
    let draws: Vec<u32> = (0..16).map(|_| rd.call()).collect();
    assert!(draws.iter().any(|&d| d != draws[0]));
}

#[test]
fn sampler_off_never_samples() {
    let mut s = Sampler::default();
    assert_eq!(s.sampling_interval(), 0);
    for _ in 0..1000 {
        assert!(!s.maybe_sample(1 << 20));
    }
    assert!(!s.definitely_sample(1 << 20));
    assert_eq!(s.sample_size(100), 100);
}

#[test]
fn sampler_samples_about_once_per_interval() {
    let mut s = Sampler::new();
    s.set_sampling_interval(1000);
    assert_eq!(s.sampling_interval(), 1000);
    // Allocations smaller than the interval report the interval as their sampled size.
    assert_eq!(s.sample_size(10), 1000);
    assert_eq!(s.sample_size(5000), 5000);
    let mut samples = 0u32;
    for _ in 0..100_000 {
        if s.maybe_sample(10) && s.definitely_sample(10) {
            samples += 1;
        }
    }
    // 1,000,000 bytes allocated with one sample per 1000 bytes on average.
    assert!((700..1300).contains(&samples), "{samples} samples");

    // An allocation larger than the interval is sampled and the count moves past it.
    let mut big = 0u32;
    for _ in 0..100 {
        if s.maybe_sample(1 << 20) && s.definitely_sample(1 << 20) {
            big += 1;
        }
    }
    assert!(big >= 1);
    // A sampler that was not due declines.
    s.set_sampling_interval(1 << 40);
    assert!(!s.definitely_sample(1));
}

#[test]
fn sampling_can_be_paused_and_resumed() {
    let mut s = Sampler::new();
    s.set_sampling_interval(64);
    {
        // SAFETY: `s` outlives the guard and is not used while it is paused.
        let _paused = unsafe { Sampler::pause_sampling(&mut s) };
    }
    assert_eq!(s.sampling_interval(), 64);
    let p: *mut Sampler = &mut s;
    // SAFETY: `s` is live; it is read through the same pointer while paused.
    let guard = unsafe { Sampler::pause_sampling(p) };
    // SAFETY: as above.
    assert_eq!(unsafe { (*p).sampling_interval() }, 0);
    drop(guard);
    assert_eq!(s.sampling_interval(), 64);
}

#[test]
fn bit_counts_and_logarithms() {
    assert_eq!(count_leading_zeros(1), 31);
    assert_eq!(count_leading_zeros_ul(1), 63);
    assert_eq!(count_leading_zeros_ull(1 << 40), 23);
    assert_eq!(count_trailing_zeros(8), 3);
    assert_eq!(count_trailing_zeros_ul(1 << 33), 33);
    assert_eq!(count_trailing_zeros_ull(6), 1);

    for (n, ceil, floor) in
        [(1u64, 0, 0), (2, 1, 1), (3, 2, 1), (8, 3, 3), (9, 4, 3), (1 << 40, 40, 40)]
    {
        assert_eq!(log2ceil(n), ceil, "log2ceil({n})");
        assert_eq!(log2floor(n), floor, "log2floor({n})");
        assert_eq!(log2ceil_u64(n), ceil);
        assert_eq!(log2floor_u64(n), floor);
        assert_eq!(log2ceil_usize(n as usize), ceil);
        assert_eq!(log2floor_usize(n as usize), floor);
        assert_eq!(log2ceil(n as usize), ceil);
        assert_eq!(log2floor(n as u128), floor);
    }
    assert_eq!(log2ceil(5u8), 3);
    assert_eq!(log2floor(200u8), 7);
    assert_eq!(log2ceil(300u16), 9);
    assert_eq!(log2floor(300u16), 8);
    assert_eq!(log2ceil(1u32), 0);
    assert_eq!(log2ceil(17u32), 5);
    assert_eq!(log2ceil_u32(17), 5);
    assert_eq!(log2floor_u32(17), 4);
    assert_eq!(log2floor(17u32), 4);
}

struct Tag;
type Flag = BoolClass<Tag>;

#[test]
fn bool_class_behaves_as_a_boolean() {
    let yes = Flag::YES;
    let no = Flag::NO;
    assert!(yes.as_bool());
    assert!(!no.as_bool());
    assert_eq!(Flag::default(), no);
    assert_eq!(Flag::new(true), yes);
    let copy = yes;
    assert_eq!(copy.clone(), yes);
    assert_eq!(yes | no, yes);
    assert_eq!(no | no, no);
    assert_eq!(yes & no, no);
    assert_eq!(yes & yes, yes);
    assert_eq!(yes.or(no), yes);
    assert_eq!(yes.and(no), no);
    assert_eq!(!yes, no);
    assert!(bool::from(yes));
    assert_eq!(format!("{yes} {no}"), "true false");
    assert_eq!(format!("{yes:?}"), "true");
}

#[derive(Clone, Default, PartialEq, Debug)]
struct MaybeName(String);

impl OptimizableOptional for MaybeName {
    fn as_bool(&self) -> bool {
        !self.0.is_empty()
    }
}

impl fmt::Display for MaybeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[test]
fn optimized_optional_uses_the_empty_value_as_none() {
    let mut o = OptimizedOptional::<MaybeName>::nullopt();
    assert!(!o.as_bool());
    assert_eq!(o.to_string(), "null");
    o.assign(MaybeName("x".into()));
    assert!(o.as_bool());
    assert_eq!(o.to_string(), "x");
    assert_eq!(o.0, "x");
    o.0.push('y');
    assert_eq!(*o, MaybeName("xy".into()));
    o.reset();
    assert!(!o.as_bool());

    let some = OptimizedOptional::from_option(Some(MaybeName("a".into())));
    assert!(some.as_bool());
    let none = OptimizedOptional::<MaybeName>::from_option(None);
    assert!(!none.as_bool());
    assert!(some == OptimizedOptional::new(MaybeName("a".into())));
    assert!(none == OptimizedOptional::default());
    let from: OptimizedOptional<MaybeName> = MaybeName("b".into()).into();
    assert!(from.as_bool());
}

#[test]
fn function_input_iterator_counts_states_and_calls_the_function() {
    let calls = std::cell::Cell::new(0);
    let f = || {
        calls.set(calls.get() + 1);
        calls.get()
    };
    let mut it = make_function_input_iterator(f, 0u32);
    assert_eq!(it.deref(), 1);
    it.inc();
    assert_eq!(it._state, 1);
    let before = it.post_inc();
    assert_eq!(before._state, 1);
    assert_eq!(it._state, 2);
    assert!(before != it);

    let seven: fn() -> i32 = || 7;
    let range = FunctionInputRange {
        begin: make_function_input_iterator(seven, 0usize),
        end: make_function_input_iterator(seven, 3usize),
    };
    assert_eq!(range.collect::<Vec<_>>(), vec![7, 7, 7]);
    let d = make_function_input_iterator_default::<_, _, i64>(|| "v");
    assert_eq!(d._state, 0);
    assert_eq!(d.deref(), "v");
}

#[test]
fn printing_helpers_format_their_input() {
    assert_eq!(format_separated(["a", "b", "c"], ", "), "a, b, c");
    assert_eq!(format_separated(Vec::<String>::new(), ","), "");
    assert_eq!(format_separated(["only"], ","), "only");
    assert_eq!(usecfmt(Duration::from_micros(3_000_042)).to_string(), "           3.000042");
    let since = usecfmt(std::time::Instant::now()).to_string();
    assert_eq!(since.len(), 19, "{since:?}");
    assert!(since.trim_start().contains('.'));
    let mut out = Vec::new();
    fmt_print(&mut out, format_args!("{}-{}", 1, 2));
    assert_eq!(out, b"1-2");
    log(format_args!("printing helper test\n"));
}

type SmallHistogram =
    ApproximateExponentialHistogram<512, 33554432, 4, { num_buckets(512, 33554432, 4) }>;

#[test]
fn estimated_histogram_buckets_values_by_magnitude() {
    assert_eq!(num_buckets(512, 33554432, 4), 65);
    let mut h = SmallHistogram::new();
    assert_eq!(h.size(), 65);
    assert_eq!(h.count(), 0);
    assert_eq!(h.min(), 0);
    assert_eq!(h.max(), 0);
    assert_eq!(h.mean(), 0);
    assert_eq!(h.quantile(0.5).unwrap(), 0);
    assert!(h.quantile(1.5).is_err());
    assert!(h.quantile(-0.1).is_err());

    // Four buckets per power of two: 512, 640, 768, 896, 1024, ...
    assert_eq!(h.get_bucket_lower_limit(0), 512);
    assert_eq!(h.get_bucket_lower_limit(1), 640);
    assert_eq!(h.get_bucket_lower_limit(4), 1024);
    assert_eq!(h.get_bucket_upper_limit(3), 1024);
    assert_eq!(h.get_bucket_lower_limit(64), 33554432);
    assert_eq!(h.get_bucket_upper_limit(64), u64::MAX);
    assert_eq!(h.find_bucket_index(1), 0);
    assert_eq!(h.find_bucket_index(1000), 3);
    assert_eq!(h.find_bucket_index(1 << 30), 64);

    h.add(1000);
    h.add(1000);
    h.add(5000);
    h.add(1 << 30);
    assert_eq!(h.count(), 4);
    assert_eq!(h[3], 2);
    assert_eq!(h.get(3), 2);
    assert_eq!(h.min(), 896);
    assert_eq!(h.max(), u64::MAX);
    assert_eq!(h.quantile(0.5).unwrap(), 896);
    assert_eq!(h.quantile(0.75).unwrap(), h.get_bucket_lower_limit(h.find_bucket_index(5000)));
    assert_eq!(h.quantile(1.0).unwrap(), 33554432);
    // The mean leaves out the overflow bucket and uses the lower limits.
    let low = h.get_bucket_lower_limit(h.find_bucket_index(5000));
    assert_eq!(h.mean(), (2 * 896 + low) / 3);

    let hist = h.to_metrics_histogram();
    assert_eq!(hist.buckets.len(), 64);
    assert_eq!(hist.sample_count, 4);
    assert_eq!(hist.buckets[3].count, 2);
    assert_eq!(hist.buckets[3].upper_bound, 1024.0);
    assert_eq!(hist.buckets[63].count, 3);
    let native = hist.native_histogram.unwrap();
    assert_eq!(native.schema, 2);
    assert_eq!(native.min_id, h.min_as_native_histogram_id());
    assert_eq!(h.get_schema(), 2);

    h[0] = 10;
    assert_eq!(h.min(), 512);
    h *= 0.5;
    assert_eq!(h[0], 5);
    assert_eq!(h[3], 1);

    let merged = base_estimated_histogram_merge(h, &h);
    assert_eq!(merged[0], 10);
    h.clear();
    assert_eq!(h.count(), 0);
}

#[test]
fn time_histogram_counts_microseconds() {
    let mut a = TimeEstimatedHistogram::new();
    a.add(Duration::from_millis(1));
    a.add_micro(700);
    let mut b = TimeEstimatedHistogram::default();
    b.add(Duration::from_secs(100));
    let m = time_estimated_histogram_merge(a, &b);
    assert_eq!(m.count(), 3);
    assert_eq!(m.max(), u64::MAX);
    a.merge(&b);
    assert_eq!(a, m);
    a.clear();
    assert_eq!(a.count(), 0);
}

#[test]
fn prefetch_hints_leave_memory_unchanged() {
    let data: Vec<u64> = (0..64).collect();
    let ptrs: Vec<*const u64> = data.iter().map(|x| x as *const u64).collect();
    let p = data.as_ptr();
    let b = p as *const u8;
    let bytes: Vec<*const u8> = ptrs.iter().map(|&x| x as *const u8).collect();
    prefetch::<128>(b);
    prefetchw::<128>(b);
    prefetch_loc::<64, 1>(b);
    prefetchw_loc::<64, 2>(b);
    prefetch_n::<64, _>(bytes.iter().copied());
    prefetch_n_loc::<64, _, 0>(bytes.iter().copied());
    prefetchw_n::<64, _>(bytes.iter().copied());
    prefetchw_n_loc::<64, _, 1>(bytes.iter().copied());
    builtin_prefetch::<0, 3>(b);
    builtin_prefetch::<1, 3>(b);
    prefetcher::<128, 0, 3>(p as usize);
    prefetcher::<64, 1, 0>(p as usize);
    prefetcher::<64, 1, 1>(p as usize);
    prefetcher::<64, 1, 2>(p as usize);
    prefetcher::<64, 0, 0>(p as usize);
    prefetcher::<64, 0, 1>(p as usize);
    prefetcher::<64, 0, 2>(p as usize);
    prefetch_object(p);
    prefetch_object_loc::<_, 1>(p);
    prefetch_objects(ptrs.iter().copied());
    prefetch_objects_loc::<_, _, 2>(ptrs.iter().copied());
    prefetchw_object(p);
    prefetchw_object_loc::<_, 0>(p);
    prefetchw_objects(ptrs.iter().copied());
    prefetchw_objects_loc::<_, _, 3>(ptrs.iter().copied());
    // SAFETY: `ptrs` holds at least the 4 pointers read, each to a live value.
    unsafe {
        prefetch_objects_n::<4, u64>(ptrs.as_ptr());
        prefetch_objects_n_loc::<4, u64, 1>(ptrs.as_ptr());
        prefetch_lines_n::<2, 4, u64>(ptrs.as_ptr());
        prefetch_lines_n_loc::<2, 4, u64, 2>(ptrs.as_ptr());
        prefetchw_objects_n::<4, u64>(ptrs.as_ptr());
        prefetchw_objects_n_loc::<4, u64, 0>(ptrs.as_ptr());
        prefetchw_lines_n::<2, 4, u64>(ptrs.as_ptr());
        prefetchw_lines_n_loc::<2, 4, u64, 1>(ptrs.as_ptr());
    }
    assert_eq!(data, (0..64).collect::<Vec<u64>>());
}
