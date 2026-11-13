//! The extensions of the matcher a node runs (contract of `teseladb-mql`): `$geoWithin` and
//! `$geoIntersects` on documents without an index come from 009 (FR-012, T022); the operators of
//! the engines this node does not link yet answer as [`NoExtensions`] does; `$expr` and the
//! expressions of projections and `let` come from 010's engine (`teseladb_exec::expr`).

use teseladb_exec::expr::{Expr, Scope};
use teseladb_index::geo::query::{GeoOperator, matches, parse_query};
use teseladb_mql::{Extensions, GeoQuery, MqlError, NoExtensions, Result, TextQuery, codes};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

/// The shape of a `$geoWithin` or `$geoIntersects`; every error of a shape is a `BadValue`
/// (`teseladb_index::geo::query::BAD_VALUE`).
fn geo_shape(op: GeoOperator, query: &GeoQuery) -> Result<teseladb_index::geo::query::Shape> {
    parse_query(op, query.spec.get()).map_err(|e| MqlError::new(codes::BAD_VALUE, e.message))
}

/// Checks the shapes of the geospatial operators of `filter` before any document is read, as
/// RefDb parses them with the query.
pub fn check_geo(filter: &teseladb_mql::Filter) -> Result<()> {
    use teseladb_mql::{Expr as F, Pred};
    fn pred(p: &Pred) -> Result<()> {
        match p {
            Pred::Geo(g) => match GeoOperator::of(&g.operator) {
                Some(op) if !g.near => geo_shape(op, g).map(|_| ()),
                _ => Ok(()),
            },
            Pred::And(ps) => ps.iter().try_for_each(pred),
            Pred::Not(p) | Pred::ElemMatchValue(p) => pred(p),
            Pred::ElemMatchDoc(e) => walk(e),
            _ => Ok(()),
        }
    }
    fn walk(e: &F) -> Result<()> {
        match e {
            F::And(items) | F::Or(items) | F::Nor(items) => items.iter().try_for_each(walk),
            F::Field { pred: p, .. } => pred(p),
            _ => Ok(()),
        }
    }
    walk(filter.expr())
}

thread_local! {
    /// The state of the numbers of `$rand` and `$sampleRate` the node's matcher draws, set from
    /// the backend's random source at every command ([`reseed`]).
    static DRAWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Sets the state of the matcher's draws to `seed` (the dispatcher, from the backend).
pub(crate) fn reseed(seed: u64) {
    DRAWS.with(|d| d.set(seed));
}

/// The next 64 random bits of this thread's draws (SplitMix64).
pub(crate) fn next_seed() -> u64 {
    DRAWS.with(|d| {
        let state = d.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        d.set(state);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

/// A scope over `d` whose `$rand` draws from this thread's state.
fn drawing_scope(d: &Document, vars: Option<&Document>) -> Scope {
    Scope::new(d.clone(), vars).seeded(next_seed()).with_js(Some(super::js::engine()))
}

/// The node's extensions.
#[derive(Clone, Copy, Debug, Default)]
pub struct NodeExtensions;

impl Extensions for NodeExtensions {
    fn expr(&self, e: RawValue<'_>, doc: RawDoc<'_>, vars: Option<&Document>) -> Result<bool> {
        let (expr, d) = (Expr::parse(e)?, Document::from(doc));
        let v = expr.eval(&d, &drawing_scope(&d, vars))?;
        Ok(teseladb_exec::expr::truthy(&v))
    }

    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool> {
        super::js::where_js(code, doc)
    }

    fn text(&self, query: &TextQuery, doc: RawDoc<'_>) -> Result<bool> {
        NoExtensions.text(query, doc)
    }

    fn geo(&self, query: &GeoQuery, value: RawValue<'_>) -> Result<bool> {
        let op = match GeoOperator::of(&query.operator) {
            Some(op) if !query.near => op,
            _ => return NoExtensions.geo(query, value),
        };
        Ok(matches(&geo_shape(op, query)?, value))
    }

    fn random(&self) -> f64 {
        (next_seed() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn meta(&self, name: &str, doc: RawDoc<'_>) -> Result<Option<Value>> {
        NoExtensions.meta(name, doc)
    }

    fn eval(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>> {
        let (expr, d) = (Expr::parse(e)?, Document::from(doc));
        expr.eval(&d, &drawing_scope(&d, vars))
    }

    fn eval_projection(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>> {
        let expr =
            teseladb_exec::pipeline::projected_expr(e, &teseladb_exec::expr::Ctx::default())?;
        let d = Document::from(doc);
        expr.eval(&d, &drawing_scope(&d, vars))
    }

    fn update_pipeline(
        &self,
        pipeline: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document> {
        run_update_pipeline(pipeline, doc, vars, teseladb_exec::collation::Collation::default())
    }
}

/// The node's extensions under the collation of the operation (the command's, or else the
/// collection's default): the expressions of a filter (`$expr`) and of a `find` projection
/// compare strings with it, as RefDb 9.0.4 evaluates them with the operation's collator.
#[derive(Clone, Debug, Default)]
pub struct CollatedExtensions(teseladb_exec::collation::Collation);

impl CollatedExtensions {
    /// The extensions comparing strings with `collation`, the binary order for `None`.
    pub fn new(collation: Option<teseladb_collation::Collation>) -> CollatedExtensions {
        CollatedExtensions(
            collation
                .map(|c| teseladb_exec::collation::Collation::new(std::rc::Rc::new(c)))
                .unwrap_or_default(),
        )
    }

    fn scope(&self, d: &Document, vars: Option<&Document>) -> Scope {
        drawing_scope(d, vars).with_collation(self.0.clone())
    }
}

impl Extensions for CollatedExtensions {
    fn expr(&self, e: RawValue<'_>, doc: RawDoc<'_>, vars: Option<&Document>) -> Result<bool> {
        let (expr, d) = (Expr::parse(e)?, Document::from(doc));
        let v = expr.eval(&d, &self.scope(&d, vars))?;
        Ok(teseladb_exec::expr::truthy(&v))
    }

    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool> {
        NodeExtensions.where_js(code, doc)
    }

    fn text(&self, query: &TextQuery, doc: RawDoc<'_>) -> Result<bool> {
        NodeExtensions.text(query, doc)
    }

    fn geo(&self, query: &GeoQuery, value: RawValue<'_>) -> Result<bool> {
        NodeExtensions.geo(query, value)
    }

    fn random(&self) -> f64 {
        NodeExtensions.random()
    }

    fn meta(&self, name: &str, doc: RawDoc<'_>) -> Result<Option<Value>> {
        NodeExtensions.meta(name, doc)
    }

    fn eval(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>> {
        let (expr, d) = (Expr::parse(e)?, Document::from(doc));
        expr.eval(&d, &self.scope(&d, vars))
    }

    fn eval_projection(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>> {
        let expr =
            teseladb_exec::pipeline::projected_expr(e, &teseladb_exec::expr::Ctx::default())?;
        let d = Document::from(doc);
        expr.eval(&d, &self.scope(&d, vars))
    }

    fn update_pipeline(
        &self,
        pipeline: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document> {
        run_update_pipeline(pipeline, doc, vars, self.0.clone())
    }
}

/// Runs the pipeline of an update on `doc` with the command's variables `vars`, its expressions
/// comparing strings with `collation` (the operation's, as RefDb 9.0.4 evaluates them).
fn run_update_pipeline(
    pipeline: RawValue<'_>,
    doc: RawDoc<'_>,
    vars: Option<&Document>,
    collation: teseladb_exec::collation::Collation,
) -> Result<Document> {
    let stages: Vec<RawValue<'_>> = match pipeline {
        RawValue::Array(a) => a.iter().collect(),
        _ => Vec::new(),
    };
    refuse_outside_update(&stages)?;
    let names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
    let ctx = teseladb_exec::expr::Ctx::checked(names);
    let p = teseladb_exec::pipeline::Pipeline::parse(&stages, &ctx)?;
    let env = VarsEnv(vars.cloned().unwrap_or_default(), collation);
    let d = Document::from(doc);
    // An empty pipeline leaves the document as it is.
    Ok(p.run(vec![d.clone()], &env)?.into_iter().next().unwrap_or(d))
}

/// The stages an update's pipeline takes (measured on 9.0.4).
const UPDATE_STAGES: [&str; 6] =
    ["$addFields", "$set", "$project", "$unset", "$replaceRoot", "$replaceWith"];

/// `InvalidOptions`: a stage an update's pipeline does not take.
const INVALID_OPTIONS: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 72, name: "InvalidOptions" };

/// Checks the pipeline `u` of an update with variables `vars`: its stages and their arguments,
/// as RefDb parses it with the statement.
pub(crate) fn check_update_pipeline(u: &Value, vars: Option<&Document>) -> Result<()> {
    let holder = Document::new().with("u", u.clone());
    let bytes = holder.to_bytes().map_err(|e| MqlError::new(codes::BAD_VALUE, e.to_string()))?;
    let raw =
        RawDoc::from_bytes(&bytes).map_err(|e| MqlError::new(codes::BAD_VALUE, e.to_string()))?;
    let stages: Vec<RawValue<'_>> = match raw.get("u") {
        Some(RawValue::Array(a)) => a.iter().collect(),
        _ => Vec::new(),
    };
    refuse_outside_update(&stages)?;
    let names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
    teseladb_exec::pipeline::Pipeline::parse(&stages, &teseladb_exec::expr::Ctx::checked(names))?;
    Ok(())
}

/// Refuses a stage of an update's pipeline other than [`UPDATE_STAGES`].
pub(crate) fn refuse_outside_update(stages: &[RawValue<'_>]) -> Result<()> {
    for s in stages {
        let name = match s {
            RawValue::Document(d) => d.iter().next().map_or("", |(k, _)| k),
            _ => "",
        };
        if !UPDATE_STAGES.contains(&name) {
            let msg = format!("{name} is not allowed to be used within an update");
            return Err(MqlError::new(INVALID_OPTIONS, msg));
        }
    }
    Ok(())
}

/// The environment of a pipeline that reads only the command's variables (an update's, the
/// `whenMatched` pipeline of `$merge`).
pub(crate) struct VarsEnv(Document, teseladb_exec::collation::Collation);

impl VarsEnv {
    /// The environment of the variables `vars`, comparing strings in binary order.
    pub(crate) fn new(vars: Document) -> VarsEnv {
        VarsEnv(vars, teseladb_exec::collation::Collation::default())
    }
}

impl teseladb_exec::pipeline::Env for VarsEnv {
    fn collation(&self) -> teseladb_exec::collation::Collation {
        self.1.clone()
    }

    fn ext(&self) -> &dyn Extensions {
        &NodeExtensions
    }

    fn vars(&self) -> Option<&Document> {
        Some(&self.0)
    }

    fn collection(&self, _name: &str) -> Result<Vec<Document>> {
        Ok(Vec::new())
    }

    fn random(&self) -> f64 {
        NodeExtensions.random()
    }

    fn js(&self) -> Option<std::rc::Rc<dyn teseladb_exec::expr::js::JsEngine>> {
        Some(super::js::engine())
    }
}
