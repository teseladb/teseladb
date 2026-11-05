//! `$geoNear` (010 FR-004, with the geospatial indexes of 009), as a `refrouter` 9.0.4 runs it
//! (measured in `crates/teseladb-exec/tests/agg/geo_near.out`): the documents of the collection
//! that the `query` matches, nearest first, each with its distance to `near` through the
//! collection's 2d or 2dsphere index. A 2dsphere index measures on the sphere, in meters from a
//! GeoJSON point and in radians from a legacy pair; a 2d index measures on the plane, or in
//! radians with `spherical`.

use teseladb_index::geo::geojson::{self, Geometry};
use teseladb_index::geo::sphere::{Point, distance_to_edge};
use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, RawValue, Value};

use super::errors::{BAD_VALUE, INDEX_NOT_FOUND, TYPE_MISMATCH, error};
use super::stages::Match;
use super::{Env, shape_set};
use crate::expr::Ctx;
use crate::expr::arith::raw_type_name;
use crate::expr::field_path;

/// The Earth's radius in meters that RefDb measures with (`kRadiusOfEarthInMeters`).
const EARTH_RADIUS_METERS: f64 = 6_378_100.0;

/// The bounds of a 2d index without `min` and `max`.
const FLAT_BOUND: f64 = 180.0;

/// A location code.
macro_rules! loc {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const NOT_OBJECT: ErrorCode = loc!(10065);
const NEAR_MISSING: ErrorCode = loc!(5860400);
const NEAR_TYPE: ErrorCode = loc!(5860401);
const DISTANCE_FIELD: ErrorCode = loc!(16606);
const INCLUDE_LOCS: ErrorCode = loc!(16607);
const NOT_FIRST: ErrorCode = loc!(40603);
const FLAT_RANGE: ErrorCode = loc!(16433);
/// RefDb's `NoQueryExecutionPlans`.
const NO_PLANS: ErrorCode = ErrorCode { code: 291, name: "NoQueryExecutionPlans" };

/// A geospatial index of the collection: its field and whether it is a 2dsphere one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeoIndex {
    /// The indexed field.
    pub field: String,
    /// A 2dsphere index (a 2d one otherwise).
    pub sphere: bool,
}

/// The point to measure from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Near {
    /// A GeoJSON point: distances in meters.
    GeoJson(f64, f64),
    /// A legacy pair.
    Legacy(f64, f64),
}

/// A `$geoNear`.
#[derive(Clone, Debug)]
pub struct GeoNear {
    near: Near,
    distance_field: Option<String>,
    spherical: bool,
    max: Option<f64>,
    min: Option<f64>,
    query: Option<Match>,
    include_locs: Option<String>,
    multiplier: Option<f64>,
    key: Option<String>,
}

/// The error of `$geoNear` anywhere but first after the pipeline's `$match`es.
pub(crate) fn not_first() -> MqlError {
    error(
        NOT_FIRST,
        "$geoNear was not the first stage in the pipeline after optimization. Is optimization disabled or inhibited?",
    )
}

/// A non-negative number of `$geoNear`'s argument `name`.
fn distance(name: &str, v: RawValue<'_>) -> Result<f64, MqlError> {
    let x = geojson::number(v).ok_or_else(|| {
        error(TYPE_MISMATCH, format!("$geoNear requires ${name} to evaluate to a number"))
    })?;
    if x < 0.0 {
        return Err(error(BAD_VALUE, format!("{name} must be nonnegative")));
    }
    Ok(x)
}

/// The point of `near`: a GeoJSON point or a legacy pair.
fn near_point(v: RawValue<'_>) -> Result<Near, MqlError> {
    let wrong = || {
        error(
            NEAR_TYPE,
            "$geoNear requires near argument to be a GeoJSON object or a legacy point(array)",
        )
    };
    match v {
        RawValue::Array(_) => {
            let (x, y) = geojson::coordinates(Some(v)).map_err(|e| error(BAD_VALUE, e.0))?;
            Ok(Near::Legacy(x, y))
        }
        RawValue::Document(d) if d.get("type").is_some() => {
            let invalid = || {
                error(
                    BAD_VALUE,
                    format!("invalid point provided to geo near query: {}", teseladb_mql::shell(v)),
                )
            };
            if !matches!(d.get("type"), Some(RawValue::String("Point"))) {
                return Err(invalid());
            }
            let (lng, lat) = geojson::coordinates(d.get("coordinates")).map_err(|_| invalid())?;
            if !(-180.0..=180.0).contains(&lng) || !(-90.0..=90.0).contains(&lat) {
                return Err(invalid());
            }
            Ok(Near::GeoJson(lng, lat))
        }
        RawValue::Document(_) => {
            let (x, y) = geojson::coordinates(Some(v)).map_err(|e| error(BAD_VALUE, e.0))?;
            Ok(Near::Legacy(x, y))
        }
        _ => Err(wrong()),
    }
}

impl GeoNear {
    /// Reads a `$geoNear` (measured on a `refrouter` 9.0.4).
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<GeoNear, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(NOT_OBJECT, "invalid parameter: expected an object ($geoNear)"));
        };
        let mut g = GeoNear {
            near: Near::Legacy(0.0, 0.0),
            distance_field: None,
            spherical: false,
            max: None,
            min: None,
            query: None,
            include_locs: None,
            multiplier: None,
            key: None,
        };
        let mut near = None;
        for (k, x) in d.iter() {
            match k {
                "near" => near = Some(near_point(x)?),
                "distanceField" => match x {
                    RawValue::String(s) => g.distance_field = Some(s.to_owned()),
                    _ => {
                        return Err(error(
                            DISTANCE_FIELD,
                            "$geoNear requires that the 'distanceField' option is a String",
                        ));
                    }
                },
                "includeLocs" => match x {
                    RawValue::String(s) => g.include_locs = Some(s.to_owned()),
                    _ => {
                        return Err(error(
                            INCLUDE_LOCS,
                            "$geoNear requires that 'includeLocs' option is a String",
                        ));
                    }
                },
                "maxDistance" => g.max = Some(distance("maxDistance", x)?),
                "minDistance" => g.min = Some(distance("minDistance", x)?),
                "distanceMultiplier" => {
                    let m = geojson::number(x).ok_or_else(|| {
                        error(TYPE_MISMATCH, "distanceMultiplier must be a number")
                    })?;
                    if m < 0.0 {
                        return Err(error(BAD_VALUE, "distanceMultiplier must be nonnegative"));
                    }
                    g.multiplier = Some(m);
                }
                "spherical" => g.spherical = crate::expr::truthy(&Some(Value::from(x))),
                "query" => match x {
                    RawValue::Document(_) => g.query = Some(Match::parse(x, ctx)?),
                    _ => return Err(error(TYPE_MISMATCH, "query must be an object")),
                },
                "key" => match x {
                    RawValue::String("") => {
                        return Err(error(
                            BAD_VALUE,
                            "$geoNear parameter 'key' cannot be the empty string",
                        ));
                    }
                    RawValue::String(s) => g.key = Some(s.to_owned()),
                    other => {
                        return Err(error(
                            TYPE_MISMATCH,
                            format!(
                                "$geoNear parameter 'key' must be of type string but found type: {}",
                                raw_type_name(other)
                            ),
                        ));
                    }
                },
                other => {
                    return Err(error(BAD_VALUE, format!("Unknown argument to $geoNear: {other}")));
                }
            }
        }
        g.near = near.ok_or_else(|| error(NEAR_MISSING, "$geoNear requires a 'near' argument"))?;
        Ok(g)
    }

    /// Chooses the index the stage reads before it runs: the errors of planning (no index, none
    /// that answers `near`) come before any document, as RefDb's planner gives them.
    pub fn check_plan(&self, env: &dyn Env) -> Result<(), MqlError> {
        let (ns, indexes) = env.geo_indexes()?;
        self.index(&ns, &indexes).map(|_| ())
    }

    /// The documents of `docs` the query matches, nearest first, with their distances.
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let (ns, indexes) = env.geo_indexes()?;
        let index = self.index(&ns, &indexes)?;
        if let (false, Near::Legacy(x, y)) = (index.sphere, self.near)
            && ![x, y].iter().all(|c| (-FLAT_BOUND..FLAT_BOUND).contains(c))
        {
            let pair = Document::new().with("0", Value::Double(x)).with("1", Value::Double(y));
            let bytes = crate::raw::bytes_of(&pair)?;
            let raw = teseladb_bson::RawDoc::from_bytes(&bytes).map_err(crate::raw::bad)?;
            return Err(error(
                FLAT_RANGE,
                format!(
                    "point not in interval of [ -180, 180 ] :: caused by :: {}",
                    teseladb_mql::shell(RawValue::Document(raw))
                ),
            ));
        }
        let docs = match &self.query {
            Some(q) => q.run(docs, env)?,
            None => docs,
        };
        let mut found: Vec<(f64, Document, Value)> = Vec::new();
        for d in docs {
            let Some(loc) = field_path(&Value::Document(d.clone()), &index.field) else { continue };
            let Some(dist) = self.distance_to(&loc, index.sphere)? else { continue };
            if self.min.is_some_and(|m| dist < m) || self.max.is_some_and(|m| dist > m) {
                continue;
            }
            found.push((dist, d, loc));
        }
        found.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(found
            .into_iter()
            .map(|(dist, mut d, loc)| {
                if let Some(f) = &self.distance_field {
                    let segs: Vec<&str> = f.split('.').collect();
                    shape_set(&mut d, &segs, Value::Double(dist * self.multiplier.unwrap_or(1.0)));
                }
                if let Some(f) = &self.include_locs {
                    let segs: Vec<&str> = f.split('.').collect();
                    shape_set(&mut d, &segs, loc);
                }
                d
            })
            .collect())
    }

    /// The index the stage reads: the one of `key`, else the only 2d index, else the only
    /// 2dsphere one; it must answer the kind of `near` (a 2d index takes legacy pairs, a 2dsphere
    /// one a GeoJSON point or a pair with `spherical`).
    fn index<'a>(&self, ns: &str, indexes: &'a [GeoIndex]) -> Result<&'a GeoIndex, MqlError> {
        // With `key`, a collection without geo indexes fails in the planner (measured on a
        // `refrouter` 9.0.4 on 05-11-2026).
        if indexes.is_empty() && self.key.is_none() {
            return Err(error(
                INDEX_NOT_FOUND,
                "$geoNear requires a 2d or 2dsphere index, but none were found",
            ));
        }
        let chosen = match &self.key {
            Some(k) => indexes.iter().find(|i| i.field == *k),
            None => {
                let flat: Vec<&GeoIndex> = indexes.iter().filter(|i| !i.sphere).collect();
                let round: Vec<&GeoIndex> = indexes.iter().filter(|i| i.sphere).collect();
                match (flat.as_slice(), round.as_slice()) {
                    ([one], _) | ([], [one]) => Some(*one),
                    _ => {
                        return Err(error(
                            BAD_VALUE,
                            format!(
                                "There is more than one 2d or 2dsphere index on {ns}; unsure which to use for $geoNear"
                            ),
                        ));
                    }
                }
            }
        };
        let field =
            self.key.clone().or_else(|| chosen.map(|i| i.field.clone())).unwrap_or_default();
        let fits = chosen.filter(|i| match (i.sphere, self.near) {
            (true, Near::Legacy(..)) => self.spherical,
            (true, Near::GeoJson(..)) => true,
            (false, Near::Legacy(..)) => true,
            (false, Near::GeoJson(..)) => false,
        });
        fits.ok_or_else(|| {
            error(
                NO_PLANS,
                format!(
                    "error enumerating plans for query: ns={ns}Tree: GEONEAR  field={field} maxdist={} isNearSphere={}\nSort: {{}}\nProj: {{}}\nCollation: {{ locale: \"simple\" }}\n planner returned error :: caused by :: unable to find index for $geoNear query",
                    teseladb_mql::stream_double(self.max.unwrap_or(f64::MAX)),
                    u8::from(self.spherical)
                ),
            )
        })
    }

    /// The distance from `near` to location `loc` through an index of the sphere or the plane;
    /// `None` for a value that is no location.
    fn distance_to(&self, loc: &Value, sphere: bool) -> Result<Option<f64>, MqlError> {
        let holder = Document::new().with("v", loc.clone());
        let bytes = crate::raw::bytes_of(&holder)?;
        let raw = teseladb_bson::RawDoc::from_bytes(&bytes).map_err(crate::raw::bad)?;
        let Some(v) = raw.get("v") else { return Ok(None) };
        let (nx, ny) = match self.near {
            Near::GeoJson(x, y) | Near::Legacy(x, y) => (x, y),
        };
        if !sphere {
            let Ok((x, y)) = geojson::coordinates(Some(v)) else { return Ok(None) };
            return Ok(Some(match self.spherical {
                true => sphere_degrees((nx, ny), (x, y)),
                false => (x - nx).hypot(y - ny),
            }));
        }
        let Ok(g) = geojson::parse_indexed(v) else { return Ok(None) };
        let meters = to_geometry(Point::from_degrees(nx, ny), &g) * EARTH_RADIUS_METERS;
        Ok(Some(match self.near {
            Near::GeoJson(..) => meters,
            Near::Legacy(..) => meters / EARTH_RADIUS_METERS,
        }))
    }
}

/// The angle in radians between two points given in degrees, as RefDb gives it: the angle of
/// the sphere's points through meters and back (`(angle * R) / R`).
fn sphere_degrees((x1, y1): (f64, f64), (x2, y2): (f64, f64)) -> f64 {
    let angle = Point::from_degrees(x1, y1).angle(Point::from_degrees(x2, y2));
    angle * EARTH_RADIUS_METERS / EARTH_RADIUS_METERS
}

/// The angle in radians from `p` to the nearest point of `g`.
fn to_geometry(p: Point, g: &Geometry) -> f64 {
    let to_line = |line: &[Point]| -> f64 {
        match line {
            [one] => p.angle(*one),
            _ => line
                .windows(2)
                .map(|w| distance_to_edge(p, w[0], w[1]))
                .fold(f64::INFINITY, f64::min),
        }
    };
    let to_polygon = |poly: &teseladb_index::geo::sphere::Polygon| -> f64 {
        match poly.contains(p) {
            true => 0.0,
            false => {
                poly.edges().map(|(a, b)| distance_to_edge(p, a, b)).fold(f64::INFINITY, f64::min)
            }
        }
    };
    match g {
        Geometry::Point(q) => p.angle(*q),
        Geometry::LineString(line) => to_line(line),
        Geometry::Polygon(poly) | Geometry::BigPolygon(poly) => to_polygon(poly),
        Geometry::MultiPoint(points) => {
            points.iter().map(|q| p.angle(*q)).fold(f64::INFINITY, f64::min)
        }
        Geometry::MultiLineString(lines) => {
            lines.iter().map(|l| to_line(l)).fold(f64::INFINITY, f64::min)
        }
        Geometry::MultiPolygon(polys) => polys.iter().map(to_polygon).fold(f64::INFINITY, f64::min),
        Geometry::Collection(items) => {
            items.iter().map(|g| to_geometry(p, g)).fold(f64::INFINITY, f64::min)
        }
    }
}
