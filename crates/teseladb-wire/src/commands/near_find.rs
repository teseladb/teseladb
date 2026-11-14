//! `$near` and `$nearSphere` in a `find` (009 FR-011, T035): the read becomes a `$geoNear` stage
//! on the operator's field, with the rest of the filter as its query, followed by the command's
//! sort, skip, limit and projection, so the documents come nearest first through the collection's
//! `2d` or `2dsphere` index. Without an index that answers the point, the error is the planner's
//! of the reference router 9.0.4 (measured on `lc12`): `NoQueryExecutionPlans` naming the `GEONEAR`
//! node of the query tree, its field, its `$maxDistance` and whether it is a legacy `$nearSphere`.

use teseladb_bson::{Document, Value};

use crate::error::{CommandError, INDEX_NOT_FOUND, NO_QUERY_EXECUTION_PLANS};

const NEAR: &str = "$near";
const NEAR_SPHERE: &str = "$nearSphere";
const GEOMETRY: &str = "$geometry";
const MAX_DISTANCE: &str = "$maxDistance";
const MIN_DISTANCE: &str = "$minDistance";
const AND: &str = "$and";
/// The options of `find` that carry over to the stages after `$geoNear`.
const CARRIED: [&str; 4] = ["sort", "skip", "limit", "projection"];

/// A `find` whose filter asks for the documents near a point.
#[derive(Clone, Debug, PartialEq)]
pub struct NearFind {
    /// The field of the operator.
    pub field: String,
    /// The `$geoNear` stage the read becomes.
    pub stage: Document,
    /// The rest of the filter.
    pub rest: Document,
    /// `$maxDistance`, as the planner shows it (the largest double without one).
    pub max: f64,
    /// A `$nearSphere` on a legacy pair: the planner's `isNearSphere` (measured: one with
    /// `$geometry` shows 0).
    pub legacy_sphere: bool,
}

/// The operator document of a `$near` or `$nearSphere`, if `v` is one.
fn near_of(v: &Value) -> Option<(&str, &Document)> {
    let Value::Document(d) = v else { return None };
    match d.iter().next() {
        Some((k @ (NEAR | NEAR_SPHERE), _)) => Some((k, d)),
        _ => None,
    }
}

/// The fields of `d` that `keep` takes, in their order.
fn kept(d: &Document, keep: impl Fn(&str) -> bool) -> Document {
    let mut out = Document::new();
    for (k, v) in d.iter().filter(|(k, _)| keep(k)) {
        out.push(k, v.clone());
    }
    out
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(x) => Some(*x),
        Value::Decimal128(d) => Some(teseladb_mql::decimal_to_f64(*d)),
        _ => None,
    }
}

/// The `$near` or `$nearSphere` of `filter` (at the top or in a top-level `$and`, the only
/// places the parser lets it be), split from the rest of the filter.
pub fn split(filter: &Document) -> Option<NearFind> {
    for (field, v) in filter.iter() {
        if let Some((op, d)) = near_of(v) {
            return Some(build(field, op, d, kept(filter, |k| k != field)));
        }
    }
    let Some(Value::Array(clauses)) = filter.get(AND) else { return None };
    for (i, clause) in clauses.iter().enumerate() {
        let Value::Document(c) = clause else { continue };
        let Some((field, (op, d))) = c.iter().find_map(|(k, v)| near_of(v).map(|n| (k, n))) else {
            continue;
        };
        let mut left: Vec<Value> = clauses.clone();
        let own = kept(c, |k| k != field);
        if own.is_empty() {
            left.remove(i);
        } else {
            left[i] = Value::Document(own);
        }
        let mut rest = kept(filter, |k| k != AND);
        if !left.is_empty() {
            rest.push(AND, Value::Array(left));
        }
        return Some(build(field, op, d, rest));
    }
    None
}

fn build(field: &str, op: &str, d: &Document, rest: Document) -> NearFind {
    let arg = d.get(op).cloned().unwrap_or(Value::Null);
    // A GeoJSON point keeps its distances inside the operator; a legacy pair beside it.
    let (point, distances, legacy) = match &arg {
        Value::Document(inner) if inner.get(GEOMETRY).is_some() => {
            (inner.get(GEOMETRY).cloned().unwrap_or(Value::Null), inner.clone(), false)
        }
        _ => (arg.clone(), d.clone(), true),
    };
    let mut stage = Document::new()
        .with("near", point)
        .with("key", field)
        .with("spherical", Value::Boolean(op == NEAR_SPHERE));
    let max = distances.get(MAX_DISTANCE).cloned();
    if let Some(m) = &max {
        stage.push("maxDistance", m.clone());
    }
    if let Some(m) = distances.get(MIN_DISTANCE) {
        stage.push("minDistance", m.clone());
    }
    if !rest.is_empty() {
        stage.push("query", rest.clone());
    }
    NearFind {
        field: field.to_owned(),
        stage,
        rest,
        max: max.as_ref().and_then(number).unwrap_or(f64::MAX),
        legacy_sphere: legacy && op == NEAR_SPHERE,
    }
}

impl NearFind {
    /// The stages of the read: `$geoNear`, then the command's sort, skip, limit and projection.
    pub fn stages(&self, cmd: &Document, ns_text: &str) -> Result<Vec<Document>, CommandError> {
        let carried = kept(cmd, |k| CARRIED.contains(&k));
        let mut stages = vec![Document::new().with("$geoNear", self.stage.clone())];
        stages.extend(super::views::find_stages(&carried, ns_text)?);
        Ok(stages)
    }

    /// The query tree of the planner's message: the rest of the filter, then the `GEONEAR`
    /// node.
    fn tree(&self, rest_tree: &str) -> String {
        let node = format!(
            "GEONEAR  field={} maxdist={} isNearSphere={}",
            self.field,
            teseladb_mql::stream_double(self.max),
            u8::from(self.legacy_sphere)
        );
        if self.rest.is_empty() {
            return node;
        }
        let children = match rest_tree.strip_prefix("$and\n") {
            Some(inner) => inner.to_owned(),
            None => format!("    {rest_tree}"),
        };
        format!("$and\n{children}\n    {node}")
    }

    /// The error of a read no index answers, as the planner of the reference router 9.0.4 gives it;
    /// any other error as it is.
    pub fn planner_error(
        &self,
        e: CommandError,
        ns_text: &str,
        rest_tree: &str,
        (sort, projection): (&Value, &Value),
    ) -> CommandError {
        if e.code.code != NO_QUERY_EXECUTION_PLANS.code && e.code.code != INDEX_NOT_FOUND.code {
            return e;
        }
        CommandError::new(
            NO_QUERY_EXECUTION_PLANS,
            format!(
                "error enumerating plans for query: ns={ns_text}Tree: {}\nSort: {}\nProj: {}\n \
                 planner returned error :: caused by :: unable to find index for $geoNear query",
                self.tree(rest_tree),
                super::display(sort),
                super::display(projection),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(json: &str) -> Document {
        teseladb_bson::extjson::parse(json).unwrap()
    }

    #[test]
    fn a_legacy_near_carries_its_distances_and_the_rest_of_the_filter() {
        let n = split(&d(r#"{"x": 1, "q": {"$near": [0, 0], "$maxDistance": 3}}"#)).unwrap();
        assert_eq!(n.field, "q");
        assert_eq!(n.rest, d(r#"{"x": 1}"#));
        assert_eq!(
            n.stage,
            d(
                r#"{"near": [0, 0], "key": "q", "spherical": false, "maxDistance": 3, "query": {"x": 1}}"#
            )
        );
        assert_eq!(n.max, 3.0);
        assert!(!n.legacy_sphere);
    }

    #[test]
    fn a_geojson_near_sphere_keeps_its_distances_inside() {
        let filter = r#"{"p": {"$nearSphere": {"$geometry": {"type": "Point", "coordinates": [1, 1]},
            "$minDistance": 1000}}}"#;
        let n = split(&d(filter)).unwrap();
        assert_eq!(
            n.stage,
            d(r#"{"near": {"type": "Point", "coordinates": [1, 1]}, "key": "p", "spherical": true,
                "minDistance": 1000}"#)
        );
        assert_eq!(n.max, f64::MAX);
        assert!(!n.legacy_sphere, "measured: isNearSphere=0 with $geometry");
        let legacy = split(&d(r#"{"p": {"$nearSphere": [1, 1]}}"#)).unwrap();
        assert!(legacy.legacy_sphere);
    }

    #[test]
    fn a_near_inside_a_top_level_and_leaves_the_other_clauses() {
        let filter = r#"{"$and": [{"a": 1}, {"p": {"$near": [1, 1]}, "b": 2}], "c": 3}"#;
        let n = split(&d(filter)).unwrap();
        assert_eq!(n.rest, d(r#"{"c": 3, "$and": [{"a": 1}, {"b": 2}]}"#));
        let alone = split(&d(r#"{"$and": [{"p": {"$near": [1, 1]}}]}"#)).unwrap();
        assert!(alone.rest.is_empty());
        assert!(split(&d(r#"{"$and": [{"a": 1}, 5]}"#)).is_none());
        assert!(split(&d(r#"{"a": {"$gt": 1}}"#)).is_none());
    }

    #[test]
    fn the_planner_error_names_the_geonear_node() {
        let n = split(&d(r#"{"p": {"$near": [1, 1]}}"#)).unwrap();
        let e = CommandError::new(INDEX_NOT_FOUND, "none");
        let sort = Value::Document(d(r#"{"_id": 1}"#));
        let empty = Value::Document(Document::new());
        let e = n.planner_error(e, "db.c", "", (&sort, &empty));
        assert_eq!(e.code.code, NO_QUERY_EXECUTION_PLANS.code);
        assert_eq!(
            e.errmsg,
            "error enumerating plans for query: ns=db.cTree: GEONEAR  field=p maxdist=1.79769e+308 \
             isNearSphere=0\nSort: { _id: 1 }\nProj: {}\n planner returned error :: caused by :: \
             unable to find index for $geoNear query"
        );
        let n = split(&d(r#"{"x": 1, "p": {"$near": [1, 1]}}"#)).unwrap();
        let e = n.planner_error(
            CommandError::new(NO_QUERY_EXECUTION_PLANS, "x"),
            "db.c",
            "x $eq 1",
            (&empty, &empty),
        );
        assert!(
            e.errmsg.contains("Tree: $and\n    x $eq 1\n    GEONEAR  field=p "),
            "{}",
            e.errmsg
        );
        let other = CommandError::new(crate::error::BAD_VALUE, "kept");
        assert_eq!(n.planner_error(other, "db.c", "", (&empty, &empty)).errmsg, "kept");
    }
}
