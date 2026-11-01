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
use teseladb_exec::pipeline::meta::{INDEX_KEY, Meta};
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

/// The key of a 2d index's field.
const FLAT_INDEX: &str = "2d";
/// The key of a 2dsphere index's field.
const SPHERE_INDEX: &str = "2dsphere";

/// The command's name.
pub const AGGREGATE: &str = super::status::AGGREGATE;

/// The collection of an `aggregate: 1`.
const COLLECTIONLESS: &str = "$cmd.aggregate";

/// What the pipeline reads besides its input, from the node.
struct WireEnv<'a, B: Backend + ?Sized> {
    backend: RefCell<&'a mut B>,
    db: &'a str,
    vars: Document,
    concern: ReadConcern,
    /// The collation strings compare with (010 FR-014).
    collation: Collation,
    /// `allowDiskUse` (010 FR-010).
    allow_disk_use: bool,
    /// The node's shared state and the aggregated collection, for the stages the node answers
    /// itself (`$collStats`); `None` inside a view's pipeline.
    node: Option<(&'a super::Shared, Namespace)>,
}

/// An error of the node as an error of the pipeline.
fn to_mql(e: CommandError) -> MqlError {
    MqlError::new(teseladb_mql::ErrorCode { code: e.code.code, name: e.code.name }, e.errmsg)
}

impl<B: Backend + ?Sized> WireEnv<'_, B> {
    /// `$listLocalSessions`: the sessions this node holds in memory, as the records of
    /// `config.system.sessions` write them, for the users `spec` asks for.
    fn local_sessions(&self, spec: &Document) -> Result<Vec<Document>, CommandError> {
        let Some((shared, _)) = &self.node else { return Ok(Vec::new()) };
        let used = super::lock(shared).sessions.used_since(0);
        Ok(used
            .into_iter()
            .map(|(lsid, last)| {
                Document::new()
                    .with("_id", super::registry::record_id(lsid))
                    .with("lastUse", Value::DateTime(i64::try_from(last).unwrap_or(i64::MAX)))
            })
            .filter(|d| wanted_session(d.get("_id"), spec))
            .collect())
    }

    /// The namespace of the aggregated collection and its ready 2d and 2dsphere indexes, in
    /// the catalog's order (`$geoNear`).
    fn geo_index_list(
        &self,
    ) -> Result<(String, Vec<teseladb_exec::pipeline::geo_near::GeoIndex>), CommandError> {
        let Some((_, ns)) = &self.node else { return Ok((String::new(), Vec::new())) };
        let mut backend = self.backend.borrow_mut();
        let mut out = Vec::new();
        if let Some(coll) = backend.collection(ns)? {
            for entry in backend.indexes(coll.id)? {
                if entry.state != teseladb_meta::indexes::IndexState::Ready {
                    continue;
                }
                let spec = Document::from(RawDoc::from_bytes(&entry.spec)?);
                let Some(Value::Document(key)) = spec.get("key") else { continue };
                for (field, kind) in key.iter() {
                    match kind {
                        Value::String(k) if k == FLAT_INDEX || k == SPHERE_INDEX => {
                            out.push(teseladb_exec::pipeline::geo_near::GeoIndex {
                                field: field.to_owned(),
                                sphere: k == SPHERE_INDEX,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok((names::full(ns), out))
    }

    /// `$listCatalog`: the catalog entry of the aggregated collection, or of every collection
    /// with `aggregate: 1` on `admin`.
    fn list_catalog(&self) -> Result<Vec<Document>, CommandError> {
        let Some((_, ns)) = &self.node else { return Ok(Vec::new()) };
        let only = (ns.coll != COLLECTIONLESS).then_some(ns.coll.as_str());
        super::list_catalog::entries(&mut **self.backend.borrow_mut(), &ns.db, only)
    }

    /// `$listClusterCatalog`: every collection and view of the command's database (of every
    /// database on `admin`), as `listCollections` writes them with their namespace and database,
    /// none sharded; `shards` names the node and `tracked` says none is (measured on a `refrouter`
    /// 9.0.4).
    fn cluster_catalog(&self, spec: &Document) -> Result<Vec<Document>, CommandError> {
        let mut backend = self.backend.borrow_mut();
        let node = super::shard_name(backend.node());
        let mut found = backend.collections()?;
        found.retain(|(ns, _)| self.db == super::admin::ADMIN_DB || ns.db == self.db);
        // Views first, as RefDb lists them; the collections in namespace order (RefDb's own
        // follows its catalog's hash table).
        let is_view = |c: &teseladb_meta::catalog::CollectionEntry| {
            matches!(super::kinds::kind_of(c), Ok(CollectionKind::View { .. }))
        };
        found.sort_by(|a, b| {
            (!is_view(&a.1), names::full(&a.0)).cmp(&(!is_view(&b.1), names::full(&b.0)))
        });
        let flag = |k: &str| matches!(spec.get(k), Some(Value::Boolean(true)));
        let mut out = Vec::new();
        for (ns, c) in found {
            let kind = super::kinds::kind_of(&c)?;
            let mut d = Document::new()
                .with("ns", names::full(&ns))
                .with("db", ns.db.as_str())
                .with("type", kind.type_name())
                .with("options", super::catalog::options_of(&c)?);
            match kind {
                CollectionKind::View { .. } => {
                    d.push("info", Document::new().with("readOnly", true))
                }
                _ => d.push(
                    "info",
                    Document::new()
                        .with("readOnly", false)
                        .with("uuid", Value::Binary { subtype: 4, bytes: c.uuid.to_vec() }),
                ),
            }
            if matches!(kind, CollectionKind::Normal | CollectionKind::Capped { .. }) {
                d.push("idIndex", super::catalog::id_index(&c)?);
            }
            d.push("sharded", false);
            if flag("shards") && !matches!(kind, CollectionKind::View { .. }) {
                d.push("shards", vec![Value::String(node.clone())]);
            }
            if flag("tracked") {
                d.push("tracked", false);
            }
            out.push(d);
        }
        Ok(out)
    }

    /// `$currentOp`: the operations running on every node, as `currentOp` describes them.
    fn current_ops(&self) -> Result<Vec<Document>, CommandError> {
        let Some((shared, _)) = &self.node else { return Ok(Vec::new()) };
        let mut backend = self.backend.borrow_mut();
        super::admin::current_operations(&mut **backend, shared)
    }

    /// `$planCacheStats`: the entries of the node's plan cache for the aggregated collection,
    /// which must exist (50933).
    fn plan_cache_stats(&self) -> Result<Vec<Document>, CommandError> {
        let Some((shared, ns)) = &self.node else { return Ok(Vec::new()) };
        let mut backend = self.backend.borrow_mut();
        let ns_text = names::full(ns);
        let Some(coll) = backend.collection(ns)? else {
            return Err(CommandError::new(
                crate::error::PLAN_CACHE_NO_COLLECTION,
                format!("collection '{ns_text}' does not exist"),
            ));
        };
        let indexes = super::plan_exec::plan_indexes(&mut **backend, &coll)?;
        let catalog = super::plan_cache::catalog_signature(&coll, &indexes);
        super::lock(shared).plan_cache.forget_other(&ns_text, &catalog);
        let facts = backend.process();
        let host = format!("{}:{}", facts.host.hostname, facts.port);
        let shard = super::shard_name(backend.node());
        Ok(super::lock(shared).plan_cache.stats(&ns_text, (&host, &shard)))
    }

    /// `$indexStats`: each index of the aggregated collection with how many reads and writes
    /// its plans scanned it and since when, as a `refrouter` 9.0.4 lists them (measured in
    /// `tests/aggregate/index_stats.out`); nothing for a collection that does not exist.
    fn index_stats(&self) -> Result<Vec<Document>, CommandError> {
        let Some((shared, ns)) = &self.node else { return Ok(Vec::new()) };
        let mut backend = self.backend.borrow_mut();
        let Some(coll) = backend.collection(ns)? else { return Ok(Vec::new()) };
        let mut specs = match super::catalog::has_id_index(ns) {
            true => vec![super::catalog::id_index(&coll)?],
            false => Vec::new(),
        };
        specs.extend(super::indexes::listed(&mut **backend, &coll)?);
        let facts = backend.process();
        let host = format!("{}:{}", facts.host.hostname, facts.port);
        let shard = super::shard_name(backend.node());
        let now = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
        let ns_text = names::full(ns);
        let mut g = super::lock(shared);
        let names: Vec<String> = specs
            .iter()
            .filter_map(|s| match s.get("name") {
                Some(Value::String(n)) => Some(n.clone()),
                _ => None,
            })
            .collect();
        g.index_usage.keep_only(&ns_text, &names);
        Ok(specs
            .into_iter()
            .zip(names)
            .map(|(spec, name)| {
                let (ops, since) = g.index_usage.of(&ns_text, &name, now);
                let accesses =
                    Document::new().with("ops", ops).with("since", Value::DateTime(since));
                Document::new()
                    .with("name", name)
                    .with("key", spec.get("key").cloned().unwrap_or(Value::Null))
                    .with("host", host.clone())
                    .with("accesses", accesses)
                    .with("shard", shard.clone())
                    .with("spec", spec)
            })
            .collect())
    }

    /// `$listSessions`: the records of `config.system.sessions` for the users `spec` asks for.
    fn recorded_sessions(&self, spec: &Document) -> Result<Vec<Document>, CommandError> {
        let Some((_, ns)) = &self.node else { return Ok(Vec::new()) };
        let mut backend = self.backend.borrow_mut();
        let records = read(&mut **backend, ns, self.concern)?;
        Ok(records.into_iter().filter(|d| wanted_session(d.get("_id"), spec)).collect())
    }

    /// The documents of `$listSearchIndexes` on the pipeline's collection (011 FR-008).
    fn search_indexes(&self, spec: &Document) -> Result<Vec<Document>, CommandError> {
        let Some((_, ns)) = &self.node else { return Ok(Vec::new()) };
        let mut backend = self.backend.borrow_mut();
        super::search_indexes::stage(&mut **backend, ns, spec)
    }

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
            let scale = st.get("scale").and_then(teseladb_exec::pipeline::scale_of);
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

    fn allow_disk_use(&self) -> bool {
        self.allow_disk_use
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

    fn geo_indexes(
        &self,
    ) -> Result<(String, Vec<teseladb_exec::pipeline::geo_near::GeoIndex>), MqlError> {
        self.geo_index_list().map_err(to_mql)
    }

    fn system_stage(&self, name: &str, spec: &Document) -> Result<Vec<Document>, MqlError> {
        match name {
            "$collStats" => self.coll_stats(spec).map_err(to_mql),
            teseladb_exec::pipeline::LIST_SEARCH_INDEXES => {
                self.search_indexes(spec).map_err(to_mql)
            }
            "$listLocalSessions" => self.local_sessions(spec).map_err(to_mql),
            "$listSessions" => self.recorded_sessions(spec).map_err(to_mql),
            "$planCacheStats" => self.plan_cache_stats().map_err(to_mql),
            "$indexStats" => self.index_stats().map_err(to_mql),
            "$currentOp" => self.current_ops().map_err(to_mql),
            "$listClusterCatalog" => self.cluster_catalog(spec).map_err(to_mql),
            // TeselaDB loads no extensions.
            "$listExtensions" => Ok(Vec::new()),
            "$listCatalog" => self.list_catalog().map_err(to_mql),
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

/// [`read`], and whether the stages of the views it read through all run in RefDb's
/// slot-based engine (the stages that follow them may then run there too).
fn read_engine<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    concern: ReadConcern,
) -> Result<(Vec<Document>, bool), CommandError> {
    read_through_engine(backend, ns, &[], None, concern)
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
    read_through_engine(backend, ns, extra, vars, concern).map(|(docs, _)| docs)
}

/// [`read_through`], and whether its stages all run in RefDb's slot-based engine.
fn read_through_engine<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    extra: &[Document],
    vars: Option<&Document>,
    concern: ReadConcern,
) -> Result<(Vec<Document>, bool), CommandError> {
    read_through_staged(backend, ns, (extra, vars), concern)?
}

/// [`read_through_engine`] with the errors of running the stages apart from those of reading
/// and parsing them (the outer error).
pub(crate) fn read_through_staged<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    (extra, vars): (&[Document], Option<&Document>),
    concern: ReadConcern,
) -> Result<Result<(Vec<Document>, bool), CommandError>, CommandError> {
    // `admin.system.users`, `.roles` and `.version` come from Group 0 (013 FR-030).
    if let Some(docs) = super::authz_collections::documents(backend, ns)? {
        return Ok(Ok((docs, true)));
    }
    let view = super::views::resolve(backend, ns)?;
    let own = view.as_ref().and_then(|v| v.collation.as_deref()).map(RawDoc::from_bytes);
    let collation = collation_value(own.transpose()?.map(Document::from).as_ref())?;
    let base = match &view {
        Some(v) => names::ns(&ns.db, &v.collection),
        None => ns.clone(),
    };
    let Some(coll) = backend.collection(&base)? else { return Ok(Ok((Vec::new(), true))) };
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
        return Ok(Ok((docs, true)));
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
        allow_disk_use: true,
        node: None,
    };
    let sbe = pipeline.runs_in_sbe();
    Ok(pipeline.run(docs, &env).map(|out| (out, sbe)).map_err(from_mql))
}

/// The arguments of `aggregate` besides its pipeline.
struct AggregateArgs {
    batch_size: Option<usize>,
    /// `allowDiskUse`, true by default.
    allow_disk_use: bool,
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

fn aggregate_args<B: Backend + ?Sized>(
    backend: &mut B,
    cmd: &Document,
) -> Result<AggregateArgs, CommandError> {
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
        allow_disk_use: !matches!(cmd.get("allowDiskUse"), Some(Value::Boolean(false))),
        collation,
        vars: command_let(backend, cmd, AGGREGATE)?,
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
    let a = aggregate_args(&mut *ctx.backend, ctx.cmd)?;
    if matches!(ctx.cmd.get(super::explain::EXPLAIN), Some(Value::Boolean(true))) {
        return super::explain::aggregate_flag(ctx);
    }
    let stages: Vec<RawValue<'_>> = match raw_cmd.get("pipeline") {
        Some(RawValue::Array(p)) => p.iter().collect(),
        _ => Vec::new(),
    };
    if super::change_stream::first_stage(&stages)
        == Some(teseladb_changestream::stage::CHANGE_STREAM)
    {
        return super::change_stream::open(ctx, &ns, on_collection, &stages);
    }
    super::change_stream::not_first(&stages)?;
    let vars = a.vars.clone().unwrap_or_default();
    let names: Vec<String> = vars.iter().map(|(k, _)| k.to_owned()).collect();
    let ctx_vars = ExprCtx::checked(names.iter().map(String::as_str));
    // `$currentOp` answers its namespace before its arguments (measured).
    let first_name = match stages.first() {
        Some(RawValue::Document(d)) => d.iter().next().map(|(k, _)| k),
        _ => None,
    };
    if first_name == Some("$currentOp") && (on_collection || ns.db != super::admin::ADMIN_DB) {
        return Err(CommandError::new(
            INVALID_NAMESPACE,
            "$currentOp must be run against the 'admin' database with {aggregate: 1}",
        ));
    }
    let pipeline = Pipeline::parse_command(&stages, &ctx_vars, on_collection).map_err(from_mql)?;
    teseladb_exec::pipeline::optimize::optimize_stages(&stages, a.vars.as_ref())
        .map_err(from_mql)?;
    check_system_namespace(&pipeline, &ns, on_collection)?;
    // On a view whose pipeline has stages, `$listCatalog` comes after them (measured).
    if pipeline.system_stage() == Some("$listCatalog")
        && let Some(c) = ctx.backend.collection(&ns)?
        && matches!(super::kinds::kind_of(&c)?, CollectionKind::View { pipeline, .. } if !pipeline.is_empty())
    {
        return Err(CommandError::new(
            crate::error::CS_NOT_FIRST,
            "$listCatalog is only valid as the first stage in a pipeline",
        ));
    }
    if on_collection && ctx.backend.collection(&ns)?.is_some() {
        pipeline.check_collection_plan().map_err(from_mql)?;
    }
    let collation = command_collation(&mut *ctx.backend, &ns, on_collection, a.collation.as_ref())?;
    let planned = match on_collection && pipeline.reads_collection() && collation.get().is_none() {
        true => planned_read(
            ctx,
            &ns,
            (&stages, a.vars.as_ref()),
            a.concern,
            pipeline.reads_index_key(),
        )
        .map_err(|e| executor_on(&ns_text, e))?,
        false => None,
    };
    let (docs, metas, sbe) = match planned {
        Some((docs, metas)) => (docs, metas, true),
        None if on_collection && pipeline.reads_collection() => {
            let (docs, sbe) = read_engine(&mut *ctx.backend, &ns, a.concern)?;
            (docs, None, sbe)
        }
        None => (Vec::new(), None, true),
    };
    let env = WireEnv {
        backend: RefCell::new(&mut *ctx.backend),
        db: ctx.db,
        vars,
        concern: a.concern,
        collation,
        allow_disk_use: a.allow_disk_use,
        node: Some((ctx.shared, ns.clone())),
    };
    let executor = |mut err: CommandError| {
        err.errmsg = format!(
            "Executor error during aggregate command on namespace: {ns_text} :: caused by :: {}",
            err.errmsg
        );
        err
    };
    pipeline.check_plan(&env).map_err(from_mql)?;
    let (mut results, _) =
        pipeline.run_with_meta(docs, metas, &env, sbe).map_err(|e| executor(from_mql(e)))?;
    let vars = env.vars.clone();
    drop(env);
    // `$out` and `$merge` write what reaches them and give no documents (010 FR-012).
    if let Some(w) = pipeline.write() {
        let docs = std::mem::take(&mut results);
        super::out_merge::write(ctx, w, docs, vars, executor)?;
    }
    let mut state = QueryCursor {
        source: Source::Done,
        buffered: VecDeque::from(results),
        skip: 0,
        left: None,
        tail: None,
        read_mark: ctx.backend.read_mark(),
        stream: None,
        origin: None,
    };
    let size = Some(a.batch_size.unwrap_or(DEFAULT_FIRST_BATCH));
    let (batch, short) = state.batch(ctx.backend, size)?;
    let exhausted = short && state.buffered.is_empty();
    let id = keep(ctx, &ns_text, state, exhausted, false)?;
    let batch: Vec<Value> = batch.into_iter().map(Value::Document).collect();
    // The fields of the cursor come in this order on a collection, and the other way for
    // `aggregate: 1` (measured).
    // The stages a `refrouter` sends to a shard or the config server answer in the order of a
    // collection's cursor (measured).
    // `$currentOp` with `localOps` answers on the `refrouter` itself (measured).
    let local_ops = matches!(
        stages.first(),
        Some(RawValue::Document(d)) if matches!(
            d.get("$currentOp"),
            Some(RawValue::Document(o)) if matches!(o.get("localOps"), Some(RawValue::Boolean(true)))
        )
    );
    let forwarded = match pipeline.system_stage() {
        Some("$listClusterCatalog" | "$querySettings") => true,
        Some("$currentOp") => !local_ops,
        _ => false,
    };
    let cursor = if on_collection || forwarded {
        Document::new().with("id", id).with("ns", ns_text).with("firstBatch", batch)
    } else {
        Document::new().with("firstBatch", batch).with("id", id).with("ns", ns_text)
    };
    Ok(Document::new().with("cursor", cursor).with("ok", 1.0))
}

/// `err` as an error of running the pipeline on `ns_text`.
fn executor_on(ns_text: &str, mut err: CommandError) -> CommandError {
    err.errmsg = format!(
        "Executor error during aggregate command on namespace: {ns_text} :: caused by :: {}",
        err.errmsg
    );
    err
}

/// The documents of plain collection `ns` that the index plan of the query a pipeline of
/// `stages` starts with reads, in its order (010 FR-001): the stages run on them as on the whole
/// collection, and stages that keep the order of their input give the index's order. `None` for
/// a view, a collection that is not a plain one, a pipeline without filter or sort, or when the
/// collection scan wins. With `index_keys`, each document's metadata has the key of the index
/// entry it was read by (`{$meta: "indexKey"}`, measured in `tests/aggregate/index_key.out`).
fn planned_read<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    ns: &Namespace,
    (stages, vars): (&[RawValue<'_>], Option<&Document>),
    concern: ReadConcern,
    index_keys: bool,
) -> Result<Option<PlannedRead>, CommandError> {
    let stages: Vec<Document> = stages
        .iter()
        .map(|v| match v {
            RawValue::Document(d) => Some(Document::from(*d)),
            _ => None,
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    let (filter, sort, limit) = super::explain::planned_query(&stages);
    if filter.is_empty() && sort.is_none() {
        return Ok(None);
    }
    if super::authz_collections::documents(ctx.backend, ns)?.is_some()
        || super::views::resolve(ctx.backend, ns)?.is_some()
    {
        return Ok(None);
    }
    let Some(coll) = ctx.backend.collection(ns)? else { return Ok(None) };
    let parsed = super::plan_exec::parsed_filter(&filter, vars)?;
    let indexes = super::plan_exec::plan_indexes(ctx.backend, &coll)?;
    let read = super::plan_exec::ReadPlan {
        filter: &filter,
        parsed: &parsed,
        sort: sort.as_ref().map(super::explain::sort_fields).unwrap_or_default(),
        limit,
        hint: None,
        vars,
        concern,
        intersect: super::lock(ctx.shared).params.hash_intersection,
        index_keys: index_keys && !super::query::id_point(&filter),
    };
    if indexes.is_empty() || super::explain::matches_nothing(&parsed) {
        return Ok(None);
    }
    let note = super::plan_exec::CacheNote {
        shared: ctx.shared,
        ns,
        from: Document::new()
            .with("query", filter.clone())
            .with("sort", sort.clone().unwrap_or_default())
            .with("projection", Document::new()),
        shape: super::plan_cache::read_shape(&parsed, sort.as_ref(), None, None),
    };
    let Some((found, keys)) =
        super::plan_exec::planned_keyed(ctx.backend, &coll, &read, Some(note))?
    else {
        return Ok(None);
    };
    let docs = found
        .iter()
        .map(|b| Ok(Document::from(RawDoc::from_bytes(b)?)))
        .collect::<Result<_, CommandError>>()?;
    let metas = index_keys.then(|| {
        let mut keys = keys.into_iter();
        found
            .iter()
            .map(|_| {
                let mut m = Meta::new();
                if let Some(Some(k)) = keys.next() {
                    m.insert(INDEX_KEY.to_owned(), Value::Document(k));
                }
                m
            })
            .collect()
    });
    Ok(Some((docs, metas)))
}

/// The documents an index plan read and, when the pipeline reads `{$meta: "indexKey"}`, the
/// metadata of each with its index key.
type PlannedRead = (Vec<Document>, Option<Vec<Meta>>);

/// Refuses a stage the node answers itself on a namespace it does not run on (measured on a
/// `refrouter` 9.0.4): `$listLocalSessions` only with `aggregate: 1`, `$listSessions` only on
/// `config.system.sessions`.
fn check_system_namespace(
    pipeline: &Pipeline,
    ns: &Namespace,
    on_collection: bool,
) -> Result<(), CommandError> {
    let sessions =
        ns.db == super::registry::SESSIONS_DB && ns.coll == super::registry::SESSIONS_COLL;
    match pipeline.system_stage() {
        Some("$listLocalSessions") if on_collection => Err(CommandError::new(
            INVALID_NAMESPACE,
            "$listLocalSessions must be run against the database with {aggregate: 1}, not a collection",
        )),
        Some("$currentOp") if on_collection || ns.db != super::admin::ADMIN_DB => {
            Err(CommandError::new(
                INVALID_NAMESPACE,
                "$currentOp must be run against the 'admin' database with {aggregate: 1}",
            ))
        }
        Some("$listExtensions") if on_collection || ns.db != super::admin::ADMIN_DB => {
            Err(CommandError::new(
                INVALID_NAMESPACE,
                "$listExtensions must be run against the 'admin' database with {aggregate: 1}",
            ))
        }
        Some("$listCatalog") if !on_collection && ns.db != super::admin::ADMIN_DB => {
            Err(CommandError::new(
                INVALID_NAMESPACE,
                "Collectionless $listCatalog must be run against the 'admin' database with \
                 {aggregate: 1}",
            ))
        }
        Some("$listClusterCatalog") if on_collection => Err(CommandError::new(
            crate::error::CLUSTER_CATALOG_ON_COLLECTION,
            "The $listClusterCatalog stage must run on a database, not a collection.",
        )),
        Some("$listSessions") if !sessions => Err(CommandError::new(
            INVALID_NAMESPACE,
            teseladb_exec::pipeline::LIST_SESSIONS_NAMESPACE,
        )),
        _ => Ok(()),
    }
}

/// Whether a session record's `_id` belongs to the users a sessions stage's `spec` asks for:
/// every user with `allUsers`, those of `users`, else the user of the command, who without a
/// user's name in the record is the record's own (`uid` is the SHA-256 of `user@db`).
fn wanted_session(id: Option<&Value>, spec: &Document) -> bool {
    if matches!(spec.get("allUsers"), Some(Value::Boolean(true))) {
        return true;
    }
    let uid = match id {
        Some(Value::Document(d)) => d.get("uid").cloned(),
        _ => None,
    };
    let of = |user: &str| Value::Binary {
        subtype: 0,
        bytes: teseladb_hash::sha256(user.as_bytes()).to_vec(),
    };
    match spec.get("users") {
        Some(Value::Array(list)) if !list.is_empty() => list.iter().any(|u| {
            let Value::Document(u) = u else { return false };
            let (Some(Value::String(user)), Some(Value::String(db))) = (u.get("user"), u.get("db"))
            else {
                return false;
            };
            uid.as_ref() == Some(&of(&format!("{user}@{db}")))
        }),
        _ => uid.as_ref() == Some(&of("")),
    }
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
