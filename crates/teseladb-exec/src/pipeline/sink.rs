//! The blocking stages taking their input a batch at a time, as the stages before them give it
//! (010 FR-028): `$group`, `$sortByCount`, `$sort` (and `$sort` followed by `$limit`), `$bucket`,
//! `$bucketAuto` and `$facet`, whose pipelines each take every batch. A stage counts the bytes
//! it holds as each batch comes: past its limit it fails with 292 without `allowDiskUse` and,
//! with it, writes to the node's store what it holds and goes on (010 FR-010). It never needs
//! the whole output of the stages before it in memory.

use std::borrow::Cow;

use teseladb_bson::{Document, Value};
use teseladb_mql::MqlError;

use super::errors::{self, document_size};
use super::group::{BucketAutoRun, Bucketing, Grouping, by_count, counting};
use super::sort::{SortSpec, TOP_K_INSERTION};
use super::spill::Sorter;
use super::{Env, Pipeline, Stage, for_each_batch};

/// A blocking stage taking batches.
pub(super) enum Sink<'p> {
    Group(Grouping<'p>),
    SortByCount(Grouping<'p>),
    Sort(Sorting<'p>),
    SortTop(SortTop<'p>),
    Bucket(Bucketing<'p>),
    BucketAuto(BucketAutoRun<'p>),
    Facet(Vec<(&'p str, Feed<'p>)>),
}

impl<'p> Sink<'p> {
    /// The stage at `i` of `p` taking batches, `sbe` when the reference server runs it with its slot-based
    /// engine; `None` when it is not a blocking stage that does. A `$sort` that sets the sort
    /// key of each document's metadata runs over the whole input.
    pub(super) fn of(p: &'p Pipeline, i: usize, env: &'p dyn Env, sbe: bool) -> Option<Sink<'p>> {
        Some(match p.stages.get(i)? {
            Stage::Group(g) => Sink::Group(g.start(env, sbe)),
            Stage::SortByCount(e) => {
                Sink::SortByCount(Grouping::of(Cow::Owned(counting(e)), env, sbe))
            }
            // `$sort` followed by `$limit` keeps only the documents the limit takes (010
            // FR-029); the `$limit` then takes them all.
            Stage::Sort(s) => match p.stages.get(i + 1) {
                Some(Stage::Limit(n)) => {
                    let k = usize::try_from(*n).unwrap_or(usize::MAX);
                    Sink::SortTop(SortTop { spec: s, env, k, kept: Vec::new() })
                }
                _ if p.reads_sort_key => return None,
                _ => Sink::Sort(Sorting { spec: s, env, docs: Vec::new(), held: 0, sorter: None }),
            },
            Stage::Bucket(b) => Sink::Bucket(b.start(env, sbe)),
            Stage::BucketAuto(b) => Sink::BucketAuto(b.start(env)),
            Stage::Facet(f) if !f.has_text() => Sink::Facet(
                f.facets().iter().map(|(name, p)| (name.as_str(), Feed::new(p, env))).collect(),
            ),
            _ => return None,
        })
    }

    /// Takes the documents of `batch`, after those before it.
    pub(super) fn absorb(&mut self, batch: Vec<Document>) -> Result<(), MqlError> {
        match self {
            Sink::Group(g) | Sink::SortByCount(g) => g.absorb(batch),
            Sink::Sort(s) => s.absorb(batch),
            Sink::SortTop(s) => s.absorb(batch),
            Sink::Bucket(b) => b.absorb(batch),
            Sink::BucketAuto(b) => b.absorb(batch),
            Sink::Facet(feeds) => {
                let Some(((_, last), rest)) = feeds.split_last_mut() else { return Ok(()) };
                for (_, feed) in rest {
                    feed.push(batch.clone())?;
                }
                last.push(batch)
            }
        }
    }

    /// The output of the stage over everything it took.
    pub(super) fn finish(self) -> Result<Vec<Document>, MqlError> {
        match self {
            Sink::Group(g) => g.finish(),
            Sink::SortByCount(g) => Ok(by_count(g.finish()?)),
            Sink::Sort(s) => s.finish(),
            Sink::SortTop(s) => s.finish(),
            Sink::Bucket(b) => b.finish(),
            Sink::BucketAuto(b) => b.finish(),
            Sink::Facet(feeds) => {
                let mut out = Document::new();
                for (name, feed) in feeds {
                    let results = feed.finish()?;
                    out.push(
                        name,
                        Value::Array(results.into_iter().map(Value::Document).collect()),
                    );
                }
                Ok(vec![out])
            }
        }
    }
}

/// A `$sort` taking batches: the documents wait in memory while they hold no more than the
/// stage's limit; past it the sort fails with 292 without `allowDiskUse` and, with a store,
/// writes them to it as sorted runs and goes on.
pub(super) struct Sorting<'p> {
    spec: &'p SortSpec,
    env: &'p dyn Env,
    docs: Vec<Document>,
    held: usize,
    sorter: Option<Sorter<'p, Vec<Value>>>,
}

impl<'p> Sorting<'p> {
    /// A run of `$sort` `spec` that takes its input a batch at a time.
    pub(super) fn new(spec: &'p SortSpec, env: &'p dyn Env) -> Sorting<'p> {
        Sorting { spec, env, docs: Vec::new(), held: 0, sorter: None }
    }

    pub(super) fn absorb(&mut self, batch: Vec<Document>) -> Result<(), MqlError> {
        let env = self.env;
        let limit = env.stage_memory_limit();
        for d in batch {
            if let Some(sorter) = &mut self.sorter {
                sorter.push(d)?;
                continue;
            }
            self.held += document_size(&d);
            self.docs.push(d);
            if self.held <= limit {
                continue;
            }
            if !env.allow_disk_use() {
                return Err(errors::sort_memory_error(limit));
            }
            let Some(store) = env.spill() else { continue };
            let (spec, collation) = (self.spec, env.collation());
            let keys = collation.clone();
            let mut sorter = Sorter::new(
                Box::new(move |d: &Document| spec.key(d, keys.get())),
                Box::new(move |a: &Vec<Value>, b: &Vec<Value>| spec.compare(a, b, collation.get())),
                (limit, store),
            );
            for d in self.docs.drain(..) {
                sorter.push(d)?;
            }
            self.sorter = Some(sorter);
        }
        Ok(())
    }

    /// The documents it took, sorted.
    pub(super) fn finish(self) -> Result<Vec<Document>, MqlError> {
        let Some(sorter) = self.sorter else {
            return self.spec.sort(self.docs, self.env.collation().get());
        };
        let mut out = Vec::new();
        sorter.finish(|_, d| {
            out.push(d);
            Ok(())
        })?;
        Ok(out)
    }
}

/// A `$sort` followed by a `$limit` of `k` taking batches: only the first `k` documents so far
/// stay, and they are what the stage holds against its limit, as in the reference server's top-k sort. A
/// `k` past [`TOP_K_INSERTION`] keeps every document and sorts them at the end.
pub(super) struct SortTop<'p> {
    spec: &'p SortSpec,
    env: &'p dyn Env,
    k: usize,
    kept: Vec<Document>,
}

impl SortTop<'_> {
    pub(super) fn absorb(&mut self, batch: Vec<Document>) -> Result<(), MqlError> {
        self.kept.extend(batch);
        if self.k > TOP_K_INSERTION {
            return Ok(());
        }
        let collation = self.env.collation();
        self.kept = self.spec.sort_top(std::mem::take(&mut self.kept), collation.get(), self.k)?;
        self.check()
    }

    /// The 292 of the documents it keeps past the stage's limit without `allowDiskUse`.
    fn check(&self) -> Result<(), MqlError> {
        let env = self.env;
        errors::check_sort_memory(&self.kept, env.allow_disk_use(), env.stage_memory_limit())
    }

    fn finish(mut self) -> Result<Vec<Document>, MqlError> {
        if self.k > TOP_K_INSERTION {
            let collation = self.env.collation();
            self.kept =
                self.spec.sort_top(std::mem::take(&mut self.kept), collation.get(), self.k)?;
            self.check()?;
        }
        Ok(self.kept)
    }
}

/// A pipeline of a `$facet` taking batches: its stages that change or filter each document run
/// over each batch, the blocking stage after them takes what comes out, and the rest of its
/// stages run at the end over that stage's output.
pub(super) struct Feed<'p> {
    pipeline: &'p Pipeline,
    env: &'p dyn Env,
    /// The end of the stages that change or filter each document on their own.
    head: usize,
    sink: Option<Box<Sink<'p>>>,
    /// Without a blocking stage after them, their output so far.
    out: Vec<Document>,
}

impl<'p> Feed<'p> {
    /// `pipeline` taking batches, as the slot-based engine never runs a `$facet`'s pipeline.
    fn new(pipeline: &'p Pipeline, env: &'p dyn Env) -> Feed<'p> {
        let head = pipeline.streaming_end(0);
        let sink = Sink::of(pipeline, head, env, false).map(Box::new);
        Feed { pipeline, env, head, sink, out: Vec::new() }
    }

    fn push(&mut self, batch: Vec<Document>) -> Result<(), MqlError> {
        let (stages, env) = (&self.pipeline.stages[..self.head], self.env);
        let (sink, out) = (&mut self.sink, &mut self.out);
        for_each_batch(stages, batch, env, false, &mut |b| match sink {
            Some(sink) => sink.absorb(b),
            None => {
                out.extend(b);
                Ok(())
            }
        })
    }

    fn finish(self) -> Result<Vec<Document>, MqlError> {
        let (docs, at) = match self.sink {
            Some(sink) => (sink.finish()?, self.head + 1),
            None => (self.out, self.head),
        };
        self.pipeline.run_from(at, (docs, None), self.env, false).map(|(docs, _)| docs)
    }
}
