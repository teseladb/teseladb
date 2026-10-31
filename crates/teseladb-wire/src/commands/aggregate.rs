//! `aggregate` (010 FR-004, FR-011): parses the pipeline with 010's engine
//! (`teseladb_exec::pipeline`), reads the collection — through the definitions of the views it
//! stands on — runs the pipeline over the documents and returns them through a cursor that
//! `getMore` continues, with the arguments, replies and errors of a `refrouter` 9.0.4 (measured
//! 31-10-2026).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::collation::Collation;
use teseladb_exec::expr::Ctx as ExprCtx;
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_meta::catalog::Namespace;
use teseladb_meta::collection_types::CollectionKind;
use teseladb_mql::{Extensions, MqlError};
use teseladb_range::read::ReadConcern;

use super::engines::NodeExtensions;
use super::mutation::from_mql;
use super::query::{
    DEFAULT_FIRST_BATCH, QueryCursor, Source, command_let, documents_of, for_each_match_with, keep,
    target,
};
use super::{Backend, Ctx, args, names};
use crate::error::{BAD_VALUE, CommandError, FAILED_TO_PARSE, INVALID_NAMESPACE, TYPE_MISMATCH};

/// The command's name.
pub const AGGREGATE: &str = super::status::AGGREGATE;

/// The collection of an `aggregate: 1`.
const COLLECTIONLESS: &str = "$cmd.aggregate";

/// The stages an `aggregate: 1` may start with: they read no collection.
const COLLECTIONLESS_STAGES: [&str; 6] = [
    "$documents",
    "$currentOp",
    "$listLocalSessions",
    "$changeStream",
    "$listClusterCatalog",
    "$querySettings",
];

/// What the pipeline reads besides its input, from the node.
struct WireEnv<'a, B: Backend + ?Sized> {
    backend: RefCell<&'a mut B>,
    db: &'a str,
    vars: Document,
    concern: ReadConcern,
    /// The collation strings compare with (010 FR-014).
    collation: Collation,
    /// The node's shared state and the aggregated collection, for the stages the node answers
    /// itself (`$collStats`); `None` inside a view's pipeline.
    node: Option<(&'a super::Shared, Namespace)>,
}

/// An error of the node as an error of the pipeline.
fn to_mql(e: CommandError) -> MqlError {
    MqlError::new(teseladb_mql::ErrorCode { code: e.code.code, name: e.code.name }, e.errmsg)
}

impl<B: Backend + ?Sized> WireEnv<'_, B> {
    /// `$collStats` of the aggregated collection (015 FR-010), with the sections `spec` asks
    /// for in RefDb's order. TeselaDB has no shards: the document has no `shard` (DV-038).
    fn coll_stats(&self, spec: &Document) -> Result<Vec<Document>, CommandError> {
        let Some((shared, ns)) = &self.node else { return Ok(Vec::new()) };
        let mut backend = self.backend.borrow_mut();
        let ns_text = names::full(ns);
        let mut d = Document::new()
            .with("ns", ns_text.clone())
            .with("host", backend.process().host_name())
            .with("localTime", Value::DateTime(backend.now_ms() as i64));
        let coll = backend.collection(ns)?;
        let needs =
            |section: &str| -> Result<&teseladb_meta::catalog::CollectionEntry, CommandError> {
                match &coll {
                    None => Err(CommandError::new(
                        crate::error::NAMESPACE_NOT_FOUND,
                        format!(
                            "Unable to retrieve {section} in $collStats stage :: caused by :: Collection [{ns_text}] not found."
                        ),
                    )),
                    Some(c) => {
                        if matches!(super::kinds::kind_of(c)?, CollectionKind::View { .. }) {
                            return Err(CommandError::new(
                                crate::error::COMMAND_NOT_SUPPORTED_ON_VIEW,
                                format!("Namespace {ns_text} is a view, not a collection"),
                            ));
                        }
                        Ok(c)
                    }
                }
            };
        if let Some(Value::Document(l)) = spec.get("latencyStats") {
            let histograms = matches!(l.get("histograms"), Some(Value::Boolean(true)));
            d.push(
                "latencyStats",
                super::lock(shared).status.collection_latency(&ns_text, histograms),
            );
        }
        if let Some(Value::Document(st)) = spec.get("storageStats") {
            let c = needs("storageStats")?;
            let scale = st.get("scale").and_then(|v| match v {
                Value::Int32(n) => Some(i64::from(*n)),
                Value::Int64(n) => Some(*n),
                Value::Double(x) => Some(*x as i64),
                _ => None,
            });
            let stats = super::diagnostics::storage_stats(
                &mut **backend,
                shared,
                ns,
                c,
                scale.unwrap_or(1),
            )?;
            d.push("storageStats", stats);
        }
        if spec.get("count").is_some() {
            let c = needs("count")?;
            let (start, end) = documents_of(c);
            let mut n: i64 = 0;
            for_each_match_with(
                &mut **backend,
                (start, end),
                &Document::new(),
                None,
                None,
                self.concern,
                |_, _| {
                    n += 1;
                    Ok(true)
                },
            )?;
            d.push("count", i32::try_from(n).map_or(Value::Int64(n), Value::Int32));
        }
        if spec.get("queryExecStats").is_some() {
            let scans = super::lock(shared).status.collection_scans_of(&ns_text);
            d.push(
                "queryExecStats",
                Document::new().with(
                    "collectionScans",
                    Document::new()
                        .with("total", Value::Int64(scans))
                        .with("nonTailable", Value::Int64(scans)),
                ),
            );
        }
        Ok(vec![d])
    }
}

impl<B: Backend + ?Sized> Env for WireEnv<'_, B> {
    fn ext(&self) -> &dyn Extensions {
        &NodeExtensions
    }

    fn vars(&self) -> Option<&Document> {
        Some(&self.vars)
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        let ns = names::ns(self.db, name);
        let mut backend = self.backend.borrow_mut();
        read(&mut **backend, &ns, self.concern).map_err(to_mql)
    }

    fn collation(&self) -> Collation {
        self.collation.clone()
    }

    fn random(&self) -> f64 {
        let bits = self.backend.borrow_mut().random();
        (bits >> 11) as f64 / (1u64 << 53) as f64
    }

    fn system_stage(&self, name: &str, spec: &Document) -> Result<Vec<Document>, MqlError> {
        match name {
            "$collStats" => self.coll_stats(spec).map_err(to_mql),
            other => Err(MqlError::new(
                teseladb_mql::ErrorCode { code: BAD_VALUE.code, name: BAD_VALUE.name },
                format!("{other} is not supported by this node"),
            )),
        }
    }
}

/// The documents of collection `ns`, read through the views it stands on (their pipelines run
/// over the collection at the end of the chain, the innermost first).
fn read<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    concern: ReadConcern,
) -> Result<Vec<Document>, CommandError> {
    read_through(backend, ns, &[], None, concern)
}

/// The documents of `ns` read through its views as [`read`] does, then through `extra`, the
/// stages a read on a view becomes (015 FR-001), with the variables `vars` of its `let`.
pub(crate) fn read_through<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    extra: &[Document],
    vars: Option<&Document>,
    concern: ReadConcern,
) -> Result<Vec<Document>, CommandError> {
    // `admin.system.users`, `.roles` and `.version` come from Group 0 (013 FR-030).
    if let Some(docs) = super::authz_collections::documents(backend, ns)? {
        return Ok(docs);
    }
    let view = super::views::resolve(backend, ns)?;
    let own = view.as_ref().and_then(|v| v.collation.as_deref()).map(RawDoc::from_bytes);
    let collation = collation_value(own.transpose()?.map(Document::from).as_ref())?;
    let base = match &view {
        Some(v) => names::ns(&ns.db, &v.collection),
        None => ns.clone(),
    };
    let Some(coll) = backend.collection(&base)? else { return Ok(Vec::new()) };
    let mut docs = Vec::new();
    for_each_match_with(
        backend,
        documents_of(&coll),
        &Document::new(),
        None,
        None,
        concern,
        |_, bytes| {
            docs.push(Document::from(RawDoc::from_bytes(bytes)?));
            Ok(true)
        },
    )?;
    let mut stages: Vec<Vec<u8>> = view.map(|v| v.pipeline).unwrap_or_default();
    for d in extra {
        stages.push(d.to_bytes()?);
    }
    if stages.is_empty() {
        return Ok(docs);
    }
    let raws: Vec<RawDoc<'_>> =
        stages.iter().map(|b| RawDoc::from_bytes(b)).collect::<Result<_, _>>()?;
    let values: Vec<RawValue<'_>> = raws.into_iter().map(RawValue::Document).collect();
    let vars = vars.cloned().unwrap_or_default();
    let names: Vec<String> = vars.iter().map(|(k, _)| k.to_owned()).collect();
    let pipeline = Pipeline::parse(&values, &ExprCtx::checked(names.iter().map(String::as_str)))
        .map_err(from_mql)?;
    let env = WireEnv {
        backend: RefCell::new(backend),
        db: &ns.db,
        vars,
        concern,
        collation,
        node: None,
    };
    pipeline.run(docs, &env).map_err(from_mql)
}

/// The arguments of `aggregate` besides its pipeline.
struct AggregateArgs {
    batch_size: Option<usize>,
    /// The command's `collation`.
    collation: Option<Document>,
    vars: Option<Document>,
    concern: ReadConcern,
}

/// Reads the `cursor` document: `batchSize` only.
fn cursor_batch_size(cmd: &Document) -> Result<Option<usize>, CommandError> {
    let c = match cmd.get("cursor") {
        None => {
            return Err(CommandError::new(
                FAILED_TO_PARSE,
                "The 'cursor' option is required, except for aggregate with the explain argument",
            ));
        }
        Some(Value::Document(c)) => c,
        Some(_) => {
            return Err(CommandError::new(
                TYPE_MISMATCH,
                "cursor field must be missing or an object",
            ));
        }
    };
    let mut size = None;
    for (k, v) in c.iter() {
        if k != "batchSize" {
            return Err(CommandError::new(
                crate::error::IDL_UNKNOWN_FIELD,
                format!("BSON field 'cursor.{k}' is an unknown field."),
            ));
        }
        let n = match v {
            Value::Int32(n) => i64::from(*n),
            Value::Int64(n) => *n,
            Value::Double(x) => *x as i64,
            Value::Decimal128(d) => teseladb_mql::decimal_saturating(*d),
            other => {
                return Err(CommandError::new(
                    TYPE_MISMATCH,
                    format!(
                        "BSON field 'cursor.batchSize' is the wrong type '{}', expected types '[int, decimal, long, double]'",
                        crate::types::type_name(other)
                    ),
                ));
            }
        };
        if n < 0 {
            return Err(CommandError::new(
                BAD_VALUE,
                format!("BSON field 'batchSize' value must be >= 0, actual value '{n}'"),
            ));
        }
        size = Some(usize::try_from(n).unwrap_or(usize::MAX));
    }
    Ok(size)
}

fn aggregate_args(cmd: &Document) -> Result<AggregateArgs, CommandError> {
    match cmd.get("pipeline") {
        None => {
            return Err(CommandError::new(
                crate::error::IDL_FAILED_TO_PARSE,
                "BSON field 'aggregate.pipeline' is missing but a required field",
            ));
        }
        Some(Value::Array(_)) => {}
        Some(_) => {
            return Err(CommandError::new(TYPE_MISMATCH, "A pipeline must be an array of objects"));
        }
    }
    let batch_size = cursor_batch_size(cmd)?;
    if let Some(v) = cmd.get("allowDiskUse")
        && !matches!(v, Value::Boolean(_))
    {
        return Err(CommandError::new(
            TYPE_MISMATCH,
            format!(
                "Field 'allowDiskUse' should be a boolean value, but found: {}",
                crate::types::type_name(v)
            ),
        ));
    }
    let collation = args::doc(cmd, AGGREGATE, "collation")?.cloned();
    Ok(AggregateArgs {
        batch_size,
        collation,
        vars: command_let(cmd, AGGREGATE)?,
        concern: args::read_concern(cmd, AGGREGATE)?,
    })
}

/// Whether `v` is the number 1 (`aggregate: 1`).
fn is_one(v: &Value) -> bool {
    match v {
        Value::Int32(n) => *n == 1,
        Value::Int64(n) => *n == 1,
        Value::Double(x) => *x == 1.0,
        Value::Decimal128(d) => teseladb_mql::decimal_exact(*d) == Some(1),
        _ => false,
    }
}

/// `aggregate`.
pub fn aggregate<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let first = ctx.first().clone();
    let (ns, on_collection) = match &first {
        Value::String(_) => (target(ctx)?, true),
        v if is_one(v) => (names::ns(ctx.db, COLLECTIONLESS), false),
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => {
            return Err(CommandError::new(
                BAD_VALUE,
                "Invalid command format: the 'aggregate' field must specify a collection name or 1",
            ));
        }
        _ => (target(ctx)?, true),
    };
    let ns_text = names::full(&ns);
    let cmd_bytes = ctx.cmd.to_bytes()?;
    let raw_cmd = RawDoc::from_bytes(&cmd_bytes)?;
    let a = aggregate_args(ctx.cmd)?;
    let stages: Vec<RawValue<'_>> = match raw_cmd.get("pipeline") {
        Some(RawValue::Array(p)) => p.iter().collect(),
        _ => Vec::new(),
    };
    let mut vars = a.vars.clone().unwrap_or_default();
    let names: Vec<String> = vars.iter().map(|(k, _)| k.to_owned()).collect();
    vars.insert("NOW", Value::DateTime(ctx.backend.now_ms() as i64));
    let pipeline = Pipeline::parse(&stages, &ExprCtx::checked(names.iter().map(String::as_str)))
        .map_err(from_mql)?;
    if !on_collection {
        collectionless(&stages)?;
    }
    pipeline.check_target(on_collection).map_err(from_mql)?;
    let collation = command_collation(&mut *ctx.backend, &ns, on_collection, a.collation.as_ref())?;
    let docs = if on_collection && pipeline.reads_collection() {
        read(&mut *ctx.backend, &ns, a.concern)?
    } else {
        Vec::new()
    };
    let env = WireEnv {
        backend: RefCell::new(&mut *ctx.backend),
        db: ctx.db,
        vars,
        concern: a.concern,
        collation,
        node: Some((ctx.shared, ns.clone())),
    };
    let results = pipeline.run(docs, &env).map_err(|e| {
        let mut err = from_mql(e);
        err.errmsg = format!(
            "Executor error during aggregate command on namespace: {ns_text} :: caused by :: {}",
            err.errmsg
        );
        err
    })?;
    drop(env);
    let mut state = QueryCursor {
        source: Source::Done,
        buffered: VecDeque::from(results),
        skip: 0,
        left: None,
        tail: None,
        read_mark: ctx.backend.read_mark(),
    };
    let size = Some(a.batch_size.unwrap_or(DEFAULT_FIRST_BATCH));
    let (batch, short) = state.batch(ctx.backend, size)?;
    let exhausted = short && state.buffered.is_empty();
    let id = keep(ctx, &ns_text, state, exhausted, false)?;
    let batch: Vec<Value> = batch.into_iter().map(Value::Document).collect();
    // The fields of the cursor come in this order on a collection, and the other way for
    // `aggregate: 1` (measured).
    let cursor = if on_collection {
        Document::new().with("id", id).with("ns", ns_text).with("firstBatch", batch)
    } else {
        Document::new().with("firstBatch", batch).with("id", id).with("ns", ns_text)
    };
    Ok(Document::new().with("cursor", cursor).with("ok", 1.0))
}

/// A collation document as the pipeline compares with it: none for the simple one.
pub(crate) fn collation_value(doc: Option<&Document>) -> Result<Collation, CommandError> {
    let Some(d) = doc else { return Ok(Collation::default()) };
    let bytes = d.to_bytes()?;
    match teseladb_collation::parse_collation(RawDoc::from_bytes(&bytes)?)? {
        teseladb_collation::ParsedCollation::Simple => Ok(Collation::default()),
        teseladb_collation::ParsedCollation::Collation(c, _) => Ok(Collation::new(Rc::new(c))),
    }
}

/// The collation of an `aggregate` (010 FR-014): the command's, else the view's or the
/// collection's default; a view refuses another than its own.
fn command_collation<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    on_collection: bool,
    given: Option<&Document>,
) -> Result<Collation, CommandError> {
    if !on_collection {
        return collation_value(given);
    }
    if let Some(view) = super::views::resolve(backend, ns)? {
        super::views::check_collation(&view, given)?;
        let own = view.collation.as_deref().map(RawDoc::from_bytes).transpose()?;
        return collation_value(given.or(own.map(Document::from).as_ref()));
    }
    let default = backend.collection(ns)?.and_then(|c| super::query::options_collation(&c));
    collation_value(given.or(default.as_ref()))
}

/// Checks the first stage of an `aggregate: 1`, which must read no collection.
fn collectionless(stages: &[RawValue<'_>]) -> Result<(), CommandError> {
    let Some(RawValue::Document(first)) = stages.first() else {
        return Err(CommandError::new(
            INVALID_NAMESPACE,
            "{aggregate: 1} is not valid for an empty pipeline.",
        ));
    };
    let name = first.iter().next().map(|(k, _)| k).unwrap_or("");
    if !COLLECTIONLESS_STAGES.contains(&name) {
        return Err(CommandError::new(
            INVALID_NAMESPACE,
            format!("{{aggregate: 1}} is not valid for '{name}'; a collection is required."),
        ));
    }
    Ok(())
}
