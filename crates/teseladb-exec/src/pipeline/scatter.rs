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

use teseladb_mql::{Extensions, MqlError, NoExtensions};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::group::{Group, PartialGroups};
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
    /// Whether RefDb runs the `$group` with its slot-based engine.
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

impl Program {
    /// The partial groups of the stored documents `rows` (key and bytes, in key order), one
    /// part of the input; `None` when the core declines: a document that does not read, a stage
    /// that fails, keys past the limit or a state that does not merge. The coordinator then runs
    /// the pipeline over the whole input, which gives the error or the spill a run in one core
    /// gives.
    pub fn run(&self, rows: &[(Vec<u8>, Vec<u8>)]) -> Option<PartialGroups> {
        let mut docs = Vec::with_capacity(rows.len());
        for (_, bytes) in rows {
            docs.push(Document::from_stored(bytes.clone()).ok()?);
        }
        let docs = match self.matches.is_empty() {
            true => docs,
            false => parse(&self.matches).ok()?.run_in(docs, &PlainEnv, self.sbe).ok()?,
        };
        let spec = self.group.to_bytes().ok()?;
        let raw = RawDoc::from_bytes(&spec).ok()?;
        let group =
            Group::parse(RawValue::Document(raw), &Ctx::checked(std::iter::empty())).ok()?;
        group.partial(docs, &PlainEnv, self.sbe, self.limit).ok()?
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
    /// over the ranges of several cores; `sbe` when RefDb could run it with its slot-based
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
