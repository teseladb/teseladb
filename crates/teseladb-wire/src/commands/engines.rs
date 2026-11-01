//! The extensions of the matcher a node runs (contract of `teseladb-mql`): `$geoWithin` and
//! `$geoIntersects` on documents without an index come from 009 (FR-012, T022); the operators of
//! the engines this node does not link yet answer as [`NoExtensions`] does; `$expr` and the
//! expressions of projections and `let` come from 010's engine (`teseladb_exec::expr`).

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::expr::{Expr, Scope};
use teseladb_index::geo::query::{GeoOperator, matches, parse_query};
use teseladb_mql::{Extensions, GeoQuery, MqlError, NoExtensions, Result, TextQuery, codes};

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

/// The node's extensions.
#[derive(Clone, Copy, Debug, Default)]
pub struct NodeExtensions;

impl Extensions for NodeExtensions {
    fn expr(&self, e: RawValue<'_>, doc: RawDoc<'_>, vars: Option<&Document>) -> Result<bool> {
        let (expr, d) = (Expr::parse(e)?, Document::from(doc));
        let v = expr.eval(&d, &Scope::new(d.clone(), vars))?;
        Ok(teseladb_exec::expr::truthy(&v))
    }

    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool> {
        NoExtensions.where_js(code, doc)
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
        NoExtensions.random()
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
        expr.eval(&d, &Scope::new(d.clone(), vars))
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
        expr.eval(&d, &Scope::new(d.clone(), vars))
    }

    fn update_pipeline(
        &self,
        pipeline: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document> {
        let stages: Vec<RawValue<'_>> = match pipeline {
            RawValue::Array(a) => a.iter().collect(),
            _ => Vec::new(),
        };
        refuse_outside_update(&stages)?;
        let names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
        let ctx = teseladb_exec::expr::Ctx::checked(names);
        let p = teseladb_exec::pipeline::Pipeline::parse(&stages, &ctx)?;
        let env = VarsEnv(vars.cloned().unwrap_or_default());
        let d = Document::from(doc);
        // An empty pipeline leaves the document as it is.
        Ok(p.run(vec![d.clone()], &env)?.into_iter().next().unwrap_or(d))
    }
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
pub(crate) struct VarsEnv(pub(crate) Document);

impl teseladb_exec::pipeline::Env for VarsEnv {
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
        0.0
    }
}
