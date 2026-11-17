//! Writing a document allocates its buffer once: an array's position names are written from the
//! stack (a `String` per position was allocated), and the own bytes of a reply written as
//! [`Fragments`] are known before it is written ([`Document::fragments_own_len`]), so its buffer
//! does not grow. Allocations are counted per thread by this program's global allocator.
// The counting allocator implements `GlobalAlloc`, an unsafe trait.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use teseladb_bson::{Document, Fragments, Limits, Value};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every call forwards to the system allocator with the same arguments; the thread-local
// counter has a constant initializer and no destructor, so counting never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: forwarded as received.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded as received.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: forwarded as received.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The allocations (and reallocations) `f` makes on this thread.
fn allocations<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATIONS.with(Cell::get);
    let out = f();
    (out, ALLOCATIONS.with(Cell::get) - before)
}

fn stored(d: &Document) -> Document {
    Document::from_stored(d.to_bytes().expect("encodes")).expect("stored")
}

/// A reply as the wire writes one: counters, an array of numbers and nested documents, one of
/// them stored.
fn reply(items: usize) -> Document {
    let numbers: Vec<Value> =
        (0..items).map(|i| Value::Int64(i64::try_from(i).expect("small"))).collect();
    let kept = stored(&Document::new().with("_id", 7i64).with("pad", "x".repeat(300)));
    let time = Document::new().with("hash", Value::Binary { subtype: 0, bytes: vec![0; 20] });
    Document::new()
        .with("n", 1i32)
        .with("tags", numbers)
        .with("nested", Document::new().with("deep", vec![Value::Int32(1), Value::Int32(2)]))
        .with("doc", kept)
        .with("$clusterTime", time)
        .with("ok", 1.0)
}

#[test]
fn a_document_with_arrays_is_written_with_one_allocation() {
    let doc = reply(25);
    let (bytes, n) = allocations(|| doc.to_bytes().expect("encodes"));
    assert_eq!(n, 1, "only the buffer, sized up front");
    // The names of the positions are the decimal positions.
    let back = Document::from_stored(bytes).expect("reads");
    let tags = back.get("tags").expect("tags");
    let Value::Array(items) = tags else { panic!("an array: {tags:?}") };
    assert_eq!(items.len(), 25);
    assert_eq!(items[24], Value::Int64(24));
}

#[test]
fn the_own_bytes_of_fragments_are_known_before_they_are_written() {
    for items in [0, 1, 9, 10, 11, 100, 1000] {
        let doc = reply(items);
        for min in [1, 64, 10_000] {
            let own = doc.fragments_own_len(min);
            let (f, n) = allocations(|| {
                let mut f = Fragments::with_capacity(min, own);
                doc.append_fragments(&mut f, Limits::default()).expect("writes");
                f
            });
            assert_eq!(f.len() - f.borrowed(), own, "{items} items, min {min}");
            // The own buffer, and the list of borrowed pieces when there is one.
            let most = if f.borrowed() > 0 { 2 } else { 1 };
            assert!(n <= most, "{n} allocations for {items} items, min {min}");
            let mut whole = Vec::new();
            f.for_each_piece(|p| whole.extend_from_slice(p));
            assert_eq!(whole, doc.to_bytes().expect("encodes"), "{items} items, min {min}");
        }
    }
}
