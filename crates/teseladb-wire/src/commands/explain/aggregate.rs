//! `explain` of `aggregate` (010 FR-003, T018), in the shape a `refrouter` 9.0.4 gives it (measured
//! 01-11-2026): the leading stages a query plan runs, and the others after it under `stages`.
//!
//! The query plan takes the leading `$match`es as its filter, then `$sort`, `$skip`, `$limit`
//! and the projections. When the stage after them is a `$group` or an equality `$lookup` that
//! RefDb's slot-based engine runs (and every stage before it), the plan is that engine's
//! (`explainVersion` "2"): it takes the `$group`s, `$lookup`s and projections that follow, and
//! numbers its stages (`planNodeId`). Under the plan go only the fields the rest of the pipeline
//! uses (RefDb's dependency analysis): a projection, below a sort that it covers and above one
//! it does not; the engine shows no projection of fields right under a `$group`, though it keeps
//! its number.
//!
//! The pipeline is checked first and then read into [`Step`]s, which the plan is built from.

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::pipeline::Pipeline;
use teseladb_meta::catalog::Namespace;

use super::super::mutation::from_mql;
use super::super::query::check_filter;
use super::super::{Backend, Ctx, names};
use super::forms::{fill_values, off, project, unset_projection};
use super::moves::{rename_of, split_before};
use super::steps::{Kind, Lookup, Step, UnionWith, Window, docs};
use super::{
    NodeParts, Read, SORT_MEM_LIMIT, Stage, Verbosity, collscan, extend, long, matches_nothing,
    node_parts, parsed, scan, simple_projection, skip_stage, sort_counters,
};
use crate::error::CommandError;

/// A count as the stages after a plan write it: a 64-bit integer.
pub(super) fn int64(n: u64) -> Value {
    Value::Int64(i64::try_from(n).unwrap_or(i64::MAX))
}

/// How many leading stages are the query's (its filter, sort, skip, limit and projections);
/// `engine`: whether the slot-based engine's plan follows, which takes every kind of projection
/// (the classic plan, only one that keeps fields).
fn query_part(steps: &[Step], engine: bool) -> usize {
    let mut i = leading_matches(steps);
    let mut sorted = false;
    let mut cut = false;
    let mut projected = false;
    while let Some(st) = steps.get(i) {
        match &st.kind {
            Kind::Sort(_) if !sorted && !cut && !projected => sorted = true,
            Kind::Skip(_) | Kind::Limit(_) if !projected => cut = true,
            _ if engine && st.is_projection() => projected = true,
            Kind::Unset(_) if !projected => projected = true,
            // A `$project` that computes fields only right after the filter (classic plan).
            Kind::Project(p) if !projected && (fields_only(p) || (!sorted && !cut)) => {
                projected = true;
            }
            _ => break,
        }
        i += 1;
    }
    i
}

/// Whether a `$project` only keeps or drops fields (dotted paths too).
fn fields_only(p: &Document) -> bool {
    p.iter().all(|(_, v)| {
        matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Boolean(_))
    })
}

/// The query a pipeline of `stages` starts with, as the planner reads it: its leading filter
/// (with the `$match`es that move before the stages ahead of them), its sort and the documents it
/// reads at most.
pub(crate) fn planned_query(stages: &[Document]) -> (Document, Option<Document>, Option<u64>) {
    let steps = swapped(stages.iter().cloned().map(Step::new).collect());
    let i = leading_matches(&steps);
    let first = query_part(&steps, false);
    let mut filter = Document::new();
    for st in &steps[..i] {
        if let Kind::Match(m) = &st.kind {
            for (k, v) in m.iter() {
                filter.push(k, v.clone());
            }
        }
    }
    let (sort, limit) = sort_and_limit(&steps[i..first]);
    (filter, sort, limit)
}

/// The sort of the query's stages `steps` and the documents it reads at most (its skip and
/// limit), as the planner takes them.
fn sort_and_limit(steps: &[Step]) -> (Option<Document>, Option<u64>) {
    let mut sort = None;
    let (mut skip, mut limit) = (0u64, None::<u64>);
    for st in steps {
        match &st.kind {
            Kind::Sort(s) => sort = Some(s.clone()),
            Kind::Skip(n) => {
                skip = skip.saturating_add(*n);
                limit = limit.map(|l: u64| l.saturating_sub(*n));
            }
            Kind::Limit(n) => limit = Some(limit.map_or(*n, |l| l.min(*n))),
            _ => {}
        }
    }
    (sort, limit.map(|l| skip.saturating_add(l)))
}

/// How many `$match`es the pipeline starts with.
fn leading_matches(steps: &[Step]) -> usize {
    steps.iter().take_while(|s| matches!(s.kind, Kind::Match(_))).count()
}

/// Whether the slot-based engine starts its plan with stage `st` after the query's own: a
/// `$group` in any of its forms, or an equality `$lookup`.
fn starts_engine(st: &Step) -> bool {
    match &st.kind {
        Kind::Group(_) | Kind::Bucket(_) | Kind::SortByCount(_) | Kind::Count(_) => true,
        Kind::Lookup(l) => l.is_equality(),
        _ => false,
    }
}

/// What the scan of the collection counted.
struct Run {
    examined: u64,
    matched: u64,
}

/// The numbers of an engine plan's stages, as RefDb gives them: in the order it makes them,
/// with the projections it hides counted.
struct Ids {
    next: Option<u32>,
}

impl Ids {
    fn take(&mut self) -> Option<u32> {
        let id = self.next;
        self.next = id.map(|n| n + 1);
        id
    }
}

/// `stage` numbered `id` over `input`.
fn numbered(mut stage: Stage, id: Option<u32>, input: Option<Stage>) -> Stage {
    stage.id = id;
    match input {
        Some(i) => stage.over(i),
        None => stage,
    }
}

/// `aggregate`: the query plan of its leading stages, the others after it under `stages` (a
/// `$cursor` first), in the shape of a `refrouter` (`splitPipeline: null` and one section per
/// node).
pub(super) fn aggregate<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    inner: &Document,
    verbosity: Verbosity,
    command: Document,
) -> Result<Document, CommandError> {
    let stages: Vec<Document> = match inner.get("pipeline") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Document(d) => Ok(d.clone()),
                _ => Err(CommandError::new(
                    crate::error::TYPE_MISMATCH,
                    "Each element of the 'pipeline' array must be an object",
                )),
            })
            .collect::<Result<_, _>>()?,
        _ => Vec::new(),
    };
    let pipeline = checked(&stages)?;
    let steps = swapped(stages.into_iter().map(Step::new).collect());
    let engine_from = query_part(&steps, true);
    let engine = engine_runs(&pipeline, &steps, engine_from);
    let first = if engine { engine_from } else { query_part(&steps, false) };
    let i = leading_matches(&steps);
    let mut filter = Document::new();
    let leading = steps[..i].iter().filter_map(|s| match &s.kind {
        Kind::Match(m) => Some(m),
        _ => None,
    });
    for (k, v) in leading.flat_map(Document::iter) {
        filter.push(k, v.clone());
    }
    let coll = ctx.backend.collection(ns)?;
    let run = verbosity >= Verbosity::ExecutionStats;
    let read = Read {
        filter: &filter,
        collation: None,
        vars: None,
        concern: teseladb_range::read::ReadConcern::Local,
    };
    check_filter(&filter, None, None)?;
    let stop = scan_stop(&steps[i..first]);
    let (examined, matched) =
        if run { scan(ctx, coll.as_ref(), &read, stop, |_| Ok(()))? } else { (0, 0) };
    let shown = parsed(&filter, None)?;
    // The planner chooses how the filter reads, with the query's sort and limit (measured: an
    // `_id` range through its index, a sort on `_id` read backward).
    let (sort, limit) = sort_and_limit(&steps[i..first]);
    let plan_read = super::super::plan_exec::ReadPlan {
        filter: &filter,
        parsed: &shown,
        sort: sort.as_ref().map(super::sort_fields).unwrap_or_default(),
        limit,
        hint: None,
        vars: None,
        concern: teseladb_range::read::ReadConcern::Local,
        intersect: super::super::lock(ctx.shared).params.hash_intersection,
        index_keys: false,
    };
    let chosen = super::planned_read(ctx, coll.as_ref(), &plan_read, None)?;
    let access = chosen.as_ref().map(|(indexes, c)| {
        (
            (
                super::planned_access(ns, &shown, indexes, &c.winner, run, None),
                super::sorted(&c.winner),
            ),
            c.others
                .iter()
                .map(|r| {
                    (super::planned_access(ns, &shown, indexes, r, false, None), super::sorted(r))
                })
                .collect::<Vec<(Stage, bool)>>(),
        )
    });
    let (mut input, rejected) = match access {
        Some((a, r)) => (Some(a), r),
        None => (None, Vec::new()),
    };
    let counted = Run { examined, matched };
    let mut ids = Ids { next: engine.then_some(1) };
    let mut foreign = Vec::new();
    if run {
        for st in steps.iter().skip(first) {
            if let Kind::Lookup(Lookup { from: Some(from), .. }) = &st.kind {
                let n = through(ctx, &names::ns(&ns.db, from), &[])?;
                foreign.push((from.clone(), n));
            }
        }
    }
    let (plan, last, extra) = if engine {
        let base = query_plan(ns, &steps, (i, first), &shown, &counted, &mut ids, input.take());
        engine_plan(ns, &steps, (first, &foreign), base, &mut ids)?
    } else {
        let mut plan = query_plan(ns, &steps, (i, first), &shown, &counted, &mut ids, input.take());
        if makes_sort_keys(&steps[first..]) {
            let mut keys = Stage::new("SORT_KEY_GENERATOR");
            keys.returned = plan.returned;
            plan = keys.over(plan);
        }
        (plan, first, Vec::new())
    };
    let mut remaining: Vec<(Document, usize)> =
        extra.into_iter().map(|d| (d, 0)).chain(listed(&steps[last..])).collect();
    for (d, _) in &mut remaining {
        if let Kind::UnionWith(u) = Step::new(d.clone()).kind {
            *d = union_with(ctx, ns, &u, verbosity)?;
        }
        if let Some(out) = d.get("$out").cloned() {
            *d = Document::new().with("$out", out_target(&out, &ns.db));
        }
    }
    let mut planner = Document::new()
        .with("namespace", names::full(ns))
        .with("parsedQuery", shown.clone())
        .with("indexFilterSet", false)
        .with("optimizationTimeMillis", 0);
    if remaining.is_empty() {
        planner.push("optimizedPipeline", true);
    } else {
        planner.push("cursorType", "regular");
    }
    for k in [
        "maxIndexedOrSolutionsReached",
        "maxIndexedAndSolutionsReached",
        "maxScansToExplodeReached",
        "prunedSimilarIndexes",
    ] {
        planner.push(k, false);
    }
    let mut winning = Document::new().with("isCached", false);
    if engine {
        winning.push("queryPlan", plan.planned());
    } else {
        extend(&mut winning, &plan.planned());
    }
    planner.push("winningPlan", winning);
    // Each plan the planner rejected, under the query's own stages.
    let rejected: Vec<Value> = rejected
        .into_iter()
        .map(|access| {
            let mut none = Ids { next: None };
            let other =
                query_plan(ns, &steps, (i, first), &shown, &counted, &mut none, Some(access));
            let mut d = Document::new().with("isCached", false);
            extend(&mut d, &other.planned());
            Value::Document(d)
        })
        .collect();
    planner.push("rejectedPlans", rejected);
    let mut cursor = Document::new().with("queryPlanner", planner);
    let returned =
        if run && engine { through(ctx, ns, &docs(&steps[..last]))? } else { plan.returned };
    if run {
        let mut executed = Document::new().with("isCached", false);
        extend(&mut executed, &plan.executed());
        let mut st = Document::new()
            .with("executionSuccess", true)
            .with("nReturned", long(returned))
            .with("executionTimeMillis", 0)
            .with("totalKeysExamined", long(plan.total("keysExamined").unsigned_abs()))
            .with("totalDocsExamined", long(plan.total("docsExamined").unsigned_abs()))
            .with("executionStages", executed);
        if verbosity == Verbosity::AllPlansExecution {
            st.push("allPlansExecution", Vec::<Value>::new());
        }
        cursor.push("executionStats", st);
    }
    let NodeParts { shard, host, server, params } = node_parts(ctx);
    let version = if engine { "2" } else { "1" };
    let mut section = Document::new().with("host", host).with("explainVersion", version);
    if remaining.is_empty() {
        extend(&mut section, &cursor);
    } else {
        let mut list = Vec::new();
        let mut head = Document::new().with("$cursor", cursor);
        if run {
            head.push("nReturned", int64(returned));
            head.push("executionTimeMillisEstimate", Value::Int64(0));
        }
        list.push(Value::Document(head));
        let mut upto: Vec<Document> = docs(&steps[..last]);
        let mut next = last;
        for (d, span) in remaining {
            let mut d = d;
            if run {
                let before = upto.clone();
                upto.extend(docs(&steps[next..next + span]));
                let mut more = Document::new();
                match steps.get(next).filter(|_| span > 0).map(|s| (s, &s.kind)) {
                    Some((_, Kind::Facet(branches))) => {
                        let counts = facet_counts(ctx, ns, &before, branches)?;
                        d = Document::new().with("$facet", counts);
                    }
                    Some((st, Kind::Lookup(l))) => (d, more) = lookup_run(ctx, ns, &before, st, l)?,
                    _ => {}
                }
                next += span;
                let n = through(ctx, ns, &upto)?;
                d = with_counters(d, n);
                extend(&mut d, &more);
            }
            list.push(Value::Document(d));
        }
        section.push("stages", list);
    }
    let mut out = Document::new()
        .with("serverInfo", server)
        .with("serverParameters", params)
        .with("splitPipeline", Value::Null)
        .with("shards", Document::new().with(shard.as_str(), section));
    if let Some(hash) = super::super::query_shape::explained_hash(ctx.db, &command) {
        out.push("queryShapeHash", hash);
    }
    out.push("command", command);
    out.push("ok", 1.0);
    Ok(out)
}

/// The target of `$out` as `explain` writes it: its collection, then its database (the
/// command's, `db`, when it names none).
fn out_target(spec: &Value, db: &str) -> Document {
    let (coll, named) = match spec {
        Value::Document(d) => (d.get("coll").cloned(), d.get("db").cloned()),
        other => (Some(other.clone()), None),
    };
    Document::new()
        .with("coll", coll.unwrap_or(Value::Null))
        .with("db", named.unwrap_or(Value::from(db)))
}

/// Where a scan under `steps` (the query's own after its filter) stops: after the documents a
/// `$limit` without a `$sort` takes, skipped ones included.
fn scan_stop(steps: &[Step]) -> Option<u64> {
    let (mut skip, mut limit) = (0u64, None::<u64>);
    for st in steps {
        match st.kind {
            Kind::Sort(_) => return None,
            Kind::Skip(n) => {
                skip = skip.saturating_add(n);
                limit = limit.map(|l| l.saturating_sub(n));
            }
            Kind::Limit(n) => limit = Some(limit.map_or(n, |l| l.min(n))),
            _ => {}
        }
    }
    limit.map(|l| skip.saturating_add(l))
}

/// The branches of a `$facet` as `explain` lists them run: each stage with the documents it
/// gave, after the stages `before` it.
fn facet_counts<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    before: &[Document],
    branches: &[(String, Vec<Step>)],
) -> Result<Document, CommandError> {
    let entering = through(ctx, ns, before)?;
    let mut out = Document::new();
    for (name, stages) in branches {
        let tee = Document::new().with("$internalFacetTeeConsumer", Document::new());
        let mut list = vec![Value::Document(with_counters(tee, entering))];
        for k in 0..stages.len() {
            let prefix =
                Value::Array(docs(&stages[..=k]).into_iter().map(Value::Document).collect());
            let mut run = before.to_vec();
            run.push(Document::new().with("$facet", Document::new().with("x", prefix)));
            let found = super::super::aggregate::read_through(
                ctx.backend,
                ns,
                &run,
                None,
                teseladb_range::read::ReadConcern::Local,
            )?;
            // The one document of the `$facet`, with the branch's documents.
            let n = match found.first().and_then(|d| d.get("x")) {
                Some(Value::Array(items)) => u64::try_from(items.len()).unwrap_or(u64::MAX),
                _ => 0,
            };
            for e in super::forms::forms(&stages[k]) {
                list.push(Value::Document(with_counters(e, n)));
            }
        }
        out.push(name, Value::Array(list));
    }
    Ok(out)
}

/// A `$lookup` listed after the plan, run after the stages `before` it (measured): its form with
/// the values its variables took for the last document, and what it counted; the pipeline of
/// one that reads no field of the document runs once.
fn lookup_run<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    before: &[Document],
    st: &Step,
    l: &Lookup,
) -> Result<(Document, Document), CommandError> {
    let inputs = super::super::aggregate::read_through(
        ctx.backend,
        ns,
        before,
        None,
        teseladb_range::read::ReadConcern::Local,
    )?;
    let vars = lookup_vars(l, inputs.last())?;
    let form = super::forms::forms_with(st, vars.as_ref()).into_iter().next().unwrap_or_default();
    let entering = u64::try_from(inputs.len()).unwrap_or(u64::MAX);
    let correlated = l.local_field.is_some() || vars.is_some();
    let (scans, foreign) = match (&l.from, correlated) {
        (None, _) => (0, 0),
        (Some(from), true) => (entering, through(ctx, &names::ns(&ns.db, from), &[])?),
        (Some(from), false) => (entering.min(1), through(ctx, &names::ns(&ns.db, from), &[])?),
    };
    let counters = Document::new()
        .with("totalDocsExamined", int64(scans.saturating_mul(foreign)))
        .with("totalKeysExamined", int64(0))
        .with("collectionScans", int64(scans))
        .with("indexesUsed", Vec::<Value>::new())
        .with("expressionEvaluationPeakMemoryBytes", int64(0));
    Ok((form, counters))
}

/// The values of a `$lookup`'s variables (`let`) for document `doc`, when it has any.
fn lookup_vars(l: &Lookup, doc: Option<&Document>) -> Result<Option<Document>, CommandError> {
    let lets = match (&l.let_vars, doc) {
        (Some(Value::Document(lets)), Some(doc)) if !lets.is_empty() => (lets, doc),
        _ => return Ok(None),
    };
    let (lets, doc) = lets;
    let holder = lets.to_bytes()?;
    let raw = RawDoc::from_bytes(&holder)?;
    let scope = teseladb_exec::expr::Scope::new(doc.clone(), None);
    let mut vars = Document::new();
    for (name, e) in raw.iter() {
        let expr = teseladb_exec::expr::Expr::parse(e).map_err(from_mql)?;
        let value = expr.eval(doc, &scope).map_err(from_mql)?;
        vars.push(name, value.unwrap_or(Value::Null));
    }
    Ok(Some(vars))
}

/// What a `$sort` and a `$group` listed after the plan count when they run, beside the
/// documents they give (`nReturned`): their memory, and that they never spilled.
fn with_counters(d: Document, returned: u64) -> Document {
    let n = int64(returned);
    let spilled = ["spilledBytes", "spilledRecords", "spilledDataStorageSize"];
    if d.get("$sort").is_some() {
        let mut out =
            d.with("totalDataSizeSortedBytesEstimate", Value::Int64(0)).with("usedDisk", false);
        out.push("spills", Value::Int64(0));
        for k in spilled {
            out.push(k, Value::Int64(0));
        }
        return out.with("nReturned", n).with("executionTimeMillisEstimate", Value::Int64(0));
    }
    let mut out = d.with("nReturned", n).with("executionTimeMillisEstimate", Value::Int64(0));
    if let Some(Value::Document(g)) = out.get("$group").cloned() {
        let mut memory = Document::new();
        for (k, _) in g.iter().filter(|(k, _)| *k != "_id" && *k != "$willBeMerged") {
            memory.push(k, Value::Int64(0));
        }
        out.push("maxAccumulatorMemoryUsageBytes", memory);
        out.push("totalOutputDataSizeBytes", Value::Int64(0));
        out.push("usedDisk", false);
        out.push("spills", Value::Int64(0));
        out.push("spilledDataStorageSize", Value::Int64(0));
        out.push("spilledBytes", Value::Int64(0));
        out.push("spilledRecords", Value::Int64(0));
    }
    out
}

/// The path a `$group` groups by, when its key is one.
fn group_path(st: &Step) -> Option<String> {
    let Kind::Group(entries) = &st.kind else { return None };
    entries.iter().find_map(|e| match e {
        super::steps::GroupEntry::Key(k) => rename_of(k).map(str::to_owned),
        super::steps::GroupEntry::Field(..) => None,
    })
}

/// The pipeline as RefDb rewrites it before planning: a `$sort` before each
/// `$setWindowFields`, `$fill` and `$densify` that sorts, each `$sort` after an `$unwind` that
/// it does not read moved before it, `$skip` and `$limit` before the stages that change each
/// document on its own, each part of a `$match` that can run before the stage ahead of it moved
/// there, and each `$match` on `_id` right after a `$group` by a path moved before it, on that
/// path.
pub(super) fn swapped(steps: Vec<Step>) -> Vec<Step> {
    let mut steps: Vec<Step> = steps
        .into_iter()
        .map(without_constant_partition)
        .filter(|st| !matches!(&st.kind, Kind::Match(m) if m.is_empty()))
        .flat_map(|st| match sort_before(&st) {
            Some(key) => vec![Step::new(Document::new().with("$sort", key)), st],
            None => vec![st],
        })
        .collect();
    let mut k = 0;
    while k + 1 < steps.len() {
        // A `$sort` right before another gives way to it.
        if matches!(steps[k].kind, Kind::Sort(_)) && matches!(steps[k + 1].kind, Kind::Sort(_)) {
            steps.remove(k);
            k = k.saturating_sub(1);
            continue;
        }
        // Two projections that drop fields are one.
        if let (Some(a), Some(b)) = (dropped(&steps[k]), dropped(&steps[k + 1])) {
            steps.remove(k + 1);
            steps[k] = merged_exclusion(a, b);
            continue;
        }
        let by = group_path(&steps[k]);
        let on_id = match &steps[k + 1].kind {
            Kind::Match(m) if m.len() == 1 => m.get("_id").cloned(),
            _ => None,
        };
        if let Kind::Unwind(u) = &steps[k].kind
            && sort_moves_before_unwind(&steps[k + 1], u.field())
            && u.index.as_deref().is_none_or(|i| sort_moves_before_unwind(&steps[k + 1], i))
        {
            let moved = steps.remove(k + 1);
            steps.insert(k, moved);
            k = k.saturating_sub(1);
            continue;
        }
        // `$skip` and `$limit` run before a stage that changes each document on its own.
        if matches!(steps[k + 1].kind, Kind::Skip(_) | Kind::Limit(_)) && steps[k].is_one_to_one() {
            let moved = steps.remove(k + 1);
            steps.insert(k, moved);
            k = k.saturating_sub(1);
            continue;
        }
        if let Kind::Match(m) = &steps[k + 1].kind
            && let Some((moved, kept)) = split_before(&steps[k], m)
        {
            steps.remove(k + 1);
            if let Some(rest) = kept {
                steps.insert(k + 1, Step::new(Document::new().with("$match", rest)));
            }
            steps.insert(k, Step::new(Document::new().with("$match", moved)));
            k = k.saturating_sub(1);
            continue;
        }
        if let (Some(path), Some(cond)) = (by, on_id) {
            let moved = Document::new().with("$match", Document::new().with(path.as_str(), cond));
            steps.remove(k + 1);
            steps.insert(k, Step::new(moved));
            k = k.saturating_sub(1);
            continue;
        }
        k += 1;
    }
    steps
}

/// The specification of a projection that only drops fields (`$unset`, or a `$project` that
/// excludes).
fn dropped(st: &Step) -> Option<Document> {
    match &st.kind {
        Kind::Unset(fields) => Some(fields.iter().fold(Document::new(), |d, f| d.with(f, 0))),
        Kind::Project(p) if !super::moves::inclusion(p) => Some(p.clone()),
        _ => None,
    }
}

/// The projection that drops what `a` and then `b` drop: an `$unset` unless one of them says
/// what becomes of `_id`.
fn merged_exclusion(a: Document, b: Document) -> Step {
    let mut spec = a;
    for (k, v) in b.iter() {
        spec.insert(k, v.clone());
    }
    let doc = Document::new().with("$project", spec.clone());
    match spec.get("_id") {
        Some(_) => Step { doc, kind: Kind::Project(spec) },
        None => {
            let fields = spec.iter().map(|(k, _)| k.to_owned()).collect();
            Step { doc, kind: Kind::Unset(fields) }
        }
    }
}

/// A `$setWindowFields` without a partition by a constant, which RefDb drops.
fn without_constant_partition(st: Step) -> Step {
    match &st.kind {
        Kind::SetWindowFields(w) if super::forms::constant_partition(w) => {
            let mut out = Document::new();
            for (k, v) in w.spec.iter().filter(|(k, _)| *k != "partitionBy") {
                out.push(k, v.clone());
            }
            Step::new(Document::new().with("$setWindowFields", out))
        }
        _ => st,
    }
}

/// Whether the stages after the query's own reach a `$setWindowFields` by an expression with an
/// order through `$unwind`s only, for which RefDb's query plan makes the sort keys
/// (`SORT_KEY_GENERATOR`, measured).
fn makes_sort_keys(rest: &[Step]) -> bool {
    for st in rest {
        match &st.kind {
            Kind::Unwind(_) => {}
            Kind::SetWindowFields(w) => {
                return w.sort_by.is_some()
                    && super::forms::partition_expression(w.partition_by.as_ref()).is_some();
            }
            _ => return false,
        }
    }
    false
}

/// The sort of a window over `partition_by` in the order of `sort_by`: the partition's path,
/// then the order (`None` for a partition by an expression).
fn window_sort(partition_by: Option<&Value>, sort_by: Option<&Document>) -> Option<Document> {
    let mut key = Document::new();
    if let Some(p) = partition_by {
        key.push(rename_of(p)?, 1);
    }
    for (k, v) in sort_by.iter().flat_map(|s| s.iter()) {
        key.push(k, v.clone());
    }
    Some(key)
}

/// The sort a `$setWindowFields`, a `$fill` or a `$densify` starts with: its partition's
/// fields, then its own order (`None` for any other stage, or a partition by an expression).
fn sort_before(st: &Step) -> Option<Document> {
    let key = match &st.kind {
        Kind::SetWindowFields(Window { partition_by, sort_by, .. }) => {
            window_sort(partition_by.as_ref(), sort_by.as_ref())?
        }
        Kind::Fill(f) if !f.fills_values() => {
            window_sort(f.partition_by.as_ref(), f.sort_by.as_ref())?
        }
        Kind::Densify(d) => {
            let mut key = Document::new();
            for f in d.partition_by_fields.iter().chain([&d.field]) {
                key.push(f, 1);
            }
            key
        }
        _ => return None,
    };
    (!key.is_empty()).then_some(key)
}

/// `$unionWith` as `explain` lists it: the collection and the explain of its pipeline on it.
fn union_with<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    u: &UnionWith,
    verbosity: Verbosity,
) -> Result<Document, CommandError> {
    let coll = u.coll.clone().unwrap_or(ns.coll.clone());
    let inner = Document::new()
        .with("aggregate", coll.as_str())
        .with("pipeline", Value::Array(u.pipeline.clone()))
        .with("cursor", Document::new());
    let sub = aggregate(ctx, &names::ns(&ns.db, &coll), &inner, verbosity, Document::new())?;
    let explained = Document::new()
        .with("splitPipeline", sub.get("splitPipeline").cloned().unwrap_or(Value::Null))
        .with("shards", sub.get("shards").cloned().unwrap_or(Value::Null));
    Ok(Document::new().with(
        "$unionWith",
        Document::new().with("coll", coll.as_str()).with("pipeline", explained),
    ))
}

/// Whether a `$sort` after an `$unwind` of `path` reads none of it, so RefDb runs it before.
fn sort_moves_before_unwind(st: &Step, path: &str) -> bool {
    matches!(&st.kind, Kind::Sort(s) if !s.iter().any(|(k, _)| super::moves::touches(k, path)))
}

/// How many documents the pipeline's first stages give.
fn through<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    stages: &[Document],
) -> Result<u64, CommandError> {
    let n = super::super::aggregate::read_through(
        ctx.backend,
        ns,
        stages,
        None,
        teseladb_range::read::ReadConcern::Local,
    )?
    .len();
    Ok(u64::try_from(n).unwrap_or(u64::MAX))
}

/// The pipeline, parsed and optimised as the command would.
/// Whether the slot-based engine runs the query plan of `steps` of `pipeline`, from step
/// `engine_from` on: a `$group` or an equality `$lookup` there, every stage before it in the
/// engine, and a filter that may match something (one that matches nothing is the classic
/// `EOF`, measured).
fn engine_runs(pipeline: &Pipeline, steps: &[Step], engine_from: usize) -> bool {
    let nothing = match steps.first().map(|s| &s.kind) {
        Some(Kind::Match(d)) => parsed(d, None).is_ok_and(|p| matches_nothing(&p)),
        _ => false,
    };
    !nothing
        && pipeline.sbe_prefix() > engine_from
        && steps.get(engine_from).is_some_and(starts_engine)
}

/// The framework that runs pipeline `stages` on a collection, as the profiler's
/// `queryFramework` names it: `sbe` when the slot-based engine runs its query plan, `classic`
/// otherwise (measured on a `refnode` 9.0.4, 01-11-2026).
pub(crate) fn query_framework(stages: &[Document]) -> &'static str {
    let Ok(pipeline) = checked(stages) else { return "classic" };
    let steps = swapped(stages.iter().cloned().map(Step::new).collect());
    let engine_from = query_part(&steps, true);
    match engine_runs(&pipeline, &steps, engine_from) {
        true => "sbe",
        false => "classic",
    }
}

fn checked(stages: &[Document]) -> Result<Pipeline, CommandError> {
    let holder = Document::new()
        .with("p", Value::Array(stages.iter().cloned().map(Value::Document).collect()));
    let hb = holder.to_bytes()?;
    let raws: Vec<RawValue<'_>> = match RawDoc::from_bytes(&hb)?.get("p") {
        Some(RawValue::Array(a)) => a.iter().collect(),
        _ => Vec::new(),
    };
    let pipeline = Pipeline::parse_command(&raws, &teseladb_exec::expr::Ctx::checked([]), true)
        .map_err(from_mql)?;
    teseladb_exec::pipeline::optimize::optimize_stages(&raws, None).map_err(from_mql)?;
    Ok(pipeline)
}

/// The plan of the query's own stages (`steps[from..to]` after the `$match`es): the scan, then
/// the sort, skip and limit, with the projection of the fields the rest uses, and the
/// projections.
fn query_plan(
    ns: &Namespace,
    steps: &[Step],
    (from, to): (usize, usize),
    shown: &Document,
    counted: &Run,
    ids: &mut Ids,
    access: Option<(Stage, bool)>,
) -> Stage {
    let engine = ids.next.is_some();
    let indexed = access.is_some();
    // An index that reads in the sort's order leaves no sort to do.
    let index_sorts = access.as_ref().is_some_and(|(_, sorts)| *sorts);
    let scan_id = if indexed { None } else { ids.take() };
    let mut plan = match access {
        Some((a, _)) => number_chain(a, ids),
        None => collscan(ns, shown, counted.examined, counted.matched),
    };
    if matches_nothing(shown) {
        plan.id = scan_id;
        return plan;
    }
    // Under a rooted `$or`, the `SUBPLAN` stands over the whole plan.
    let subplan = match (plan.name, plan.input.take()) {
        ("SUBPLAN", Some(scan)) => Some(std::mem::replace(&mut plan, *scan)),
        (_, input) => {
            plan.input = input;
            None
        }
    };
    if !indexed {
        plan.id = scan_id;
    }
    if engine && !indexed && plan.head.get("filter").is_none() {
        plan.head.push("filter", Document::new());
    }
    let mut sort: Option<Document> = None;
    let (mut skip, mut limit) = (0u64, None::<u64>);
    let mut projections: Vec<&Step> = Vec::new();
    for st in &steps[from..to] {
        match &st.kind {
            Kind::Sort(_) if index_sorts => {}
            Kind::Sort(s) => sort = Some(s.clone()),
            Kind::Skip(n) => {
                skip = skip.saturating_add(*n);
                limit = limit.map(|l| l.saturating_sub(*n));
            }
            Kind::Limit(n) => limit = Some(limit.map_or(*n, |l| l.min(*n))),
            _ => projections.push(st),
        }
    }
    // A projection of fields right after a sort without a limit runs below it when it keeps
    // the sort's fields (classic plan, measured).
    let pushed = match (&sort, projections.first()) {
        (Some(s), Some(p)) if !engine && limit.is_none() && keeps_sort(p, s) => {
            Some(projections.remove(0))
        }
        _ => None,
    };
    let first_projection =
        steps[from..to].iter().position(Step::is_projection).map_or(to, |p| from + p);
    let user_keeps = pushed.or(projections.first().copied()).is_some_and(keeps_fields);
    let fields = if user_keeps {
        None
    } else {
        teseladb_exec::pipeline::deps::needed(&docs(&steps[first_projection..]))
            .filter(|f| !f.is_empty())
    };
    let covers_sort = |s: &Document, f: &[String]| s.iter().all(|(k, _)| f.iter().any(|x| x == k));
    let below = skip == 0
        && match (&sort, &fields) {
            (Some(s), Some(f)) => covers_sort(s, f),
            (None, Some(_)) => true,
            _ => false,
        };
    let fields_stage = |f: &[String], ids: &mut Ids| {
        let dotted = f.iter().any(|x| x.contains('.'));
        let mut p = Stage::new(if dotted { "PROJECTION_DEFAULT" } else { "PROJECTION_SIMPLE" });
        p.tail.push("transformBy", dependency_projection(f, engine));
        p.id = ids.take();
        p
    };
    if let Some(p) = pushed {
        let mut st = projection(p, engine);
        st.returned = plan.returned;
        plan = st.over(plan);
    }
    let (under, over) = match fields {
        Some(f) if below => (Some(f), None),
        f => (None, f),
    };
    if let Some(f) = &under {
        let mut p = fields_stage(f, ids);
        p.returned = plan.returned;
        plan = p.over(plan);
    }
    if let Some(s) = &sort {
        let mut st = Stage::new("SORT");
        st.tail.push("sortPattern", s.clone());
        st.tail.push("memLimit", SORT_MEM_LIMIT);
        let bound = limit.map(|l| skip.saturating_add(l));
        if let Some(b) = bound {
            st.tail.push("limitAmount", long(b));
        }
        st.tail.push("type", "simple");
        sort_counters(&mut st);
        st.returned = bound.map_or(plan.returned, |b| plan.returned.min(b));
        st.id = ids.take();
        plan = st.over(plan);
    }
    if skip > 0 {
        let id = ids.take();
        plan = skip_stage(skip, plan);
        plan.id = id;
    }
    if let Some(f) = &over {
        let mut p = fields_stage(f, ids);
        p.returned = plan.returned;
        plan = p.over(plan);
    }
    for p in projections {
        let id = ids.take();
        let mut st = projection(p, engine);
        st.returned = plan.returned;
        plan = numbered(st, id, Some(plan));
    }
    // A limit without a sort stands over the projections.
    if sort.is_none()
        && let Some(l) = limit
    {
        let mut st = Stage::new("LIMIT");
        st.tail.push("limitAmount", long(l));
        st.returned = plan.returned.min(l);
        st.id = ids.take();
        plan = st.over(plan);
    }
    match subplan {
        Some(mut sub) => {
            sub.returned = plan.returned;
            sub.over(plan)
        }
        None => plan,
    }
}

/// `stage` and the stages under it numbered from the bottom, as the engine numbers an index
/// scan and its fetch.
fn number_chain(mut stage: Stage, ids: &mut Ids) -> Stage {
    if let Some(input) = stage.input.take() {
        stage.input = Some(Box::new(number_chain(*input, ids)));
    }
    stage.id = ids.take();
    stage
}

/// Whether projection `p`, which only keeps or drops fields, leaves the fields of sort `s` as
/// they are.
fn keeps_sort(p: &Step, s: &Document) -> bool {
    let touched = |f: &str| s.iter().any(|(k, _)| super::moves::touches(k, f));
    match &p.kind {
        Kind::Unset(fields) => !fields.iter().any(|f| touched(f)),
        Kind::Project(spec) if fields_only(spec) && super::moves::inclusion(spec) => {
            s.iter().all(|(k, _)| {
                spec.iter().any(|(f, v)| !off(v) && (k == f || k.starts_with(&format!("{f}."))))
            })
        }
        Kind::Project(spec) if fields_only(spec) => !spec.iter().any(|(f, v)| off(v) && touched(f)),
        _ => false,
    }
}

/// Whether find projection `p` keeps the fields of sort `s`, so that it runs below the sort.
pub(super) fn projection_keeps_sort(p: &Document, s: &Document) -> bool {
    keeps_sort(&Step::new(Document::new().with("$project", p.clone())), s)
}

/// Whether a projection stage is a `$project` that keeps fields (or computes them), which stands
/// for the projection of the fields the rest uses.
fn keeps_fields(st: &Step) -> bool {
    matches!(&st.kind, Kind::Project(p) if p.iter().any(|(k, v)| k != "_id" && !off(v)))
}

/// The projection of the fields `f` the rest of the pipeline uses, as the classic plan (`1`) or
/// the engine (`true`) writes it, `_id` last unless used.
fn dependency_projection(f: &[String], engine: bool) -> Document {
    let yes = |b: bool| if engine { Value::Boolean(b) } else { Value::Int32(i32::from(b)) };
    let mut d = Document::new();
    let uses_id = f.iter().any(|x| x == "_id");
    if uses_id {
        d.push("_id", yes(true));
    }
    for x in f.iter().filter(|x| *x != "_id") {
        d.push(x, yes(true));
    }
    if !uses_id {
        d.push("_id", yes(false));
    }
    d
}

/// A projection stage of the query plan: a `$project` that only keeps or drops fields as
/// `PROJECTION_SIMPLE`, `$replaceRoot` as `REPLACE_ROOT`, the others as `PROJECTION_DEFAULT`
/// with their expressions as RefDb optimises them.
fn projection(st: &Step, engine: bool) -> Stage {
    match &st.kind {
        Kind::Project(p) if simple_projection(p) => {
            let mut s = Stage::new("PROJECTION_SIMPLE");
            s.tail.push("transformBy", project(p));
            s
        }
        Kind::ReplaceRoot(_) => Stage::new("REPLACE_ROOT"),
        Kind::Unset(fields) => {
            let dotted = fields.iter().any(|f| f.contains('.'));
            let mut s = Stage::new(if dotted { "PROJECTION_DEFAULT" } else { "PROJECTION_SIMPLE" });
            s.tail.push("transformBy", unset_projection(fields, engine));
            s
        }
        Kind::Fill(f) => default_projection(fill_values(f)),
        Kind::Project(p) => default_projection(project(p)),
        Kind::AddFields(p) => {
            let mut d = Document::new();
            for (k, v) in p.iter() {
                d.push(k, super::forms::optimized(v));
            }
            default_projection(d)
        }
        _ => Stage::new("PROJECTION_DEFAULT"),
    }
}

fn default_projection(transform_by: Document) -> Stage {
    let mut s = Stage::new("PROJECTION_DEFAULT");
    s.tail.push("transformBy", transform_by);
    s
}

/// A projection after a `$group` in the engine's plan: every one as `PROJECTION_DEFAULT`.
fn engine_projection(st: &Step) -> Stage {
    match &st.kind {
        Kind::Project(spec) => default_projection(project(spec)),
        Kind::Unset(fields) => default_projection(unset_projection(fields, true)),
        _ => projection(st, true),
    }
}

/// The engine's stages over `base`, from `steps[first]`: its `$group`s, equality `$lookup`s
/// (with the `$unwind` of their array right after), and the projections after them; the stage
/// after the last it takes, and the stages it adds to those listed after the plan.
fn engine_plan(
    ns: &Namespace,
    steps: &[Step],
    (first, foreign_counts): (usize, &[(String, u64)]),
    base: Stage,
    ids: &mut Ids,
) -> Result<(Stage, usize, Vec<Document>), CommandError> {
    let mut plan = base;
    let mut j = first;
    let mut extra = Vec::new();
    while let Some(st) = steps.get(j) {
        match &st.kind {
            Kind::Group(_) | Kind::Bucket(_) | Kind::SortByCount(_) | Kind::Count(_) => {
                plan = hide_fields(plan);
                let id = ids.take();
                plan = numbered(Stage::new("GROUP"), id, Some(plan));
                j += 1;
                match &st.kind {
                    Kind::SortByCount(_) => {
                        extra.push(sort_entry(Document::new().with("count", -1)));
                        break;
                    }
                    Kind::Bucket(_) => {
                        extra.push(sort_entry(Document::new().with("_id", 1)));
                        break;
                    }
                    Kind::Count(field) => {
                        let id = ids.take();
                        let by = Document::new().with(field.as_str(), true).with("_id", false);
                        plan = numbered(default_projection(by), id, Some(plan));
                    }
                    _ => {}
                }
            }
            Kind::Lookup(l) if l.is_equality() => {
                let (lookup, unwound) =
                    eq_lookup(ns, l, steps.get(j + 1), foreign_counts, plan, ids);
                plan = lookup;
                j += if unwound { 2 } else { 1 };
            }
            Kind::Match(m) => {
                let id = ids.take();
                let mut filter = Stage::new("MATCH");
                filter.head.push("filter", parsed(m, None)?);
                filter.returned = plan.returned;
                plan = numbered(filter, id, Some(plan));
                j += 1;
            }
            Kind::ReplaceRoot(_) => {
                let id = ids.take();
                plan = numbered(Stage::new("REPLACE_ROOT"), id, Some(plan));
                j += 1;
            }
            _ if st.is_projection() => {
                let id = ids.take();
                plan = numbered(engine_projection(st), id, Some(plan));
                j += 1;
            }
            _ => break,
        }
    }
    Ok((plan, j, extra))
}

/// The engine's stage of an equality `$lookup` `l` over `plan` (`EQ_LOOKUP`, or
/// `EQ_LOOKUP_UNWIND` with the `$unwind` of its array, `next`), with the scan of the foreign
/// collection; and whether it took `next`.
fn eq_lookup(
    ns: &Namespace,
    l: &Lookup,
    next: Option<&Step>,
    foreign_counts: &[(String, u64)],
    plan: Stage,
    ids: &mut Ids,
) -> (Stage, bool) {
    let foreign_id = ids.take();
    let from = l.from.clone().unwrap_or_default();
    let mut foreign = Stage::new("COLLSCAN");
    foreign.id = foreign_id;
    if let Some((_, n)) = foreign_counts.iter().find(|(f, _)| *f == from) {
        foreign.counters.push("docsExamined", long(*n));
        foreign.returned = *n;
    }
    foreign.tail.push("nss", format!("{}.{from}", ns.db));
    foreign.tail.push("direction", "forward");
    let unwinds = unwinds_into(next, &l.as_field);
    let mut lookup = Stage::new(if unwinds.is_some() { "EQ_LOOKUP_UNWIND" } else { "EQ_LOOKUP" });
    lookup.tail.push("foreignCollection", format!("{}.{from}", ns.db));
    lookup.tail.push("localField", l.local_field.clone().unwrap_or(Value::Null));
    lookup.tail.push("foreignField", l.foreign_field.clone().unwrap_or(Value::Null));
    lookup.tail.push("asField", l.as_field.as_str());
    lookup.tail.push("strategy", "HashJoin");
    let unwound = unwinds.is_some();
    if let Some(u) = unwinds {
        lookup.tail.push("unwinding", u);
    }
    lookup.tail.push(
        "inputStages",
        Value::Array(vec![Value::Document(plan.planned()), Value::Document(foreign.planned())]),
    );
    lookup.id = ids.take();
    lookup.returned = plan.returned;
    lookup.inputs = vec![plan, foreign];
    (lookup, unwound)
}

/// `plan` without a projection of fields on top (one that only keeps or drops fields, dotted
/// paths too), which the engine does not show under a `$group`.
fn hide_fields(mut plan: Stage) -> Stage {
    let only_fields = match plan.tail.get("transformBy") {
        Some(Value::Document(t)) => t.iter().all(|(_, v)| matches!(v, Value::Boolean(_))),
        _ => false,
    };
    let hidden = plan.name == "PROJECTION_SIMPLE" || only_fields;
    match (hidden, plan.input.take()) {
        (true, Some(input)) => *input,
        (_, input) => {
            plan.input = input;
            plan
        }
    }
}

/// The `unwinding` of an `$unwind` of `as_field` right after a `$lookup`, if it is one.
fn unwinds_into(next: Option<&Step>, as_field: &str) -> Option<Document> {
    let Some(Kind::Unwind(u)) = next.map(|s| &s.kind) else { return None };
    (u.field() == as_field).then(|| {
        Document::new()
            .with("preserveNullAndEmptyArrays", matches!(u.preserve, Some(Value::Boolean(true))))
            .with("includeArrayIndex", u.index.clone().unwrap_or_default())
    })
}

fn sort_entry(key: Document) -> Document {
    Document::new().with("$sort", Document::new().with("sortKey", key))
}

/// The stages after the query plan as `explain` lists them, each with how many of `steps` it
/// stands for: a `$limit` right after a `$sort` folded into it, the stages one stage becomes.
fn listed(steps: &[Step]) -> Vec<(Document, usize)> {
    let mut out: Vec<(Document, usize)> = Vec::new();
    let mut i = 0;
    while let Some(st) = steps.get(i) {
        if let Kind::Sort(s) = &st.kind
            && let Some(Kind::Limit(l)) = steps.get(i + 1).map(|n| &n.kind)
        {
            let key = Document::new().with("sortKey", s.clone()).with("limit", int64(*l));
            out.push((Document::new().with("$sort", key), 2));
            i += 2;
            continue;
        }
        for (k, d) in super::forms::forms(st).into_iter().enumerate() {
            out.push((d, usize::from(k == 0)));
        }
        i += 1;
    }
    out
}
