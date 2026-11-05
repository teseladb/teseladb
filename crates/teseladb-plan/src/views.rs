//! Views (015 FR-001, research R3): a view is the pipeline of its definition run over the
//! collection or view it reads, never materialized. A query on a view runs on the collection at
//! the end of the chain with the pipelines of the views put first, the innermost one first.
//!
//! Before the catalog keeps a definition, `create` and `collMod` check it as a `refrouter` 9.0.4
//! does (measured 31-10-2026): its pipeline may not write (`$out`, `$merge`) nor open a change
//! stream, also inside `$lookup`, `$unionWith` and `$facet`; the views it reads (`viewOn` and the
//! namespaces of `$lookup`, `$graphLookup` and `$unionWith`) must have its collation; it may not
//! reach itself again; and it may not stand on more than [`MAX_VIEW_DEPTH`] views. A query refuses
//! a chain of [`MAX_VIEW_DEPTH`] views or more.

use teseladb_meta::collection_types::CollectionKind;
use teseladb_bson::{RawDoc, RawValue};

use crate::stages::{self, Reach, StageError};

/// Most views a definition may stand on, itself included.
pub const MAX_VIEW_DEPTH: usize = 20;
/// `GraphContainsCycle`.
pub const GRAPH_CONTAINS_CYCLE: i32 = 5;
/// `ViewDepthLimitExceeded`.
pub const VIEW_DEPTH_LIMIT_EXCEEDED: i32 = 165;
/// `OptionNotSupportedOnView`.
pub const OPTION_NOT_SUPPORTED_ON_VIEW: i32 = 167;
/// A writing stage inside `$facet`.
pub const FACET_WRITES: i32 = 40_600;
/// A writing stage inside the sub-pipeline of `$lookup`.
pub const LOOKUP_WRITES: i32 = 51_047;
/// A writing stage inside the sub-pipeline of `$unionWith`.
pub const UNION_WITH_WRITES: i32 = 31_441;

/// The stage a change stream turns into, as RefDb names it inside a sub-pipeline.
const CHANGE_STREAM_INNER: &str = "$_internalChangeStreamOplogMatch";

/// Why a view is refused, with RefDb's code and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewError {
    /// RefDb's code.
    pub code: i32,
    /// RefDb's message.
    pub message: String,
}

impl From<StageError> for ViewError {
    fn from(e: StageError) -> ViewError {
        ViewError { code: e.code, message: e.message }
    }
}

/// A definition of view `name` of a database.
#[derive(Clone, Copy, Debug)]
pub struct Definition<'a> {
    /// The view's name in its database.
    pub name: &'a str,
    /// The collection or view it reads.
    pub view_on: &'a str,
    /// Its stages, as BSON.
    pub pipeline: &'a [Vec<u8>],
    /// Its collation, as BSON (`None`: simple).
    pub collation: Option<&'a [u8]>,
}

/// The stage that holds a sub-pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Container {
    Lookup,
    UnionWith,
    Facet,
}

impl Container {
    fn refusal(self, stage: &str) -> (i32, String) {
        match self {
            Container::Lookup => {
                (LOOKUP_WRITES, format!("{stage} is not allowed within a $lookup's sub-pipeline"))
            }
            Container::UnionWith => (
                UNION_WITH_WRITES,
                format!("{stage} is not allowed within a $unionWith's sub-pipeline"),
            ),
            Container::Facet => {
                (FACET_WRITES, format!("{stage} is not allowed to be used within a $facet stage"))
            }
        }
    }
}

fn not_bson() -> ViewError {
    ViewError { code: OPTION_NOT_SUPPORTED_ON_VIEW, message: "a stage that is not BSON".to_owned() }
}

fn stage_doc(bytes: &[u8]) -> Result<RawDoc<'_>, ViewError> {
    RawDoc::from_bytes(bytes).map_err(|_| not_bson())
}

/// The documents of a BSON array of stages (anything else is not a sub-pipeline).
fn sub_stages(v: RawValue<'_>) -> Vec<RawDoc<'_>> {
    match v {
        RawValue::Array(a) => a
            .iter()
            .filter_map(|s| match s {
                RawValue::Document(d) => Some(d),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The sub-pipelines of a stage, each with the stage that holds it.
fn sub_pipelines<'a>(name: &str, spec: RawValue<'a>) -> Vec<(Container, Vec<RawDoc<'a>>)> {
    let RawValue::Document(spec) = spec else {
        return Vec::new();
    };
    match name {
        "$lookup" => spec.get("pipeline").map(|p| vec![(Container::Lookup, sub_stages(p))]),
        "$unionWith" => spec.get("pipeline").map(|p| vec![(Container::UnionWith, sub_stages(p))]),
        "$facet" => Some(spec.iter().map(|(_, p)| (Container::Facet, sub_stages(p))).collect()),
        _ => None,
    }
    .unwrap_or_default()
}

/// Checks stages `pipeline` of view `ns`, inside `container` when it is a sub-pipeline.
fn check_stages(
    ns: &str,
    pipeline: &[RawDoc<'_>],
    container: Option<Container>,
) -> Result<(), ViewError> {
    let invalid = |code: i32, why: String| ViewError {
        code,
        message: format!("Invalid pipeline for view {ns} :: caused by :: {why}"),
    };
    for (i, doc) in pipeline.iter().enumerate() {
        let fields: Vec<(&str, RawValue<'_>)> = doc.iter().collect();
        let names: Vec<&str> = fields.iter().map(|(k, _)| *k).collect();
        let (name, stage) = stages::named(&names)?;
        match (stage.reach, container) {
            (Reach::Reads | Reach::Internal, _) => {}
            (Reach::Writes, None) => {
                return Err(invalid(
                    OPTION_NOT_SUPPORTED_ON_VIEW,
                    format!(
                        "The aggregation stage {name} in location {i} of the pipeline cannot be used in the view definition of {ns} because it writes to disk"
                    ),
                ));
            }
            (Reach::ChangeStream, None) => {
                return Err(invalid(
                    OPTION_NOT_SUPPORTED_ON_VIEW,
                    format!("{name} cannot be used in a view definition"),
                ));
            }
            (Reach::Writes, Some(c)) => {
                let (code, why) = c.refusal(name);
                return Err(invalid(code, why));
            }
            (Reach::ChangeStream, Some(c)) => {
                let (code, why) = c.refusal(CHANGE_STREAM_INNER);
                return Err(invalid(code, why));
            }
        }
        for (inner, stages) in sub_pipelines(name, fields[0].1) {
            check_stages(ns, &stages, Some(inner))?;
        }
    }
    Ok(())
}

/// Checks the stages of the definition of view `ns` (`db.name`).
pub fn check_pipeline(ns: &str, pipeline: &[Vec<u8>]) -> Result<(), ViewError> {
    let docs = pipeline.iter().map(|s| stage_doc(s)).collect::<Result<Vec<_>, _>>()?;
    check_stages(ns, &docs, None)
}

/// The collections and views a pipeline reads besides its source, in the order it names them.
fn referenced(pipeline: &[RawDoc<'_>], out: &mut Vec<String>) {
    for doc in pipeline {
        for (name, spec) in doc.iter() {
            match (name, spec) {
                ("$lookup" | "$graphLookup", RawValue::Document(s)) => {
                    if let Some(RawValue::String(from)) = s.get("from") {
                        out.push(from.to_owned());
                    }
                }
                ("$unionWith", RawValue::String(coll)) => out.push(coll.to_owned()),
                ("$unionWith", RawValue::Document(s)) => {
                    if let Some(RawValue::String(coll)) = s.get("coll") {
                        out.push(coll.to_owned());
                    }
                }
                _ => {}
            }
            for (_, stages) in sub_pipelines(name, spec) {
                referenced(&stages, out);
            }
        }
    }
}

/// What a view reads: `viewOn` first, then the namespaces its pipeline names.
fn reads(view_on: &str, pipeline: &[Vec<u8>]) -> Result<Vec<String>, ViewError> {
    let docs = pipeline.iter().map(|s| stage_doc(s)).collect::<Result<Vec<_>, _>>()?;
    let mut out = vec![view_on.to_owned()];
    referenced(&docs, &mut out);
    Ok(out)
}

/// A view as the graph sees it.
struct Node {
    reads: Vec<String>,
}

/// The views of a database, with `def` in place of what the catalog keeps under its name.
struct Graph<'a, F> {
    def: &'a Definition<'a>,
    lookup: F,
}

impl<F: FnMut(&str) -> Option<CollectionKind>> Graph<'_, F> {
    fn node(&mut self, name: &str) -> Result<Option<Node>, ViewError> {
        if name == self.def.name {
            return Ok(Some(Node { reads: reads(self.def.view_on, self.def.pipeline)? }));
        }
        match (self.lookup)(name) {
            Some(CollectionKind::View { view_on, pipeline, .. }) => {
                Ok(Some(Node { reads: reads(&view_on, &pipeline)? }))
            }
            _ => Ok(None),
        }
    }

    /// The most views a path from `name` goes through, `name` included; a cycle on `path` is
    /// an error.
    fn depth(&mut self, db: &str, name: &str, path: &mut Vec<String>) -> Result<usize, ViewError> {
        let Some(node) = self.node(name)? else {
            return Ok(0);
        };
        if let Some(start) = path.iter().position(|p| p == name) {
            let mut cycle: Vec<&str> = path[start..].iter().map(String::as_str).collect();
            cycle.push(name);
            cycle.reverse();
            let shown: Vec<String> = cycle.iter().map(|c| format!("{db}.{c}")).collect();
            return Err(ViewError {
                code: GRAPH_CONTAINS_CYCLE,
                message: format!(
                    "View cycle detected: {db}.{} => {}",
                    self.def.name,
                    shown.join(" => ")
                ),
            });
        }
        path.push(name.to_owned());
        let mut deepest = 0;
        for child in &node.reads {
            deepest = deepest.max(self.depth(db, child, path)?);
        }
        path.pop();
        Ok(deepest + 1)
    }
}

/// Checks definition `def` of a view of database `db` against the views `lookup` gives (the kind
/// of each name of the database, as the catalog has it).
pub fn check_definition<F: FnMut(&str) -> Option<CollectionKind>>(
    db: &str,
    def: &Definition<'_>,
    mut lookup: F,
) -> Result<(), ViewError> {
    let ns = format!("{db}.{}", def.name);
    check_pipeline(&ns, def.pipeline)?;
    for read in reads(def.view_on, def.pipeline)? {
        if read == def.name {
            continue;
        }
        if let Some(CollectionKind::View { collation, .. }) = lookup(&read)
            && collation.as_deref() != def.collation
        {
            return Err(ViewError {
                code: OPTION_NOT_SUPPORTED_ON_VIEW,
                message: format!("View {ns} has conflicting collation with view {db}.{read}"),
            });
        }
    }
    let mut graph = Graph { def, lookup };
    if graph.depth(db, def.name, &mut Vec::new())? > MAX_VIEW_DEPTH {
        return Err(ViewError {
            code: VIEW_DEPTH_LIMIT_EXCEEDED,
            message: format!("View depth limit exceeded; maximum depth is {MAX_VIEW_DEPTH}"),
        });
    }
    Ok(())
}

/// A view resolved down to the collection it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The collection at the end of the chain (it may not exist).
    pub collection: String,
    /// The stages to run on it: the innermost view's first.
    pub pipeline: Vec<Vec<u8>>,
    /// The view's collation, as BSON (`None`: simple).
    pub collation: Option<Vec<u8>>,
}

/// Resolves `name` of a database through the views `lookup` gives (which may fail with the
/// caller's error): `None` if it is not a view.
pub fn resolve<E, F>(name: &str, mut lookup: F) -> Result<Option<Resolved>, E>
where
    E: From<ViewError>,
    F: FnMut(&str) -> Result<Option<CollectionKind>, E>,
{
    let Some(CollectionKind::View { view_on, pipeline, collation }) = lookup(name)? else {
        return Ok(None);
    };
    let mut chain = vec![pipeline];
    let mut current = view_on;
    loop {
        if chain.len() >= MAX_VIEW_DEPTH {
            return Err(ViewError {
                code: VIEW_DEPTH_LIMIT_EXCEEDED,
                message: format!(
                    "View depth too deep or view cycle detected; maximum depth is {MAX_VIEW_DEPTH}"
                ),
            }
            .into());
        }
        match lookup(&current)? {
            Some(CollectionKind::View { view_on, pipeline, .. }) => {
                chain.push(pipeline);
                current = view_on;
            }
            _ => break,
        }
    }
    let pipeline = chain.into_iter().rev().flatten().collect();
    Ok(Some(Resolved { collection: current, pipeline, collation }))
}
