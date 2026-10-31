//! The extensions of the matcher a node runs (contract of `teseladb-mql`): `$geoWithin` and
//! `$geoIntersects` on documents without an index come from 009 (FR-012, T022); the operators of
//! the engines this node does not link yet answer as [`NoExtensions`] does; `$expr` and the
//! expressions of projections and `let` come from 010's engine (`teseladb_exec::expr`).

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::expr::{Expr, Scope};
use teseladb_index::geo::query::{GeoOperator, matches, parse_query};
use teseladb_mql::{Extensions, GeoQuery, MqlError, NoExtensions, Result, TextQuery, codes};

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
        // Every error of a shape is a `BadValue` (`teseladb_index::geo::query::BAD_VALUE`).
        let shape = parse_query(op, query.spec.get())
            .map_err(|e| MqlError::new(codes::BAD_VALUE, e.message))?;
        Ok(matches(&shape, value))
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

    fn update_pipeline(
        &self,
        pipeline: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document> {
        NoExtensions.update_pipeline(pipeline, doc, vars)
    }
}
