//! The operator documents of the geospatial query operators, read as a `refrouter` 9.0.4 reads them
//! (measured): which fields go with `$geoWithin`, `$geoIntersects`, `$near` and `$nearSphere`, and
//! their distances. The shapes themselves are 009's (`GeoQuery::spec`).

use teseladb_bson::{RawDoc, RawValue};

use crate::error::{ErrorCode, MqlError, Result, bad_value, codes};
use crate::fmt::shell;
use crate::ops;
use crate::parse::GeoQuery;
use crate::value::{is_number, to_f64};

/// The shapes of `$geoWithin`; `$geoIntersects` only takes `$geometry`.
const WITHIN_SHAPES: [&str; 5] =
    [ops::GEOMETRY, ops::BOX, ops::POLYGON, ops::CENTER, ops::CENTER_SPHERE];

/// Whether `key` is a geospatial query operator.
pub(crate) fn is_geo(key: &str) -> bool {
    matches!(key, ops::GEO_WITHIN | ops::GEO_INTERSECTS | ops::NEAR | ops::NEAR_SPHERE)
}

/// Whether `key` is `$near` or `$nearSphere`.
pub(crate) fn is_near(key: &str) -> bool {
    matches!(key, ops::NEAR | ops::NEAR_SPHERE)
}

/// A document as RefDb writes it inside a message: `{ a: 1 }`.
fn object(d: RawDoc<'_>) -> String {
    shell(RawValue::Document(d))
}

fn query(key: &str, value: RawValue<'_>, near: bool) -> Result<GeoQuery> {
    Ok(GeoQuery {
        operator: key.to_owned(),
        spec: crate::value::OwnedValue::from_raw(value)?,
        near,
        max_distance: None,
        min_distance: None,
    })
}

/// `$geoWithin` or `$geoIntersects` (`key`, with `value`) in the operator document `doc`, where
/// it can have no other field.
pub(crate) fn within(doc: RawDoc<'_>, key: &str, value: RawValue<'_>) -> Result<GeoQuery> {
    if let Some((k, v)) = doc.iter().nth(1) {
        return Err(bad_value(format!("can't parse extra field: {k}: {}", shell(v))));
    }
    let RawValue::Document(d) = value else {
        return Err(bad_value("geometry must be an object"));
    };
    let mut fields = d.iter();
    let Some((first, arg)) = fields.next() else {
        return Err(bad_value("geo query doesn't have any geometry"));
    };
    let shapes: &[&str] = if key == ops::GEO_WITHIN { &WITHIN_SHAPES } else { &[ops::GEOMETRY] };
    let typed = match first {
        ops::GEOMETRY => matches!(arg, RawValue::Document(_)),
        _ => matches!(arg, RawValue::Array(_) | RawValue::Document(_)),
    };
    if !shapes.contains(&first) || !typed {
        return Err(bad_value(format!("unknown geo specifier: {first}: {}", shell(arg))));
    }
    if let Some((k, v)) = fields.next() {
        let msg = if key == ops::GEO_WITHIN {
            format!("$geoWithin doesn't accept multiple shapes {key}: {}", shell(value))
        } else {
            format!("unknown geo specifier: {k}: {}", shell(v))
        };
        return Err(bad_value(msg));
    }
    query(key, value, false)
}

/// The two codes of a legacy distance: not a number, negative.
fn legacy_codes(key: &str) -> (ErrorCode, ErrorCode) {
    if key == ops::MIN_DISTANCE {
        (codes::NEAR_MIN_NOT_NUMBER, codes::NEAR_MIN_NEGATIVE)
    } else {
        (codes::NEAR_MAX_NOT_NUMBER, codes::NEAR_MAX_NEGATIVE)
    }
}

/// A distance `$maxDistance` or `$minDistance` (`key`): the legacy form has codes of its own, the
/// GeoJSON form answers `BadValue`.
fn distance(key: &str, v: RawValue<'_>, legacy: bool) -> Result<f64> {
    let (not_number, negative) = legacy_codes(key);
    let err = |code: ErrorCode, why: &str| {
        let message = format!("{key} {why}");
        if legacy { MqlError::new(code, message) } else { bad_value(message) }
    };
    let d =
        to_f64(&v).filter(|_| is_number(&v)).ok_or_else(|| err(not_number, "must be a number"))?;
    if d < 0.0 {
        return Err(err(negative, "must be non-negative"));
    }
    Ok(d)
}

/// Whether a legacy point starts with two numbers (`[x, y]` or `{x: …, y: …}`).
fn legacy_point(v: RawValue<'_>) -> bool {
    let fields = match v {
        RawValue::Array(a) => a.as_doc(),
        RawValue::Document(d) => d,
        _ => return false,
    };
    let numbers = fields.iter().take(2).filter(|(_, v)| is_number(v)).count();
    numbers == 2
}

/// `$near` or `$nearSphere` (`key`, with `value`) in the operator document `doc`, where it comes
/// first, with its distances beside it (a legacy point) or inside it (a GeoJSON point).
pub(crate) fn near(doc: RawDoc<'_>, key: &str, value: RawValue<'_>) -> Result<GeoQuery> {
    let first = doc.iter().next().map(|(k, _)| k);
    if let Some(k @ (ops::MAX_DISTANCE | ops::MIN_DISTANCE)) = first {
        return Err(bad_value(format!("unknown operator: {k}")));
    }
    if first != Some(key) || !matches!(value, RawValue::Array(_) | RawValue::Document(_)) {
        return Err(bad_value(format!("near must be first in: {}", object(doc))));
    }
    let mut g = query(key, value, true)?;
    let geo_json = match value {
        RawValue::Document(d) => d.get(ops::GEOMETRY).map(|geometry| (d, geometry)),
        _ => None,
    };
    if let Some((inner, geometry)) = geo_json {
        if !matches!(geometry, RawValue::Document(_)) {
            return Err(bad_value("$geometry is required for geo near query"));
        }
        for (k, v) in inner.iter().filter(|(k, _)| *k != ops::GEOMETRY) {
            set_distance(&mut g, k, v, false)?;
        }
        if let Some((k, v)) = doc.iter().nth(1) {
            return Err(bad_value(format!(
                "geo near accepts just one argument when querying for a GeoJSON point. Extra field found: {k}: {}",
                shell(v)
            )));
        }
        return Ok(g);
    }
    if !legacy_point(value) {
        let shown = match value {
            RawValue::Array(a) => object(a.as_doc()),
            v => shell(v),
        };
        return Err(bad_value(format!("invalid point provided to geo near query: {shown}")));
    }
    for (k, v) in doc.iter().skip(1) {
        set_distance(&mut g, k, v, true)?;
    }
    Ok(g)
}

fn set_distance(g: &mut GeoQuery, key: &str, v: RawValue<'_>, legacy: bool) -> Result<()> {
    match key {
        ops::MAX_DISTANCE => g.max_distance = Some(distance(key, v, legacy)?),
        ops::MIN_DISTANCE => g.min_distance = Some(distance(key, v, legacy)?),
        _ => {
            let message = format!("invalid argument in geo near query: {key}");
            return Err(if legacy {
                MqlError::new(codes::NEAR_INVALID_ARGUMENT, message)
            } else {
                bad_value(message)
            });
        }
    }
    Ok(())
}
