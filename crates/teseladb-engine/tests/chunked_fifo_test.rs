//! Tests of `ChunkedFifo`: pushes and pops within and across chunks, `pop_n`, constructor
//! and destructor calls, copies and moves, failing constructors, equality, iterators, and
//! optional benchmarks. Plain `#[test]` cases, with no reactor.

#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;

use teseladb::engine::core::chunked_fifo::{ChunkedFifo, Iter};

// Compile-time checks: both iterators are iterators, and the fifo is a range.
const _: () = {
    fn assert_iterator<I: Iterator>() {}
    fn assert_range<R>()
    where
        for<'a> &'a R: IntoIterator,
    {
    }
    fn checks() {
        assert_iterator::<Iter<'static, i32, 128>>();
        assert_iterator::<teseladb::engine::core::chunked_fifo::IterMut<'static, i32, 128>>();
        assert_range::<ChunkedFifo<i32>>();
    }
    let _ = checks;
};

#[test]
fn chunked_fifo_small() {
    // Check all the methods of chunked_fifo but with a trivial type (int) and
    // only a few elements - and in particular a single chunk is enough.
    let mut fifo: ChunkedFifo<i32> = ChunkedFifo::new();
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
    fifo.push_back(3);
    assert_eq!(fifo.size(), 1usize);
    assert!(!fifo.empty());
    assert_eq!(*fifo.front(), 3);
    fifo.push_back(17);
    assert_eq!(fifo.size(), 2usize);
    assert!(!fifo.empty());
    assert_eq!(*fifo.front(), 3);
    fifo.pop_front();
    assert_eq!(fifo.size(), 1usize);
    assert!(!fifo.empty());
    assert_eq!(*fifo.front(), 17);
    fifo.pop_front();
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
    // The previously allocated chunk should have been freed, and now
    // a new one will need to be allocated:
    fifo.push_back(57);
    assert_eq!(fifo.size(), 1usize);
    assert!(!fifo.empty());
    assert_eq!(*fifo.front(), 57);
    // check miscelleneous methods (at least they shouldn't crash)
    fifo.clear();
    fifo.shrink_to_fit();
    fifo.reserve(1);
    fifo.reserve(100);
    fifo.reserve(1280);
    fifo.shrink_to_fit();
    fifo.reserve(1280);
}

#[test]
fn chunked_fifo_fullchunk() {
    // Grow a chunked_fifo to exactly fill a chunk, and see what happens when
    // we cross that chunk.
    const N: usize = 128;
    let mut fifo: ChunkedFifo<i32, N> = ChunkedFifo::new();
    for i in 0..N as i32 {
        fifo.push_back(i);
    }
    assert_eq!(fifo.size(), N);
    fifo.push_back(N as i32);
    assert_eq!(fifo.size(), N + 1);
    for i in 0..(N + 1) as i32 {
        assert_eq!(*fifo.front(), i);
        assert_eq!(fifo.size(), N + 1 - i as usize);
        fifo.pop_front();
    }
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
}

/// The constructor, copy and destructor counts of the trackable values.
#[derive(Default)]
struct TrackableTotals {
    cons_called: Cell<usize>,
    dtor_called: Cell<usize>,
    mcons_called: Cell<usize>,
    ccons_called: Cell<usize>,
}

impl TrackableTotals {
    /// Sets every count to zero.
    fn reset(&self) {
        self.cons_called.set(0);
        self.dtor_called.set(0);
        self.mcons_called.set(0);
        self.ccons_called.set(0);
    }
}

fn bump(c: &Cell<usize>) {
    c.set(c.get() + 1);
}

/// A value that counts its constructions, copies and drops: a Rust move is a bitwise copy
/// that runs no code, so `mcons_called` stays at zero.
struct Trackable<'a> {
    tracker: &'a TrackableTotals,
}

impl<'a> Trackable<'a> {
    /// A new value counted in `tracker`.
    fn new(tracker: &'a TrackableTotals) -> Self {
        bump(&tracker.cons_called);
        Trackable { tracker }
    }
}

impl Clone for Trackable<'_> {
    /// A copy, counted as one.
    fn clone(&self) -> Self {
        bump(&self.tracker.ccons_called);
        Trackable { tracker: self.tracker }
    }
}

impl Drop for Trackable<'_> {
    fn drop(&mut self) {
        bump(&self.tracker.dtor_called);
    }
}

#[test]
fn chunked_fifo_pop_n() {
    let ctor_calls = TrackableTotals::default();
    const N: usize = 4;
    let mut fifo: ChunkedFifo<Trackable<'_>, { N / 2 }> = ChunkedFifo::new();
    // Ties the lifetime of the closure's fifo to `ctor_calls` (a closure argument with an
    // elided lifetime would accept any).
    fn tie<'a, F: Fn(&mut ChunkedFifo<Trackable<'a>, { N / 2 }>, usize)>(f: F) -> F {
        f
    }
    let fill_and_reset = tie(|fifo, size| {
        fifo.clear();
        ctor_calls.reset();
        for _ in 0..size {
            // we add 2, remove 1 in order to stress the case where chunk::begin and end
            // are outside the range [0, items_per_chunk], i.e., where proper use of mask()
            // is required
            fifo.push_back(Trackable::new(&ctor_calls));
            fifo.push_back(Trackable::new(&ctor_calls));
            fifo.pop_front_n(1);
        }
    });

    for size in 0..2 * N {
        for pop_count in 0..size + 1 {
            let ctx = format!("size: {}, pop_count: {}", size, pop_count);
            fill_and_reset(&mut fifo, size);
            // note that we add 2 and delete 1 element for every element added in
            // fill_and_reset so that affects the numbers below
            assert_eq!(fifo.size(), size, "{}", ctx);
            assert_eq!(ctor_calls.cons_called.get(), size * 2, "{}", ctx);
            assert_eq!(ctor_calls.dtor_called.get(), size, "{}", ctx);

            fifo.pop_front_n(pop_count);
            assert_eq!(fifo.size(), size - pop_count, "{}", ctx);
            assert_eq!(ctor_calls.cons_called.get(), size * 2, "{}", ctx);
            assert_eq!(ctor_calls.dtor_called.get(), size + pop_count, "{}", ctx);

            fifo.push_back(Trackable::new(&ctor_calls));
            assert_eq!(fifo.size(), size - pop_count + 1, "{}", ctx);
            assert_eq!(ctor_calls.cons_called.get(), size * 2 + 1, "{}", ctx);
            assert_eq!(ctor_calls.dtor_called.get(), size + pop_count, "{}", ctx);
        }
    }
}

#[test]
fn chunked_fifo_big() {
    // Grow a chunked_fifo to many elements, and see things are working as
    // expected
    let mut fifo: ChunkedFifo<i32> = ChunkedFifo::new();
    const N: usize = 100_000;
    for i in 0..N as i32 {
        fifo.push_back(i);
    }
    assert_eq!(fifo.size(), N);
    assert!(!fifo.empty());
    for i in 0..N as i32 {
        assert_eq!(*fifo.front(), i);
        assert_eq!(fifo.size(), N - i as usize);
        fifo.pop_front();
    }
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
}

#[test]
fn chunked_fifo_constructor() {
    // Check that chunked_fifo appropriately calls the type's constructor
    // and destructor, and doesn't need anything else.
    struct Typ<'a> {
        val: i32,
        #[allow(dead_code)]
        constructed: &'a Cell<u32>,
        destructed: &'a Cell<u32>,
    }
    impl<'a> Typ<'a> {
        fn new(val: i32, constructed: &'a Cell<u32>, destructed: &'a Cell<u32>) -> Self {
            constructed.set(constructed.get() + 1);
            Typ { val, constructed, destructed }
        }
    }
    impl Drop for Typ<'_> {
        fn drop(&mut self) {
            self.destructed.set(self.destructed.get() + 1);
        }
    }
    let constructed = Cell::new(0u32);
    let destructed = Cell::new(0u32);
    let mut fifo: ChunkedFifo<Typ<'_>> = ChunkedFifo::new();
    const N: u32 = 1000;
    for i in 0..N {
        fifo.push_back(Typ::new(i as i32, &constructed, &destructed));
    }
    assert_eq!(fifo.size(), N as usize);
    assert_eq!(constructed.get(), N);
    assert_eq!(destructed.get(), 0u32);
    for i in 0..N {
        assert_eq!(fifo.front().val, i as i32);
        assert_eq!(fifo.size(), (N - i) as usize);
        fifo.pop_front();
        assert_eq!(destructed.get(), i + 1);
    }
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
    // Check that destructing a fifo also destructs the objects it still
    // contains
    constructed.set(0);
    destructed.set(0);
    {
        let mut fifo: ChunkedFifo<Typ<'_>> = ChunkedFifo::new();
        for i in 0..N {
            fifo.push_back(Typ::new(i as i32, &constructed, &destructed));
            assert_eq!(fifo.front().val, 0);
            assert_eq!(fifo.size(), (i + 1) as usize);
            assert!(!fifo.empty());
            assert_eq!(constructed.get(), i + 1);
            assert_eq!(destructed.get(), 0u32);
        }
    }
    assert_eq!(constructed.get(), N);
    assert_eq!(destructed.get(), N);
}

#[test]
fn chunked_fifo_copy_move_test() {
    const N: usize = 4;
    type Ftype<'a> = ChunkedFifo<Trackable<'a>, { N / 2 }>;

    let calls = TrackableTotals::default();
    let mut fifo1: Ftype<'_> = ChunkedFifo::new();
    let mut fifo2: Ftype<'_> = ChunkedFifo::new();

    // Ties the lifetime of the closure's fifo to `calls` (a closure argument with an elided
    // lifetime would accept any).
    fn tie<'a, F: Fn(usize, &mut Ftype<'a>)>(f: F) -> F {
        f
    }
    let fill = tie(|size, fifo| {
        fifo.clear();
        calls.reset();
        for _ in 0..size {
            fifo.push_back(Trackable::new(&calls));
        }
    });

    let check = |cons: usize, ccons: usize, mcons: usize, dtor: usize| {
        assert_eq!(calls.cons_called.get(), cons);
        assert_eq!(calls.ccons_called.get(), ccons);
        assert_eq!(calls.mcons_called.get(), mcons);
        assert_eq!(calls.dtor_called.get(), dtor);
    };

    check(0, 0, 0, 0);

    fill(0, &mut fifo1);
    check(0, 0, 0, 0);

    fill(5, &mut fifo1);
    check(5, 0, 0, 0);

    {
        let _fifox = fifo1.clone(); // copy ctor
        check(5, 5, 0, 0);
    }

    assert_eq!(calls.dtor_called.get(), 5);
    fifo1.clear();
    assert_eq!(calls.dtor_called.get(), 10);

    fill(5, &mut fifo1);

    {
        // move ctor, no element ctors are called at all
        let _fifox = std::mem::take(&mut fifo1);
        check(5, 0, 0, 0);
    }

    fill(5, &mut fifo1);
    fifo2.clone_from(&fifo1);
    check(5, 5, 0, 0);

    fifo1.clear();
    check(5, 5, 0, 5);

    fifo2.clone_from(&fifo1);
    check(5, 5, 0, 10);

    fill(5, &mut fifo1);
    fifo2 = std::mem::take(&mut fifo1);
    check(5, 0, 0, 0);

    fill(1, &mut fifo1);
    fill(2, &mut fifo2);
    calls.reset();
    fifo1.clone_from(&fifo2);
    check(0, 2, 0, 1);
}

#[test]
fn chunked_copy_ctor() {
    type Ftype = ChunkedFifo<i32, 1>;
    let mut f1: Ftype = ChunkedFifo::new();
    f1.push_back(1);
    let mut f2: Ftype = ChunkedFifo::new();
    f2.push_back(1);
    f2.push_back(2);
    {
        let f0: Ftype = ChunkedFifo::new();
        assert!(f0 == Ftype::new());
    }
    {
        let f0 = f1.clone();
        assert!(f0 == f1);
    }
    {
        let f0 = f2.clone();
        assert!(f0 == f2);
    }
}

/// The error of a failing constructor.
#[derive(Debug)]
struct MyException;

impl fmt::Display for MyException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("my_exception")
    }
}

#[test]
fn chunked_fifo_construct_fail() {
    // Check that if we fail to construct the item pushed, the queue remains
    // empty.
    struct Typ;
    impl Typ {
        fn new() -> Result<Self, MyException> {
            Err(MyException)
        }
    }
    let mut fifo: ChunkedFifo<Typ> = ChunkedFifo::new();
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
    // The constructor runs first
    // and the push only happens when it succeeds.
    match Typ::new() {
        Ok(t) => fifo.push_back(t),
        Err(MyException) => {
            // expected, ignore
        }
    }
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
}

#[test]
fn chunked_fifo_construct_fail2() {
    // A slightly more elaborate test, with a chunk size of 2
    // items, and the third addition failing, so the question is
    // not whether empty() is wrong immediately, but whether after
    // we pop the two items, it will become true or we'll be left
    // with an empty chunk.
    struct Typ;
    impl Typ {
        fn new(fail: bool) -> Result<Self, MyException> {
            if fail {
                return Err(MyException);
            }
            Ok(Typ)
        }
    }
    let mut fifo: ChunkedFifo<Typ, 2> = ChunkedFifo::new();
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
    let emplace_back = |fifo: &mut ChunkedFifo<Typ, 2>, fail: bool| match Typ::new(fail) {
        Ok(t) => fifo.push_back(t),
        Err(MyException) => {
            // expected, ignore
        }
    };
    emplace_back(&mut fifo, false);
    emplace_back(&mut fifo, false);
    emplace_back(&mut fifo, true);
    assert_eq!(fifo.size(), 2usize);
    assert!(!fifo.empty());
    fifo.pop_front();
    assert_eq!(fifo.size(), 1usize);
    assert!(!fifo.empty());
    fifo.pop_front();
    assert_eq!(fifo.size(), 0usize);
    assert!(fifo.empty());
}

#[test]
fn chunked_fifo_equals_op() {
    let empty: ChunkedFifo<i32> = ChunkedFifo::new();
    let mut vec123: ChunkedFifo<i32> = ChunkedFifo::new();
    vec123.push_back(1);
    vec123.push_back(2);
    vec123.push_back(3);
    let mut empty2 = vec123.clone();
    empty2.clear();

    assert!(empty == empty);
    assert!(empty == empty2);
    assert!(empty != vec123);
    assert!(vec123 == vec123);
}

#[test]
fn chunked_fifo_copy() {
    let empty: ChunkedFifo<i32> = ChunkedFifo::new();
    let mut vec123: ChunkedFifo<i32> = ChunkedFifo::new();
    vec123.push_back(1);
    vec123.push_back(2);
    vec123.push_back(3);

    let empty_copy = empty.clone();
    assert!(empty_copy.empty());

    let vec123_copy = vec123.clone();
    assert_eq!(vec123.size(), 3);
    assert!(vec123_copy == vec123);
}

#[test]
fn chunked_fifo_assignment() {
    let empty: ChunkedFifo<i32> = ChunkedFifo::new();
    let mut vec123: ChunkedFifo<i32> = ChunkedFifo::new();
    let mut vec: ChunkedFifo<i32> = ChunkedFifo::new();
    vec123.push_back(1);
    vec123.push_back(2);
    vec123.push_back(3);

    assert!(vec.empty());
    vec.clone_from(&empty);
    assert!(vec.empty());

    vec.clone_from(&vec123);
    assert_eq!(vec.size(), 3);
    assert!(vec == vec123);

    vec = ChunkedFifo::new();
    assert!(vec.empty());

    // move assignment
    vec = std::mem::take(&mut vec123);
    assert_eq!(vec.size(), 3);
    assert!(vec123.empty());
}

// Enable the following to run some benchmarks on different queue options
#[cfg(any())]
mod benchmarks {
    use std::collections::{LinkedList, VecDeque};
    use std::time::Instant;

    use teseladb::engine::core::chunked_fifo::ChunkedFifo;
    use teseladb::engine::core::circular_buffer::CircularBuffer;

    // The name of a type, for the benchmark report.
    fn type_name<T>() -> &'static str {
        std::any::type_name::<T>()
    }

    unsafe extern "C" {
        fn rand_r(seed: *mut u32) -> i32;
    }

    /// The queue operations the benchmarks use, common to the four queue types.
    trait FifoType: Default {
        fn push_back(&mut self, v: i32);
        fn pop_front(&mut self);
        fn empty(&self) -> bool;
        fn size(&self) -> usize;
    }

    impl FifoType for ChunkedFifo<i32> {
        fn push_back(&mut self, v: i32) {
            ChunkedFifo::push_back(self, v)
        }
        fn pop_front(&mut self) {
            ChunkedFifo::pop_front(self)
        }
        fn empty(&self) -> bool {
            ChunkedFifo::empty(self)
        }
        fn size(&self) -> usize {
            ChunkedFifo::size(self)
        }
    }

    impl FifoType for CircularBuffer<i32> {
        fn push_back(&mut self, v: i32) {
            CircularBuffer::push_back(self, v)
        }
        fn pop_front(&mut self) {
            CircularBuffer::pop_front(self)
        }
        fn empty(&self) -> bool {
            CircularBuffer::empty(self)
        }
        fn size(&self) -> usize {
            CircularBuffer::size(self)
        }
    }

    impl FifoType for VecDeque<i32> {
        fn push_back(&mut self, v: i32) {
            VecDeque::push_back(self, v)
        }
        fn pop_front(&mut self) {
            VecDeque::pop_front(self);
        }
        fn empty(&self) -> bool {
            self.is_empty()
        }
        fn size(&self) -> usize {
            self.len()
        }
    }

    impl FifoType for LinkedList<i32> {
        fn push_back(&mut self, v: i32) {
            LinkedList::push_back(self, v)
        }
        fn pop_front(&mut self) {
            LinkedList::pop_front(self);
        }
        fn empty(&self) -> bool {
            self.is_empty()
        }
        fn size(&self) -> usize {
            self.len()
        }
    }

    fn benchmark_random_push_pop<F: FifoType>() {
        // A test involving a random sequence of pushes and pops. Because the
        // random walk is bounded the 0 end (the queue cannot be popped after
        // being empty), the queue's expected length at the end of the test is
        // not zero.
        // The test uses the same random sequence each time so can be used for
        // benchmarking different queue implementations on the same sequence.
        const N: i32 = 1_000_000_000;
        let mut fifo = F::default();
        let mut seed: u32 = 0;
        let mut entropy: i32 = 0;
        let start = Instant::now();
        for i in 0..N {
            if entropy == 0 {
                // SAFETY: `seed` is a live u32.
                entropy = unsafe { rand_r(&mut seed) };
            }
            if entropy & 1 != 0 {
                fifo.push_back(i);
            } else if !fifo.empty() {
                fifo.pop_front();
            }
            entropy >>= 1;
        }
        let ms = start.elapsed().as_millis();
        eprintln!("{}, {} random push-and-pop {} {}ms.", type_name::<F>(), N, fifo.size(), ms);
    }

    fn benchmark_push_pop<F: FifoType>() {
        // A benchmark involving repeated push and then pop to a queue, which
        // will have 0 or 1 items at all times.
        const N: i32 = 1_000_000_000;
        let mut fifo = F::default();
        let start = Instant::now();
        for _ in 0..N {
            fifo.push_back(1);
            fifo.pop_front();
        }
        let ms = start.elapsed().as_millis();
        eprintln!("{}, {} push-and-pop {}ms.", type_name::<F>(), N, ms);
    }

    fn benchmark_push_pop_k<F: FifoType>() {
        // A benchmark involving repeated pushes of a few items and then popping
        // to a queue, which will have just one chunk (or 0) at all times.
        const N: i32 = 1_000_000_000;
        const K: i32 = 100;
        let mut fifo = F::default();
        let start = Instant::now();
        for _ in 0..N / K {
            for j in 0..K {
                fifo.push_back(j);
            }
            for _ in 0..K {
                fifo.pop_front();
            }
        }
        let ms = start.elapsed().as_millis();
        eprintln!("{}, {} push-and-pop-{} {}ms.", type_name::<F>(), N, K, ms);
    }

    fn benchmark_pushes_pops<F: FifoType>() {
        // A benchmark of pushing a lot of items, and then popping all of them
        const N: i32 = 100_000_000;
        let mut fifo = F::default();
        let start = Instant::now();
        for _ in 0..N {
            fifo.push_back(1);
        }
        for _ in 0..N {
            fifo.pop_front();
        }
        let ms = start.elapsed().as_millis();
        eprintln!("{}, {} push-all-then-pop-all {}ms.", type_name::<F>(), N, ms);
    }

    fn benchmark_all<F: FifoType>() {
        eprintln!("\n  --- {}: ", type_name::<F>());
        benchmark_random_push_pop::<F>();
        benchmark_push_pop::<F>();
        benchmark_push_pop_k::<F>();
        benchmark_pushes_pops::<F>();
    }

    #[test]
    fn chunked_fifo_benchmark() {
        benchmark_all::<ChunkedFifo<i32>>();
        benchmark_all::<CircularBuffer<i32>>();
        benchmark_all::<VecDeque<i32>>();
        benchmark_all::<LinkedList<i32>>();
    }
}

#[test]
fn chunked_fifo_iterator() {
    const ITEMS_PER_CHUNK: usize = 8;
    let mut fifo: ChunkedFifo<i32, ITEMS_PER_CHUNK> = ChunkedFifo::new();
    let mut reference: VecDeque<i32> = VecDeque::new();

    assert!(fifo.iter_mut().next().is_none());

    for i in 0..(ITEMS_PER_CHUNK * 4) as i32 {
        fifo.push_back(i);
        reference.push_back(i);
        assert!(fifo.iter_mut().map(|v| *v).eq(reference.iter().copied()));
    }

    for _ in 0..ITEMS_PER_CHUNK * 2 {
        fifo.pop_front();
        reference.pop_front();
        assert!(fifo.iter_mut().map(|v| *v).eq(reference.iter().copied()));
    }
}

#[test]
fn chunked_fifo_const_iterator() {
    const ITEMS_PER_CHUNK: usize = 8;
    let mut fifo: ChunkedFifo<i32, ITEMS_PER_CHUNK> = ChunkedFifo::new();
    let mut reference: VecDeque<i32> = VecDeque::new();

    assert!(fifo.iter().next().is_none());

    for i in 0..(ITEMS_PER_CHUNK * 4) as i32 {
        fifo.push_back(i);
        reference.push_back(i);
        assert!(fifo.iter().eq(reference.iter()));
    }

    for _ in 0..ITEMS_PER_CHUNK * 2 {
        fifo.pop_front();
        reference.pop_front();
        assert!(fifo.iter().eq(reference.iter()));
    }
}
