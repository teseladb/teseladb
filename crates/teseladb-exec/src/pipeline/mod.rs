//! Aggregation pipelines (010 FR-004, T010): a pipeline is parsed once ([`Pipeline::parse`]) and
//! run over a batch of documents ([`Pipeline::run`]), each stage over the output of the one
//! before, as a `refrouter` 9.0.4 runs them (measured in `crates/teseladb-exec/tests/agg_*.rs`).
//! What a pipeline reads besides its input — other collections, the command's variables, the
//! extensions of the matcher — comes from its [`Env`].

pub mod deps;
pub mod errors;
mod fill;
pub mod fusion;
pub mod geo_near;
mod granularity;
mod group;
pub mod meta;
pub mod optimize;
pub mod out_merge;
mod sbe;
mod scoped;
pub mod search;
mod shape;
pub mod sort;
mod stages;
mod system;

pub use shape::projected_expr;
pub use stages::check_exprs;
pub use system::{LIST_SEARCH_INDEXES, scale_of};

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::{Extensions, MqlError};
use teseladb_plan::stages::named;

use crate::expr::{Ctx, Expr, Scope};
use errors::{TYPE_MISMATCH, error};
use group::{Bucket, BucketAuto, Group};
use shape::{AddFields, Projection, ReplaceRoot};
use sort::SortSpec;
use stages::{Facet, Lookup, Match, Sample, UnionWith, Unwind};

/// What a pipeline reads besides its input.
pub trait Env {
    /// The extensions of the matcher (`$expr`, geometry, text).
    fn ext(&self) -> &dyn Extensions;

    /// The command's variables (`let`, `NOW`, `CLUSTER_TIME`).
    fn vars(&self) -> Option<&Document>;

    /// The documents of collection `name` of the pipeline's database (none when it does not
    /// exist).
    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError>;

    /// A uniform number in `[0, 1)` (`$sample`).
    fn random(&self) -> f64;

    /// The documents of a stage the node answers itself (`$collStats`), with its checked
    /// specification.
    fn system_stage(&self, name: &str, _spec: &Document) -> Result<Vec<Document>, MqlError> {
        Err(error(
            errors::BAD_VALUE,
            format!("{name} is not supported by this node's aggregation engine"),
        ))
    }

    /// The documents of a search stage (`$search`, `$searchMeta` or `$vectorSearch`) with its
    /// specification, from the node's search indexes (011 FR-022). Without search, RefDb's
    /// `SearchNotEnabled`.
    fn search_source(
        &self,
        _stage: &str,
        _spec: &Document,
    ) -> Result<Vec<search::Scored>, MqlError> {
        Err(search::not_enabled())
    }

    /// The collation strings compare with (010 FR-014): the binary order by default.
    fn collation(&self) -> crate::collation::Collation {
        crate::collation::Collation::default()
    }

    /// The namespace of the pipeline's collection and its 2d and 2dsphere indexes, in the
    /// catalog's order (`$geoNear`); none by default.
    fn geo_indexes(&self) -> Result<(String, Vec<geo_near::GeoIndex>), MqlError> {
        Ok((String::new(), Vec::new()))
    }

    /// Whether the command lets stages that hold more than their memory limit write to disk
    /// (`allowDiskUse`, true by default since 6.0).
    fn allow_disk_use(&self) -> bool {
        true
    }

    /// The engine of `$function` and `$accumulator` (016); none by default.
    fn js(&self) -> Option<std::rc::Rc<dyn crate::expr::js::JsEngine>> {
        None
    }

    /// The scope of an expression over `doc`.
    fn scope(&self, doc: &Document) -> Scope {
        Scope::new(doc.clone(), self.vars()).with_collation(self.collation()).with_js(self.js())
    }
}

/// One stage, parsed.
#[derive(Clone, Debug)]
enum Stage {
    Match(Match),
    Project(Projection),
    AddFields(AddFields),
    ReplaceRoot(ReplaceRoot),
    Sort(SortSpec),
    Limit(u64),
    Skip(u64),
    Count(String),
    Unwind(Unwind),
    Group(Group),
    SortByCount(Expr),
    Bucket(Bucket),
    BucketAuto(BucketAuto),
    Sample(Sample),
    Redact(Expr),
    Facet(Facet),
    Lookup(Box<Lookup>),
    UnionWith(Box<UnionWith>),
    Documents(Expr),
    SetWindowFields(Box<crate::window::SetWindowFields>),
    Densify(Box<fill::Densify>),
    Fill(Box<fill::Fill>),
    GraphLookup(Box<fill::GraphLookup>),
    GeoNear(Box<geo_near::GeoNear>),
    Score(Box<meta::Score>),
    Fusion(Box<fusion::Fusion>),
    /// A stage the node answers itself, first in its pipeline.
    System(&'static str, Document),
    /// A search stage, first in its pipeline (011 FR-022).
    Search(&'static str, Document),
    /// `$out` or `$merge`, last: the node writes what reaches it.
    Write(Box<out_merge::Write>),
}

/// A parsed pipeline.
#[derive(Clone, Debug, Default)]
pub struct Pipeline {
    stages: Vec<Stage>,
    /// For each stage, whether its specification names only operators SBE runs.
    sbe_specs: Vec<bool>,
    /// Whether a stage reads `{$meta: "sortKey"}`, which each `$sort` then sets.
    reads_sort_key: bool,
    /// The error of planning its `$sort` by a random value on an existing collection
    /// ([`sort::check_random_sort`]).
    random_sort: Option<MqlError>,
}

impl Pipeline {
    /// Parses the stages of `pipeline` (an array's elements) with variables `ctx`.
    pub fn parse(pipeline: &[RawValue<'_>], ctx: &Ctx) -> Result<Pipeline, MqlError> {
        let pipeline = parse_stages(pipeline, ctx)?;
        pipeline.check_positions()?;
        Ok(pipeline)
    }

    /// Parses the pipeline of a `$lookup` or `$unionWith` (`stage`) without a collection, which
    /// must start with `$documents`.
    pub(crate) fn parse_from_documents(
        pipeline: &[RawValue<'_>],
        ctx: &Ctx,
        stage: &str,
    ) -> Result<Pipeline, MqlError> {
        let pipeline = parse_stages(pipeline, ctx)?;
        if !pipeline.starts_with_documents() {
            return Err(error(
                errors::FAILED_TO_PARSE,
                format!(
                    "{stage} stage without explicit collection must have a pipeline with $documents as first stage"
                ),
            ));
        }
        pipeline.check_positions()?;
        Ok(pipeline)
    }

    /// Parses the pipeline of an `aggregate` command, on a collection or (`aggregate: 1`) on
    /// none, checked in RefDb's order: each stage's arguments, the first stage of an
    /// `aggregate: 1` (it must read no collection), the stages that only go first, and then
    /// `$documents` on a collection.
    pub fn parse_command(
        pipeline: &[RawValue<'_>],
        ctx: &Ctx,
        on_collection: bool,
    ) -> Result<Pipeline, MqlError> {
        let parsed = parse_stages(pipeline, ctx)?;
        meta::check_available(pipeline)?;
        if !on_collection {
            collectionless(pipeline)?;
        }
        let random_sort = sort::check_random_sort(pipeline).err();
        let mut pipeline = parsed;
        pipeline.random_sort = random_sort;
        pipeline.check_positions()?;
        if on_collection && pipeline.starts_with_documents() {
            return Err(error(
                errors::INVALID_NAMESPACE,
                "'$documents' can only be run with database or cluster-level aggregation {aggregate: 1}",
            ));
        }
        Ok(pipeline)
    }

    /// Refuses `$documents` and the stages the node answers itself anywhere but first.
    fn check_positions(&self) -> Result<(), MqlError> {
        if self.stages.iter().skip(1).any(|s| matches!(s, Stage::Fusion(_))) {
            return Err(fusion::not_first());
        }
        // `$geoNear` goes first, after the `$match`es it takes as its query (measured).
        let leading = self.stages.iter().take_while(|s| matches!(s, Stage::Match(_))).count();
        if self
            .stages
            .iter()
            .enumerate()
            .any(|(i, s)| matches!(s, Stage::GeoNear(_)) && i > leading)
        {
            return Err(geo_near::not_first());
        }
        let last = self.stages.len().saturating_sub(1);
        for st in &self.stages[..last] {
            if let Stage::Write(w) = st {
                return Err(out_merge::not_last(w.name()));
            }
        }
        for st in self.stages.iter().skip(1) {
            let name = match st {
                Stage::Documents(_) => "$documents",
                Stage::System(name, _) => name,
                Stage::Search(name, _) => search::first_stage_name(name),
                _ => continue,
            };
            return Err(error(
                errors::FIRST_STAGE,
                format!("{name} is only valid as the first stage in a pipeline"),
            ));
        }
        Ok(())
    }

    /// The name of the first stage when the node answers it itself (`$collStats`,
    /// `$listSessions`…).
    pub fn system_stage(&self) -> Option<&'static str> {
        match self.stages.first() {
            Some(Stage::System(name, _)) => Some(name),
            _ => None,
        }
    }

    /// The errors of planning the pipeline on a collection that exists, before it runs: a
    /// `refrouter` 9.0.4 only plans the start of a pipeline on the shard that has the collection
    /// (measured: the `$sort` by a random value of [`sort::check_random_sort`] fails only then).
    pub fn check_collection_plan(&self) -> Result<(), MqlError> {
        self.random_sort.clone().map_or(Ok(()), Err)
    }

    /// The errors of planning the pipeline's reads on `env`, before it runs (the index of
    /// `$geoNear`).
    pub fn check_plan(&self, env: &dyn Env) -> Result<(), MqlError> {
        for st in &self.stages {
            if let Stage::GeoNear(g) = st {
                g.check_plan(env)?;
            }
        }
        Ok(())
    }

    /// The last `$sort` of the pipeline.
    pub(crate) fn last_sort(&self) -> Option<&SortSpec> {
        self.stages.iter().rev().find_map(|s| match s {
            Stage::Sort(sort) => Some(sort),
            _ => None,
        })
    }

    /// Whether the pipeline has no stage.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Whether the pipeline starts with `$documents` (it then reads no collection).
    pub fn starts_with_documents(&self) -> bool {
        matches!(self.stages.first(), Some(Stage::Documents(_)))
    }

    /// Whether the pipeline reads its collection: not when it starts with `$documents` or a
    /// stage the node answers itself.
    pub fn reads_collection(&self) -> bool {
        !matches!(
            self.stages.first(),
            Some(Stage::Documents(_) | Stage::System(..) | Stage::Search(..))
        )
    }

    /// Runs the pipeline over `docs`.
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        self.run_in(docs, env, true)
    }

    /// Whether RefDb runs every stage with its slot-based engine, so that stages that follow
    /// them in the same pipeline (a read through a view, then the user's) can too.
    pub fn runs_in_sbe(&self) -> bool {
        !self.starts_with_documents()
            && self.stages.iter().zip(&self.sbe_specs).all(|(st, spec)| *spec && sbe_capable(st))
    }

    /// How many of the leading stages RefDb runs with its slot-based engine, each with all the
    /// stages before it (`explain` shows them in one query plan).
    pub fn sbe_prefix(&self) -> usize {
        if self.starts_with_documents() {
            return 0;
        }
        self.stages
            .iter()
            .zip(&self.sbe_specs)
            .take_while(|(st, spec)| **spec && sbe_capable(st))
            .count()
    }

    /// Runs the pipeline over `docs`; `sbe` when RefDb could run it with its slot-based
    /// engine (a pipeline over a collection whose stages before it all run there, not a
    /// sub-pipeline of `$facet` or `$lookup`). Its stages run there while each before them can,
    /// and `$documents` cannot.
    pub fn run_in(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        sbe: bool,
    ) -> Result<Vec<Document>, MqlError> {
        self.run_with_meta(docs, None, env, sbe).map(|(docs, _)| docs)
    }

    /// [`Pipeline::run_in`] over documents that carry metadata (`metas`, one per document, from
    /// a stage that scores them), which the stages keep, drop or set as RefDb does
    /// ([`meta`]); the documents and their metadata at the end.
    pub fn run_with_meta(
        &self,
        mut docs: Vec<Document>,
        mut metas: Option<Vec<meta::Meta>>,
        env: &dyn Env,
        sbe: bool,
    ) -> Result<(Vec<Document>, Option<Vec<meta::Meta>>), MqlError> {
        let mut sbe = sbe && !self.starts_with_documents();
        for (st, spec) in self.stages.iter().zip(&self.sbe_specs) {
            sbe = sbe && *spec && sbe_capable(st);
            if let Stage::Fusion(f) = st {
                let (fused, scores) = f.run(docs, env)?;
                (docs, metas) = (fused, Some(scores));
                continue;
            }
            if let Stage::Search(name, spec) = st {
                let own = meta::search_score_of(name);
                let (found, scores): (Vec<Document>, Vec<meta::Meta>) = env
                    .search_source(name, spec)?
                    .into_iter()
                    .map(|s| (s.doc, meta::with_score(s.meta, own)))
                    .unzip();
                (docs, metas) = (found, Some(scores));
                continue;
            }
            if let Stage::Score(s) = st {
                let own = metas.take().unwrap_or_else(|| vec![meta::Meta::new(); docs.len()]);
                metas = Some(s.run(&docs, &own, env)?);
                continue;
            }
            // `$sample` gives each document it takes a random value (`{$meta: "randVal"}`).
            let sample = matches!(st, Stage::Sample(_));
            if metas.is_none() && (sample || self.reads_sort_key && matches!(st, Stage::Sort(_))) {
                metas = Some(vec![meta::Meta::new(); docs.len()]);
            }
            (docs, metas) = match metas {
                None => (run_stage(st, docs, env, sbe)?, None),
                Some(m) => run_stage_meta(st, docs, m, env, sbe)?,
            };
        }
        Ok((docs, metas))
    }

    /// Runs the pipeline with `between` applied to the output of its first stage (the equality
    /// of a `$lookup` without `from`, whose pipeline starts with `$documents`).
    pub(crate) fn run_split(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        between: impl FnOnce(Vec<Document>) -> Result<Vec<Document>, MqlError>,
    ) -> Result<Vec<Document>, MqlError> {
        let Some((first, rest)) = self.stages.split_first() else { return between(docs) };
        let mut docs = between(run_stage(first, docs, env, false)?)?;
        for st in rest {
            docs = run_stage(st, docs, env, false)?;
        }
        Ok(docs)
    }
}

/// RefDb's code for `$$SEARCH_META` with no `$search` before it.
const SEARCH_META_WITHOUT_SEARCH: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 6_347_902, name: "Location6347902" };

/// Whether a stage specification reads `$$SEARCH_META` (a `$literal`'s value is no expression).
fn reads_search_meta(v: RawValue<'_>) -> bool {
    match v {
        RawValue::String(s) => s
            .strip_prefix("$$SEARCH_META")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.')),
        RawValue::Document(d) => d.iter().any(|(k, x)| k != "$literal" && reads_search_meta(x)),
        RawValue::Array(a) => a.iter().any(reads_search_meta),
        _ => false,
    }
}

/// The stages of `pipeline`, each one's arguments checked.
fn parse_stages(pipeline: &[RawValue<'_>], ctx: &Ctx) -> Result<Pipeline, MqlError> {
    let mut stages = Vec::with_capacity(pipeline.len());
    let mut sbe_specs = Vec::with_capacity(pipeline.len());
    for (i, v) in pipeline.iter().enumerate() {
        let RawValue::Document(d) = v else {
            return Err(error(
                TYPE_MISMATCH,
                "Each element of the 'pipeline' array must be an object",
            ));
        };
        if i > 0 && matches!(d.get("$match"), Some(RawValue::Document(m)) if has_text(m)) {
            return Err(error(
                errors::TEXT_NOT_FIRST,
                "$match with $text is only allowed as the first pipeline stage",
            ));
        }
        if reads_search_meta(RawValue::Document(*d)) {
            return Err(error(
                SEARCH_META_WITHOUT_SEARCH,
                "Can't access $$SEARCH_META without a $search stage earlier in the pipeline",
            ));
        }
        stages.push(parse_stage(*d, ctx)?);
        sbe_specs.push(d.iter().all(|(name, arg)| sbe::spec_runs_in_sbe(name, arg)));
    }
    let mut read = Vec::new();
    pipeline.iter().for_each(|v| meta::metas_read(*v, &mut read));
    let reads_sort_key = read.iter().any(|n| n == crate::expr::SORT_KEY);
    Ok(Pipeline { stages, sbe_specs, reads_sort_key, random_sort: None })
}

fn parse_stage(d: RawDoc<'_>, ctx: &Ctx) -> Result<Stage, MqlError> {
    let names: Vec<&str> = d.iter().map(|(k, _)| k).collect();
    let (name, _) = named(&names).map_err(|e| {
        MqlError::new(
            teseladb_mql::ErrorCode { code: e.code, name: plan_code_name(e.code) },
            e.message,
        )
    })?;
    let Some(arg) = d.get(name) else {
        return Err(error(TYPE_MISMATCH, "a stage without its argument"));
    };
    Ok(match name {
        "$match" => Stage::Match(Match::parse(arg, ctx)?),
        "$project" => Stage::Project(Projection::parse(arg, ctx).map_err(|e| invalid(name, e))?),
        "$addFields" | "$set" => {
            Stage::AddFields(AddFields::parse(name, arg, ctx).map_err(|e| invalid(name, e))?)
        }
        "$unset" => Stage::Project(Projection::unset(arg).map_err(|e| invalid(name, e))?),
        "$replaceRoot" => Stage::ReplaceRoot(ReplaceRoot::parse(arg, false, ctx)?),
        "$replaceWith" => Stage::ReplaceRoot(ReplaceRoot::parse(arg, true, ctx)?),
        "$sort" => Stage::Sort(SortSpec::parse(arg)?),
        "$limit" => Stage::Limit(stages::limit(arg)?),
        "$skip" => Stage::Skip(stages::skip(arg)?),
        "$count" => Stage::Count(stages::count(arg)?),
        "$unwind" => Stage::Unwind(Unwind::parse(arg)?),
        "$group" => Stage::Group(Group::parse(arg, ctx)?),
        "$sortByCount" => Stage::SortByCount(group::sort_by_count(arg, ctx)?),
        "$bucket" => Stage::Bucket(Bucket::parse(arg, ctx)?),
        "$bucketAuto" => Stage::BucketAuto(BucketAuto::parse(arg, ctx)?),
        "$sample" => Stage::Sample(Sample::parse(arg)?),
        "$redact" => Stage::Redact(Expr::parse_in(arg, &ctx.with(["KEEP", "PRUNE", "DESCEND"]))?),
        "$facet" => Stage::Facet(Facet::parse(arg, ctx)?),
        "$lookup" => Stage::Lookup(Box::new(Lookup::parse(arg, ctx)?)),
        "$unionWith" => Stage::UnionWith(Box::new(UnionWith::parse(arg, ctx)?)),
        "$documents" => Stage::Documents(Expr::parse_in(arg, ctx)?),
        "$densify" => Stage::Densify(Box::new(fill::Densify::parse(arg)?)),
        "$fill" => Stage::Fill(Box::new(fill::Fill::parse(arg, ctx)?)),
        "$graphLookup" => Stage::GraphLookup(Box::new(fill::GraphLookup::parse(arg, ctx)?)),
        "$collStats" => Stage::System("$collStats", system::coll_stats(arg)?),
        LIST_SEARCH_INDEXES => {
            Stage::System(LIST_SEARCH_INDEXES, system::list_search_indexes(arg)?)
        }
        "$geoNear" => Stage::GeoNear(Box::new(geo_near::GeoNear::parse(arg, ctx)?)),
        "$score" => Stage::Score(Box::new(meta::Score::parse(arg, ctx)?)),
        "$rankFusion" | "$scoreFusion" => {
            Stage::Fusion(Box::new(fusion::Fusion::parse(name, arg, ctx)?))
        }
        "$listLocalSessions" => {
            Stage::System("$listLocalSessions", system::sessions("$listLocalSessions", arg)?)
        }
        "$listSessions" => Stage::System("$listSessions", system::sessions("$listSessions", arg)?),
        "$planCacheStats" => Stage::System("$planCacheStats", system::plan_cache_stats(arg)?),
        "$currentOp" => Stage::System("$currentOp", system::current_op(arg)?),
        "$listClusterCatalog" => {
            Stage::System("$listClusterCatalog", system::cluster_catalog(arg)?)
        }
        "$search" => Stage::Search("$search", search::parse(name, arg)?),
        "$searchMeta" => Stage::Search("$searchMeta", search::parse(name, arg)?),
        "$vectorSearch" => Stage::Search("$vectorSearch", search::parse(name, arg)?),
        out_merge::OUT => {
            Stage::Write(Box::new(out_merge::Write::Out(out_merge::Out::parse(arg)?)))
        }
        out_merge::MERGE => Stage::Write(Box::new(out_merge::Write::Merge(Box::new(
            out_merge::Merge::parse(arg, ctx)?,
        )))),
        "$setWindowFields" => {
            Stage::SetWindowFields(Box::new(crate::window::SetWindowFields::parse(arg, ctx)?))
        }
        other => {
            return Err(error(
                errors::BAD_VALUE,
                format!("{other} is not supported by this node's aggregation engine"),
            ));
        }
    })
}

/// An error of the arguments of stage `name` as the stages that parse their fields as a
/// projection write it (`$project`, `$unset`, `$addFields`, `$set`).
pub(crate) fn invalid(name: &str, e: MqlError) -> MqlError {
    MqlError::new(e.code, format!("Invalid {name} :: caused by :: {}", e.message))
}

/// The stages an `aggregate: 1` may start with: they read no collection.
const COLLECTIONLESS_STAGES: [&str; 6] = [
    "$documents",
    "$currentOp",
    "$listLocalSessions",
    "$changeStream",
    "$listClusterCatalog",
    "$querySettings",
];

/// The error of `$listSessions` anywhere but on `config.system.sessions`.
pub const LIST_SESSIONS_NAMESPACE: &str =
    "$listSessions may only be run against config.system.sessions";

/// Checks the first stage of an `aggregate: 1`, which must read no collection.
fn collectionless(stages: &[RawValue<'_>]) -> Result<(), MqlError> {
    let Some(RawValue::Document(first)) = stages.first() else {
        return Err(error(
            errors::INVALID_NAMESPACE,
            "{aggregate: 1} is not valid for an empty pipeline.",
        ));
    };
    let name = first.iter().next().map(|(k, _)| k).unwrap_or_default();
    if name == "$listSessions" {
        return Err(error(errors::INVALID_NAMESPACE, LIST_SESSIONS_NAMESPACE));
    }
    if !COLLECTIONLESS_STAGES.contains(&name) {
        return Err(error(
            errors::INVALID_NAMESPACE,
            format!("{{aggregate: 1}} is not valid for '{name}'; a collection is required."),
        ));
    }
    Ok(())
}

/// The name of a code the plan crate gives.
fn plan_code_name(code: i32) -> &'static str {
    match code {
        teseladb_plan::stages::STAGE_ONE_FIELD => "Location40323",
        teseladb_plan::stages::UNRECOGNIZED_STAGE => "Location40324",
        teseladb_plan::stages::INTERNAL_STAGE => "Location5491300",
        _ => "UnknownError",
    }
}

/// Whether filter `m` holds `$text`, at its top level or inside `$and`, `$or` or `$nor`.
pub(crate) fn has_text(m: RawDoc<'_>) -> bool {
    m.iter().any(|(k, v)| match (k, v) {
        ("$text", _) => true,
        ("$and" | "$or" | "$nor", RawValue::Array(items)) => {
            items.iter().any(|i| matches!(i, RawValue::Document(d) if has_text(d)))
        }
        _ => false,
    })
}

/// A key that is the same for values that compare equal (groups and partitions).
pub(crate) fn group_key(
    v: &teseladb_bson::Value,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Vec<u8> {
    group::canonical(v, col)
}

/// Sets `v` at `segs` of `doc` as `$addFields` does (each element of an array in the way).
pub(crate) fn shape_set(doc: &mut Document, segs: &[&str], v: teseladb_bson::Value) {
    shape::set_path_value(doc, segs, v);
}

/// Whether RefDb's slot-based engine runs a stage of `st`'s kind (measured on a `refrouter`
/// 9.0.4 by the shard's `explainVersion`): the stages it cannot run send the rest of the
/// pipeline to the classic engine, as the operators of [`sbe`] do. A `$lookup` runs there with
/// `localField` and `foreignField` and an empty or no `pipeline`.
fn sbe_capable(st: &Stage) -> bool {
    match st {
        Stage::Match(_)
        | Stage::Project(_)
        | Stage::AddFields(_)
        | Stage::ReplaceRoot(_)
        | Stage::Sort(_)
        | Stage::Limit(_)
        | Stage::Skip(_)
        | Stage::Count(_)
        | Stage::Group(_)
        | Stage::SortByCount(_)
        | Stage::Bucket(_) => true,
        Stage::Lookup(l) => l.is_equality(),
        Stage::Fill(f) => f.is_values_only(),
        Stage::Sample(_)
        | Stage::Unwind(_)
        | Stage::BucketAuto(_)
        | Stage::Redact(_)
        | Stage::Facet(_)
        | Stage::UnionWith(_)
        | Stage::Documents(_)
        | Stage::SetWindowFields(_)
        | Stage::Densify(_)
        | Stage::GraphLookup(_)
        | Stage::GeoNear(_)
        | Stage::Score(_)
        | Stage::Fusion(_)
        | Stage::System(..)
        | Stage::Search(..)
        | Stage::Write(_) => false,
    }
}

fn run_stage(
    st: &Stage,
    docs: Vec<Document>,
    env: &dyn Env,
    sbe: bool,
) -> Result<Vec<Document>, MqlError> {
    Ok(match st {
        Stage::Match(m) => m.run(docs, env)?,
        Stage::Project(p) => {
            docs.iter().map(|d| p.apply(d, &env.scope(d))).collect::<Result<_, _>>()?
        }
        Stage::AddFields(a) => {
            docs.iter().map(|d| a.apply(d, &env.scope(d))).collect::<Result<_, _>>()?
        }
        Stage::ReplaceRoot(r) => {
            docs.iter().map(|d| r.apply(d, &env.scope(d))).collect::<Result<_, _>>()?
        }
        Stage::Sort(s) => {
            errors::check_sort_memory(&docs, env.allow_disk_use())?;
            s.sort(docs, env.collation().get())?
        }
        Stage::Limit(n) => {
            docs.into_iter().take(usize::try_from(*n).unwrap_or(usize::MAX)).collect()
        }
        Stage::Skip(n) => {
            docs.into_iter().skip(usize::try_from(*n).unwrap_or(usize::MAX)).collect()
        }
        Stage::Count(field) => stages::run_count(field, docs.len()),
        Stage::Unwind(u) => u.run(docs),
        Stage::Group(g) => g.run(docs, env, sbe)?,
        Stage::SortByCount(e) => group::run_sort_by_count(e, docs, env, sbe)?,
        Stage::Bucket(b) => b.run(docs, env, sbe)?,
        Stage::BucketAuto(b) => {
            errors::check_sort_memory(&docs, env.allow_disk_use())?;
            b.run(docs, env)?
        }
        Stage::Sample(s) => s.run(docs, env),
        Stage::Redact(e) => stages::run_redact(e, docs, env)?,
        Stage::Facet(f) => f.run(docs, env)?,
        Stage::Lookup(l) => l.run(docs, env)?,
        Stage::UnionWith(u) => u.run(docs, env)?,
        Stage::Documents(e) => stages::run_documents(e, env)?,
        Stage::SetWindowFields(w) => {
            errors::check_sort_memory(&docs, env.allow_disk_use())?;
            w.run(docs, env)?
        }
        Stage::Densify(d) => d.run(docs, env.collation().get())?,
        Stage::Fill(f) => f.run(docs, env)?,
        Stage::GraphLookup(g) => g.run(docs, env)?,
        Stage::GeoNear(g) => g.run(docs, env)?,
        // Without metadata before it, `$score` gives each document its own.
        Stage::Score(_) => docs,
        Stage::Fusion(f) => f.run(docs, env)?.0,
        Stage::System(name, spec) => env.system_stage(name, spec)?,
        Stage::Search(name, spec) => {
            env.search_source(name, spec)?.into_iter().map(|s| s.doc).collect()
        }
        // The node writes what reaches it (`out_merge`).
        Stage::Write(_) => docs,
    })
}

/// `docs`, each marked with its place.
fn marked(docs: Vec<Document>) -> Vec<Document> {
    docs.into_iter()
        .enumerate()
        .map(|(i, mut d)| {
            d.push(meta::MARK, i as i64);
            d
        })
        .collect()
}

/// Runs stage `st` over documents with metadata (`metas`): a stage that changes, filters,
/// reorders, unwinds or joins documents runs over each with its metadata in scope and keeps it;
/// one that makes documents from many, or adds others, drops it (measured on a `refrouter` 9.0.4).
fn run_stage_meta(
    st: &Stage,
    docs: Vec<Document>,
    metas: Vec<meta::Meta>,
    env: &dyn Env,
    sbe: bool,
) -> Result<(Vec<Document>, Option<Vec<meta::Meta>>), MqlError> {
    let each = |docs: Vec<Document>, metas: Vec<meta::Meta>| -> Result<_, MqlError> {
        let mut out = (Vec::new(), Vec::new());
        for (d, m) in docs.into_iter().zip(metas) {
            let menv = meta::MetaEnv { inner: env, meta: &m, marked: &[] };
            for r in run_stage(st, vec![d], &menv, sbe)? {
                out.0.push(r);
                out.1.push(m.clone());
            }
        }
        Ok((out.0, Some(out.1)))
    };
    Ok(match st {
        Stage::Match(_)
        | Stage::Project(_)
        | Stage::AddFields(_)
        | Stage::ReplaceRoot(_)
        | Stage::Redact(_)
        | Stage::Unwind(_)
        | Stage::Lookup(_)
        | Stage::GraphLookup(_)
        | Stage::Write(_) => each(docs, metas)?,
        Stage::Limit(n) => {
            let n = usize::try_from(*n).unwrap_or(usize::MAX);
            (docs.into_iter().take(n).collect(), Some(metas.into_iter().take(n).collect()))
        }
        Stage::Skip(n) => {
            let n = usize::try_from(*n).unwrap_or(usize::MAX);
            (docs.into_iter().skip(n).collect(), Some(metas.into_iter().skip(n).collect()))
        }
        Stage::Sort(s) => {
            let col = env.collation();
            let mut keyed = Vec::with_capacity(docs.len());
            for (d, mut m) in docs.into_iter().zip(metas) {
                let key = s.key_with(&d, col.get(), &m)?;
                m.insert(crate::expr::SORT_KEY.to_owned(), Value::Array(key.clone()));
                keyed.push((key, d, m));
            }
            crate::expr::try_sort_by(&mut keyed, |(a, _, _), (b, _, _)| {
                s.compare(a, b, col.get())
            })?;
            let (docs, metas): (Vec<_>, Vec<_>) = keyed.into_iter().map(|(_, d, m)| (d, m)).unzip();
            (docs, Some(metas))
        }
        // Each document a sample takes keeps its metadata and gets its random value.
        Stage::Sample(s) => {
            let mut out = (Vec::new(), Vec::new());
            for (i, r) in s.pick(docs.len(), env) {
                let mut m = metas[i].clone();
                m.insert(meta::RAND_VAL.to_owned(), Value::Double(r));
                out.0.push(docs[i].clone());
                out.1.push(m);
            }
            (out.0, Some(out.1))
        }
        Stage::Facet(f) => (f.run_with_meta(docs, metas, env)?, None),
        // A window keeps each document and reorders them: a mark finds each one's metadata.
        Stage::SetWindowFields(_) => {
            let empty = meta::Meta::new();
            let menv = meta::MetaEnv { inner: env, meta: &empty, marked: &metas };
            let mut out = (Vec::new(), Vec::new());
            for mut d in run_stage(st, marked(docs), &menv, sbe)? {
                if let Some(Value::Int64(i)) = d.remove(meta::MARK)
                    && let Some(m) = usize::try_from(i).ok().and_then(|i| metas.get(i))
                {
                    out.1.push(m.clone());
                }
                meta::unmark_doc(&mut d);
                out.0.push(d);
            }
            (out.0, Some(out.1))
        }
        // A stage that makes documents from many reads each one's metadata and drops it.
        Stage::Group(_) | Stage::Bucket(_) | Stage::BucketAuto(_) | Stage::SortByCount(_) => {
            let empty = meta::Meta::new();
            let menv = meta::MetaEnv { inner: env, meta: &empty, marked: &metas };
            let mut out = run_stage(st, marked(docs), &menv, sbe)?;
            out.iter_mut().for_each(meta::unmark_doc);
            (out, None)
        }
        _ => (run_stage(st, docs, env, sbe)?, None),
    })
}
