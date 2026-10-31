//! Integration points that other specifications complete: the expression engine of 010 (`$expr`,
//! projections with expressions), JavaScript of 016 (`$where`), text search of 011 (`$text`,
//! `$meta`), the geometry of 009 (geospatial operators), the pipelines of updates (010) and the
//! random source of `$sampleRate`.

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use crate::error::{MqlError, Result, codes};
use crate::parse::{GeoQuery, TextQuery};

/// What the matcher and the projection ask of the layers above.
pub trait Extensions {
    /// Evaluates `$expr` on `doc` with the command's `let` variables (010).
    fn expr(&self, expr: RawValue<'_>, doc: RawDoc<'_>, vars: Option<&Document>) -> Result<bool>;

    /// Evaluates the JavaScript of `$where` on `doc` (016).
    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool>;

    /// Whether `doc` matches the text search (011).
    fn text(&self, query: &TextQuery, doc: RawDoc<'_>) -> Result<bool>;

    /// Whether `value` matches the geospatial operator (009).
    fn geo(&self, query: &GeoQuery, value: RawValue<'_>) -> Result<bool>;

    /// A uniform number in `[0, 1)` for `$sampleRate`.
    fn random(&self) -> f64;

    /// The value of `{$meta: name}` for `doc` (`textScore` of 011, `searchScore` and the others
    /// of 010 and 011), `None` when the query has no such value.
    fn meta(&self, name: &str, doc: RawDoc<'_>) -> Result<Option<Value>>;

    /// Evaluates an aggregation expression on `doc` with the command's `let` variables (010): a
    /// projection's, or a `let` value's with an empty document; `None` when it gives nothing (a
    /// missing field), which a projection leaves out.
    fn eval(
        &self,
        expr: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>>;

    /// Runs the aggregation pipeline of a pipeline-style update on `doc` with the command's `let`
    /// variables (010).
    fn update_pipeline(
        &self,
        pipeline: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document>;
}

/// The extensions of a node without the engines of 009, 010, 011 and 016: each operator that
/// needs them fails with the error RefDb gives when the engine has nothing to run it.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoExtensions;

/// The error of an operator whose engine is not linked.
fn missing(what: &str) -> MqlError {
    MqlError::new(codes::INTERNAL_ERROR, format!("{what} needs an engine this node does not have"))
}

impl Extensions for NoExtensions {
    fn expr(&self, _: RawValue<'_>, _: RawDoc<'_>, _: Option<&Document>) -> Result<bool> {
        Err(missing("$expr"))
    }

    fn where_js(&self, _: RawValue<'_>, _: RawDoc<'_>) -> Result<bool> {
        Err(missing("$where"))
    }

    fn text(&self, _: &TextQuery, _: RawDoc<'_>) -> Result<bool> {
        Err(MqlError::new(codes::INDEX_NOT_FOUND, "text index required for $text query"))
    }

    fn geo(&self, query: &GeoQuery, _: RawValue<'_>) -> Result<bool> {
        if query.near {
            return Err(MqlError::new(
                codes::NO_QUERY_EXECUTION_PLANS,
                "error processing query: planner returned error :: caused by :: unable to find \
                 index for $geoNear query",
            ));
        }
        Err(missing(&query.operator))
    }

    fn random(&self) -> f64 {
        0.0
    }

    fn meta(&self, _: &str, _: RawDoc<'_>) -> Result<Option<Value>> {
        Ok(None)
    }

    fn eval(&self, _: RawValue<'_>, _: RawDoc<'_>, _: Option<&Document>) -> Result<Option<Value>> {
        Err(missing("an expression"))
    }

    fn update_pipeline(
        &self,
        _: RawValue<'_>,
        _: RawDoc<'_>,
        _: Option<&Document>,
    ) -> Result<Document> {
        Err(missing("a pipeline-style update"))
    }
}
