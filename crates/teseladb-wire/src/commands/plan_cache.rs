//! The plan cache of a node (010 FR-002, T017), as a `refrouter` 9.0.4 keeps it (measured in
//! `crates/teseladb-wire/tests/explain/plan_cache.out`): a read whose plans competed leaves an
//! entry for its shape (the filter's fields and operators without their values, the sort, the
//! projection and the key of `distinct`), inactive at first; the next read of that shape whose
//! winner worked no more makes it active. Building or dropping an index of the collection and
//! `planCacheClear` drop its entries; `explain` leaves none. `$planCacheStats` lists them, the
//! newest first.

use teseladb_plan::rules::Access;
use teseladb_bson::{Document, Value};

use super::{Backend, Ctx, args, lock, names};
use crate::error::{BAD_VALUE, CommandError};

/// The command's name.
pub const PLAN_CACHE_CLEAR: &str = "planCacheClear";

/// How much an inactive entry's works grow when a read of its shape works more
/// (`internalQueryCacheWorksGrowthCoefficient`).
const WORKS_GROWTH: u64 = 2;

/// What a read whose plans competed learned.
#[derive(Clone, Debug, PartialEq)]
pub struct Learned {
    /// The read's shape ([`read_shape`]).
    pub shape: String,
    /// `planCacheKey`: the shape's hash with the indexability of its predicates.
    pub key: String,
    /// The collection and its indexes when the read ran ([`catalog_signature`]).
    pub catalog: String,
    /// `createdFromQuery`.
    pub from: Document,
    /// The winner's works in its trial.
    pub works: u64,
    /// The index entries and documents the winner read in its trial.
    pub reads: u64,
    /// The plan that won, as `explain` writes it.
    pub plan: Document,
    /// Each plan's trial, the winner first.
    pub trials: Vec<Document>,
    /// Each plan's score, the winner first.
    pub scores: Vec<f64>,
    /// How the winner reads, for the reads of the shape that take the cached plan.
    pub access: Access,
}

/// One entry.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    ns: String,
    active: bool,
    created_ms: i64,
    learned: Learned,
}

/// The entries of a node, the newest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanCache {
    entries: Vec<Entry>,
}

impl PlanCache {
    /// Takes what a read of `ns` learned at `now_ms`; the entries of `ns` learned with other
    /// indexes go (an index built, dropped or hidden, the collection dropped).
    pub fn learn(&mut self, ns: &str, learned: Learned, now_ms: i64) {
        self.forget_other(ns, &learned.catalog);
        let found =
            self.entries.iter().position(|e| e.ns == ns && e.learned.shape == learned.shape);
        let entry = match found {
            None => Entry { ns: ns.to_owned(), active: false, created_ms: now_ms, learned },
            Some(i) => {
                let mut e = self.entries.remove(i);
                if e.active || learned.works <= e.learned.works {
                    e = Entry { ns: ns.to_owned(), active: true, created_ms: now_ms, learned };
                } else {
                    e.learned.works = e.learned.works.saturating_mul(WORKS_GROWTH);
                }
                e
            }
        };
        self.entries.insert(0, entry);
    }

    /// Drops the entries of `ns` learned with another catalog than `catalog`.
    pub fn forget_other(&mut self, ns: &str, catalog: &str) {
        self.entries.retain(|e| e.ns != ns || e.learned.catalog == catalog);
    }

    /// Whether `ns` has an active entry of `shape` learned with `catalog`.
    pub fn is_active(&self, ns: &str, shape: &str, catalog: &str) -> bool {
        self.active(ns, shape, catalog).is_some()
    }

    /// The plan of the active entry of `shape` on `ns` learned with `catalog`, and the works of
    /// its trial: what a read of that shape runs without competing plans (`fromPlanCache`). The
    /// entry becomes the newest, as in RefDb's cache of the least recently used.
    pub fn cached(&mut self, ns: &str, shape: &str, catalog: &str) -> Option<(Access, u64)> {
        let i = self.active(ns, shape, catalog)?;
        let entry = self.entries.remove(i);
        let found = (entry.learned.access.clone(), entry.learned.works);
        self.entries.insert(0, entry);
        Some(found)
    }

    /// The position of the active entry of `shape` on `ns` learned with `catalog`.
    fn active(&self, ns: &str, shape: &str, catalog: &str) -> Option<usize> {
        self.entries.iter().position(|e| {
            e.ns == ns && e.active && e.learned.shape == shape && e.learned.catalog == catalog
        })
    }

    /// Drops the entries of `ns`, or only that of `shape`.
    pub fn clear(&mut self, ns: &str, shape: Option<&str>) {
        self.entries.retain(|e| e.ns != ns || shape.is_some_and(|s| e.learned.shape != s));
    }

    /// The entries of `ns` as `$planCacheStats` gives them, on node `shard` of `host`.
    pub fn stats(&self, ns: &str, (host, shard): (&str, &str)) -> Vec<Document> {
        self.entries
            .iter()
            .filter(|e| e.ns == ns)
            .map(|e| {
                let l = &e.learned;
                let hash = super::plan_hash::shape_hash(&l.shape);
                let key = l.key.clone();
                let plan_bytes = l.plan.to_bytes().unwrap_or_default();
                let solution = i64::from(teseladb_hash::crc32c(&plan_bytes));
                let mut d = Document::new()
                    .with("version", "1")
                    .with("queryHash", hash.clone())
                    .with("planCacheShapeHash", hash)
                    .with("planCacheKey", key)
                    .with("isActive", e.active)
                    .with("works", Value::Int64(i64::try_from(l.works).unwrap_or(i64::MAX)))
                    .with("reads", Value::Int64(i64::try_from(l.reads).unwrap_or(i64::MAX)))
                    .with("timeOfCreation", Value::DateTime(e.created_ms))
                    .with("createdFromQuery", l.from.clone())
                    .with("cachedPlan", l.plan.clone())
                    .with(
                        "creationExecStats",
                        l.trials.iter().cloned().map(Value::Document).collect::<Vec<_>>(),
                    )
                    .with(
                        "candidatePlanScores",
                        l.scores.iter().map(|s| Value::Double(*s)).collect::<Vec<_>>(),
                    )
                    .with("indexFilterSet", false);
                let size = d.to_bytes().map_or(0, |b| b.len());
                d.push("estimatedSizeBytes", Value::Int64(i64::try_from(size).unwrap_or(i64::MAX)));
                d.push("solutionHash", Value::Int64(solution));
                d.push("host", host);
                d.push("shard", shard);
                d
            })
            .collect()
    }
}

/// `planCacheClear`: drops the node's entries of the collection, or the one of `query`'s shape
/// with its `sort`, `projection` and `collation`; the reply has the node's own part as a
/// `refrouter` gives its shard's (measured on a `refrouter` 9.0.4).
pub fn plan_cache_clear<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let ns = super::query::target(ctx)?;
    let query = args::doc(ctx.cmd, PLAN_CACHE_CLEAR, "query")?.cloned();
    let sort = args::doc(ctx.cmd, PLAN_CACHE_CLEAR, "sort")?.cloned();
    let projection = args::doc(ctx.cmd, PLAN_CACHE_CLEAR, "projection")?.cloned();
    let collation = args::doc(ctx.cmd, PLAN_CACHE_CLEAR, "collation")?.cloned();
    let shape = match query {
        None if sort.is_some() || projection.is_some() || collation.is_some() => {
            return Err(CommandError::new(
                BAD_VALUE,
                "sort, projection, or collation provided without query",
            ));
        }
        None => None,
        Some(q) => {
            let parsed = super::plan_exec::parsed_filter(&q, None)?;
            Some(read_shape(&parsed, &q, sort.as_ref(), projection.as_ref(), None))
        }
    };
    lock(ctx.shared).plan_cache.clear(&names::full(&ns), shape.as_deref());
    let node = super::shard_name(ctx.backend.node());
    Ok(Document::new().with("ok", 1.0).with(node, Document::new().with("ok", 1.0)))
}

/// The shape of a read as a `refnode` 9.0.4 keys its plan cache ([`super::plan_hash`]): its
/// parsed filter (`original` as the command wrote it), its sort, its projection and the key of a
/// `distinct`, with the simple collation the cached reads have.
pub fn read_shape(
    parsed: &Document,
    original: &Document,
    sort: Option<&Document>,
    projection: Option<&Document>,
    distinct: Option<&str>,
) -> String {
    super::plan_hash::shape_key(&super::plan_hash::ShapeInput {
        parsed: Some(parsed),
        original: Some(original),
        sort,
        projection,
        distinct,
        ..Default::default()
    })
}

/// The collection `coll` and its indexes as the planner sees them, which an entry was learned
/// with.
pub fn catalog_signature(
    coll: &teseladb_meta::catalog::CollectionEntry,
    indexes: &[super::plan_exec::PlanIndex],
) -> String {
    let names: Vec<String> = indexes
        .iter()
        .map(|i| format!("{}:{}:{}", i.entry.id, i.entry.name, i.meta.plannable))
        .collect();
    format!("{}|{}", coll.id, names.join(","))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn learned(shape: &str, works: u64) -> Learned {
        Learned {
            shape: shape.to_owned(),
            key: String::new(),
            catalog: String::new(),
            from: Document::new(),
            works,
            reads: works,
            plan: Document::new(),
            trials: Vec::new(),
            scores: vec![1.5],
            access: Access::Collscan,
        }
    }

    #[test]
    fn only_an_active_entry_of_the_same_catalog_gives_its_plan() {
        let mut cache = PlanCache::default();
        cache.learn("d.c", learned("s", 4), 1);
        assert_eq!(cache.cached("d.c", "s", ""), None);
        cache.learn("d.c", learned("s", 4), 2);
        assert_eq!(cache.cached("d.c", "s", ""), Some((Access::Collscan, 4)));
        assert_eq!(cache.cached("d.c", "s", "other"), None);
        assert_eq!(cache.cached("d.c", "t", ""), None);
        assert_eq!(cache.cached("d.o", "s", ""), None);
        cache.learn("d.c", learned("t", 4), 3);
        assert_eq!(cache.cached("d.c", "s", ""), Some((Access::Collscan, 4)));
        // The entry it used is the newest and keeps the time it was created.
        let stats = cache.stats("d.c", ("h", "s"));
        let created: Vec<Option<&Value>> = stats.iter().map(|d| d.get("timeOfCreation")).collect();
        assert_eq!(created, [Some(&Value::DateTime(2)), Some(&Value::DateTime(3))]);
    }

    #[test]
    fn an_entry_becomes_active_when_a_read_works_no_more() {
        let mut c = PlanCache::default();
        c.learn("d.c", learned("a", 4), 1);
        assert_eq!(c.stats("d.c", ("h", "s"))[0].get("isActive"), Some(&Value::Boolean(false)));
        c.learn("d.c", learned("a", 9), 2);
        assert_eq!(c.stats("d.c", ("h", "s"))[0].get("works"), Some(&Value::Int64(8)));
        c.learn("d.c", learned("a", 3), 3);
        let s = c.stats("d.c", ("h", "s"));
        assert_eq!(s[0].get("isActive"), Some(&Value::Boolean(true)));
        assert_eq!(s[0].get("works"), Some(&Value::Int64(3)));
        c.learn("d.c", learned("a", 30), 4);
        assert_eq!(c.stats("d.c", ("h", "s"))[0].get("works"), Some(&Value::Int64(30)));
    }

    #[test]
    fn clearing_drops_a_shape_or_a_collection() {
        let mut c = PlanCache::default();
        c.learn("d.c", learned("a", 1), 1);
        c.learn("d.c", learned("b", 1), 1);
        c.learn("d.o", learned("a", 1), 1);
        c.clear("d.c", Some("a"));
        assert_eq!(c.stats("d.c", ("h", "s")).len(), 1);
        c.clear("d.c", None);
        assert!(c.stats("d.c", ("h", "s")).is_empty());
        assert_eq!(c.stats("d.o", ("h", "s")).len(), 1);
    }

    #[test]
    fn shapes_keep_operators_and_drop_values() {
        let parsed = |n: i32| {
            Document::new().with(
                "$and",
                vec![
                    Value::Document(Document::new().with("g", Document::new().with("$eq", n))),
                    Value::Document(Document::new().with("n", Document::new().with("$gt", n))),
                ],
            )
        };
        let shape = |n| read_shape(&parsed(n), &parsed(n), None, None, None);
        assert_eq!(shape(1), shape(5));
        assert_eq!(shape(1), "an[eqg,gtn]||||ftr");
    }
}
