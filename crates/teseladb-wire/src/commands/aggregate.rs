//! `aggregate` (010 FR-004, FR-011): parses the pipeline with 010's engine
//! (`teseladb_exec::pipeline`), reads the collection — through the definitions of the views it
//! stands on — runs the pipeline over the documents and returns them through a cursor that
//! `getMore` continues, with the arguments, replies and errors of a `refrouter` 9.0.4 (measured
//! 31-10-2026).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use teseladb_exec::collation::Collation;
use teseladb_exec::expr::Ctx as ExprCtx;
use teseladb_exec::pipeline::meta::{INDEX_KEY, Meta};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_meta::catalog::Namespace;
use teseladb_meta::collection_types::CollectionKind;
use teseladb_mql::{Extensions, MqlError};
use teseladb_range::read::ReadConcern;
use teseladb_bson::{Document, RawDoc, RawValue, Value};

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

/// The node's shared state and the aggregated collection, which the stages of the node read.
type Node<'a> = (&'a super::Shared, &'a Namespace);

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
    /// What the search stages' queries gave while the pipeline was planned, by stage and
    /// specification, for the run to take (011 FR-022).
    searched: RefCell<Vec<(String, Document, Vec<teseladb_exec::pipeline::search::Scored>)>>,
    /// The node's temporary store and the memory of a stage (010 FR-010, D-105).
    spill: super::Spill,
}

/// An error of the node as an error of the pipeline.
fn to_mql(e: CommandError) -> MqlError {
    MqlError::new(teseladb_mql::ErrorCode { code: e.code.code, name: e.code.name }, e.errmsg)
}

impl<B: Backend + ?Sized> WireEnv<'_, B> {
    /// `$listLocalSessions`: the sessions this node holds in memory, as the records of
    /// `config.system.sessions` write them, for the users `spec` asks for.
    fn local_sessions(
        &self,
        (shared, _): Node<'_>,
        spec: &Document,
    ) -> Result<Vec<Document>, CommandError> {
        let used = super::lock(shared).sessions.used_since(0);
        let me = super::session::caller(&**self.backend.borrow()).uid;
        Ok(used
            .into_iter()
            .map(|(lsid, owner, last)| {
                let mut d = Document::new()
                    .with("_id", super::registry::record_id(lsid, &owner.uid))
                    .with("lastUse", Value::DateTime(i64::try_from(last).unwrap_or(i64::MAX)));
                // The user's name, as `$listLocalSessions` of a `refrouter` 9.0.4 gives it (measured
                // on 03-11-2026).
                if let Some(user) = owner.user {
                    d.push("user", user);
                }
                d
            })
            .filter(|d| wanted_session(d.get("_id"), spec, &me))
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
        let Some(coll) = backend.collection(ns)? else {
            // Measured on a `refnode` 9.0.4 on 03-11-2026.
            let msg = format!(
                "$geoNear needs a collection to run, but {} does not exist",
                names::full(ns)
            );
            return Err(CommandError::new(crate::error::NAMESPACE_NOT_FOUND, msg));
        };
        for entry in backend.indexes(coll.id)? {
            if entry.state != teseladb_meta::indexes::IndexState::Ready {
                continue;
            }
            // The node stored the specification with its key.
            let spec = Document::from(RawDoc::from_bytes(&entry.spec)?);
            let key = match spec.get("key") {
                Some(Value::Document(key)) => key.clone(),
                _ => Document::new(),
            };
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
        Ok((names::full(ns), out))
    }

    /// `$listCatalog`: the catalog entry of the aggregated collection, or of every collection
    /// with `aggregate: 1` on `admin`.
    /// On a view (whose stages are none, or the command is refused) it is the entry of the
    /// collection under it (measured on a `refnode` 9.0.4).
    fn list_catalog(&self, (_, ns): Node<'_>) -> Result<Vec<Document>, CommandError> {
        let mut backend = self.backend.borrow_mut();
        let base = super::views::resolve(&mut **backend, ns)?.map(|v| v.collection);
        let coll = base.as_deref().unwrap_or(&ns.coll);
        let only = (coll != COLLECTIONLESS).then_some(coll);
        super::list_catalog::entries(&mut **backend, &ns.db, only)
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
    fn current_ops(&self, (shared, _): Node<'_>) -> Result<Vec<Document>, CommandError> {
        let mut backend = self.backend.borrow_mut();
        super::admin::current_operations(&mut **backend, shared)
    }

    /// `$planCacheStats`: the entries of the node's plan cache for the aggregated collection,
    /// which must exist (50933).
    /// `$queryStats`: what this node counted of each query shape.
    fn query_stats(
        &self,
        (shared, _): Node<'_>,
        spec: &Document,
    ) -> Result<Vec<Document>, CommandError> {
        if let Some((k, _)) = spec.iter().next() {
            let msg = format!("BSON field '$queryStats.{k}' is an unknown field.");
            return Err(CommandError::new(crate::error::IDL_UNKNOWN_FIELD, msg));
        }
        let now = self.backend.borrow_mut().now_ms();
        Ok(super::lock(shared).query_stats.listed(i64::try_from(now).unwrap_or(i64::MAX)))
    }

    /// `$listSampledQueries`: the queries this node sampled.
    fn sampled_queries(
        &self,
        (shared, _): Node<'_>,
        spec: &Document,
    ) -> Result<Vec<Document>, CommandError> {
        let now = self.backend.borrow_mut().now_ms();
        Ok(super::query_analysis::listed(shared, spec, now))
    }

    fn plan_cache_stats(&self, (shared, ns): Node<'_>) -> Result<Vec<Document>, CommandError> {
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
        let host = backend.host_port();
        let shard = super::shard_name(backend.node());
        Ok(super::lock(shared).plan_cache.stats(&ns_text, (&host, &shard)))
    }

    /// `$indexStats`: each index of the aggregated collection with how many reads and writes
    /// its plans scanned it and since when, as a `refrouter` 9.0.4 lists them (measured in
    /// `tests/aggregate/index_stats.out`); nothing for a collection that does not exist.
    fn index_stats(&self, (shared, ns): Node<'_>) -> Result<Vec<Document>, CommandError> {
        let mut backend = self.backend.borrow_mut();
        let Some(coll) = backend.collection(ns)? else { return Ok(Vec::new()) };
        // A time series lists its buckets' indexes alone, as `listIndexes` shows them (measured
        // on a `refnode` and a `refrouter` 9.0.4 on 03-11-2026); a clustered collection's index is
        // not listed (measured on a `refrouter` 9.0.4 on 05-11-2026).
        let series = super::timeseries::spec_of(&coll)?;
        let clustered = super::catalog::is_clustered(&coll)?;
        let mut specs = match super::catalog::has_id_index(ns) && series.is_none() && !clustered {
            true => vec![super::catalog::id_index(&coll)?],
            false => Vec::new(),
        };
        let listed = super::indexes::listed(&mut **backend, &coll)?;
        match &series {
            Some(ts) => {
                specs.extend(listed.iter().filter_map(|s| super::timeseries::user_index(ts, s)))
            }
            None => specs.extend(listed),
        }
        let host = backend.host_port();
        let shard = super::shard_name(backend.node());
        let now = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
        let ns_text = names::full(ns);
        let usage = &super::hot::of(shared).index_usage;
        let names: Vec<String> = specs
            .iter()
            .filter_map(|s| match s.get("name") {
                Some(Value::String(n)) => Some(n.clone()),
                _ => None,
            })
            .collect();
        usage.keep_only(&ns_text, &names);
        Ok(specs
            .into_iter()
            .zip(names)
            .map(|(spec, name)| {
                let (ops, since) = usage.of(&ns_text, &name, now);
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
    fn recorded_sessions(
        &self,
        (shared, ns): Node<'_>,
        spec: &Document,
    ) -> Result<Vec<Document>, CommandError> {
        let mut backend = self.backend.borrow_mut();
        let me = super::session::caller(&**backend).uid;
        let records = read(&mut **backend, Some(shared), ns, self.concern)?;
        Ok(records.into_iter().filter(|d| wanted_session(d.get("_id"), spec, &me)).collect())
    }

    /// The documents of `$listSearchIndexes` on the pipeline's collection (011 FR-008).
    fn search_indexes(
        &self,
        (shared, ns): Node<'_>,
        spec: &Document,
    ) -> Result<Vec<Document>, CommandError> {
        let mut backend = self.backend.borrow_mut();
        super::search_indexes::stage(&mut **backend, shared, ns, spec)
    }

    /// The documents of a `$match` with `$text` (`filter`, whose `$expr` sees `vars`) on `ns`,
    /// read through a view without stages.
    fn text_of(
        &self,
        ns: &Namespace,
        filter: &Document,
        vars: Option<&Document>,
    ) -> Result<Vec<teseladb_exec::pipeline::search::Scored>, CommandError> {
        let mut backend = self.backend.borrow_mut();
        let ns = super::text_search::read_namespace(&mut **backend, ns)?;
        super::text_search::source(&mut **backend, &ns, filter, (vars, self.concern))
    }

    /// `$collStats` of the aggregated collection (015 FR-010), with the sections `spec` asks
    /// for in RefDb's order. TeselaDB has no shards: the document has no `shard` (DV-038).
    fn coll_stats(
        &self,
        (shared, ns): Node<'_>,
        spec: &Document,
    ) -> Result<Vec<Document>, CommandError> {
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
                super::txn_stats::status(&super::lock(shared))
                    .collection_latency(&ns_text, histograms),
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

    fn interrupted(&self) -> Result<(), MqlError> {
        // The stages run between the reads of the pipeline; a read under way checks for itself.
        match self.backend.try_borrow() {
            Ok(backend) => backend.interrupted().map_err(to_mql),
            Err(_) => Ok(()),
        }
    }

    fn js(&self) -> Option<std::rc::Rc<dyn teseladb_exec::expr::js::JsEngine>> {
        Some(super::js::engine())
    }

    fn allow_disk_use(&self) -> bool {
        self.allow_disk_use
    }

    fn stage_memory_limit(&self) -> usize {
        self.spill.stage_limit()
    }

    fn spill(&self) -> Option<&dyn teseladb_exec::pipeline::spill::SpillStore> {
        self.spill.store.as_deref().map(|s| s as &dyn teseladb_exec::pipeline::spill::SpillStore)
    }

    fn operation_memory_limit(&self) -> usize {
        match &self.node {
            Some((shared, _)) => {
                usize::try_from(super::lock(shared).params.operation_memory).unwrap_or(usize::MAX)
            }
            None => teseladb_exec::pipeline::errors::OPERATION_MEMORY_LIMIT,
        }
    }

    fn vars(&self) -> Option<&Document> {
        Some(&self.vars)
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        let ns = names::ns(self.db, name);
        let mut backend = self.backend.borrow_mut();
        let shared = self.node.as_ref().map(|(shared, _)| *shared);
        read(&mut **backend, shared, &ns, self.concern).map_err(to_mql)
    }

    fn collation(&self) -> Collation {
        self.collation.clone()
    }

    fn collection_matching(
        &self,
        name: &str,
        filter: &Document,
    ) -> Result<Vec<Document>, MqlError> {
        let Some((shared, _)) = &self.node else { return self.collection(name) };
        let ns = names::ns(self.db, name);
        let stage = Document::new().with("$match", filter.clone());
        let limits = crate::msg::command_limits();
        let bytes = stage.to_bytes_with(limits).map_err(|e| to_mql(e.into()))?;
        let raw = RawDoc::from_bytes_with(&bytes, limits).map_err(|e| to_mql(e.into()))?;
        let mut backend = self.backend.borrow_mut();
        let cmd = Document::new();
        let mut ctx = Ctx { backend: &mut **backend, shared, db: self.db, cmd: &cmd };
        let stages = [RawValue::Document(raw)];
        match planned_read(&mut ctx, &ns, (&stages, None), self.concern, false).map_err(to_mql)? {
            Some((docs, _, _)) => Ok(docs),
            None => read(&mut **backend, Some(shared), &ns, self.concern).map_err(to_mql),
        }
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

    fn search_source(
        &self,
        stage: &str,
        spec: &Document,
    ) -> Result<Vec<teseladb_exec::pipeline::search::Scored>, MqlError> {
        let mut searched = self.searched.borrow_mut();
        if let Some(i) = searched.iter().position(|(n, s, _)| n == stage && s == spec) {
            return Ok(searched.remove(i).2);
        }
        drop(searched);
        let Some((shared, ns)) = &self.node else { return Ok(Vec::new()) };
        let mut backend = self.backend.borrow_mut();
        super::search_ops::source(&mut **backend, shared, ns, stage, spec).map_err(to_mql)
    }

    fn text_source(
        &self,
        filter: &Document,
    ) -> Result<Vec<teseladb_exec::pipeline::search::Scored>, MqlError> {
        let ns = match &self.node {
            Some((_, ns)) => ns.clone(),
            None => return Err(to_mql(super::text_search::no_index_here())),
        };
        self.text_of(&ns, filter, Some(&self.vars)).map_err(to_mql)
    }

    fn joined_text_source(
        &self,
        name: &str,
        filter: &Document,
        vars: Option<&Document>,
    ) -> Result<Vec<teseladb_exec::pipeline::search::Scored>, MqlError> {
        self.text_of(&names::ns(self.db, name), filter, vars).map_err(to_mql)
    }

    fn joined_search_source(
        &self,
        name: &str,
        (stage, spec): (&str, &Document),
        from_lookup: bool,
    ) -> Result<Vec<teseladb_exec::pipeline::search::Scored>, MqlError> {
        let Some((shared, _)) = &self.node else { return Ok(Vec::new()) };
        let ns = names::ns(self.db, name);
        let mut backend = self.backend.borrow_mut();
        let found =
            super::search_ops::source(&mut **backend, shared, &ns, stage, spec).map_err(to_mql)?;
        // The `$search` of a `$lookup` on a view finds no documents, while its `$searchMeta`
        // counts them and a `$unionWith` reads them (measured on 9.0.4, `scratchpad/lane3t/m113.js`).
        let on_view = super::views::resolve(&mut **backend, &ns).map_err(to_mql)?.is_some();
        match from_lookup && on_view && stage == SEARCH {
            true => Ok(Vec::new()),
            false => Ok(found),
        }
    }

    fn check_text(&self, filter: &Document) -> Result<(), MqlError> {
        let Some((_, ns)) = &self.node else { return Ok(()) };
        let mut backend = self.backend.borrow_mut();
        let ns = super::text_search::read_namespace(&mut **backend, ns).map_err(to_mql)?;
        super::text_search::check(&mut **backend, &ns, filter).map_err(to_mql)
    }

    fn check_search(&self, stage: &str, spec: &Document) -> Result<(), MqlError> {
        // Only `$search` asks `refsearch` while the pipeline is planned (measured on 9.0.4: the
        // errors of `$searchMeta` and `$vectorSearch` carry the executor's prefix).
        if stage != "$search" {
            return Ok(());
        }
        let Some((shared, ns)) = &self.node else { return Ok(()) };
        let mut backend = self.backend.borrow_mut();
        let found =
            super::search_ops::source(&mut **backend, shared, ns, stage, spec).map_err(to_mql)?;
        self.searched.borrow_mut().push((stage.to_owned(), spec.clone(), found));
        Ok(())
    }

    fn system_stage(&self, name: &str, spec: &Document) -> Result<Vec<Document>, MqlError> {
        match name {
            "$listClusterCatalog" => return self.cluster_catalog(spec).map_err(to_mql),
            // TeselaDB loads no extensions.
            "$listExtensions" => return Ok(Vec::new()),
            super::query_settings::STAGE => {
                let mut backend = self.backend.borrow_mut();
                return super::query_settings::listed(&mut **backend, spec).map_err(to_mql);
            }
            teseladb_exec::pipeline::SHARDED_DATA_DISTRIBUTION => {
                let mut backend = self.backend.borrow_mut();
                return super::data_distribution::documents(&mut **backend).map_err(to_mql);
            }
            _ => {}
        }
        // The other stages read the node's state and the aggregated collection, which a view's
        // pipeline read without them (the synonyms of a search index) does not have: it lists
        // nothing.
        let Some((shared, ns)) = &self.node else { return Ok(Vec::new()) };
        let node = (*shared, ns);
        match name {
            "$collStats" => self.coll_stats(node, spec).map_err(to_mql),
            teseladb_exec::pipeline::LIST_SEARCH_INDEXES => {
                self.search_indexes(node, spec).map_err(to_mql)
            }
            "$listLocalSessions" => self.local_sessions(node, spec).map_err(to_mql),
            "$listSessions" => self.recorded_sessions(node, spec).map_err(to_mql),
            "$planCacheStats" => self.plan_cache_stats(node).map_err(to_mql),
            "$indexStats" => self.index_stats(node).map_err(to_mql),
            "$currentOp" => self.current_ops(node).map_err(to_mql),
            "$listCatalog" => self.list_catalog(node).map_err(to_mql),
            super::query_stats::STAGE => self.query_stats(node, spec).map_err(to_mql),
            teseladb_exec::pipeline::LIST_SAMPLED_QUERIES => {
                self.sampled_queries(node, spec).map_err(to_mql)
            }
            other => Err(MqlError::new(
                teseladb_mql::ErrorCode { code: BAD_VALUE.code, name: BAD_VALUE.name },
                format!("{other} is not supported by this node"),
            )),
        }
    }
}

/// The documents of collection `ns`, read through the views it stands on (their pipelines run
/// over the collection at the end of the chain, the innermost first), the stages of the node with
/// its state `shared`.
fn read<B: Backend + ?Sized>(
    backend: &mut B,
    shared: Option<&super::Shared>,
    ns: &Namespace,
    concern: ReadConcern,
) -> Result<Vec<Document>, CommandError> {
    read_engine(backend, shared, ns, concern).map(|(docs, _)| docs)
}

/// [`read`], and whether the stages of the views it read through all run in RefDb's
/// slot-based engine (the stages that follow them may then run there too).
fn read_engine<B: Backend + ?Sized>(
    backend: &mut B,
    shared: Option<&super::Shared>,
    ns: &Namespace,
    concern: ReadConcern,
) -> Result<(Vec<Document>, bool), CommandError> {
    read_through_on(backend, shared, ns, (&[], None), concern, (super::Spill::default(), true))?
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
    let none = (super::Spill::default(), true);
    read_through_on(backend, None, ns, (extra, vars), concern, none)?.map(|(docs, _)| docs)
}

/// [`read_through`] with the node's state `shared` (the stages of the node read it) and the
/// errors of running the stages apart from those of reading and parsing them (the outer error).
pub(crate) fn read_through_staged<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    ns: &Namespace,
    stages: (&[Document], Option<&Document>),
    concern: ReadConcern,
) -> Result<Result<(Vec<Document>, bool), CommandError>, CommandError> {
    read_through_on(backend, Some(shared), ns, stages, concern, (super::Spill::default(), true))
}

/// [`read_through_staged`] with the node's store `spill` for stages past their memory, used
/// when `allow_disk_use`.
pub(crate) fn read_through_spilling<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    ns: &Namespace,
    stages: (&[Document], Option<&Document>),
    concern: ReadConcern,
    spilling: (super::Spill, bool),
) -> Result<Result<(Vec<Document>, bool), CommandError>, CommandError> {
    read_through_on(backend, Some(shared), ns, stages, concern, spilling)
}

/// [`read_through`] with the node's state, so that the stages see the collection's indexes
/// (`$geoNear` of a `find` with `$near`, 009 T035).
pub(crate) fn read_through_node<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
    ns: &Namespace,
    stages: (&[Document], Option<&Document>),
    concern: ReadConcern,
) -> Result<Vec<Document>, CommandError> {
    read_through_on(backend, Some(shared), ns, stages, concern, (super::Spill::default(), true))?
        .map(|(docs, _)| docs)
}

fn read_through_on<B: Backend + ?Sized>(
    backend: &mut B,
    shared: Option<&super::Shared>,
    ns: &Namespace,
    (extra, vars): (&[Document], Option<&Document>),
    concern: ReadConcern,
    (spill, allow_disk_use): (super::Spill, bool),
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
        stages.push(super::query::raw_bytes(d)?);
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
        allow_disk_use,
        node: shared.map(|sh| (sh, base.clone())),
        searched: RefCell::new(Vec::new()),
        spill,
    };
    let sbe = pipeline.runs_in_sbe();
    Ok(pipeline.run(docs, &env).map(|out| (out, sbe)).map_err(from_mql))
}

/// Runs `stages` (BSON documents) of database `db` over `docs`, as the stages of a view run
/// (no search stages, simple collation).
pub(crate) fn run_stages<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    stages: &[Vec<u8>],
    docs: Vec<Document>,
) -> Result<Vec<Document>, CommandError> {
    let raws: Vec<RawDoc<'_>> =
        stages.iter().map(|b| RawDoc::from_bytes(b)).collect::<Result<_, _>>()?;
    let values: Vec<RawValue<'_>> = raws.into_iter().map(RawValue::Document).collect();
    let pipeline = Pipeline::parse(&values, &ExprCtx::checked(std::iter::empty::<&str>()))
        .map_err(from_mql)?;
    let env = WireEnv {
        backend: RefCell::new(backend),
        db,
        vars: Document::new(),
        concern: ReadConcern::Local,
        collation: collation_value(None)?,
        allow_disk_use: true,
        node: None,
        searched: RefCell::new(Vec::new()),
        spill: super::Spill::default(),
    };
    pipeline.run(docs, &env).map_err(from_mql)
}

/// The most bytes of a document a pipeline gives (`BSONObjectMaxInternalSize`).
const MAX_OUTPUT_DOCUMENT: usize = 16 * 1024 * 1024 + 16 * 1024;

/// The 10334 of a document a classic stage cannot write.
fn check_output_size(d: &Document) -> Result<(), CommandError> {
    let wide = teseladb_bson::Limits {
        max_size: usize::MAX,
        max_depth: teseladb_bson::Limits::default().max_depth,
    };
    let size = d.to_bytes_with(wide)?.len();
    if size <= MAX_OUTPUT_DOCUMENT {
        return Ok(());
    }
    Err(CommandError::new(
        crate::error::BSON_OBJECT_TOO_LARGE,
        format!(
            "Serializing Document failed :: caused by :: Size {size} exceeds maximum {MAX_OUTPUT_DOCUMENT}"
        ),
    ))
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
        // `explain: true` needs no cursor (measured on a `refnode` 9.0.4).
        None if matches!(cmd.get(super::explain::EXPLAIN), Some(Value::Boolean(true))) => {
            return Ok(None);
        }
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

/// The stages that write a pipeline's documents to a collection.
const WRITE_STAGES: [&str; 2] = ["$out", "$merge"];

/// A pipeline that ends writing (`$out`, `$merge`) refuses a `linearizable` read concern, as a
/// `refrouter` 9.0.4 does (measured on 03-11-2026).
fn check_write_stage_concern(cmd: &Document, stages: &[RawValue<'_>]) -> Result<(), CommandError> {
    let last = match stages.last() {
        Some(RawValue::Document(d)) => d.iter().next().map(|(k, _)| k),
        _ => None,
    };
    let Some(stage) = last.filter(|k| WRITE_STAGES.contains(k)) else { return Ok(()) };
    let Some(rc) = crate::concerns::ReadConcernArgs::parse(cmd, AGGREGATE)? else { return Ok(()) };
    if rc.level != Some(crate::concerns::Level::Linearizable) {
        return Ok(());
    }
    let why = format!("{stage} cannot be used with a 'linearizable' read concern level");
    Err(crate::concerns::read_refused(AGGREGATE, &rc, &why))
}

/// `aggregate`.
pub fn aggregate<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    aggregate_from(ctx, false)
}

/// [`aggregate`]; with `find`, the command is a `find` run as a pipeline, whose `$match` may hold
/// `$where`.
pub(crate) fn aggregate_from<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
    find: bool,
) -> Result<Document, CommandError> {
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
    let cmd_bytes = super::query::raw_bytes(ctx.cmd)?;
    let raw_cmd = RawDoc::from_bytes_with(&cmd_bytes, crate::msg::command_limits())?;
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
    check_write_stage_concern(ctx.cmd, &stages)?;
    let vars = a.vars.clone().unwrap_or_default();
    let names: Vec<String> = vars.iter().map(|(k, _)| k.to_owned()).collect();
    let ctx_vars = ExprCtx::checked(names.iter().map(String::as_str));
    let ctx_vars = if find { ctx_vars.with_where() } else { ctx_vars };
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
    // On a view whose pipeline has stages, `$listCatalog` and a `$match` with `$text` come after
    // them (measured; `$text`: `scratchpad/lane3s/m89.js`).
    if pipeline.system_stage() == Some("$listCatalog") && on_view_with_stages(ctx.backend, &ns)? {
        return Err(CommandError::new(
            crate::error::CS_NOT_FIRST,
            "$listCatalog is only valid as the first stage in a pipeline",
        ));
    }
    if starts_with_text(&stages) && resolves_with_stages(ctx.backend, &ns)? {
        return Err(text_not_first());
    }
    check_joined_views(ctx.backend, ctx.db, &stages)?;
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
    let (docs, metas, sbe, applied) = match planned {
        Some((docs, metas, applied)) => (docs, metas, true, applied),
        None if on_collection && pipeline.reads_collection() => {
            let (docs, sbe) = read_engine(&mut *ctx.backend, Some(ctx.shared), &ns, a.concern)?;
            (docs, None, sbe, 0)
        }
        None => (Vec::new(), None, true, 0),
    };
    let env = WireEnv {
        backend: RefCell::new(&mut *ctx.backend),
        db: ctx.db,
        vars,
        concern: a.concern,
        collation,
        allow_disk_use: a.allow_disk_use,
        node: Some((ctx.shared, ns.clone())),
        searched: RefCell::new(Vec::new()),
        spill: super::lock(ctx.shared).spill.clone(),
    };
    let executor = |mut err: CommandError| {
        err.errmsg = format!(
            "Executor error during aggregate command on namespace: {ns_text} :: caused by :: {}",
            err.errmsg
        );
        err
    };
    pipeline.check_plan(&env).map_err(from_mql)?;
    let (mut results, _) = pipeline
        .run_after_matches(applied, (docs, metas), &env, sbe)
        .map_err(|e| executor(from_mql(e)))?;
    let vars = env.vars.clone();
    drop(env);
    // A classic stage at the end writes each document it gives as BSON, which may not pass
    // 16 MiB and 16 KiB; the slot-based engine hands its documents on as they are (measured on a
    // `refnode` 9.0.4, 02-11-2026).
    if pipeline.write().is_none() && !(sbe && pipeline.runs_in_sbe()) {
        for d in &results {
            check_output_size(d).map_err(executor)?;
        }
    }
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
        held: 0,
        return_key: false,
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
        // `$shardedDataDistribution` too (measured on 05-11-2026).
        Some("$listClusterCatalog" | "$querySettings" | "$shardedDataDistribution") => true,
        Some("$currentOp") => !local_ops,
        _ => false,
    };
    // A `refrouter` 9.0.4 answers `id`, `ns`, `firstBatch` for a collection; a `refnode`, which the
    // replica set presentation (018) is, `firstBatch`, `id`, `ns` (measured on 04-11-2026).
    let as_refrouter = forwarded || (on_collection && !super::config_db::as_member(ctx.backend)?);
    let cursor = if as_refrouter {
        Document::new().with("id", id).with("ns", ns_text).with("firstBatch", batch)
    } else {
        Document::new().with("firstBatch", batch).with("id", id).with("ns", ns_text)
    };
    Ok(Document::new().with("cursor", cursor).with("ok", 1.0))
}

/// RefDb's code for a `$match` with `$text` that is not the first stage.
const TEXT_NOT_FIRST: crate::error::Code =
    crate::error::Code { code: 17_313, name: "Location17313" };

/// Whether `ns` is a view whose pipeline has stages.
fn on_view_with_stages<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<bool, CommandError> {
    Ok(match backend.collection(ns)? {
        Some(c) => matches!(
            super::kinds::kind_of(&c)?,
            CollectionKind::View { pipeline, .. } if !pipeline.is_empty()
        ),
        None => false,
    })
}

/// Whether `ns` is a view whose pipeline, with those of the views it stands on, has stages.
fn resolves_with_stages<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<bool, CommandError> {
    Ok(super::views::resolve(backend, ns)?.is_some_and(|v| !v.pipeline.is_empty()))
}

/// RefDb's error for a `$match` with `$text` that is not the first stage.
fn text_not_first() -> CommandError {
    CommandError::new(
        TEXT_NOT_FIRST,
        "$match with $text is only allowed as the first pipeline stage",
    )
}

/// Refuses, anywhere in `stages` of database `db`, the pipeline of a `$lookup` or `$unionWith`
/// that starts with a `$match` with `$text` on a view with stages (they go before it, and the
/// `$text` is no longer first) or with a `$search` on a view whose stages it cannot read
/// (measured on 9.0.4 when the command is planned, without input documents, after the stages are
/// parsed; `scratchpad/lane3t/m97.js` to `m113.js`, 011 T054 and FR-004).
fn check_joined_views<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    stages: &[RawValue<'_>],
) -> Result<(), CommandError> {
    for st in stages {
        let RawValue::Document(d) = st else { continue };
        let (coll, sub) = match d.iter().next() {
            Some(("$lookup", RawValue::Document(l))) => (l.get("from"), l.get("pipeline")),
            Some(("$unionWith", RawValue::Document(u))) => (u.get("coll"), u.get("pipeline")),
            Some(("$facet", RawValue::Document(f))) => {
                for (_, p) in f.iter() {
                    if let RawValue::Array(p) = p {
                        check_joined_views(backend, db, &p.iter().collect::<Vec<_>>())?;
                    }
                }
                continue;
            }
            _ => continue,
        };
        let Some(RawValue::Array(sub)) = sub else { continue };
        let sub: Vec<RawValue<'_>> = sub.iter().collect();
        if let Some(RawValue::String(c)) = coll {
            let view = super::views::resolve(backend, &names::ns(db, c))?;
            let stages = view.map(|v| v.pipeline).unwrap_or_default();
            if starts_with_text(&sub) && !stages.is_empty() {
                return Err(text_not_first());
            }
            // A `$search` refuses a view whose stages it cannot read (measured on 9.0.4, also
            // without input documents).
            if first_stage_name(&sub) == Some(SEARCH) {
                super::search_indexes::check_view_stages(&stages)?;
            }
        }
        check_joined_views(backend, db, &sub)?;
    }
    Ok(())
}

/// The name of the first of `stages`.
fn first_stage_name<'a>(stages: &[RawValue<'a>]) -> Option<&'a str> {
    match stages.first() {
        Some(RawValue::Document(d)) => d.iter().next().map(|(k, _)| k),
        _ => None,
    }
}

/// The name of the `$search` stage.
const SEARCH: &str = "$search";

/// Whether the first of `stages` is a `$match` with `$text`.
fn starts_with_text(stages: &[RawValue<'_>]) -> bool {
    match stages.first() {
        Some(RawValue::Document(d)) => match d.get("$match") {
            Some(RawValue::Document(m)) => super::views::has_text(&Document::from(m)),
            _ => false,
        },
        _ => false,
    }
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
    // An `_id` equality alone reads by RefDb's express path, without the planner and with no
    // index key (H-011 of the native tests; measured on a `refnode` 9.0.4). The caller runs
    // with no collation, and the pipeline applies its stages to what this reads.
    let express = match filter.len() {
        1 => super::crud::id_equality(&filter),
        _ => None,
    };
    if let Some(id) =
        express.filter(|_| super::timeseries::spec_of(&coll).is_ok_and(|t| t.is_none()))
    {
        let docs = super::query::read_by_id(ctx.backend, &coll, &id, concern)?
            .iter()
            .map(|b| Ok(Document::from(RawDoc::from_bytes(b)?)))
            .collect::<Result<Vec<_>, CommandError>>()?;
        let metas = index_keys.then(|| docs.iter().map(|_| Meta::new()).collect());
        return Ok(Some((docs, metas, 0)));
    }
    let parsed = super::plan_exec::parsed_filter(&filter, vars)?;
    let read = super::plan_exec::ReadPlan {
        filter: &filter,
        parsed: &parsed,
        sort: sort.as_ref().map(super::explain::sort_fields).unwrap_or_default(),
        limit,
        hint: None,
        vars,
        concern,
        intersect: super::hot::of(ctx.shared)
            .hash_intersection
            .load(std::sync::atomic::Ordering::Relaxed),
        index_keys: index_keys && !super::query::id_point(&filter),
        key_only: false,
    };
    // The planner builds the list of indexes itself: only whether there is one is asked here.
    if !super::plan_exec::plannable_kind(&coll)? || super::explain::matches_nothing(&parsed) {
        return Ok(None);
    }
    let note = super::plan_exec::CacheNote {
        shared: ctx.shared,
        ns,
        from: Document::new()
            .with("query", filter.clone())
            .with("sort", sort.clone().unwrap_or_default())
            .with("projection", Document::new()),
        shape: super::plan_cache::read_shape(&parsed, &filter, sort.as_ref(), None, None),
    };
    let Some((found, keys, _)) = super::plan_exec::planned_keyed(ctx.backend, &coll, &read, note)?
    else {
        return Ok(None);
    };
    // Taken whole, as `find` takes the rows it reads from the node's storage: their fields are
    // read when a stage asks for them.
    let count = found.len();
    let docs = found
        .into_iter()
        .map(|b| Ok(Document::from_stored(b)?))
        .collect::<Result<_, CommandError>>()?;
    let metas = index_keys.then(|| {
        let mut keys = keys.into_iter();
        (0..count)
            .map(|_| {
                let mut m = Meta::new();
                if let Some(Some(k)) = keys.next() {
                    m.insert(INDEX_KEY.to_owned(), Value::Document(k));
                }
                m
            })
            .collect()
    });
    Ok(Some((docs, metas, filter_of_leading_matches(&stages, &filter))))
}

/// How many of `stages`, from the first, are `$match`es whose filters, one after the other, are
/// `filter`: the documents an index plan read with `filter` passed them. Any other order the
/// planner took them in counts as none.
fn filter_of_leading_matches(stages: &[Document], filter: &Document) -> usize {
    let mut merged = Document::new();
    let mut n = 0;
    for st in stages {
        let Some(Value::Document(m)) = st.get("$match").filter(|_| st.len() == 1) else { break };
        for (k, v) in m.iter() {
            merged.push(k, v.clone());
        }
        n += 1;
    }
    if n > 0 && &merged == filter { n } else { 0 }
}

/// RefDb's code for a `$shardedDataDistribution` outside `admin`.
const SHARDED_DATA_DISTRIBUTION_NOT_ADMIN: crate::error::Code =
    crate::error::Code { code: 6_789_102, name: "Location6789102" };

/// The documents an index plan read, when the pipeline reads `{$meta: "indexKey"}` the metadata
/// of each with its index key, and how many of the pipeline's first `$match`es its filter is
/// (the documents already passed them).
type PlannedRead = (Vec<Document>, Option<Vec<Meta>>, usize);

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
        Some(teseladb_exec::pipeline::LIST_SAMPLED_QUERIES)
            if on_collection || ns.db != super::admin::ADMIN_DB =>
        {
            Err(CommandError::new(
                INVALID_NAMESPACE,
                "$listSampledQueries must be run against the 'admin' database with {aggregate: 1}",
            ))
        }
        Some(teseladb_exec::pipeline::SHARDED_DATA_DISTRIBUTION)
            if ns.db != super::admin::ADMIN_DB =>
        {
            Err(CommandError::new(
                SHARDED_DATA_DISTRIBUTION_NOT_ADMIN,
                "The $shardedDataDistribution stage must be run on the admin database",
            ))
        }
        Some("$listSessions") if !sessions => Err(CommandError::new(
            INVALID_NAMESPACE,
            teseladb_exec::pipeline::LIST_SESSIONS_NAMESPACE,
        )),
        _ => Ok(()),
    }
}

/// Whether a session record's `_id` belongs to the users a sessions stage's `spec` asks for:
/// every user with `allUsers`, those of `users`, else `me`, the `uid` of the command's user
/// (`uid` is the SHA-256 of `user@db`).
fn wanted_session(id: Option<&Value>, spec: &Document, me: &[u8; 32]) -> bool {
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
        // The stage's parser checked that each user is `{user, db}`, two strings.
        Some(Value::Array(list)) if !list.is_empty() => list.iter().any(|u| match u {
            Value::Document(u) => match (u.get("user"), u.get("db")) {
                (Some(Value::String(user)), Some(Value::String(db))) => {
                    uid.as_ref() == Some(&of(&format!("{user}@{db}")))
                }
                _ => false,
            },
            _ => false,
        }),
        _ => uid.as_ref() == Some(&Value::Binary { subtype: 0, bytes: me.to_vec() }),
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
