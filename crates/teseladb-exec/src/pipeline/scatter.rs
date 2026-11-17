//! An aggregation over the ranges of several cores (010 FR-034, T064): a pipeline that starts
//! with `$match`es and a `$group` whose accumulators merge ([`Group::merges`]) sends its
//! [`Program`] to each core that owns a range; the core runs the `$match`es and the `$group`
//! over its documents and gives back only its partial groups, and the coordinator merges them in
//! the order of the ranges ([`Group::merge_parts`]) and runs the rest of the stages. The result
//! is the one of the run in one core: the groups come in the order their first document came,
//! each with the key of that document, `$first` and `$last` take the first and last document of
//! the whole input, and a merge that would not give the same totals (doubles with fractions,
//! decimals, integers past 2^53) or groups past the memory limit are refused, after which the
//! caller runs the pipeline over the whole input as before.
//!
//! The program holds only the stages' specifications, so it goes to another core's thread; the
//! core parses them again. The stages it runs read nothing but the document: no variables
//! (`$$`), no `$expr` (evaluated through the node's extensions), no JavaScript, no random
//! numbers, no text or geographic operators, and the command has no collation.

use std::sync::Arc;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

use teseladb_keys::columns::ColumnsView;

use super::group::{Fed, Group, PartialGroups};
use super::{Env, Pipeline, Stage};
use crate::collation::Collation;
use crate::expr::Ctx;

/// The operators whose value depends on more than the document, or that the node's extensions
/// evaluate: a stage that names one does not run on the cores.
const OUTSIDE_THE_DOCUMENT: [&str; 13] = [
    "$expr",
    "$where",
    "$text",
    "$near",
    "$nearSphere",
    "$geoWithin",
    "$geoIntersects",
    "$within",
    "$sampleRate",
    "$function",
    "$accumulator",
    "$rand",
    "$meta",
];

/// What each core runs over its documents: the `$match`es and the `$group`, as specified.
#[derive(Clone, Debug)]
pub struct Program {
    matches: Vec<Document>,
    group: Document,
    /// Whether the reference server runs the `$group` with its slot-based engine.
    sbe: bool,
    /// The bytes of group keys past which the run declines (the groups of the whole would pass
    /// them too).
    limit: usize,
}

/// A pipeline split to run over the ranges of several cores.
#[derive(Clone, Debug)]
pub struct Scatter {
    program: Arc<Program>,
    group: Group,
    /// The first stage after the `$group`.
    at: usize,
}

/// The environment of a [`Program`] on a core: no variables, no collation, no extensions.
struct PlainEnv;

impl Env for PlainEnv {
    fn collation(&self) -> Collation {
        Collation::default()
    }

    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _name: &str) -> Result<Vec<Document>, MqlError> {
        Ok(Vec::new())
    }

    fn random(&self) -> f64 {
        0.0
    }
}

/// Whether `v` names nothing but the document: no variable and no operator of
/// [`OUTSIDE_THE_DOCUMENT`].
fn of_the_document(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.starts_with("$$"),
        Value::Document(d) => {
            d.iter().all(|(k, v)| !OUTSIDE_THE_DOCUMENT.contains(&k) && of_the_document(v))
        }
        Value::Array(a) => a.iter().all(of_the_document),
        _ => true,
    }
}

/// `stages` parsed as a pipeline over a collection.
fn parse(stages: &[Document]) -> Result<Pipeline, MqlError> {
    let bytes: Vec<Vec<u8>> =
        stages.iter().map(Document::to_bytes).collect::<Result<_, _>>().map_err(crate::raw::bad)?;
    let raws: Vec<RawDoc<'_>> = bytes
        .iter()
        .map(|b| RawDoc::from_bytes(b))
        .collect::<Result<_, _>>()
        .map_err(crate::raw::bad)?;
    let values: Vec<RawValue<'_>> = raws.into_iter().map(RawValue::Document).collect();
    Pipeline::parse(&values, &Ctx::checked(std::iter::empty()))
}

/// One part of the input of a [`Program`], in key order (010 FR-033).
#[derive(Clone, Debug)]
pub enum Input<'a> {
    /// The stored bytes of a document.
    Row(&'a [u8]),
    /// The documents of a data block, as its column block.
    Columns(ColumnsView<'a>),
}

/// Operators that read a field named by a string rather than a path, which the dependency
/// analysis does not see: a program that names one takes no columns.
const FIELDS_BY_NAME: [&str; 3] = ["$getField", "$setField", "$unsetField"];

/// Whether `v` names an operator of `ops`.
fn names_any(v: &Value, ops: &[&str]) -> bool {
    match v {
        Value::Document(d) => d.iter().any(|(k, v)| ops.contains(&k) || names_any(v, ops)),
        Value::Array(a) => a.iter().any(|v| names_any(v, ops)),
        _ => false,
    }
}

/// The stored document `bytes`.
fn document(bytes: &[u8]) -> Option<Document> {
    let bson = teseladb_keys::stored::bson(bytes).ok()?;
    Document::from_stored(bson.to_vec()).ok()
}

impl Program {
    /// The partial groups of the stored documents `rows` (key and bytes, in key order), one
    /// part of the input; `None` when the core declines: a document that does not read, a stage
    /// that fails, keys past the limit or a state that does not merge. The coordinator then runs
    /// the pipeline over the whole input, which gives the error or the spill a run in one core
    /// gives.
    pub fn run(&self, rows: &[(Vec<u8>, Vec<u8>)]) -> Option<PartialGroups> {
        let inputs: Vec<Input<'_>> = rows.iter().map(|(_, b)| Input::Row(b)).collect();
        self.run_inputs(&inputs)
    }

    /// The top-level fields the program reads, when it reads nothing else (010 FR-033): the
    /// columns a scan may hand over in place of its documents.
    pub fn columns(&self) -> Option<Vec<String>> {
        let group = Document::new().with("$group", self.group.clone());
        let stages: Vec<Document> = self.matches.iter().cloned().chain([group]).collect();
        let named = stages.iter().any(|d| names_any(&Value::Document(d.clone()), &FIELDS_BY_NAME));
        if named {
            return None;
        }
        let mut out: Vec<String> = Vec::new();
        for path in super::deps::needed(&stages)? {
            let top = path.split('.').next().unwrap_or_default();
            if top.is_empty() || top.starts_with('$') {
                return None;
            }
            if !out.iter().any(|f| f == top) {
                out.push(top.to_owned());
            }
        }
        Some(out)
    }

    /// [`Program::run`] over a part of the input that comes as stored documents and column
    /// blocks (010 FR-033), in key order: the same partial groups as over the documents of
    /// those blocks. Without `$match`es the `$group` takes each column block on its columns;
    /// otherwise each of its documents is made again from the fields the program reads.
    pub fn run_inputs(&self, inputs: &[Input<'_>]) -> Option<PartialGroups> {
        let columnar = inputs.iter().any(|i| matches!(i, Input::Columns(_)));
        let want = match columnar {
            true => self.columns()?,
            false => Vec::new(),
        };
        let spec = self.group.to_bytes().ok()?;
        let raw = RawDoc::from_bytes(&spec).ok()?;
        let group =
            Group::parse(RawValue::Document(raw), &Ctx::checked(std::iter::empty())).ok()?;
        if self.matches.is_empty() {
            let mut fed: Vec<Fed<'_>> = Vec::new();
            for input in inputs {
                match (input, fed.last_mut()) {
                    (Input::Row(b), Some(Fed::Docs(docs))) => docs.push(document(b)?),
                    (Input::Row(b), _) => fed.push(Fed::Docs(vec![document(b)?])),
                    (Input::Columns(view), _) => fed.push(Fed::Columns(view.clone())),
                }
            }
            return group.partial_fed(&fed, &want, (&PlainEnv, self.sbe, self.limit)).ok()?;
        }
        // Stored documents only: the filter runs on their bytes first and only the documents it
        // keeps are made (010 FR-027); they go through the `$match`es as before.
        let prefiltered = match columnar {
            false => Some(self.matching_rows(inputs)?),
            true => None,
        };
        if let Some(docs) = prefiltered {
            let docs = parse(&self.matches).ok()?.run_in(docs, &PlainEnv, self.sbe).ok()?;
            return group.partial(docs, &PlainEnv, self.sbe, self.limit).ok()?;
        }
        let mut docs = Vec::with_capacity(inputs.len());
        for input in inputs {
            match input {
                Input::Row(b) => docs.push(document(b)?),
                Input::Columns(view) => docs.extend(super::group::documents_of(view, &want)),
            }
        }
        let docs = parse(&self.matches).ok()?.run_in(docs, &PlainEnv, self.sbe).ok()?;
        group.partial(docs, &PlainEnv, self.sbe, self.limit).ok()?
    }

    /// The filter of the program's `$match`es: the `$match`es, one after the other, are the
    /// conjunction of their filters. `None` when a stage is not a `$match` of a document or the
    /// filter does not parse.
    fn filter(&self) -> Option<teseladb_mql::Filter> {
        // Each of `matches` is a whole `$match` stage: its filter is the stage's value.
        let filters = self
            .matches
            .iter()
            .map(|m| match m.get("$match") {
                Some(Value::Document(f)) => Some(f.clone()),
                _ => None,
            })
            .collect::<Option<Vec<Document>>>()?;
        let spec = match filters.as_slice() {
            [one] => one.clone(),
            all => Document::new()
                .with("$and", Value::Array(all.iter().cloned().map(Value::Document).collect())),
        };
        let bytes = spec.to_bytes().ok()?;
        let raw = RawDoc::from_bytes(&bytes).ok()?;
        teseladb_mql::Filter::parse(raw, teseladb_mql::FilterUse::Query).ok()
    }

    /// What decides, where the documents are read, which of them the program needs (010
    /// FR-027): the filter of its `$match`es on the stored bytes, and the data blocks it rules
    /// out by their zone maps (004 FR-039). `None` when the filter does not parse; the reader
    /// then hands over every document.
    pub fn row_filter(&self) -> Option<RowFilter> {
        match self.matches.is_empty() {
            true => Some(RowFilter { filter: None }),
            false => Some(RowFilter { filter: Some(self.filter()?) }),
        }
    }

    /// The documents of `inputs` (stored documents only) that the program's `$match`es keep, in
    /// their order, evaluated on the stored bytes a batch at a time on the register machine (010
    /// FR-028, FR-031) with the environment of a core, no collation and no extensions. `None`
    /// when a document does not read or the filter fails; the coordinator then runs the pipeline
    /// over the whole input, which gives that error.
    fn matching_rows(&self, inputs: &[Input<'_>]) -> Option<Vec<Document>> {
        let filter = self.filter()?;
        let ctx = teseladb_mql::MatchContext { collation: None, ext: &NoExtensions, vars: None };
        let rows: Vec<&[u8]> = inputs
            .iter()
            .map(|i| match i {
                Input::Row(b) => Some(*b),
                Input::Columns(_) => None,
            })
            .collect::<Option<_>>()?;
        let mut docs = Vec::new();
        for batch in rows.chunks(teseladb_mql::vm::BATCH) {
            let stored = batch
                .iter()
                .map(|b| teseladb_keys::stored::split(b).ok())
                .collect::<Option<Vec<_>>>()?;
            let kept = filter.matches_batch(&stored, &ctx).ok()?;
            for (lane, b) in batch.iter().enumerate() {
                if (kept >> lane) & 1 == 1 {
                    docs.push(document(b)?);
                }
            }
        }
        Some(docs)
    }
}

/// The part of a [`Program`] that runs where its documents are read (010 FR-027): its
/// `$match`es' filter on the stored bytes and on the zone maps of the data blocks.
pub struct RowFilter {
    /// `None`: the program has no `$match`, every document goes to it.
    filter: Option<teseladb_mql::Filter>,
}

impl RowFilter {
    /// Which of the stored documents of `batch` (at most 64) the program may keep, a bit per
    /// document from the lowest. A batch with a document whose frame does not read keeps them
    /// all, as does a filter that fails on it: the program reads them again and declines,
    /// and the coordinator runs the pipeline over the whole input, which gives that error.
    pub fn keep(&self, batch: &[&[u8]]) -> u64 {
        let all = match batch.len() {
            n if n >= 64 => u64::MAX,
            n => (1u64 << n) - 1,
        };
        let Some(filter) = &self.filter else { return all };
        let Some(stored) =
            batch.iter().map(|b| teseladb_keys::stored::split(b).ok()).collect::<Option<Vec<_>>>()
        else {
            return all;
        };
        let ctx = teseladb_mql::MatchContext { collation: None, ext: &NoExtensions, vars: None };
        filter.matches_batch(&stored, &ctx).unwrap_or(all)
    }

    /// Whether a data block whose zone maps are `zones` may hold a document the program keeps
    /// (004 FR-039); a program runs with no collation.
    pub fn block_may_match(&self, zones: &dyn teseladb_keys::zone::Zones) -> bool {
        self.filter.as_ref().is_none_or(|f| crate::zone::block_may_match(f.expr(), false, zones))
    }
}

impl Program {
    /// The program in bytes, as it travels to the leader of a range on another node (010
    /// FR-034): the documents of its `$match`es and of its `$group`, whether it runs with the
    /// slot-based engine and its limit; `None` when they cannot be written.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let matches = self.matches.iter().cloned().map(Value::Document).collect();
        let limit = i64::try_from(self.limit).unwrap_or(i64::MAX);
        Document::new()
            .with("m", Value::Array(matches))
            .with("g", self.group.clone())
            .with("s", self.sbe)
            .with("l", limit)
            .to_bytes_with(super::group::wire_limits())
            .ok()
    }

    /// What [`Program::encode`] wrote; `None` for anything else.
    pub fn decode(bytes: &[u8]) -> Option<Program> {
        let raw = RawDoc::from_bytes_with(bytes, super::group::wire_limits()).ok()?;
        let doc = Document::from(raw);
        let [
            ("m", Value::Array(m)),
            ("g", Value::Document(g)),
            ("s", Value::Boolean(sbe)),
            ("l", Value::Int64(l)),
        ] = doc.iter().collect::<Vec<_>>().as_slice()
        else {
            return None;
        };
        let matches = m
            .iter()
            .map(|v| match v {
                Value::Document(d) => Some(d.clone()),
                _ => None,
            })
            .collect::<Option<Vec<Document>>>()?;
        let limit = usize::try_from(*l).ok()?;
        Some(Program { matches, group: g.clone(), sbe: *sbe, limit })
    }
}

impl Scatter {
    /// What each core runs.
    pub fn program(&self) -> &Arc<Program> {
        &self.program
    }

    /// The output of `pipeline` from the partial groups `parts` of consecutive parts of its
    /// input, in their order: the groups merged, then the stages after the `$group`. `None` when
    /// the groups do not merge into those of the whole, and the caller runs the pipeline over
    /// the whole input.
    pub fn finish(
        &self,
        pipeline: &Pipeline,
        parts: Vec<PartialGroups>,
        env: &dyn Env,
    ) -> Result<Option<Vec<Document>>, MqlError> {
        let sbe = self.program.sbe;
        let Some(groups) = self.group.merge_parts(parts, env, sbe)? else { return Ok(None) };
        pipeline.run_from(self.at, (groups, None), env, sbe).map(|(docs, _)| Some(docs))
    }
}

impl Pipeline {
    /// The split of this pipeline, whose stages are `stages` as the command gave them, for a run
    /// over the ranges of several cores; `sbe` when the reference server could run it with its slot-based
    /// engine; `limit` the bytes a stage may hold. `None` when it does not start with `$match`es and a `$group` whose accumulators
    /// merge, or those stages read more than the document.
    pub fn scatter(&self, stages: &[Document], sbe: bool, limit: usize) -> Option<Scatter> {
        if stages.len() != self.stages.len() || self.reads_index_key {
            return None;
        }
        let n = self.leading_matches();
        let Some(Stage::Group(group)) = self.stages.get(n) else { return None };
        let named = |i: usize, name: &str| stages[i].len() == 1 && stages[i].get(name).is_some();
        if !group.merges() || !named(n, "$group") || !(0..n).all(|i| named(i, "$match")) {
            return None;
        }
        if !stages[..=n].iter().all(|s| s.iter().all(|(_, v)| of_the_document(v))) {
            return None;
        }
        let mut sbe = sbe && !self.starts_with_documents();
        for (st, spec) in self.stages.iter().zip(&self.sbe_specs).take(n + 1) {
            sbe = sbe && *spec && super::sbe_capable(st);
        }
        let Some(Value::Document(spec)) = stages[n].get("$group") else { return None };
        let program = Program { matches: stages[..n].to_vec(), group: spec.clone(), sbe, limit };
        Some(Scatter { program: Arc::new(program), group: group.clone(), at: n + 1 })
    }
}
