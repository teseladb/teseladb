//! A randomized stress of the engine allocator: allocations of exponentially distributed
//! sizes, filled with a poison byte, verified and freed in random order, plus aligned
//! allocations of every power-of-two alignment. A program with its own `main`
//! (`harness = false`).

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/minstd.rs"]
mod minstd;

use std::alloc::{Layout, alloc, dealloc};
use std::time::{Duration, Instant};

use minstd::{DefaultRandomEngine, UniformIntDistribution};
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::testing::random::testing::local_random;

/// A uniform draw in [0, 1): one draw of the engine, scaled.
fn uniform_real01(g: &mut DefaultRandomEngine) -> f64 {
    let r = (g.next() - DefaultRandomEngine::MIN) as f64;
    let range = (DefaultRandomEngine::MAX - DefaultRandomEngine::MIN) as f64 + 1.0;
    let v = r / range;
    if v >= 1.0 { f64::from_bits(1.0f64.to_bits() - 1) } else { v }
}

/// A draw from the exponential distribution of rate `lambda`.
fn exponential(g: &mut DefaultRandomEngine, lambda: f64) -> f64 {
    -(1.0 - uniform_real01(g)).ln() / lambda
}

struct Allocation {
    n: usize,
    data: Option<Box<[u8]>>,
    poison: u8,
}

impl Allocation {
    fn new(n: usize, poison: u8) -> Self {
        Allocation { n, data: Some(vec![poison; n].into_boxed_slice()), poison }
    }

    fn verify(&self) {
        if let Some(data) = &self.data {
            assert!(data[..self.n].iter().all(|&c| c == self.poison));
        }
    }

    /// Verifies and frees this allocation, then takes over `x`.
    fn assign(&mut self, mut x: Allocation) {
        self.verify();
        self.data = x.data.take();
        self.n = x.n;
        self.poison = x.poison;
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        self.verify();
    }
}

/// An allocator of objects aligned (and sized) to a fixed power of two.
trait Test17 {
    fn alloc(&self) -> Handle<'_>;
    fn free(&self, ptr: *mut u8);
}

struct Handle<'a> {
    d: Option<&'a dyn Test17>,
    p: *mut u8,
}

impl Drop for Handle<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.d {
            d.free(self.p);
        }
    }
}

/// Allocates zero-filled objects of size and alignment N.
struct Test17Concrete<const N: usize>;

impl<const N: usize> Test17Concrete<N> {
    const LAYOUT: Layout = match Layout::from_size_align(N, N) {
        Ok(l) => l,
        Err(_) => panic!("language does not guarantee size >= align"),
    };
}

impl<const N: usize> Test17 for Test17Concrete<N> {
    fn alloc(&self) -> Handle<'_> {
        // SAFETY: the layout has a nonzero size.
        let ptr = unsafe { alloc(Self::LAYOUT) };
        assert!(!ptr.is_null());
        // zero-initialised
        // SAFETY: `ptr` points at N writable bytes.
        unsafe { ptr.write_bytes(0, N) };
        assert!((ptr as usize & (N - 1)) == 0);
        Handle { d: Some(self), p: ptr }
    }

    fn free(&self, ptr: *mut u8) {
        // SAFETY: `ptr` was allocated by alloc() with the same layout.
        unsafe { dealloc(ptr, Self::LAYOUT) };
    }
}

fn test_cpp17_aligned_allocator() {
    let tv: Vec<Box<dyn Test17>> = vec![
        Box::new(Test17Concrete::<1>),
        Box::new(Test17Concrete::<2>),
        Box::new(Test17Concrete::<4>),
        Box::new(Test17Concrete::<8>),
        Box::new(Test17Concrete::<16>),
        Box::new(Test17Concrete::<64>),
        Box::new(Test17Concrete::<128>),
        Box::new(Test17Concrete::<2048>),
        Box::new(Test17Concrete::<4096>),
        Box::new(Test17Concrete::<{ 4096 * 16 }>),
        Box::new(Test17Concrete::<{ 4096 * 256 }>),
    ];
    let mut random_engine = DefaultRandomEngine::new(u64::from(local_random()));
    let type_dist = UniformIntDistribution::new(0, 1);
    let size_dist = UniformIntDistribution::new(0, tv.len() as u64 - 1);
    let mut allocs: Vec<Handle<'_>> = Vec::new();
    for _ in 0..10000 {
        let type_ = type_dist.sample(&mut random_engine);
        match type_ {
            0 => {
                let sz_idx = size_dist.sample(&mut random_engine) as usize;
                allocs.push(tv[sz_idx].alloc());
            }
            1 if !allocs.is_empty() => {
                let idx = (uniform_real01(&mut random_engine) * allocs.len() as f64) as usize;
                let last = allocs.len() - 1;
                allocs.swap(idx, last);
                allocs.pop();
            }
            _ => {}
        }
    }
}

pub(crate) fn main() {
    let mut opts = bpo::OptionsDescription::new("Allowed options");
    opts.add_options()
        .flag("help", "produce this help message")
        .opt("iterations", bpo::value::<u32>(), "run s specified number of iterations")
        .opt(
            "time",
            bpo::value::<f32>().default_value(5.0),
            "run for a specified amount of time, in seconds",
        )
        .opt("random-seed", bpo::value::<u32>(), "Random number generator seed");
    let mut vm = bpo::VariablesMap::default();
    // The parser skips the program name in `args[0]`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    bpo::store(bpo::command_line_parser(args).options(&opts).run().expect("parse"), &mut vm)
        .expect("store");
    bpo::notify(&mut vm).expect("notify");

    test_cpp17_aligned_allocator();

    let seed: u64 = match vm.get::<u32>("random-seed") {
        Some(s) => u64::from(*s),
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1),
    };
    let mut random_engine = DefaultRandomEngine::new(seed);
    let type_ = UniformIntDistribution::new(0, 1);
    let poison = UniformIntDistribution::new(0, 255); // -128..127 as a char
    let mut allocations: Vec<Allocation> = Vec::new();

    let mut iteration = || {
        let typ = type_.sample(&mut random_engine);
        match typ {
            0 => {
                let n =
                    exponential(&mut random_engine, 0.2).exp().min(f64::from(1u32 << 25)) as usize;
                // A failed allocation of a Vec aborts; try_reserve lets the test tolerate
                // running out of memory instead.
                if allocations.try_reserve(1).is_ok() {
                    let p = poison.sample(&mut random_engine) as u8;
                    allocations.push(Allocation::new(n, p));
                }
            }
            1 => {
                if allocations.is_empty() {
                    return;
                }
                let i = (uniform_real01(&mut random_engine) * allocations.len() as f64) as usize;
                let back = allocations.pop().expect("not empty");
                if i < allocations.len() {
                    allocations[i].assign(back);
                } else {
                    // Moving the last element onto itself only verifies it.
                    back.verify();
                }
            }
            _ => {}
        }
    };

    if vm.find("help").is_some() {
        println!("{}", opts);
        std::process::exit(1);
    }
    println!("random-seed={}", seed);
    if let Some(iterations) = vm.get::<u32>("iterations") {
        for _ in 0..*iterations {
            iteration();
        }
    } else {
        let time = *vm.get::<f32>("time").expect("default");
        let end = Instant::now() + Duration::from_secs_f64(f64::from(time));
        while Instant::now() < end {
            for _ in 0..1000 {
                iteration();
            }
        }
    }
}
