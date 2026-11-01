//! `explain` (010 FR-003, T018): the plan of a `find`, `count`, `distinct`, `update`, `delete` or
//! `findAndModify` in the shape a `refrouter` 9.0.4 gives it (measured 01-11-2026), with one section
//! for the node that runs it, and with `executionStats` or `allPlansExecution` what running it
//! counted, without writing anything. The plan is the one this node runs: a collection scan, or
//! the key of an `_id` equality; sorts, skips, limits and projections over it.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_mql::update::{Update, UpdateContext};
use teseladb_mql::{Filter, FilterUse, MatchContext};

use super::mutation::from_mql;
use super::query::{
    FindArgs, check_filter, check_projection, documents_of, find_args, for_each_match_counted,
    options_collation,
};
use super::{Backend, Ctx, args, crud, is_data_command, names};
use crate::error::{
    BAD_VALUE, COMMAND_NOT_FOUND, CommandError, ILLEGAL_OPERATION, INVALID_LENGTH,
    INVALID_NAMESPACE,
};
use crate::types::wrong_type;

pub(crate) mod aggregate;
mod forms;
mod moves;
mod steps;
mod views;

pub(crate) use aggregate::planned_query;

/// `explain`.
pub const EXPLAIN: &str = "explain";

/// The commands `explain` takes.
const EXPLAINABLE: [&str; 8] = [
    "find",
    "aggregate",
    "count",
    "distinct",
    "update",
    "delete",
    "findAndModify",
    "findandmodify",
];

/// The memory a sort may use (`memLimit`).
const SORT_MEM_LIMIT: i32 = 104_857_600;

/// The server parameters every `explain` reply lists, with their defaults.
const SERVER_PARAMETERS: [(&str, i64); 3] = [
    ("internalQueryFacetMaxOutputDocSizeBytes", 104_857_600),
    ("internalLookupStageIntermediateDocumentMaxSizeBytes", 104_857_600),
    ("internalQueryProhibitBlockingMergeOnRefRouter", 0),
];

/// How much `explain` tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Verbosity {
    QueryPlanner,
    ExecutionStats,
    AllPlansExecution,
}

/// A stage of a plan and what it gave when run.
struct Stage {
    name: &'static str,
    /// Its description, before the counters.
    head: Document,
    /// Its description after the counters.
    tail: Document,
    /// Its own counters (`docsExamined`, `nMatched`…).
    counters: Document,
    /// The counters it shows right after the common ones (`keysExamined` of `COUNT_SCAN`).
    lead: Document,
    returned: u64,
    input: Option<Box<Stage>>,
    /// The other inputs of a stage that reads several (`EQ_LOOKUP`), which it lists itself.
    inputs: Vec<Stage>,
    /// Its number in a plan of RefDb's slot-based engine (`planNodeId`).
    id: Option<u32>,
    /// The plan comes from an active entry of the plan cache (`isCached`).
    cached: bool,
}

impl Stage {
    fn new(name: &'static str) -> Stage {
        Stage {
            name,
            head: Document::new(),
            tail: Document::new(),
            counters: Document::new(),
            lead: Document::new(),
            returned: 0,
            input: None,
            inputs: Vec::new(),
            id: None,
            cached: false,
        }
    }

    fn over(mut self, input: Stage) -> Stage {
        self.input = Some(Box::new(input));
        self
    }

    /// The `works` of the stage: one per document it read or got, and one to end.
    fn works(&self) -> i64 {
        match &self.input {
            Some(i) => i.works(),
            None => {
                count(self.counters.get("docsExamined"))
                    .max(count(self.counters.get("keysExamined")))
                    + 1
            }
        }
    }

    /// The stage as `queryPlanner` shows it.
    fn planned(&self) -> Document {
        let mut d = Document::new().with("stage", self.name);
        if let Some(id) = self.id {
            d.push("planNodeId", Value::Int32(i32::try_from(id).unwrap_or(i32::MAX)));
        }
        extend(&mut d, &self.head);
        extend(&mut d, &self.tail);
        if let Some(i) = &self.input {
            d.push("inputStage", i.planned());
        }
        d
    }

    /// The stage with what running it counted.
    fn executed(&self) -> Document {
        let mut d = Document::new().with("stage", self.name);
        if let Some(id) = self.id {
            d.push("planNodeId", Value::Int32(i32::try_from(id).unwrap_or(i32::MAX)));
        }
        extend(&mut d, &self.head);
        let returned = i64::try_from(self.returned).unwrap_or(i64::MAX);
        let works = self.works();
        d.push("nReturned", long(self.returned));
        d.push("executionTimeMillisEstimate", 0);
        d.push("works", long(works.unsigned_abs()));
        d.push("advanced", long(self.returned));
        d.push("needTime", long((works - returned - 1).max(0).unsigned_abs()));
        for k in ["needYield", "saveState", "restoreState"] {
            d.push(k, 0);
        }
        d.push("isEOF", 1);
        extend(&mut d, &self.lead);
        // A stage that reads several lists them with what each counted.
        for (k, v) in self.tail.iter() {
            if k == "inputStages" {
                let run: Vec<Value> =
                    self.inputs.iter().map(|i| Value::Document(i.executed())).collect();
                d.push(k, Value::Array(run));
            } else {
                d.push(k, v.clone());
            }
        }
        extend(&mut d, &self.counters);
        if let Some(i) = &self.input {
            d.push("inputStage", i.executed());
        }
        d
    }

    fn total(&self, key: &str) -> i64 {
        count(self.counters.get(key))
            + count(self.lead.get(key))
            + self.input.as_ref().map_or(0, |i| i.total(key))
            + self.inputs.iter().map(|i| i.total(key)).sum::<i64>()
    }
}

fn count(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Int64(n)) => *n,
        Some(Value::Int32(n)) => i64::from(*n),
        _ => 0,
    }
}

fn extend(d: &mut Document, more: &Document) {
    for (k, v) in more.iter() {
        d.push(k, v.clone());
    }
}

/// A count as `explain` writes it: a 32-bit integer while it fits.
fn long(n: u64) -> Value {
    i32::try_from(n).map_or(Value::Int64(i64::try_from(n).unwrap_or(i64::MAX)), Value::Int32)
}

/// What a plan is about: its namespace, its parsed filter and the stage the router shows.
struct Explained {
    parsed: Value,
    router_stage: &'static str,
    plan: Stage,
    /// The plans the planner tried and did not choose.
    rejected: Vec<Document>,
    /// Each plan's trial, the winner first (`allPlansExecution`).
    trials: Vec<Document>,
}

/// `explain`.
pub fn explain<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let inner = match ctx.cmd.get(EXPLAIN) {
        Some(Value::Document(d)) => d.clone(),
        Some(v) => return Err(wrong_type("explain.explain", v, "object")),
        None => Document::new(),
    };
    let verbosity = match ctx.cmd.get("verbosity") {
        None => Verbosity::AllPlansExecution,
        Some(Value::String(s)) => match s.as_str() {
            "queryPlanner" => Verbosity::QueryPlanner,
            "executionStats" => Verbosity::ExecutionStats,
            "allPlansExecution" => Verbosity::AllPlansExecution,
            other => {
                return Err(CommandError::new(
                    BAD_VALUE,
                    format!(
                        "Enumeration value '{other}' for field 'explain.verbosity' is not a valid value."
                    ),
                ));
            }
        },
        Some(v) => return Err(wrong_type("explain.verbosity", v, "string")),
    };
    let name = inner.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default();
    // The command explained takes the `$db` of `explain`: another one is refused, the same one
    // is a duplicate (measured).
    match inner.get("$db") {
        Some(Value::String(db)) if db != ctx.db => {
            let msg =
                format!("Mismatched $db in explain command. Expected {} but got {db}", ctx.db);
            return Err(CommandError::new(INVALID_NAMESPACE, msg));
        }
        Some(_) => {
            let msg = format!("BSON field '{name}.$db' is a duplicate field");
            return Err(CommandError::new(crate::error::IDL_DUPLICATE_FIELD, msg));
        }
        None => {}
    }
    if !EXPLAINABLE.contains(&name.as_str()) {
        let known = is_data_command(&name) || super::diagnostics::listed(&name);
        if known {
            // A known command parses before it is refused.
            crate::generic::parse(&inner, &name)?;
            match name.as_str() {
                "getMore" => {
                    args::long(inner.get(&name).unwrap_or(&Value::Null), "getMore.getMore")?;
                }
                crud::BULK_WRITE => {
                    crud::batch(&inner, crud::BULK_WRITE, "ops")?;
                }
                _ => {}
            }
        }
        return Err(if known {
            let e = CommandError::new(ILLEGAL_OPERATION, format!("Cannot explain cmd: {name}"));
            // A `refrouter` hands an `insert` to the shard, which refuses it.
            if name == crud::INSERT { on_node(ctx, e) } else { e }
        } else {
            CommandError::new(
                COMMAND_NOT_FOUND,
                format!("Explain failed due to unknown command: {name}"),
            )
        });
    }
    crate::generic::parse(&inner, &name)?;
    let command = echoed(&inner, &name, ctx.db);
    let run = verbosity >= Verbosity::ExecutionStats;
    let mut sub = Ctx { backend: &mut *ctx.backend, shared: ctx.shared, db: ctx.db, cmd: &inner };
    let ns = super::query::target(&sub)?;
    if let Some(view) = super::views::resolve(sub.backend, &ns)? {
        return views::on_view(ctx, &ns, &view, (&name, &inner), verbosity);
    }
    if name == "aggregate" {
        return aggregate::aggregate(ctx, &ns, &inner, verbosity, command);
    }
    let explained = match name.as_str() {
        "find" => find(&mut sub, &ns, run)?,
        "count" => count_plan(&mut sub, &ns, run)?,
        "distinct" => distinct(&mut sub, &ns, run)?,
        "update" => update(&mut sub, &ns, run)?,
        "delete" => delete(&mut sub, &ns, run)?,
        _ => find_and_modify(&mut sub, &ns, run)?,
    };
    Ok(reply(ctx, &ns, explained, verbosity, command))
}

/// The explained command as the router echoes it: `findAndModify` in the order of its fields
/// `aggregate` with `explain: true`: the explain of its pipeline as `explain` gives it in
/// `queryPlanner` mode, the command echoed without the flag (measured on a `refrouter` 9.0.4).
pub(crate) fn aggregate_flag<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let inner = ctx
        .cmd
        .iter()
        .filter(|(k, _)| *k != EXPLAIN && *k != "$db")
        .fold(Document::new(), |d, (k, v)| d.with(k, v.clone()));
    let command = echoed(&inner, "aggregate", ctx.db);
    let ns = super::query::target(ctx)?;
    if let Some(view) = super::views::resolve(ctx.backend, &ns)? {
        return views::on_view(ctx, &ns, &view, ("aggregate", &inner), Verbosity::QueryPlanner);
    }
    aggregate::aggregate(ctx, &ns, &inner, Verbosity::QueryPlanner, command)
}

/// and `aggregate` with its implicit read concern, both without `$db`; the others with `$db`.
fn echoed(inner: &Document, name: &str, db: &str) -> Document {
    let mut out = Document::new();
    match name {
        "aggregate" => {
            for (k, v) in inner.iter() {
                out.push(k, v.clone());
            }
            // The read concern given says who gave it (measured).
            let concern = match out.get("readConcern") {
                Some(Value::Document(rc)) => rc.clone().with("provenance", "clientSupplied"),
                _ => Document::new().with("level", "local").with("provenance", "implicitDefault"),
            };
            out.insert("readConcern", concern);
            return out;
        }
        "findAndModify" | "findandmodify" => {
            const ORDER: [&str; 6] = ["query", "fields", "sort", "remove", "update", "new"];
            out.push(name, inner.get(name).cloned().unwrap_or(Value::Null));
            for k in ORDER {
                if let Some(v) = inner.get(k) {
                    out.push(k, v.clone());
                }
            }
            for (k, v) in inner.iter().filter(|(k, _)| *k != name && !ORDER.contains(k)) {
                out.push(k, v.clone());
            }
            return out;
        }
        _ => {
            for (k, v) in inner.iter() {
                out.push(k, v.clone());
            }
        }
    }
    out.push("$db", db);
    out
}

/// The collation of an explained command, or of its first write statement.
fn command_collation(command: &Document) -> Option<&Document> {
    if let Some(Value::Document(c)) = command.get("collation") {
        return Some(c);
    }
    ["updates", "deletes"].iter().find_map(|k| match command.get(k) {
        Some(Value::Array(items)) => match items.first() {
            Some(Value::Document(st)) => match st.get("collation") {
                Some(Value::Document(c)) => Some(c),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    })
}

/// The reply: the router's plan with the node's section, the counters, the node and the
/// command.
fn reply<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    e: Explained,
    verbosity: Verbosity,
    command: Document,
) -> Document {
    let NodeParts { shard, host, server, params } = node_parts(ctx);
    let mut section = Document::new()
        .with("explainVersion", "1")
        .with("shardName", shard.as_str())
        .with("connectionString", format!("{shard}/{host}"))
        .with("serverInfo", server.clone())
        .with("namespace", names::full(ns));
    // A plan by key alone (the collection's count, an update by `_id`) shows no filter.
    if !matches!(e.parsed, Value::Null) {
        section.push("parsedQuery", e.parsed.clone());
        // The command's collation, complete (measured).
        if let Ok(Some(spec)) = super::views::spec_of(command_collation(&command)) {
            section.push("collation", spec.to_document());
        }
        section.push("indexFilterSet", false);
    }
    section.push("optimizationTimeMillis", 0);
    for k in [
        "maxIndexedOrSolutionsReached",
        "maxIndexedAndSolutionsReached",
        "maxScansToExplodeReached",
        "prunedSimilarIndexes",
    ] {
        section.push(k, false);
    }
    let mut winning = Document::new().with("isCached", e.plan.cached);
    extend(&mut winning, &e.plan.planned());
    section.push("winningPlan", winning);
    section
        .push("rejectedPlans", e.rejected.iter().cloned().map(Value::Document).collect::<Vec<_>>());
    let planner = Document::new().with(
        "winningPlan",
        Document::new()
            .with("stage", e.router_stage)
            .with("shards", vec![Value::Document(section)]),
    );
    let mut out = Document::new().with("queryPlanner", planner);
    if verbosity >= Verbosity::ExecutionStats {
        out.push("executionStats", stats(&e, &shard, verbosity));
    }
    if let Some(hash) = super::query_shape::explained_hash(ctx.db, &command) {
        out.push("queryShapeHash", hash);
    }
    out.push("serverInfo", server);
    out.push("serverParameters", params);
    out.push("command", command);
    out.push("ok", 1.0);
    out
}

/// The router's stage with what the node gave: its documents, and the count of a `count`.
fn router_stage(e: &Explained, returned: Value) -> Document {
    let mut d = Document::new().with("stage", e.router_stage).with("nReturned", returned);
    if let Some(n) = e.plan.counters.get("nCounted") {
        d.push("nCounted", n.clone());
    }
    d.with("executionTimeMillis", 0)
}

/// What a reply says of the node that ran the plan.
struct NodeParts {
    /// Its shard name (`node<N>`, as `config.shards` lists it).
    shard: String,
    /// Its host and port.
    host: String,
    /// `serverInfo`.
    server: Document,
    /// `serverParameters`.
    params: Document,
}

/// The node's names, its `serverInfo` and the `serverParameters` of a reply.
fn node_parts<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> NodeParts {
    let facts = ctx.backend.process();
    let server = Document::new()
        .with("host", facts.host.hostname.as_str())
        .with("port", i32::from(facts.port))
        .with("version", super::diagnostics::ANNOUNCED_VERSION)
        .with("gitVersion", facts.build.as_str());
    let mut params = Document::new();
    for (k, v) in SERVER_PARAMETERS {
        params.push(k, Value::Int32(i32::try_from(v).unwrap_or(i32::MAX)));
    }
    params.push("internalQueryFrameworkControl", "trySbeRestricted");
    params.push("internalQueryPlannerIgnoreIndexWithCollationForRegex", 1);
    let host = format!("{}:{}", facts.host.hostname, facts.port);
    NodeParts { shard: super::shard_name(ctx.backend.node()), host, server, params }
}

/// An error the node gives while it plans or runs the explained command, as a `refrouter` relays
/// it: after the shard that failed.
fn on_node<B: Backend + ?Sized>(ctx: &Ctx<'_, B>, mut e: CommandError) -> CommandError {
    e.errmsg = format!(
        "Explain command on shard {} failed :: caused by :: {}",
        super::shard_name(ctx.backend.node()),
        e.errmsg
    );
    e
}

/// A `$skip` or `$limit` argument as a count (the stage checked it).
fn whole(v: &Value) -> u64 {
    match v {
        Value::Int32(n) => u64::try_from(*n).unwrap_or(0),
        Value::Int64(n) => u64::try_from(*n).unwrap_or(0),
        Value::Double(x) => *x as u64,
        _ => 0,
    }
}

/// A plan's trial as `allPlansExecution` shows it.
fn trial_doc(stage: &Stage, t: super::plan_exec::Tried) -> Document {
    let mut executed = Document::new().with("isCached", false);
    extend(&mut executed, &stage.executed());
    Document::new()
        .with("nReturned", long(t.returned))
        .with("executionTimeMillisEstimate", 0)
        .with("totalKeysExamined", long(t.keys))
        .with("totalDocsExamined", long(t.docs))
        .with("score", t.score)
        .with("executionStages", executed)
}

/// `executionStats`: the totals, the router's stage and the node's run.
fn stats(e: &Explained, node: &str, verbosity: Verbosity) -> Document {
    let returned = long(e.plan.returned);
    let keys = long(e.plan.total("keysExamined").unsigned_abs());
    let docs = long(e.plan.total("docsExamined").unsigned_abs());
    let mut executed = Document::new().with("isCached", false);
    extend(&mut executed, &e.plan.executed());
    let mut shard = Document::new()
        .with("shardName", node)
        .with("executionSuccess", true)
        .with("nReturned", returned.clone())
        .with("executionTimeMillis", 0)
        .with("executionTimeMicros", 0)
        .with("totalKeysExamined", keys.clone())
        .with("totalDocsExamined", docs.clone())
        .with("executionStages", executed);
    let trials: Vec<Value> = e.trials.iter().cloned().map(Value::Document).collect();
    if verbosity == Verbosity::AllPlansExecution {
        shard.push("allPlansExecution", trials.clone());
    }
    let mut out = Document::new()
        .with("nReturned", returned.clone())
        .with("executionTimeMillis", 0)
        .with("totalKeysExamined", keys.clone())
        .with("totalDocsExamined", docs.clone())
        .with(
            "executionStages",
            router_stage(e, returned)
                .with("totalKeysExamined", keys)
                .with("totalDocsExamined", docs)
                .with("totalChildMillis", 0)
                .with("shards", vec![Value::Document(shard)]),
        );
    if verbosity == Verbosity::AllPlansExecution {
        let plans = Document::new().with("shardName", node).with("allPlans", trials);
        out.push("allPlansExecution", vec![Value::Document(plans)]);
    }
    out
}

/// A filter parsed as a query, with each `$expr` as RefDb rewrites it, and written back as
/// RefDb does.
pub(crate) fn parsed(filter: &Document, vars: Option<&Document>) -> Result<Document, CommandError> {
    let filter = teseladb_exec::expr::optimize::optimize_filter(filter, vars).map_err(from_mql)?;
    let bytes = filter.to_bytes()?;
    let f = Filter::parse(RawDoc::from_bytes(&bytes)?, FilterUse::Query).map_err(from_mql)?;
    Ok(f.parsed_query())
}

/// The plan of a read or write of a collection that does not exist (measured).
fn missing_namespace() -> Stage {
    let mut eof = Stage::new("EOF");
    eof.tail.push("type", "nonExistentNamespace");
    eof
}

/// Whether a value holds strings, which a collation compares.
fn collatable(v: &Value) -> bool {
    match v {
        Value::String(_) | Value::Symbol(_) => true,
        Value::Document(d) => d.iter().any(|(_, x)| collatable(x)),
        Value::Array(items) => items.iter().any(collatable),
        _ => false,
    }
}

/// A collection scan of `ns` with `filter` (shown when it is not empty); under a rooted `$or`,
/// in a `SUBPLAN` without its namespace.
fn collscan(ns: &Namespace, shown: &Document, examined: u64, matched: u64) -> Stage {
    if matches_nothing(shown) {
        let mut eof = Stage::new("EOF");
        eof.tail.push("type", "predicateEvalsToFalse");
        return eof;
    }
    let rooted_or = shown.len() == 1 && shown.get("$or").is_some();
    let shown = teseladb_mql::scan_filter(shown);
    let mut s = Stage::new("COLLSCAN");
    if !shown.is_empty() {
        s.head.push("filter", shown);
    }
    if !rooted_or {
        s.tail.push("nss", names::full(ns));
    }
    s.tail.push("direction", "forward");
    s.counters.push("docsExamined", long(examined));
    s.returned = matched;
    if rooted_or {
        let mut sub = Stage::new("SUBPLAN").over(s);
        sub.returned = matched;
        return sub;
    }
    s
}

/// Whether parsed query `shown` matches no document, which RefDb plans as `EOF`.
pub(super) fn matches_nothing(shown: &Document) -> bool {
    shown.len() == 1 && shown.get(teseladb_mql::ops::ALWAYS_FALSE).is_some()
}

/// Whether a projection only keeps or drops fields (`PROJECTION_SIMPLE`).
fn simple_projection(p: &Document) -> bool {
    p.iter().all(|(k, v)| {
        !k.contains('.')
            && matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Boolean(_))
    })
}

fn projection_stage(p: &Document, input: Stage) -> Stage {
    let name = if simple_projection(p) { "PROJECTION_SIMPLE" } else { "PROJECTION_DEFAULT" };
    let mut s = Stage::new(name);
    s.tail.push("transformBy", p.clone());
    if name == "PROJECTION_DEFAULT" {
        s.counters.push("peakTrackedMemBytes", 0);
    }
    s.returned = input.returned;
    s.over(input)
}

/// What a read of a collection takes.
struct Read<'a> {
    filter: &'a Document,
    collation: Option<&'a Document>,
    vars: Option<&'a Document>,
    concern: teseladb_range::read::ReadConcern,
}

/// Scans `coll` as `read` says: the documents read and the matches, stopping after `stop`
/// matches; `each` sees each match.
fn scan<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    coll: Option<&CollectionEntry>,
    read: &Read<'_>,
    stop: Option<u64>,
    mut each: impl FnMut(&[u8]) -> Result<(), CommandError>,
) -> Result<(u64, u64), CommandError> {
    let Some(c) = coll else { return Ok((0, 0)) };
    let mut matched = 0u64;
    let examined = for_each_match_counted(
        ctx.backend,
        documents_of(c),
        read.filter,
        read.collation,
        read.vars,
        read.concern,
        |_, bytes| {
            matched += 1;
            each(bytes)?;
            Ok(stop.is_none_or(|s| matched < s))
        },
    )?;
    Ok((examined, matched))
}

/// `find`: an `_id` equality reads its key (`EXPRESS_IXSCAN`); anything else scans, then sorts
/// (with the limit folded into the sort), skips, projects and limits as RefDb orders them.
fn find<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    run: bool,
) -> Result<Explained, CommandError> {
    let a: FindArgs = find_args(ctx.backend, ctx.cmd)?;
    let coll = ctx.backend.collection(ns)?;
    let collation = a.collation.clone().or_else(|| coll.as_ref().and_then(options_collation));
    check_filter(&a.filter, collation.as_ref(), a.vars.as_ref())?;
    if let Some(p) = &a.projection {
        super::query::check_projection_shape(p)?;
        check_projection(p, a.vars.as_ref())?;
    }
    let shown = parsed(&a.filter, a.vars.as_ref())?;
    let Some(c) = coll.as_ref() else {
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SINGLE_SHARD",
            plan: missing_namespace(),
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    };
    let sort = match ctx.cmd.get("sort") {
        Some(Value::Document(s)) if !s.is_empty() && s.get("$natural").is_none() => Some(s.clone()),
        _ => None,
    };
    let projection = a.projection.clone().filter(|p| !p.is_empty());
    // With a skip, an `_id` equality scans the index of `_id` and skips (measured), with a
    // collation too when the value has no strings for it to compare.
    let collates = |id: &Value| collation.is_some() && collatable(id);
    let plain = sort.is_none() && projection.is_none();
    if let Some(id) = crud::id_equality(&a.filter).filter(|id| a.skip > 0 && plain && !collates(id))
    {
        let found = if run { read_id(ctx, c, &id, a.concern)? } else { 0 };
        let ix = id_index_scan(ns, &id, found, found);
        let skipped = skip_stage(a.skip, ix);
        let left = skipped.returned;
        let plan = fetch_stage(Some(ns), None, skipped, left, left);
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SINGLE_SHARD",
            plan,
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    }
    let id = crud::id_equality(&a.filter).filter(|_| collation.is_none() && a.skip == 0);
    if let (Some(id), Some(c)) = (id, coll.as_ref()) {
        let found = if run { read_id(ctx, c, &id, a.concern)? } else { 0 };
        let mut s = Stage::new("EXPRESS_IXSCAN");
        s.head.push("keyPattern", Document::new().with("_id", 1));
        s.head.push("indexName", "_id_");
        s.counters.push("keysExamined", 1);
        s.counters.push("docsExamined", long(found));
        s.returned = found;
        let plan = match &projection {
            Some(p) if simple_projection(p) => {
                s.head.push("projection", p.clone());
                s.head.push("projectionCovered", false);
                s
            }
            Some(p) => {
                let mut idhack = Stage::new("IDHACK");
                idhack.counters = s.counters;
                idhack.returned = found;
                projection_stage(p, idhack)
            }
            None => s,
        };
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SINGLE_SHARD",
            plan,
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    }
    if let Some(c) = coll
        .as_ref()
        .filter(|_| collation.is_none() && a.natural.is_none() && !matches_nothing(&shown))
    {
        let indexes = super::plan_exec::plan_indexes(ctx.backend, c)?;
        let hint = super::hints::position(ctx.cmd.get("hint"), &indexes)?;
        let read = super::plan_exec::ReadPlan {
            filter: &a.filter,
            parsed: &shown,
            sort: sort.as_ref().map(sort_fields).unwrap_or_default(),
            limit: a.limit.map(|l| a.skip.saturating_add(l)),
            hint,
            vars: a.vars.as_ref(),
            concern: a.concern,
            intersect: super::lock(ctx.shared).params.hash_intersection,
            index_keys: false,
        };
        if let Some(super::plan_exec::Chosen { winner, others, .. }) =
            super::plan_exec::choose(ctx.backend, c, &indexes, &read)?
        {
            let shape = |r: &super::plan_exec::Ran, run: bool| {
                let covered = projection.as_ref().filter(|p| covers(p, &indexes, r));
                let tail = Tail {
                    sort: sort.as_ref(),
                    skip: a.skip,
                    limit: a.limit,
                    projection: if covered.is_some() { None } else { projection.as_ref() },
                };
                let access = planned_access(ns, &shown, &indexes, r, run, covered);
                tail.over(access, sorted(r))
            };
            let mut plan = shape(&winner, run);
            let no_doc = Document::new();
            let read_shape = super::plan_cache::read_shape(
                &shown,
                sort.as_ref().or(Some(&no_doc)),
                projection.as_ref().or(Some(&no_doc)),
                None,
            );
            let catalog = super::plan_cache::catalog_signature(c, &indexes);
            plan.cached = super::lock(ctx.shared).plan_cache.is_active(
                &names::full(ns),
                &read_shape,
                &catalog,
            );
            let rejected = others
                .iter()
                .map(|r| {
                    let mut d = Document::new().with("isCached", false);
                    extend(&mut d, &shape(r, false).planned());
                    d
                })
                .collect();
            let trials = std::iter::once(&winner)
                .chain(&others)
                .filter_map(|r| {
                    let t = r.tried?;
                    Some(trial_doc(&shape(&r.at_trial(), true), t))
                })
                .collect();
            return Ok(Explained {
                parsed: Value::Document(shown),
                router_stage: "SINGLE_SHARD",
                plan,
                rejected,
                trials,
            });
        }
    }
    let stop = match (&sort, a.limit) {
        (None, Some(l)) => Some(a.skip.saturating_add(l)),
        _ => None,
    };
    let read = Read {
        filter: &a.filter,
        collation: collation.as_ref(),
        vars: a.vars.as_ref(),
        concern: a.concern,
    };
    if matches_nothing(&shown) {
        let plan = collscan(ns, &shown, 0, 0);
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SINGLE_SHARD",
            plan,
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    }
    let (examined, matched) =
        if run { scan(ctx, coll.as_ref(), &read, stop, |_| Ok(()))? } else { (0, 0) };
    let tail =
        Tail { sort: sort.as_ref(), skip: a.skip, limit: a.limit, projection: projection.as_ref() };
    let mut scan_stage = collscan(ns, &shown, examined, matched);
    // A `$natural: -1` sort scans backward.
    if a.natural == Some(false) {
        scan_stage.tail.insert("direction", "backward");
    }
    let plan = tail.over(scan_stage, false);
    Ok(Explained {
        parsed: Value::Document(shown),
        router_stage: "SINGLE_SHARD",
        plan,
        rejected: Vec::new(),
        trials: Vec::new(),
    })
}

/// What a `find` does after it reads: its sort, skip, projection and limit.
struct Tail<'a> {
    sort: Option<&'a Document>,
    skip: u64,
    limit: Option<u64>,
    projection: Option<&'a Document>,
}

impl Tail<'_> {
    /// `plan` with the stages after it; `sorted`: the plan reads in the sort's order already. A
    /// `SUBPLAN` stays the root, over the stages after the scans (measured).
    fn over(&self, mut plan: Stage, sorted: bool) -> Stage {
        if plan.name == "SUBPLAN"
            && let Some(input) = plan.input.take()
        {
            let inner = self.over(*input, sorted);
            plan.returned = inner.returned;
            return plan.over(inner);
        }
        if let Some(sort) = self.sort.filter(|_| !sorted) {
            // A projection that keeps the sort's fields runs below a sort without a limit
            // (measured).
            let below = self
                .projection
                .filter(|p| self.limit.is_none() && aggregate::projection_keeps_sort(p, sort));
            if let Some(p) = below {
                plan = projection_stage(p, plan);
            }
            let mut s = Stage::new("SORT");
            s.tail.push("sortPattern", sort.clone());
            s.tail.push("memLimit", SORT_MEM_LIMIT);
            let bound = self.limit.map(|l| self.skip.saturating_add(l));
            if let Some(b) = bound {
                s.tail.push("limitAmount", long(b));
            }
            s.tail.push("type", "simple");
            sort_counters(&mut s);
            s.returned = bound.map_or(plan.returned, |b| plan.returned.min(b));
            plan = s.over(plan);
            plan = skip_stage(self.skip, plan);
            if let Some(p) = self.projection.filter(|_| below.is_none()) {
                plan = projection_stage(p, plan);
            }
            return plan;
        }
        plan = skip_stage(self.skip, plan);
        if let Some(p) = self.projection {
            plan = projection_stage(p, plan);
        }
        if let Some(l) = self.limit {
            let mut s = Stage::new("LIMIT");
            s.tail.push("limitAmount", long(l));
            s.returned = plan.returned.min(l);
            plan = s.over(plan);
        }
        plan
    }
}

/// The fields and directions of a sort.
pub(super) fn sort_fields(sort: &Document) -> Vec<(String, i32)> {
    sort.iter()
        .map(|(k, v)| {
            let neg = match v {
                Value::Int32(n) => *n < 0,
                Value::Int64(n) => *n < 0,
                Value::Double(x) => *x < 0.0,
                _ => false,
            };
            (k.to_owned(), if neg { -1 } else { 1 })
        })
        .collect()
}

/// Whether a plan reads in the order of the sort.
pub(super) fn sorted(r: &super::plan_exec::Ran) -> bool {
    match &r.access {
        teseladb_plan::rules::Access::Index(s) => s.sorts,
        teseladb_plan::rules::Access::AndHash { scans, .. } => {
            scans.last().is_some_and(|s| s.sorts)
        }
        teseladb_plan::rules::Access::Or(scans) => matches!(scans.as_slice(), [s] if s.sorts),
        teseladb_plan::rules::Access::Collscan => false,
    }
}

/// The plans of a read of `coll` that indexes serve, the winner first; `None` when the
/// collection has no index the planner may consider (or the read has a collation).
fn planned_read<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    coll: Option<&CollectionEntry>,
    read: &super::plan_exec::ReadPlan<'_>,
    collation: Option<&Document>,
) -> Result<Option<(Vec<super::plan_exec::PlanIndex>, super::plan_exec::Chosen)>, CommandError> {
    let Some(c) = coll.filter(|_| collation.is_none() && !matches_nothing(read.parsed)) else {
        return Ok(None);
    };
    let indexes = super::plan_exec::plan_indexes(ctx.backend, c)?;
    let chosen = super::plan_exec::choose(ctx.backend, c, &indexes, read)?;
    Ok(chosen.map(|chosen| (indexes, chosen)))
}

/// The plans of a write that takes the first document of `read` (its limit of one) or all of
/// them: plans compete without the limit, and the plan that wins shows its trial; a plan alone
/// stops at the first document (measured on a `refrouter` 9.0.4).
fn planned_write<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    coll: Option<&CollectionEntry>,
    read: &super::plan_exec::ReadPlan<'_>,
    collation: Option<&Document>,
) -> Result<Option<(Vec<super::plan_exec::PlanIndex>, super::plan_exec::Chosen)>, CommandError> {
    let unbounded = super::plan_exec::ReadPlan { sort: read.sort.clone(), limit: None, ..*read };
    let Some((indexes, mut chosen)) = planned_read(ctx, coll, &unbounded, collation)? else {
        return Ok(None);
    };
    if read.limit.is_none() {
        return Ok(Some((indexes, chosen)));
    }
    if chosen.others.is_empty() {
        return planned_read(ctx, coll, read, collation);
    }
    chosen.winner = chosen.winner.at_trial();
    Ok(Some((indexes, chosen)))
}

/// What the trial of `chosen`'s plans teaches the plan cache (010 FR-002): the plan that won and
/// each plan's trial, without the namespace, as `$planCacheStats` writes them; `None` when no
/// plans competed.
pub(crate) fn learned(
    ns: &Namespace,
    shown: &Document,
    indexes: &[super::plan_exec::PlanIndex],
    chosen: &super::plan_exec::Chosen,
    (from, shape): (Document, String),
) -> Option<super::plan_cache::Learned> {
    if chosen.others.is_empty() {
        return None;
    }
    let won = chosen.winner.tried?;
    // The read's sort and projection over the scans, as `find` runs them.
    let doc_of = |k: &str| match from.get(k) {
        Some(Value::Document(d)) if !d.is_empty() => Some(d.clone()),
        _ => None,
    };
    let (sort, projection) = (doc_of("sort"), doc_of("projection"));
    let tail = Tail { sort: sort.as_ref(), skip: 0, limit: None, projection: projection.as_ref() };
    let shape_of = |r: &super::plan_exec::Ran, run: bool| {
        tail.over(planned_access(ns, shown, indexes, r, run, None), sorted(r))
    };
    let plan = without_nss(shape_of(&chosen.winner, false).planned());
    let all: Vec<&super::plan_exec::Ran> =
        std::iter::once(&chosen.winner).chain(&chosen.others).collect();
    let trials = all
        .iter()
        .filter_map(|r| {
            let t = r.tried?;
            // A plan that sorts what it read gives, in its trial, every document it fetched to
            // the sort and the trial's own count from it.
            let mut at = r.at_trial();
            let blocking = sort.is_some() && !sorted(r);
            if blocking {
                let fetched = usize::try_from(t.docs).unwrap_or(usize::MAX);
                at.found = r.found.iter().take(fetched).cloned().collect();
            }
            let mut stage = shape_of(&at, true);
            if blocking {
                stage.returned = t.returned;
            }
            Some(
                Document::new()
                    .with("nReturned", long(t.returned))
                    .with("executionTimeMillisEstimate", 0)
                    .with("totalKeysExamined", long(t.keys))
                    .with("totalDocsExamined", long(t.docs))
                    .with("executionStages", without_nss(stage.executed())),
            )
        })
        .collect();
    let scores = all.iter().filter_map(|r| r.tried.map(|t| t.score)).collect();
    Some(super::plan_cache::Learned {
        shape,
        catalog: String::new(),
        from,
        works: won.works,
        reads: won.keys + won.docs,
        plan,
        trials,
        scores,
    })
}

/// `d` without the `nss` of its stages.
fn without_nss(d: Document) -> Document {
    let mut out = Document::new();
    for (k, v) in d.iter() {
        let v = match v {
            Value::Document(x) => Value::Document(without_nss(x.clone())),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|i| match i {
                        Value::Document(x) => Value::Document(without_nss(x.clone())),
                        other => other.clone(),
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        if k != "nss" {
            out.push(k, v);
        }
    }
    out
}

/// The plans `ran` as `rejectedPlans` lists them: what reads, without what writes.
fn rejected_reads(
    ns: &Namespace,
    shown: &Document,
    indexes: &[super::plan_exec::PlanIndex],
    ran: &[super::plan_exec::Ran],
) -> Vec<Document> {
    ran.iter()
        .map(|r| {
            let mut d = Document::new().with("isCached", false);
            extend(&mut d, &planned_access(ns, shown, indexes, r, false, None).planned());
            d
        })
        .collect()
}

/// Whether projection `p` keeps only fields that the index of plan `r` holds, without `_id`,
/// when the index decides the whole filter: the documents need no fetch.
fn covers(
    p: &Document,
    indexes: &[super::plan_exec::PlanIndex],
    r: &super::plan_exec::Ran,
) -> bool {
    let teseladb_plan::rules::Access::Index(scan) = &r.access else { return false };
    let index = &indexes[scan.index];
    let off = |v: &Value| matches!(v, Value::Int32(0) | Value::Int64(0) | Value::Boolean(false));
    // The fields the projection reads, computed ones too (none for one that drops fields).
    let reads =
        teseladb_exec::pipeline::deps::needed(&[Document::new().with("$project", p.clone())]);
    let holds = |k: &str| index.meta.key.iter().any(|(x, _)| x == k);
    scan.residual.is_none()
        && !index.meta.multikey
        && (p.get("_id").is_some_and(off) || holds("_id"))
        && reads.is_some_and(|f| f.iter().all(|k| holds(k)))
}

/// The stages that read for plan `r`: an index scan and its fetch (or the projection of the
/// fields it holds, `covered`), the scans of a `$or` joined.
fn planned_access(
    ns: &Namespace,
    shown: &Document,
    indexes: &[super::plan_exec::PlanIndex],
    r: &super::plan_exec::Ran,
    run: bool,
    covered: Option<&Document>,
) -> Stage {
    let returned = u64::try_from(r.found.len()).unwrap_or(u64::MAX);
    match &r.access {
        teseladb_plan::rules::Access::Collscan => collscan(ns, shown, r.docs, returned),
        teseladb_plan::rules::Access::Index(scan) => {
            let keys = if run { r.keys } else { 0 };
            let within = if run { r.keys.saturating_sub(r.past) } else { 0 };
            let mut ix = index_stage(Some(ns), &indexes[scan.index], scan, keys, within);
            if run && r.seeks > 1 {
                ix.counters.insert("seeks", long(r.seeks));
            }
            match covered {
                Some(p) => {
                    // One that computes is the default projection over the keys (measured).
                    let name = match simple_projection(p) {
                        true => "PROJECTION_COVERED",
                        false => "PROJECTION_DEFAULT",
                    };
                    let mut s = Stage::new(name);
                    s.tail.push("transformBy", p.clone());
                    s.returned = returned;
                    s.over(ix)
                }
                None => fetch_stage(Some(ns), scan.residual.as_ref(), ix, r.docs, returned),
            }
        }
        teseladb_plan::rules::Access::Or(scans) => {
            // The documents come from the `OR`, or from the one scan's own fetch.
            let docs = match scans.as_slice() {
                [_] => r.docs,
                _ => returned,
            };
            let fetch = fetch_stage(None, None, or_stage(indexes, r, scans, run), docs, returned);
            let mut sub = Stage::new("SUBPLAN").over(fetch);
            sub.returned = returned;
            sub
        }
        teseladb_plan::rules::Access::AndHash { scans, filter } => {
            let mut and = Stage::new("AND_HASH");
            let children: Vec<Stage> = scans
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let keys = if run { r.child_keys.get(i).copied().unwrap_or(0) } else { 0 };
                    let mut ix = index_stage(Some(ns), &indexes[s.index], s, keys, keys);
                    ix.returned = keys;
                    ix
                })
                .collect();
            let inputs: Vec<Value> =
                children.iter().map(|c| Value::Document(c.planned())).collect();
            and.inputs = children;
            if run {
                let held: u64 = r.child_keys.iter().rev().skip(1).sum();
                and.tail.push("memUsage", long(held.saturating_mul(AND_HASH_ENTRY_BYTES)));
                and.tail.push("memLimit", AND_HASH_MEM_LIMIT);
            }
            and.tail.push("inputStages", Value::Array(inputs));
            and.returned = returned;
            fetch_stage(Some(ns), filter.as_ref(), and, r.docs, returned)
        }
    }
}

/// The scans of a rooted `$or` (`SUBPLAN`): one scan alone, or each joined in an `OR`, a clause
/// the index does not decide alone fetching and filtering its documents.
fn or_stage(
    indexes: &[super::plan_exec::PlanIndex],
    r: &super::plan_exec::Ran,
    scans: &[teseladb_plan::rules::IndexScan],
    run: bool,
) -> Stage {
    let child = |i: usize, s: &teseladb_plan::rules::IndexScan| {
        let keys = if run { r.child_keys.get(i).copied().unwrap_or(0) } else { 0 };
        let past = if run { r.child_past.get(i).copied().unwrap_or(0) } else { 0 };
        let given = if run { r.child_given.get(i).copied().unwrap_or(0) } else { 0 };
        let within = keys.saturating_sub(past);
        let mut ix = index_stage(None, &indexes[s.index], s, keys, within);
        ix.returned = within;
        match &s.residual {
            Some(f) => fetch_stage(None, Some(f), ix, within, given),
            None => ix,
        }
    };
    if let [only] = scans {
        return child(0, only);
    }
    let children: Vec<Stage> = scans.iter().enumerate().map(|(i, s)| child(i, s)).collect();
    let mut or = Stage::new("OR");
    if run {
        let tested: u64 = r.child_given.iter().sum();
        let dropped = tested.saturating_sub(u64::try_from(r.found.len()).unwrap_or(u64::MAX));
        or.lead.push("dupsTested", long(tested));
        or.lead.push("dupsDropped", long(dropped));
    }
    let inputs: Vec<Value> = children.iter().map(|c| Value::Document(c.planned())).collect();
    or.tail.push("inputStages", Value::Array(inputs));
    or.inputs = children;
    or.returned = u64::try_from(r.found.len()).unwrap_or(u64::MAX);
    or
}

/// The memory an `AND_HASH` may hold (`internalQueryMaxAndHashMemory`, 32 MiB as measured).
const AND_HASH_MEM_LIMIT: i32 = 33_554_432;

/// The bytes TeselaDB counts for each key an `AND_HASH` holds (RefDb counts its own record
/// identifiers, which `explain` comparisons leave out).
const AND_HASH_ENTRY_BYTES: u64 = 43;

/// `DISTINCT_SCAN` of the values of `key`, the first field of `index`, as the projection of that
/// field alone.
fn distinct_scan(
    index: &super::plan_exec::PlanIndex,
    scan: &teseladb_plan::rules::IndexScan,
    key: &str,
    (keys, values): (u64, u64),
) -> Stage {
    let ix = index_stage(None, index, scan, 0, 0);
    let mut s = Stage::new("DISTINCT_SCAN");
    for (k, v) in ix.tail.iter() {
        if k == "indexVersion" {
            s.tail.push("isShardFiltering", false);
            s.tail.push("isFetching", false);
        }
        s.tail.push(k, v.clone());
    }
    // It reads one key per value (measured).
    s.counters.push("keysExamined", long(keys));
    for k in ["docsExamined", "chunkSkips", "orphanChunkSkips"] {
        s.counters.push(k, 0);
    }
    s.returned = values;
    let mut p = Stage::new("PROJECTION_COVERED");
    let keeps = match key {
        "_id" => Document::new().with("_id", 1),
        _ => Document::new().with("_id", 0).with(key, 1),
    };
    p.tail.push("transformBy", keeps);
    p.returned = values;
    p.over(s)
}

/// `COUNT_SCAN` of `index` within the one interval of each field of `scan`: its keys counted
/// without the documents.
fn count_scan(
    ns: &Namespace,
    index: &super::plan_exec::PlanIndex,
    scan: &teseladb_plan::rules::IndexScan,
    r: &super::plan_exec::Ran,
    run: bool,
) -> Stage {
    let ix = index_stage(Some(ns), index, scan, 0, 0);
    let mut s = Stage::new("COUNT_SCAN");
    for (k, v) in ix.tail.iter().filter(|(k, _)| !matches!(*k, "direction" | "indexBounds")) {
        s.tail.push(k, v.clone());
    }
    let (mut start, mut end) = (Document::new(), Document::new());
    let (mut start_in, mut end_in) = (true, true);
    for ((field, list), (_, dir)) in scan.bounds.iter().zip(&index.meta.key) {
        let b = if *dir < 0 { list[0].reversed() } else { list[0].clone() };
        start.push(field, b.low.clone());
        end.push(field, b.high.clone());
        start_in &= b.low_inclusive;
        end_in &= b.high_inclusive;
    }
    s.tail.push(
        "indexBounds",
        Document::new()
            .with("startKey", start)
            .with("startKeyInclusive", start_in)
            .with("endKey", end)
            .with("endKeyInclusive", end_in),
    );
    // The scan reads the key after the last it counts (measured).
    if run {
        s.returned = r.keys;
        s.lead.push("keysExamined", long(r.keys + u64::from(r.trial.eof)));
    } else {
        s.counters.push("keysExamined", long(0));
    }
    s
}

/// `FETCH` of the documents `input` finds, with the rest of the filter.
fn fetch_stage(
    ns: Option<&Namespace>,
    filter: Option<&Document>,
    input: Stage,
    docs: u64,
    returned: u64,
) -> Stage {
    let mut f = Stage::new("FETCH");
    if let Some(filter) = filter {
        f.head.push("filter", filter.clone());
    }
    if let Some(ns) = ns {
        f.tail.push("nss", names::full(ns));
    }
    f.counters.push("docsExamined", long(docs));
    f.counters.push("alreadyHasObj", 0);
    f.returned = returned;
    f.over(input)
}

/// `IXSCAN` of `index` within the intervals of `scan`.
fn index_stage(
    ns: Option<&Namespace>,
    index: &super::plan_exec::PlanIndex,
    scan: &teseladb_plan::rules::IndexScan,
    keys: u64,
    returned: u64,
) -> Stage {
    let mut s = Stage::new("IXSCAN");
    if let Some(ns) = ns {
        s.tail.push("nss", names::full(ns));
    }
    s.tail.push("keyPattern", index.pattern.clone());
    s.tail.push("indexName", index.entry.name.as_str());
    s.tail.push("isMultiKey", index.meta.multikey);
    let mut paths = Document::new();
    for (field, list) in &index.multikey_paths {
        paths.push(field, Value::Array(list.iter().map(|p| Value::String(p.clone())).collect()));
    }
    s.tail.push("multiKeyPaths", paths);
    s.tail.push("isUnique", index.unique());
    s.tail.push("isSparse", index.sparse());
    s.tail.push("isPartial", index.partial());
    s.tail.push("indexVersion", 2);
    s.tail.push("direction", if scan.backward { "backward" } else { "forward" });
    let mut bounds = Document::new();
    for ((field, list), (_, dir)) in scan.bounds.iter().zip(&index.meta.key) {
        let against = (*dir < 0) != scan.backward;
        let mut shown: Vec<Value> = list
            .iter()
            .map(|b| Value::String(if against { b.reversed().shown() } else { b.shown() }))
            .collect();
        if against {
            shown.reverse();
        }
        bounds.push(field, Value::Array(shown));
    }
    s.tail.push("indexBounds", bounds);
    s.counters.push("keysExamined", long(keys));
    s.counters.push("seeks", 1);
    s.counters.push("dupsTested", 0);
    s.counters.push("dupsDropped", 0);
    s.returned = returned;
    s
}

/// What a sort counts: the bytes it sorted and kept, and that it never spilled.
fn sort_counters(s: &mut Stage) {
    s.counters.push("totalDataSizeSorted", 0);
    s.counters.push("usedDisk", false);
    for k in [
        "spills",
        "spilledRecords",
        "spilledBytes",
        "spilledDataStorageSize",
        "peakTrackedMemBytes",
    ] {
        s.counters.push(k, 0);
    }
}

fn skip_stage(skip: u64, mut input: Stage) -> Stage {
    if skip == 0 {
        return input;
    }
    // A fetch without a filter fetches only what the skip leaves (measured).
    if input.name == "FETCH"
        && input.head.get("filter").is_none()
        && input.tail.get("filter").is_none()
        && let Some(scan) = input.input.take()
    {
        let skipped = skip_stage(skip, *scan);
        input.returned = input.returned.saturating_sub(skip);
        if input.counters.get("docsExamined").is_some() {
            input.counters.insert("docsExamined", long(skipped.returned));
        }
        return input.over(skipped);
    }
    let mut s = Stage::new("SKIP");
    s.tail.push("skipAmount", long(skip));
    s.returned = input.returned.saturating_sub(skip);
    s.over(input)
}

/// Whether the document of `_id` `id` exists (1) or not (0).
fn read_id<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    coll: &CollectionEntry,
    id: &Value,
    concern: teseladb_range::read::ReadConcern,
) -> Result<u64, CommandError> {
    let Some(key) = crud::id_doc_key(ctx.backend, coll, id)? else { return Ok(0) };
    let mut end = key.clone();
    end.push(0);
    for_each_match_counted(
        ctx.backend,
        (key, end),
        &Document::new(),
        None,
        None,
        concern,
        |_, _| Ok(false),
    )
}

/// `count`: without a query, the collection's count (`RECORD_STORE_FAST_COUNT`); with one, a
/// `COUNT` over a scan.
fn count_plan<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    run: bool,
) -> Result<Explained, CommandError> {
    let query = args::doc(ctx.cmd, "count", "query")?.cloned().unwrap_or_default();
    let skip = args::int_of(ctx.cmd, "count", "skip", &["int", "long", "double", "decimal"])?
        .unwrap_or(0)
        .max(0)
        .unsigned_abs();
    let limit = args::int_of(ctx.cmd, "count", "limit", &["int", "long", "double", "decimal"])?
        .map(i64::unsigned_abs)
        .filter(|l| *l > 0);
    let concern = args::read_concern(ctx.cmd, "count")?;
    let coll = ctx.backend.collection(ns)?;
    let collation = args::doc(ctx.cmd, "count", "collation")?
        .cloned()
        .or_else(|| coll.as_ref().and_then(options_collation));
    super::query::check_filter_syntax(&query, collation.as_ref(), None)?;
    // The node optimises the filter: a constant that fails fails after the shard's name.
    let optimized = teseladb_exec::expr::optimize::optimize_filter(&query, None).map_err(from_mql);
    optimized.map_err(|e| on_node(ctx, e))?;
    let read = Read { filter: &query, collation: collation.as_ref(), vars: None, concern };
    let (examined, matched) =
        if run { scan(ctx, coll.as_ref(), &read, None, |_| Ok(()))? } else { (0, 0) };
    let counted =
        limit.map_or(matched.saturating_sub(skip), |l| matched.saturating_sub(skip).min(l));
    if query.is_empty() {
        let mut s = Stage::new("RECORD_STORE_FAST_COUNT");
        s.counters.push("nCounted", long(counted));
        s.counters.push("nSkipped", 0);
        return Ok(Explained {
            parsed: Value::Null,
            router_stage: "SINGLE_SHARD",
            plan: s,
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    }
    let shown = parsed(&query, None)?;
    let mut s = Stage::new("COUNT");
    s.counters.push("nCounted", long(counted));
    s.counters.push("nSkipped", long(matched.min(skip)));
    // An `_id` equality reads that document alone (`IDHACK`, measured).
    if let (Some(id), Some(c)) =
        (crud::id_equality(&query).filter(|_| collation.is_none()), coll.as_ref())
    {
        let found = if run { read_id(ctx, c, &id, concern)? } else { 0 };
        let mut idhack = Stage::new("IDHACK");
        idhack.counters.push("keysExamined", long(found));
        idhack.counters.push("docsExamined", long(found));
        idhack.returned = found;
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SINGLE_SHARD",
            plan: s.over(idhack),
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    }
    let plan_read = super::plan_exec::ReadPlan {
        filter: &query,
        parsed: &shown,
        sort: Vec::new(),
        limit: limit.map(|l| skip.saturating_add(l)),
        hint: None,
        vars: None,
        concern,
        intersect: super::lock(ctx.shared).params.hash_intersection,
        index_keys: false,
    };
    if let Some((indexes, chosen)) =
        planned_read(ctx, coll.as_ref(), &plan_read, collation.as_ref())?
    {
        let winner = &chosen.winner;
        let input = match &winner.access {
            teseladb_plan::rules::Access::Index(scan)
                if scan.residual.is_none() && scan.bounds.iter().all(|(_, b)| b.len() == 1) =>
            {
                count_scan(ns, &indexes[scan.index], scan, winner, run)
            }
            // The index decides the whole filter: its keys are counted without the documents.
            teseladb_plan::rules::Access::Index(scan)
                if scan.residual.is_none() && !indexes[scan.index].meta.multikey =>
            {
                let keys = if run { winner.keys } else { 0 };
                let mut ix = index_stage(Some(ns), &indexes[scan.index], scan, keys, 0);
                ix.returned = if run { winner.keys.saturating_sub(winner.past) } else { 0 };
                ix
            }
            teseladb_plan::rules::Access::Or(scans)
                if scans
                    .iter()
                    .all(|s| s.residual.is_none() && !indexes[s.index].meta.multikey) =>
            {
                let mut sub = Stage::new("SUBPLAN").over(or_stage(&indexes, winner, scans, run));
                sub.returned = if run { winner.docs } else { 0 };
                sub
            }
            _ => planned_access(ns, &shown, &indexes, winner, run, None),
        };
        // Each rejected plan counts too (measured).
        let rejected = match input.name {
            "COUNT_SCAN" => Vec::new(),
            _ => chosen
                .others
                .iter()
                .map(|r| {
                    let mut d = Document::new().with("isCached", false);
                    let access = planned_access(ns, &shown, &indexes, r, false, None);
                    extend(&mut d, &Stage::new("COUNT").over(access).planned());
                    d
                })
                .collect(),
        };
        let plan = s.over(input);
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SINGLE_SHARD",
            plan,
            rejected,
            trials: Vec::new(),
        });
    }
    let plan = s.over(collscan(ns, &shown, examined, matched));
    Ok(Explained {
        parsed: Value::Document(shown),
        router_stage: "SINGLE_SHARD",
        plan,
        rejected: Vec::new(),
        trials: Vec::new(),
    })
}

/// `distinct`: a scan of its query.
fn distinct<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    run: bool,
) -> Result<Explained, CommandError> {
    match ctx.cmd.get("key") {
        Some(Value::String(_)) => {}
        Some(v) => return Err(wrong_type("distinct.key", v, "string")),
        None => return Err(args::missing("distinct", "key")),
    }
    let query = args::doc(ctx.cmd, "distinct", "query")?.cloned().unwrap_or_default();
    let concern = args::read_concern(ctx.cmd, "distinct")?;
    let coll = ctx.backend.collection(ns)?;
    let collation = args::doc(ctx.cmd, "distinct", "collation")?
        .cloned()
        .or_else(|| coll.as_ref().and_then(options_collation));
    check_filter(&query, collation.as_ref(), None)?;
    let shown = parsed(&query, None)?;
    let read = Read { filter: &query, collation: collation.as_ref(), vars: None, concern };
    let (examined, matched) =
        if run { scan(ctx, coll.as_ref(), &read, None, |_| Ok(()))? } else { (0, 0) };
    if let (Some(c), None, true) = (coll.as_ref(), collation.as_ref(), query.is_empty()) {
        let key = match ctx.cmd.get("key") {
            Some(Value::String(k)) => k.clone(),
            _ => String::new(),
        };
        let indexes = super::plan_exec::plan_indexes(ctx.backend, c)?;
        if let Some(index) = indexes
            .iter()
            .find(|i| i.meta.plannable && i.meta.key.first().is_some_and(|(f, _)| *f == key))
        {
            let values = if run {
                match super::query::distinct(ctx)?.get("values") {
                    Some(Value::Array(v)) => u64::try_from(v.len()).unwrap_or(u64::MAX),
                    _ => 0,
                }
            } else {
                0
            };
            let plan = distinct_scan(index, &all_of(index), &key, (values, values));
            return Ok(Explained {
                parsed: Value::Document(shown),
                router_stage: "SINGLE_SHARD",
                plan,
                rejected: Vec::new(),
                trials: Vec::new(),
            });
        }
    }
    let plan_read = super::plan_exec::ReadPlan {
        filter: &query,
        parsed: &shown,
        sort: Vec::new(),
        limit: None,
        hint: None,
        vars: None,
        concern,
        intersect: super::lock(ctx.shared).params.hash_intersection,
        index_keys: false,
    };
    let planned = match query.is_empty() {
        true => None,
        false => planned_read(ctx, coll.as_ref(), &plan_read, collation.as_ref())?,
    };
    if let (Some((indexes, chosen)), Some(c)) = (planned, coll.as_ref()) {
        let plan = distinct_plans(ctx, (ns, c, &shown), &indexes, &chosen, run)?;
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SINGLE_SHARD",
            plan: plan.0,
            rejected: plan.1,
            trials: Vec::new(),
        });
    }
    let plan = collscan(ns, &shown, examined, matched);
    Ok(Explained {
        parsed: Value::Document(shown),
        router_stage: "SINGLE_SHARD",
        plan,
        rejected: Vec::new(),
        trials: Vec::new(),
    })
}

/// The plan of a `distinct` with a query that indexes serve, and the plans it rejected: a plan
/// whose index holds the key after fields the query fixes, and decides the whole query, reads one
/// key per value (`DISTINCT_SCAN`) and goes first; the others fetch their documents (measured on
/// a `refrouter` 9.0.4).
fn distinct_plans<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    (ns, coll, shown): (&Namespace, &CollectionEntry, &Document),
    indexes: &[super::plan_exec::PlanIndex],
    chosen: &super::plan_exec::Chosen,
    run: bool,
) -> Result<(Stage, Vec<Document>), CommandError> {
    let key = match ctx.cmd.get("key") {
        Some(Value::String(k)) => k.clone(),
        _ => String::new(),
    };
    let by_key = |r: &super::plan_exec::Ran| match &r.access {
        teseladb_plan::rules::Access::Index(s) if reads_by_key(&indexes[s.index], s, &key) => {
            Some(s.clone())
        }
        _ => None,
    };
    let plans: Vec<&super::plan_exec::Ran> =
        std::iter::once(&chosen.winner).chain(&chosen.others).collect();
    let mut order: Vec<&super::plan_exec::Ran> =
        plans.iter().copied().filter(|r| by_key(r).is_some()).collect();
    order.extend(plans.iter().copied().filter(|r| by_key(r).is_none()));
    let shape = |r: &super::plan_exec::Ran, run: bool, counts: (u64, u64)| match by_key(r) {
        Some(scan) => distinct_scan(&indexes[scan.index], &scan, &key, counts),
        None => planned_access(ns, shown, indexes, r, run, None),
    };
    let Some((first, rest)) = order.split_first() else {
        return Ok((collscan(ns, shown, 0, 0), Vec::new()));
    };
    // The winner runs: one key per value, the entries outside its intervals, and the key after
    // its last interval that ends a scan of one.
    let first = (*first).clone();
    let first = match &chosen.winner.access {
        a if *a == first.access => chosen.winner.clone(),
        _ => first.at_trial(),
    };
    let counts = match (run, by_key(&first)) {
        (false, _) => (0, 0),
        (true, Some(scan)) => {
            let values = match super::query::distinct(ctx)?.get("values") {
                Some(Value::Array(v)) => u64::try_from(v.len()).unwrap_or(u64::MAX),
                _ => 0,
            };
            let after = scan.bounds.iter().all(|(_, b)| b.len() == 1)
                && super::plan_exec::key_after(ctx.backend, coll, &indexes[scan.index], &scan)?;
            (values + first.past + u64::from(after), values)
        }
        (true, None) => (0, 0),
    };
    let plan = shape(&first, run, counts);
    let rejected = rest
        .iter()
        .map(|r| {
            let mut d = Document::new().with("isCached", false);
            extend(&mut d, &shape(r, false, (0, 0)).planned());
            d
        })
        .collect();
    Ok((plan, rejected))
}

/// Whether scan `s` of `index` can read the values of `key` one key each: the index is no
/// multikey one, holds `key` after fields the query fixes to one value, and decides the whole
/// query.
fn reads_by_key(
    index: &super::plan_exec::PlanIndex,
    s: &teseladb_plan::rules::IndexScan,
    key: &str,
) -> bool {
    let Some(at) = index.meta.key.iter().position(|(f, _)| f == key) else { return false };
    !index.meta.multikey
        && s.residual.is_none()
        && s.bounds[..at].iter().all(|(_, b)| b.len() == 1 && b[0].is_point())
}

/// A scan of every key of `index`.
fn all_of(index: &super::plan_exec::PlanIndex) -> teseladb_plan::rules::IndexScan {
    teseladb_plan::rules::IndexScan {
        index: 0,
        bounds: index
            .meta
            .key
            .iter()
            .map(|(f, _)| (f.clone(), vec![teseladb_plan::logical::Bound::all()]))
            .collect(),
        backward: false,
        residual: None,
        sorts: false,
        assigned: 0,
        preds: Vec::new(),
    }
}

/// The one statement of an explained write batch (`field` of `cmd`).
fn single_statement(cmd: &Document, field: &str) -> Result<Document, CommandError> {
    match cmd.get(field) {
        Some(Value::Array(items)) if items.len() == 1 => match &items[0] {
            Value::Document(d) => Ok(d.clone()),
            v => Err(wrong_type(&format!("{field}.0"), v, "object")),
        },
        _ => Err(CommandError::new(INVALID_LENGTH, "explained write batches must be of size 1")),
    }
}

/// Update `u` with its `arrayFilters`, parsed.
fn parse_update(u: &Value, array_filters: Option<&Value>) -> Result<Update, CommandError> {
    let holder = Document::new()
        .with("u", u.clone())
        .with("f", array_filters.cloned().unwrap_or(Value::Array(Vec::new())));
    let hb = holder.to_bytes()?;
    let raw = RawDoc::from_bytes(&hb)?;
    let filters = match (array_filters, raw.get("f")) {
        (Some(_), Some(teseladb_bson::RawValue::Array(a))) => Some(a),
        _ => None,
    };
    Update::parse(raw.get("u").unwrap_or(teseladb_bson::RawValue::Null), filters).map_err(from_mql)
}

/// Counts what `update` would do to the documents of `read` (`multi`: all of them): the
/// documents read, the matches and those it would change.
fn would_update<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    coll: Option<&CollectionEntry>,
    read: &Read<'_>,
    update: &Update,
    multi: bool,
) -> Result<(u64, u64, u64), CommandError> {
    let collator = super::query::collation_of(read.collation)?;
    let col: Option<&dyn teseladb_bson::Collator> = collator.as_ref().map(|c| c as _);
    let now = crud::now_of(&*ctx.backend);
    let mut modified = 0u64;
    let (examined, matched) = scan(ctx, coll, read, (!multi).then_some(1), |bytes| {
        let matcher =
            MatchContext { collation: col, ext: &super::engines::NodeExtensions, vars: read.vars };
        let uctx = UpdateContext { matcher, position: None, now, insert: false };
        if update.apply(RawDoc::from_bytes(bytes)?, &uctx).map_err(from_mql)?.modified {
            modified += 1;
        }
        Ok(())
    })?;
    Ok((examined, matched, modified))
}

/// `update` of one statement: an `_id` equality without `multi` updates by key
/// (`EXPRESS_UPDATE`); anything else an `UPDATE` over a scan. What the node plans fails after
/// the shard's name.
fn update<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    run: bool,
) -> Result<Explained, CommandError> {
    let st = single_statement(ctx.cmd, "updates")?;
    let statement = crud::update_statement(&Value::Document(st.clone()), 0)?;
    let vars = super::query::command_let(ctx.backend, ctx.cmd, "update")?;
    let coll = ctx.backend.collection(ns)?;
    let collation = match st.get("collation") {
        Some(Value::Document(c)) => Some(c.clone()),
        _ => coll.as_ref().and_then(options_collation),
    };
    super::query::collation_of(collation.as_ref())?;
    let checked = crud::check_update_statement(&statement).map_err(|e| on_node(ctx, e));
    checked?;
    let statement = Statement { st: &st, coll: coll.as_ref(), collation: collation.as_ref() };
    update_on_node(ctx, ns, statement, vars.as_ref(), run).map_err(|e| on_node(ctx, e))
}

/// A statement of an explained `update` or `delete`, its collection and its collation.
struct Statement<'a> {
    st: &'a Document,
    coll: Option<&'a CollectionEntry>,
    collation: Option<&'a Document>,
}

impl Statement<'_> {
    /// Its filter.
    fn query(&self) -> Document {
        match self.st.get("q") {
            Some(Value::Document(q)) => q.clone(),
            _ => Document::new(),
        }
    }
}

/// The node's part of an explained `update`.
fn update_on_node<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    statement: Statement<'_>,
    vars: Option<&Document>,
    run: bool,
) -> Result<Explained, CommandError> {
    let Statement { st, coll, collation } = statement;
    let q = statement.query();
    let multi = matches!(st.get("multi"), Some(Value::Boolean(true)));
    let upsert = matches!(st.get("upsert"), Some(Value::Boolean(true)));
    check_filter(&q, collation, vars)?;
    let shown = parsed(&q, vars)?;
    let u = st.get("u").cloned().unwrap_or(Value::Null);
    let parsed_update = parse_update(&u, st.get("arrayFilters"))?;
    if coll.is_none() {
        return Ok(Explained {
            parsed: Value::Null,
            router_stage: "SHARD_WRITE",
            plan: missing_namespace(),
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    }
    let read =
        Read { filter: &q, collation, vars, concern: teseladb_range::read::ReadConcern::Local };
    let (examined, matched, modified) =
        if run { would_update(ctx, coll, &read, &parsed_update, multi)? } else { (0, 0, 0) };
    let upserted = u64::from(matched == 0 && upsert);
    // An `_id` equality updates through its index, `multi` or not (measured).
    let express = crud::id_equality(&q).filter(|_| collation.is_none());
    if express.is_some() {
        let mut s = Stage::new("EXPRESS_UPDATE");
        s.head.push("keyPattern", Document::new().with("_id", 1));
        s.head.push("indexName", "_id_");
        s.counters.push("keysExamined", 1);
        s.counters.push("docsExamined", long(matched));
        s.counters.push("nWouldModify", long(matched.max(modified)));
        s.counters.push("nWouldUpsert", long(upserted));
        s.counters.push("nWouldDelete", 0);
        return Ok(Explained {
            parsed: Value::Null,
            router_stage: "SHARD_WRITE",
            plan: s,
            rejected: Vec::new(),
            trials: Vec::new(),
        });
    }
    let mut s = Stage::new("UPDATE");
    s.counters.push("nMatched", long(matched));
    s.counters.push("nWouldModify", long(matched.max(modified)));
    s.counters.push("nWouldUpsert", long(upserted));
    let plan_read = super::plan_exec::ReadPlan {
        filter: &q,
        parsed: &shown,
        sort: Vec::new(),
        limit: (!multi).then_some(1),
        hint: None,
        vars,
        concern: teseladb_range::read::ReadConcern::Local,
        intersect: super::lock(ctx.shared).params.hash_intersection,
        index_keys: false,
    };
    if let Some((indexes, chosen)) = planned_write(ctx, coll, &plan_read, collation)? {
        let plan = s.over(planned_access(ns, &shown, &indexes, &chosen.winner, run, None));
        let rejected = rejected_reads(ns, &shown, &indexes, &chosen.others);
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SHARD_WRITE",
            plan,
            rejected,
            trials: Vec::new(),
        });
    }
    // An `_id` equality with a collation reads the index of `_id` (measured).
    let input = match crud::id_equality(&q).filter(|_| coll.is_some()) {
        Some(id) => {
            let ix = id_index_scan(ns, &id, examined.min(1), matched);
            fetch_stage(Some(ns), None, ix, matched, matched)
        }
        None => collscan(ns, &shown, examined, matched),
    };
    let plan = s.over(input);
    Ok(Explained {
        parsed: Value::Document(shown),
        router_stage: "SHARD_WRITE",
        plan,
        rejected: Vec::new(),
        trials: Vec::new(),
    })
}

/// `IXSCAN` of the index of `_id` at the one value `id`.
fn id_index_scan(ns: &Namespace, id: &Value, keys: u64, returned: u64) -> Stage {
    let mut s = Stage::new("IXSCAN");
    s.tail.push("nss", names::full(ns));
    s.tail.push("keyPattern", Document::new().with("_id", 1));
    s.tail.push("indexName", "_id_");
    s.tail.push("isMultiKey", false);
    s.tail.push("isUnique", true);
    s.tail.push("isSparse", false);
    s.tail.push("isPartial", false);
    s.tail.push("indexVersion", 2);
    s.tail.push("direction", "forward");
    let shown = teseladb_plan::logical::shown_value(id);
    let bounds = Value::Array(vec![Value::String(format!("[{shown}, {shown}]"))]);
    s.tail.push("indexBounds", Document::new().with("_id", bounds));
    s.counters.push("keysExamined", long(keys));
    s.counters.push("seeks", 1);
    s.counters.push("dupsTested", 0);
    s.counters.push("dupsDropped", 0);
    s.returned = returned;
    s
}

/// `delete` of one statement: `DELETE` with `limit: 1`, `BATCHED_DELETE` without. What the
/// node plans fails after the shard's name.
fn delete<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    run: bool,
) -> Result<Explained, CommandError> {
    let st = single_statement(ctx.cmd, "deletes")?;
    crud::delete_statement(&Value::Document(st.clone()), 0)?;
    let vars = super::query::command_let(ctx.backend, ctx.cmd, "delete")?;
    let coll = ctx.backend.collection(ns)?;
    let collation = match st.get("collation") {
        Some(Value::Document(c)) => Some(c.clone()),
        _ => coll.as_ref().and_then(options_collation),
    };
    super::query::collation_of(collation.as_ref())?;
    let statement = Statement { st: &st, coll: coll.as_ref(), collation: collation.as_ref() };
    delete_on_node(ctx, ns, statement, vars.as_ref(), run).map_err(|e| on_node(ctx, e))
}

/// The node's part of an explained `delete`.
fn delete_on_node<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    statement: Statement<'_>,
    vars: Option<&Document>,
    run: bool,
) -> Result<Explained, CommandError> {
    let Statement { st, coll, collation } = statement;
    let q = statement.query();
    let one = matches!(st.get("limit"), Some(Value::Int32(1) | Value::Int64(1)))
        || matches!(st.get("limit"), Some(Value::Double(x)) if *x == 1.0);
    check_filter(&q, collation, vars)?;
    let shown = parsed(&q, vars)?;
    let read =
        Read { filter: &q, collation, vars, concern: teseladb_range::read::ReadConcern::Local };
    let (examined, matched) =
        if run { scan(ctx, coll, &read, one.then_some(1), |_| Ok(()))? } else { (0, 0) };
    let mut s = Stage::new(if one { "DELETE" } else { "BATCHED_DELETE" });
    s.counters.push("nWouldDelete", long(matched));
    let plan_read = super::plan_exec::ReadPlan {
        filter: &q,
        parsed: &shown,
        sort: Vec::new(),
        limit: one.then_some(1),
        hint: None,
        vars,
        concern: teseladb_range::read::ReadConcern::Local,
        intersect: super::lock(ctx.shared).params.hash_intersection,
        index_keys: false,
    };
    if let Some((indexes, chosen)) = planned_write(ctx, coll, &plan_read, collation)? {
        let plan = s.over(planned_access(ns, &shown, &indexes, &chosen.winner, run, None));
        let rejected = rejected_reads(ns, &shown, &indexes, &chosen.others);
        return Ok(Explained {
            parsed: Value::Document(shown),
            router_stage: "SHARD_WRITE",
            plan,
            rejected,
            trials: Vec::new(),
        });
    }
    let plan = s.over(collscan(ns, &shown, examined, matched));
    Ok(Explained {
        parsed: Value::Document(shown),
        router_stage: "SHARD_WRITE",
        plan,
        rejected: Vec::new(),
        trials: Vec::new(),
    })
}

/// `findAndModify`: `UPDATE` or `DELETE` over a scan (sorted with a limit of one when it sorts),
/// projected by `fields`.
fn find_and_modify<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    run: bool,
) -> Result<Explained, CommandError> {
    let name = ctx.cmd.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default();
    let q = args::doc(ctx.cmd, &name, "query")?.cloned().unwrap_or_default();
    let remove = matches!(ctx.cmd.get("remove"), Some(Value::Boolean(true)));
    let upsert = matches!(ctx.cmd.get("upsert"), Some(Value::Boolean(true)));
    let vars = super::query::command_let(ctx.backend, ctx.cmd, &name)?;
    let coll = ctx.backend.collection(ns)?;
    let collation = args::doc(ctx.cmd, &name, "collation")?
        .cloned()
        .or_else(|| coll.as_ref().and_then(options_collation));
    check_filter(&q, collation.as_ref(), vars.as_ref())?;
    let sort = args::doc(ctx.cmd, &name, "sort")?.cloned().filter(|s| !s.is_empty());
    let read = Read {
        filter: &q,
        collation: collation.as_ref(),
        vars: vars.as_ref(),
        concern: teseladb_range::read::ReadConcern::Local,
    };
    let stop = if sort.is_some() { None } else { Some(1) };
    let parsed_update = match remove {
        true => None,
        false => Some(parse_update(
            ctx.cmd.get("update").unwrap_or(&Value::Null),
            ctx.cmd.get("arrayFilters"),
        )?),
    };
    let (examined, matched, modified) = match (run, &parsed_update) {
        (false, _) => (0, 0, 0),
        (true, None) => {
            let (e, m) = scan(ctx, coll.as_ref(), &read, stop, |_| Ok(()))?;
            (e, m, 0)
        }
        (true, Some(u)) => would_update(ctx, coll.as_ref(), &read, u, sort.is_some())?,
    };
    let shown = parsed(&q, vars.as_ref())?;
    let plan_read = super::plan_exec::ReadPlan {
        filter: &q,
        parsed: &shown,
        sort: sort.as_ref().map(sort_fields).unwrap_or_default(),
        limit: Some(1),
        hint: None,
        vars: vars.as_ref(),
        concern: teseladb_range::read::ReadConcern::Local,
        intersect: super::lock(ctx.shared).params.hash_intersection,
        index_keys: false,
    };
    let planned = match sort.is_some() {
        true => planned_read(ctx, coll.as_ref(), &plan_read, collation.as_ref())?,
        false => planned_write(ctx, coll.as_ref(), &plan_read, collation.as_ref())?,
    };
    let (mut plan, rejected, index_sorts) = match &planned {
        Some((indexes, chosen)) => (
            planned_access(ns, &shown, indexes, &chosen.winner, run, None),
            rejected_reads(ns, &shown, indexes, &chosen.others),
            sorted(&chosen.winner),
        ),
        None => (collscan(ns, &shown, examined, matched), Vec::new(), false),
    };
    // An index in the sort's order takes the first document (measured: `LIMIT` 1).
    if sort.is_some() && index_sorts {
        let mut s = Stage::new("LIMIT");
        s.tail.push("limitAmount", 1);
        s.returned = plan.returned.min(1);
        plan = s.over(plan);
    } else if let Some(sort) = sort {
        let mut s = Stage::new("SORT");
        s.tail.push("sortPattern", sort);
        s.tail.push("memLimit", SORT_MEM_LIMIT);
        s.tail.push("limitAmount", 1);
        s.tail.push("type", "default");
        sort_counters(&mut s);
        s.returned = plan.returned.min(1);
        plan = s.over(plan);
    }
    let returned = plan.returned.min(1);
    let mut write = if remove {
        let mut s = Stage::new("DELETE");
        s.counters.push("nWouldDelete", long(returned));
        s
    } else {
        let mut s = Stage::new("UPDATE");
        s.counters.push("nMatched", long(returned));
        s.counters.push("nWouldModify", long(returned.max(modified.min(1))));
        s.counters.push("nWouldUpsert", long(u64::from(returned == 0 && upsert)));
        s
    };
    write.returned = returned;
    plan = write.over(plan);
    if let Some(p) = args::doc(ctx.cmd, &name, "fields")?.filter(|p| !p.is_empty()) {
        let mut s = Stage::new("PROJECTION_DEFAULT");
        s.tail.push("transformBy", p.clone());
        s.returned = plan.returned;
        plan = s.over(plan);
    }
    Ok(Explained {
        parsed: Value::Document(shown),
        router_stage: "SINGLE_SHARD",
        plan,
        rejected,
        trials: Vec::new(),
    })
}

/// The `planSummary` of a plan: each scan it reads by (`COLLSCAN`, `IXSCAN { a: 1 }`), in the
/// order of the plan, joined by commas as a `refnode` 9.0.4 writes it in `system.profile`.
fn plan_summary(s: &Stage) -> String {
    fn leaves(s: &Stage, out: &mut Vec<String>) {
        if s.input.is_none() && s.inputs.is_empty() {
            let text = match (s.name, s.tail.get("keyPattern")) {
                (name, Some(Value::Document(k))) => {
                    let bytes = k.to_bytes().unwrap_or_default();
                    let shown = RawDoc::from_bytes(&bytes)
                        .map(|r| teseladb_mql::shell(teseladb_bson::RawValue::Document(r)))
                        .unwrap_or_default();
                    format!("{name} {shown}")
                }
                (name, _) => name.to_owned(),
            };
            out.push(text);
        }
        if let Some(i) = &s.input {
            leaves(i, out);
        }
        for i in &s.inputs {
            leaves(i, out);
        }
    }
    let mut out = Vec::new();
    leaves(s, &mut out);
    out.join(", ")
}

/// What the profiler shows of the read of `ns` with parsed filter `shown` that plan `ran` made
/// (010 FR-015): `planSummary`, `keysExamined`, `docsExamined` and `execStats` of the plan as
/// it ran. A collection scan's counters are the operation's own reads ([`collscan_report`]).
pub(crate) fn plan_report(
    ns: &Namespace,
    shown: &Document,
    indexes: &[super::plan_exec::PlanIndex],
    ran: &super::plan_exec::Ran,
) -> Document {
    let stage = planned_access(ns, shown, indexes, ran, true, None);
    Document::new()
        .with("planSummary", plan_summary(&stage))
        .with("keysExamined", long(ran.keys))
        .with("docsExamined", long(ran.docs))
        .with("execStats", stage.executed())
}

/// [`plan_report`] of a collection scan of `ns` with parsed filter `shown` that read `examined`
/// documents and gave `returned`.
pub(crate) fn collscan_report(
    ns: &str,
    shown: &Document,
    examined: u64,
    returned: u64,
) -> Document {
    let (db, coll) = ns.split_once('.').unwrap_or((ns, ""));
    let stage = collscan(&names::ns(db, coll), shown, examined, returned);
    Document::new().with("planSummary", plan_summary(&stage)).with("execStats", stage.executed())
}
