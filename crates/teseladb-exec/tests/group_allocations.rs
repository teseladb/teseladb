//! Allocations of a `$group` over stored documents (010 FR-030, FR-031): with a store to write to,
//! the group keeps its input and measures it; a stored document is measured on its bytes and
//! grouped on its columns, so the stage allocates a few times per batch of documents, not once
//! per field of each document as when the documents were read into values to be measured.

// The counting allocator wraps the system's, as in `group_memory.rs`.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use teseladb_bson::{Document, RawValue, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::spill::{MemorySpill, SpillStore};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

/// Counts the allocations of the thread that asks for them.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every call goes to the system allocator unchanged; the count is a thread-local cell
// that allocates nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

/// Documents shaped as those of the sustained load: ten fields, a padding string of up to 2 kB.
const DOCS: usize = 4_096;

struct StoreEnv<'a> {
    store: &'a dyn SpillStore,
}

impl Env for StoreEnv<'_> {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
        Ok(Vec::new())
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn spill(&self) -> Option<&dyn SpillStore> {
        Some(self.store)
    }
}

fn stored_docs() -> Vec<Document> {
    (0..DOCS)
        .map(|n| {
            let c = (n % 16) as i32;
            let d = Document::new()
                .with("_id", format!("c{c:03}-{n:08}"))
                .with("c", c)
                .with("n", n as i64)
                .with("g", (n % 16) as i32)
                .with("v", (n % 7) as i32)
                .with("s", "")
                .with("tags", teseladb_bson::Value::Array(Vec::new()))
                .with("ver", 0)
                .with("pad", "x".repeat(50 + n * 37 % 1950));
            Document::from_stored(d.to_bytes().expect("small")).expect("canonical")
        })
        .collect()
}

fn pipeline(text: &str) -> Pipeline {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("pipeline");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse(&stages, &Ctx::default()).expect("parses")
}

/// The allocations per input document of `text` over the stored documents, and its output.
fn per_document(text: &str) -> (f64, Vec<Document>) {
    let p = pipeline(text);
    let store = MemorySpill::default();
    let env = StoreEnv { store: &store };
    let docs = stored_docs();
    let before = allocations();
    let out = p.run(docs, &env).expect("runs");
    let n = allocations() - before;
    (n as f64 / DOCS as f64, out)
}

#[test]
fn a_group_of_stored_documents_does_not_read_them_into_values() {
    let (control, out) =
        per_document(r#"[{"$group":{"_id":null,"n":{"$sum":1},"v":{"$sum":"$v"}}}]"#);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get("n"), Some(&teseladb_bson::Value::Int32(DOCS as i32)));
    let (by_key, out) = per_document(
        r#"[{"$group":{"_id":"$c","n":{"$sum":1},"v":{"$avg":"$v"}}},
            {"$sort":{"n":-1,"_id":1}},{"$limit":10}]"#,
    );
    assert_eq!(out.len(), 10);
    // Read into values to be measured, each document cost an allocation per field and more.
    assert!(control < 1.0, "{control} allocations per document");
    assert!(by_key < 1.0, "{by_key} allocations per document");
}
