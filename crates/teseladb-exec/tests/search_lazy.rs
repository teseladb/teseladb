//! The results of `$search` reach the stages after it in batches of 64, each document read only
//! when a stage asks for it (011 FR-031, T059), with the same documents, metadata and order as
//! when every document is read first; the `$limit` and `$skip` right after the stage reach the
//! node as the most results the pipeline reads (FR-030).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::search::{Lazy, SEARCH_BATCH, Scored};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

/// The results of every search: `n` documents with descending scores.
fn results(n: usize) -> Vec<Scored> {
    (0..n)
        .map(|i| {
            let doc = Document::new()
                .with("_id", i as i32)
                .with("odd", i % 2 == 1)
                .with("tags", vec![Value::from(i as i32 % 3), Value::from(7)]);
            let meta = BTreeMap::from([
                ("searchScore".to_owned(), Value::Double(1000.0 - i as f64)),
                ("searchSequenceToken".to_owned(), Value::String(format!("t{i}"))),
            ]);
            Scored { doc, meta }
        })
        .collect()
}

/// A node whose search reads documents lazily, counting them, or all at once (`eager`).
struct Node {
    n: usize,
    eager: bool,
    loaded: Cell<usize>,
    ranges: RefCell<Vec<(usize, usize)>>,
    limits: RefCell<Vec<Option<usize>>>,
}

impl Node {
    fn new(n: usize, eager: bool) -> Node {
        Node {
            n,
            eager,
            loaded: Cell::new(0),
            ranges: RefCell::new(Vec::new()),
            limits: RefCell::new(Vec::new()),
        }
    }
}

impl Env for Node {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _name: &str) -> Result<Vec<Document>, MqlError> {
        Ok(vec![Document::new().with("_id", 1).with("joined", true)])
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn search_source(&self, stage: &str, _spec: &Document) -> Result<Vec<Scored>, MqlError> {
        Ok(match stage {
            "$searchMeta" => vec![Scored {
                doc: Document::new().with("count", Document::new().with("lowerBound", 1i64)),
                meta: BTreeMap::new(),
            }],
            _ => results(self.n),
        })
    }

    fn search_results(
        &self,
        stage: &str,
        spec: &Document,
        limit: Option<usize>,
    ) -> Result<Lazy<'_>, MqlError> {
        self.limits.borrow_mut().push(limit);
        if self.eager {
            return Ok(Lazy::eager(self.search_source(stage, spec)?));
        }
        let all = results(self.n);
        let metas = all.iter().map(|s| s.meta.clone()).collect();
        let load = move |from: usize, to: usize| {
            self.loaded.set(self.loaded.get() + (to - from));
            self.ranges.borrow_mut().push((from, to));
            Ok(all[from..to].iter().map(|s| s.doc.clone()).collect())
        };
        Ok(Lazy::new(metas, Box::new(load)))
    }
}

fn run(stages: &[Document], env: &Node) -> Result<Vec<Document>, MqlError> {
    let bytes: Vec<Vec<u8>> = stages.iter().map(|s| s.to_bytes().expect("small")).collect();
    let raw: Vec<RawValue<'_>> = bytes
        .iter()
        .map(|b| RawValue::Document(RawDoc::from_bytes(b).expect("a document")))
        .collect();
    let p = Pipeline::parse_command(&raw, &Ctx::default(), true)?;
    p.run(Vec::new(), env)
}

fn stage(name: &str, v: impl Into<Value>) -> Document {
    Document::new().with(name, v)
}

fn search() -> Document {
    stage("$search", Document::new().with("text", Document::new()))
}

fn odd() -> Document {
    stage("$match", Document::new().with("odd", true))
}

fn scores() -> Document {
    let meta = |n: &str| Document::new().with("$meta", n);
    stage(
        "$project",
        Document::new()
            .with("score", meta("searchScore"))
            .with("token", meta("searchSequenceToken"))
            .with("tags", 1),
    )
}

#[test]
fn a_limit_after_the_search_reads_only_its_documents() {
    let env = Node::new(1000, false);
    let got = run(&[search(), stage("$limit", 5), scores()], &env).unwrap();
    assert_eq!(got.len(), 5);
    assert_eq!(env.loaded.get(), 5);
    assert_eq!(*env.limits.borrow(), [Some(5)]);
    let env = Node::new(1000, false);
    let got = run(&[search(), stage("$skip", 70), stage("$limit", 3)], &env).unwrap();
    assert_eq!(
        got.iter().map(|d| d.get("_id").cloned()).collect::<Vec<_>>(),
        [Some(Value::Int32(70)), Some(Value::Int32(71)), Some(Value::Int32(72))]
    );
    assert_eq!((env.loaded.get(), env.limits.borrow()[0]), (3, Some(73)));
    assert_eq!(*env.ranges.borrow(), [(70, 73)]);
}

#[test]
fn stages_after_the_search_read_batches_of_64_until_their_limit() {
    let env = Node::new(1000, false);
    let got = run(&[search(), odd(), scores(), stage("$limit", 40)], &env).unwrap();
    assert_eq!(got.len(), 40);
    // 64 results hold 32 odd ones; the second batch gives the other 8.
    assert_eq!(*env.ranges.borrow(), [(0, SEARCH_BATCH), (SEARCH_BATCH, 2 * SEARCH_BATCH)]);
    assert_eq!(env.limits.borrow()[0], None);
    // A `$skip` first is counted without reading what it drops.
    let env = Node::new(1000, false);
    run(&[search(), stage("$skip", 990), odd()], &env).unwrap();
    assert_eq!((env.loaded.get(), env.ranges.borrow()[0]), (10, (990, 1000)));
    // A `$count` needs every result.
    let env = Node::new(150, false);
    let got = run(&[search(), odd(), stage("$count", "n")], &env).unwrap();
    assert_eq!(got, [Document::new().with("n", 75)]);
    assert_eq!(env.loaded.get(), 150);
}

#[test]
fn lazy_results_give_what_reading_them_all_gives() {
    let unwind = stage("$unwind", "$tags");
    let lookup = stage(
        "$lookup",
        Document::new().with("from", "c").with("as", "j").with("pipeline", Vec::<Value>::new()),
    );
    let add = stage(
        "$addFields",
        Document::new().with("s", Document::new().with("$meta", "searchScore")),
    );
    let sort = stage("$sort", Document::new().with("s", 1));
    let skip = |n: i32| stage("$skip", n);
    let limit = |n: i32| stage("$limit", n);
    let pipelines: Vec<Vec<Document>> = vec![
        vec![search()],
        vec![search(), scores()],
        vec![search(), odd(), scores()],
        vec![search(), limit(0)],
        vec![search(), skip(5000)],
        vec![search(), limit(10), skip(3), limit(100)],
        vec![search(), skip(3), limit(100), odd(), limit(7), scores()],
        vec![search(), unwind.clone(), odd(), skip(30), limit(90), scores()],
        vec![search(), lookup, add.clone(), sort.clone(), limit(5)],
        vec![search(), odd(), add, sort, scores()],
        vec![search(), unwind, limit(129)],
        vec![search(), stage("$project", Document::new().with("m", "$$SEARCH_META")), limit(2)],
    ];
    for n in [0, 1, 63, 64, 65, 200, 1000] {
        for p in &pipelines {
            let lazy = Node::new(n, false);
            let eager = Node::new(n, true);
            let (a, b) = (run(p, &lazy), run(p, &eager));
            assert_eq!(a, b, "{n} results, {p:?}");
            assert!(lazy.loaded.get() <= n, "{n} {p:?}");
        }
    }
}
