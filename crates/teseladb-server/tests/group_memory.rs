//! The memory a `$group` and a `$bucket` hold with a store to write to stays bounded by their
//! limit (010 FR-028, T066): the documents that would go to the store if the groups passed the
//! limit wait in memory only while they fit in what the groups leave of it, and the rest goes to
//! the store as it comes. The test allocator counts the bytes the process holds; the input, built
//! before the run, is the baseline, and the peak above it is what the stages took.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::{Document, RawValue, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::spill::{Counting, SpillStats, SpillStore};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions, codes};
use teseladb_sys::alloc_count::{CountingAlloc, peak_bytes, reset_peak};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// One test measures at a time, so that the other's memory does not count.
static MEASURING: Mutex<()> = Mutex::new(());

/// The limit of the stages: a few dozen of the documents they take.
const LIMIT: usize = 64 * 1024;

/// The documents of the input.
const DOCS: usize = 4_000;

/// The most a stage may hold above the batches the stages before it pass on: its limit for the
/// groups and as much for the documents waiting for the store, each about twice its bytes in
/// BSON once in the heap, the reads of the merge and the groups it gives. Before T066 the
/// documents waited in memory until the groups passed the limit: about 5.9 MiB above the
/// batches with many groups, 5.5 MiB with few (the whole input) and 1.9 MiB in a `$bucket`.
const MOST_HELD: usize = 8 * LIMIT;

/// A store over files in the tests' temporary directory.
#[derive(Debug, Default)]
struct FileStore {
    next: AtomicU64,
}

impl FileStore {
    fn path(run: u64) -> PathBuf {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("group-memory-{}-{run}.run", std::process::id()))
    }
}

fn io_error(e: std::io::Error) -> MqlError {
    MqlError::new(codes::INTERNAL_ERROR, e.to_string())
}

impl SpillStore for FileStore {
    fn write(&self, bytes: &[u8]) -> Result<u64, MqlError> {
        let run = self.next.fetch_add(1, Ordering::Relaxed);
        let mut f = std::fs::File::create(Self::path(run)).map_err(io_error)?;
        f.write_all(bytes).map_err(io_error)?;
        Ok(run)
    }

    fn read(&self, run: u64, offset: u64, len: usize) -> Result<Vec<u8>, MqlError> {
        let mut f = std::fs::File::open(Self::path(run)).map_err(io_error)?;
        f.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut out = Vec::with_capacity(len);
        f.take(u64::try_from(len).unwrap_or(u64::MAX)).read_to_end(&mut out).map_err(io_error)?;
        Ok(out)
    }

    fn remove(&self, run: u64) {
        std::fs::remove_file(Self::path(run)).expect("the run's file");
    }
}

struct LimitedEnv<'a> {
    limit: usize,
    store: Option<&'a dyn SpillStore>,
}

impl Env for LimitedEnv<'_> {
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

    fn allow_disk_use(&self) -> bool {
        true
    }

    fn stage_memory_limit(&self) -> usize {
        self.limit
    }

    fn spill(&self) -> Option<&dyn SpillStore> {
        self.store
    }
}

fn pipeline(text: &str) -> Pipeline {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("a pipeline");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse(&stages, &Ctx::default()).expect("parses")
}

/// Small documents in `groups` groups, those of a group one after the other, so that the groups
/// pass the limit late; each group's key is 200 bytes long.
fn input(groups: usize) -> Vec<Document> {
    let per_group = DOCS / groups;
    (0..DOCS)
        .map(|i| {
            let g = i / per_group;
            Document::new()
                .with("_id", i64::try_from(i).expect("small"))
                .with("k", format!("{g:0>200}"))
                .with("v", i64::try_from(i % 97).expect("small"))
                .with("s", "x".repeat(50))
        })
        .collect()
}

/// The stage `stage` after an `$addFields` that makes each document about 1 KiB.
fn grown(stage: &str) -> String {
    let parts = vec![r#""$s""#; 20].join(",");
    format!(r#"[{{"$addFields":{{"pad":{{"$concat":[{parts}]}}}}}},{stage}]"#)
}

/// The most the process holds above `docs` while `p` runs over them in `env`.
fn peak_of(p: &Pipeline, docs: Vec<Document>, env: &dyn Env) -> (Vec<Document>, usize) {
    let base = reset_peak();
    let out = p.run(docs, env).expect("runs");
    (out, peak_bytes().saturating_sub(base))
}

/// What a run of `text` over the input of `groups` groups gives with no limit and through a
/// store with [`LIMIT`], what the store was given, the most the run through the store held above
/// its input and the most a pipeline of the same stages before them but none after holds, which
/// is the memory of the batches between the stages (010 FR-028).
struct Measured {
    expected: Vec<Document>,
    out: Vec<Document>,
    stats: SpillStats,
    held: usize,
    batches: usize,
}

fn measured(text: &str, groups: usize) -> Measured {
    let _one = MEASURING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let p = pipeline(text);
    let free = LimitedEnv { limit: usize::MAX, store: None };
    let expected = p.run(input(groups), &free).expect("runs in memory");
    let files = FileStore::default();
    let store = Counting::new(&files);
    let env = LimitedEnv { limit: LIMIT, store: Some(&store) };
    let (out, held) = peak_of(&p, input(groups), &env);
    let none = pipeline(&grown(r#"{"$match":{"_id":-1}}"#));
    let (_, batches) = peak_of(&none, input(groups), &env);
    Measured { expected, out, stats: store.stats(), held, batches }
}

impl Measured {
    /// Fails unless the stage held at most [`MOST_HELD`] above the batches.
    fn bounded(&self) {
        let (held, batches) = (self.held, self.batches);
        assert!(
            held <= batches + MOST_HELD,
            "held {held} bytes above the input, the batches {batches}, at most {MOST_HELD} more"
        );
    }
}

#[test]
fn a_group_with_many_groups_holds_no_more_than_its_limit_allows() {
    let m = measured(&grown(r#"{"$group":{"_id":"$k","n":{"$sum":1},"v":{"$max":"$v"}}}"#), 400);
    let mut sorted = m.expected.clone();
    sorted.sort_by(|a, b| format!("{:?}", a.get("_id")).cmp(&format!("{:?}", b.get("_id"))));
    assert_eq!(m.out, sorted, "the groups in the order of their keys");
    assert!(m.stats.spills > 0, "{:?}", m.stats);
    m.bounded();
}

#[test]
fn a_group_with_few_groups_holds_no_more_than_its_limit_allows_and_counts_no_spill() {
    let m = measured(&grown(r#"{"$group":{"_id":"$k","n":{"$sum":1},"p":{"$first":"$_id"}}}"#), 4);
    assert_eq!(m.out, m.expected, "the groups in the order they came");
    assert_eq!(m.stats, SpillStats::default(), "the groups never passed the limit");
    m.bounded();
}

#[test]
fn a_bucket_holds_no_more_than_its_limit_allows() {
    let pushing = r#"{"$bucket":{"groupBy":"$v","boundaries":[0,10,50,97],
        "output":{"n":{"$sum":1},"s":{"$push":"$s"}}}}"#;
    let m = measured(&grown(pushing), 400);
    assert_eq!(m.out, m.expected);
    assert!(m.stats.spills > 0, "{:?}", m.stats);
    m.bounded();
    let counting = r#"{"$bucket":{"groupBy":"$v","boundaries":[0,10,50,97],
        "output":{"n":{"$sum":1}}}}"#;
    let m = measured(&grown(counting), 400);
    assert_eq!(m.out, m.expected);
    assert_eq!(m.stats, SpillStats::default(), "the buckets never passed the limit");
    m.bounded();
}
