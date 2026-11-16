//! Microbenchmarks of the engine's containers (`ChunkedFifo`, `CircularBuffer`) against
//! the standard `VecDeque`: erase from the front, clear, iterate and index.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/perf_tests.rs"]
mod perf_tests;

use std::collections::VecDeque;

use teseladb::engine::core::chunked_fifo::ChunkedFifo;
use teseladb::engine::core::circular_buffer::CircularBuffer;
use teseladb::engine::testing::perf_tests::perf_tests::{
    do_not_optimize, start_measuring_time, stop_measuring_time,
};

type TrivialElem = i32;

const BIG_SIZE: usize = 10000;
const SMALL_SIZE: usize = 3;

// test should do this many iterations, at least, inside the "measured region"
// to defray the cost of the start/stop measuring time pairs, which are expensive
const TOTAL_ITERS: usize = 10000;

#[derive(Clone)]
struct NontrivialElem {
    x: i32,
}

impl From<TrivialElem> for NontrivialElem {
    #[inline]
    fn from(x: i32) -> Self {
        let e = NontrivialElem { x };
        do_not_optimize(&e.x);
        e
    }
}

impl Drop for NontrivialElem {
    #[inline]
    fn drop(&mut self) {
        do_not_optimize(&self.x);
    }
}

/// What the benches need of a container.
trait Container<T>: Default {
    fn reserve(&mut self, _size: usize) {}
    fn push_back(&mut self, e: T);
    fn clear(&mut self);
    fn for_each(&self, f: impl FnMut(&T));
}

/// `c[i]`, for the containers with random access. `at` does not check the index where the
/// container has an unchecked form, so the loop measures only the access.
trait Indexed<T> {
    /// # Safety
    /// `i` must be less than the size of the container.
    unsafe fn at(&self, i: usize) -> &T;
}

/// Erases the first `n` elements.
trait EraseFront {
    fn erase_front(&mut self, n: usize);
}

impl<T> Container<T> for ChunkedFifo<T> {
    #[inline]
    fn reserve(&mut self, size: usize) {
        ChunkedFifo::reserve(self, size);
    }
    #[inline]
    fn push_back(&mut self, e: T) {
        ChunkedFifo::push_back(self, e);
    }
    #[inline]
    fn clear(&mut self) {
        ChunkedFifo::clear(self);
    }
    #[inline]
    fn for_each(&self, f: impl FnMut(&T)) {
        self.iter().for_each(f);
    }
}

impl<T> Container<T> for CircularBuffer<T> {
    #[inline]
    fn reserve(&mut self, size: usize) {
        CircularBuffer::reserve(self, size);
    }
    #[inline]
    fn push_back(&mut self, e: T) {
        CircularBuffer::push_back(self, e);
    }
    #[inline]
    fn clear(&mut self) {
        CircularBuffer::clear(self);
    }
    #[inline]
    fn for_each(&self, f: impl FnMut(&T)) {
        self.iter().for_each(f);
    }
}

impl<T> Indexed<T> for CircularBuffer<T> {
    #[inline]
    unsafe fn at(&self, i: usize) -> &T {
        // SAFETY: guaranteed by the caller.
        unsafe { self.get_unchecked(i) }
    }
}

impl<T> EraseFront for CircularBuffer<T> {
    #[inline]
    fn erase_front(&mut self, n: usize) {
        self.erase(self.begin(), self.begin() + n as isize);
    }
}

/// `reserve` is left as the default no-op, so the deque grows as it is filled.
impl<T> Container<T> for VecDeque<T> {
    #[inline]
    fn push_back(&mut self, e: T) {
        VecDeque::push_back(self, e);
    }
    #[inline]
    fn clear(&mut self) {
        VecDeque::clear(self);
    }
    #[inline]
    fn for_each(&self, f: impl FnMut(&T)) {
        self.iter().for_each(f);
    }
}

/// VecDeque has no unchecked access; its index stays checked.
impl<T> Indexed<T> for VecDeque<T> {
    #[inline]
    unsafe fn at(&self, i: usize) -> &T {
        &self[i]
    }
}

impl<T> EraseFront for VecDeque<T> {
    #[inline]
    fn erase_front(&mut self, n: usize) {
        self.drain(..n);
    }
}

fn make_n<C: Container<T>, T: From<TrivialElem>>(size: usize) -> C {
    do_not_optimize(&size);
    let mut c = C::default();
    c.reserve(size);
    for i in 0..size {
        let e = i as TrivialElem;
        c.push_back(T::from(e));
    }
    do_not_optimize(&c);
    c
}

/// The fixture of the container benchmarks; it holds no state.
#[derive(Default)]
#[allow(non_camel_case_types)]
pub struct container_perf {}

fn outer_loops(inner_size: usize) -> usize {
    TOTAL_ITERS / inner_size + 1
}

fn iteration_bench<C: Container<TrivialElem>>(size: usize) -> usize {
    let c = make_n::<C, TrivialElem>(size);
    let outer = outer_loops(size);
    start_measuring_time();
    for _ in 0..outer {
        c.for_each(do_not_optimize);
    }
    stop_measuring_time();
    outer * size
}

fn index_bench<C: Container<TrivialElem> + Indexed<TrivialElem>>(size: usize) -> usize {
    let c = make_n::<C, TrivialElem>(size);
    let outer = outer_loops(size);
    start_measuring_time();
    for _ in 0..outer {
        for i in 0..size {
            // SAFETY: make_n() pushed `size` elements.
            do_not_optimize(unsafe { c.at(i) });
        }
    }
    stop_measuring_time();
    size * outer
}

fn clear_bench<C: Container<T>, T: From<TrivialElem>>(size: usize) -> usize {
    let mut c = make_n::<C, T>(size);
    start_measuring_time();
    c.clear();
    stop_measuring_time();
    size
}

fn erase_bench<C: Container<T> + EraseFront, T: From<TrivialElem>>(size: usize) -> usize {
    let mut c = make_n::<C, T>(size);
    start_measuring_time();
    c.erase_front(size / 2);
    stop_measuring_time();
    do_not_optimize(&c);
    size / 2
}

fn erase_front_bench<T: From<TrivialElem>>(size: usize) -> usize {
    let mut c = make_n::<ChunkedFifo<T>, T>(size);
    start_measuring_time();
    c.pop_front_n(size / 2);
    stop_measuring_time();
    do_not_optimize(&c);
    size / 2
}

type Fifo<T> = ChunkedFifo<T>;
type Circ<T> = CircularBuffer<T>;
type StdDeque<T> = VecDeque<T>;

teseladb::perf_test_f!(container_perf, erase_trivial_chunked_fifo, |_s| -> usize {
    erase_front_bench::<TrivialElem>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, erase_trivial_circular_buffer, |_s| -> usize {
    erase_bench::<Circ<TrivialElem>, TrivialElem>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, erase_trivial_vec_deque, |_s| -> usize {
    erase_bench::<StdDeque<TrivialElem>, TrivialElem>(BIG_SIZE)
});

teseladb::perf_test_f!(container_perf, clear_nontrivial_chunked_fifo, |_s| -> usize {
    clear_bench::<Fifo<NontrivialElem>, NontrivialElem>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, clear_nontrivial_circular_buffer, |_s| -> usize {
    clear_bench::<Circ<NontrivialElem>, NontrivialElem>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, clear_nontrivial_vec_deque, |_s| -> usize {
    clear_bench::<StdDeque<NontrivialElem>, NontrivialElem>(BIG_SIZE)
});

teseladb::perf_test_f!(container_perf, clear_trivial_chunked_fifo, |_s| -> usize {
    clear_bench::<Fifo<TrivialElem>, TrivialElem>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, clear_trivial_circular_buffer, |_s| -> usize {
    clear_bench::<Circ<TrivialElem>, TrivialElem>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, clear_trivial_vec_deque, |_s| -> usize {
    clear_bench::<StdDeque<TrivialElem>, TrivialElem>(BIG_SIZE)
});

teseladb::perf_test_f!(container_perf, iter_big_chunked_fifo, |_s| -> usize {
    iteration_bench::<Fifo<TrivialElem>>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, iter_big_circular_buffer, |_s| -> usize {
    iteration_bench::<Circ<TrivialElem>>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, iter_big_vec_deque, |_s| -> usize {
    iteration_bench::<StdDeque<TrivialElem>>(BIG_SIZE)
});

teseladb::perf_test_f!(container_perf, index_big_circular_buffer, |_s| -> usize {
    index_bench::<Circ<TrivialElem>>(BIG_SIZE)
});
teseladb::perf_test_f!(container_perf, index_big_vec_deque, |_s| -> usize {
    index_bench::<StdDeque<TrivialElem>>(BIG_SIZE)
});

teseladb::perf_test_f!(container_perf, iter_small_chunked_fifo, |_s| -> usize {
    iteration_bench::<Fifo<TrivialElem>>(SMALL_SIZE)
});
teseladb::perf_test_f!(container_perf, iter_small_circular_buffer, |_s| -> usize {
    iteration_bench::<Circ<TrivialElem>>(SMALL_SIZE)
});
teseladb::perf_test_f!(container_perf, iter_small_vec_deque, |_s| -> usize {
    iteration_bench::<StdDeque<TrivialElem>>(SMALL_SIZE)
});

pub(crate) fn main() {
    erase_trivial_chunked_fifo::register();
    erase_trivial_circular_buffer::register();
    erase_trivial_vec_deque::register();
    clear_nontrivial_chunked_fifo::register();
    clear_nontrivial_circular_buffer::register();
    clear_nontrivial_vec_deque::register();
    clear_trivial_chunked_fifo::register();
    clear_trivial_circular_buffer::register();
    clear_trivial_vec_deque::register();
    iter_big_chunked_fifo::register();
    iter_big_circular_buffer::register();
    iter_big_vec_deque::register();
    index_big_circular_buffer::register();
    index_big_vec_deque::register();
    iter_small_chunked_fifo::register();
    iter_small_circular_buffer::register();
    iter_small_vec_deque::register();
    perf_tests::main();
}
