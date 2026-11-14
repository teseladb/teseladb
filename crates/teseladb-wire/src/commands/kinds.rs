//! The special kinds of collection in `create` and the catalog commands (015 FR-001 to FR-003,
//! FR-006, FR-009, FR-010): views, capped collections, time series and collections clustered by
//! `_id`. `create` checks their options and stores them as a `refrouter` 9.0.4 stores them
//! (measured 31-10-2026, `docs/` of 015, research R1): numbers truncated or rounded the same way,
//! the defaults of a time series written out, and the options in the order `listCollections`
//! shows them. A view's definition is checked against the other views of its database
//! (`teseladb-plan`), and the first view of a database brings its `system.views`, which lists
//! the definitions.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::collection_types::{CollectionKind, Granularity};
use teseladb_plan::views::{self, Definition};

use super::names::full;
use super::{Backend, display};
use crate::error::{
    BAD_VALUE, CANNOT_CREATE_INDEX, CLUSTERED_CAPPED_SIZE, CLUSTERED_CAPPED_TEST, CLUSTERED_LEGACY,
    CLUSTERED_ON_VIEW, CLUSTERED_UNIQUE, CLUSTERED_VERSION, COMMAND_NOT_SUPPORTED_ON_VIEW,
    CommandError, IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, INVALID_INDEX_SPECIFICATION_OPTION,
    INVALID_OPTIONS, NAMESPACE_EXISTS,
};
use crate::types::{wrong_type, wrong_types};

/// The collection of a database that lists its views.
pub const SYSTEM_VIEWS: &str = "system.views";

/// Options of the kinds, each named once.
pub(crate) const VIEW_ON: &str = "viewOn";
pub(crate) const PIPELINE: &str = "pipeline";
const COLLATION: &str = "collation";
const CAPPED: &str = "capped";
const SIZE: &str = "size";
/// The option of a capped collection's size, for `collMod`.
pub(crate) const CAPPED_SIZE_OPTION: &str = SIZE;
const MAX: &str = "max";
/// The option of a capped collection's most documents, for `collMod`.
pub(crate) const CAPPED_MAX_OPTION: &str = MAX;
pub(crate) const TIMESERIES: &str = "timeseries";
pub(crate) const TIME_FIELD: &str = "timeField";
pub(crate) const META_FIELD: &str = "metaField";
pub(crate) const GRANULARITY: &str = "granularity";
pub(crate) const MAX_SPAN: &str = "bucketMaxSpanSeconds";
pub(crate) const ROUNDING: &str = "bucketRoundingSeconds";
pub(crate) const FIXED_BUCKETING: &str = "fixedBucketing";
pub(crate) const EXPIRE: &str = "expireAfterSeconds";
const CLUSTERED: &str = "clusteredIndex";
const ID_INDEX: &str = "idIndex";
const KEY: &str = "key";
const UNIQUE: &str = "unique";
const NAME: &str = "name";
const VERSION: &str = "v";
const CHANGE_STREAM_IMAGES: &str = "changeStreamPreAndPostImages";

/// The options a time series refuses, in the order RefDb checks them.
const NOT_WITH_TIMESERIES: [&str; 8] = [
    VIEW_ON,
    CAPPED,
    CLUSTERED,
    "validator",
    "validationLevel",
    "validationAction",
    CHANGE_STREAM_IMAGES,
    ID_INDEX,
];
/// The fields of `timeseries`.
const TIMESERIES_FIELDS: [&str; 5] = [TIME_FIELD, META_FIELD, GRANULARITY, MAX_SPAN, ROUNDING];
/// The fields of `clusteredIndex`.
const CLUSTERED_FIELDS: [&str; 4] = [KEY, UNIQUE, NAME, VERSION];
/// The numeric types of `size`, `max`, `expireAfterSeconds` and the bucketing spans.
pub(crate) const NUMBER: [&str; 4] = ["double", "int", "long", "decimal"];
/// The types of `capped`.
const CAPPED_TYPES: [&str; 5] = ["double", "decimal", "bool", "int", "long"];
/// Largest `size` (1024⁵ bytes).
const MAX_SIZE: i128 = 1 << 50;
/// `max` is below this.
const MAX_DOCS: i128 = 1 << 31;
/// Longest bucket span of a time series (a year in seconds).
pub(crate) const MAX_BUCKET_SPAN: i128 = 31_536_000;
/// The version of a clustered index.
const CLUSTERED_INDEX_VERSION: i32 = 2;
/// The name of a clustered index by default.
const ID_INDEX_NAME: &str = "_id_";
/// The words before the errors of a time series' options.
const TIMESERIES_FAILED: &str = "Failed to create timeseries collection :: caused by :: ";
/// The error of `expireAfterSeconds` outside time series and clustered collections.
const EXPIRE_NOT_SUPPORTED: &str = "'expireAfterSeconds' is only supported on time-series collections or when the 'clusteredIndex' option is specified";

/// The kind `create` was asked for, checked.
#[derive(Clone, Debug, PartialEq)]
pub enum Asked {
    /// A collection without special options.
    Normal,
    /// A view.
    View {
        /// `viewOn`.
        view_on: String,
        /// `pipeline`.
        pipeline: Vec<Document>,
    },
    /// A capped collection.
    Capped {
        /// `size`.
        size: i64,
        /// `max`.
        max: Option<i32>,
    },
    /// A time series: its `timeseries` as stored and `expireAfterSeconds`.
    TimeSeries {
        /// `timeseries`, complete.
        options: Document,
        /// `expireAfterSeconds`.
        expire: Option<i64>,
    },
    /// A clustered collection: its `clusteredIndex` as stored and `expireAfterSeconds`.
    Clustered {
        /// `clusteredIndex`, complete.
        index: Document,
        /// `expireAfterSeconds`.
        expire: Option<i64>,
    },
}

/// An integral value of a numeric option as RefDb reads it: a `double` truncated, a `decimal`
/// rounded half to even.
pub(crate) fn integral(v: &Value) -> Option<i128> {
    // `as` saturates at the bounds of `i64` and makes NaN 0, as RefDb reads these options
    // (measured: `expireAfterSeconds: 1e30` is 9223372036854775807, NaN is 0).
    match v {
        Value::Int32(n) => Some(i128::from(*n)),
        Value::Int64(n) => Some(i128::from(*n)),
        Value::Double(x) => Some(i128::from(x.trunc() as i64)),
        Value::Decimal128(d) => {
            let x = d.to_string().parse::<f64>().unwrap_or(f64::NAN);
            Some(i128::from(x.round_ties_even() as i64))
        }
        _ => None,
    }
}

/// The integral value of numeric option `field` of `create` (in `path`).
fn number(v: &Value, path: &str) -> Result<i128, CommandError> {
    integral(v).ok_or_else(|| wrong_types(path, v, &NUMBER))
}

pub(crate) fn invalid(msg: impl Into<String>) -> CommandError {
    CommandError::new(INVALID_OPTIONS, msg.into())
}

pub(crate) fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::new(BAD_VALUE, msg.into())
}

/// `BadValue` of a number below `min` or above `max`.
pub(crate) fn bounded(
    field: &str,
    n: i128,
    min: i128,
    max: i128,
    inclusive: bool,
) -> Result<(), CommandError> {
    if n < min {
        return Err(bad(format!(
            "BSON field '{field}' value must be >= {min}, actual value '{n}'"
        )));
    }
    let over = if inclusive { n > max } else { n >= max };
    if over {
        let op = if inclusive { "<=" } else { "<" };
        return Err(bad(format!(
            "BSON field '{field}' value must be {op} {max}, actual value '{n}'"
        )));
    }
    Ok(())
}

fn doc_of<'a>(o: &'a Document, field: &str) -> Result<Option<&'a Document>, CommandError> {
    match o.get(field) {
        None => Ok(None),
        Some(Value::Document(d)) => Ok(Some(d)),
        Some(v) => Err(wrong_type(&format!("create.{field}"), v, "object")),
    }
}

/// The tier option of `create`.
const STORAGE_TIER: &str = "storageTier";
/// The option that makes a collection the server's temporary one.
const TEMP: &str = "temp";

/// Checks the options of `create` of `ns` that make a special kind, in the order a `refrouter`
/// 9.0.4 checks them.
pub fn check(ns: &Namespace, o: &Document, now_secs: i64) -> Result<Asked, CommandError> {
    let view_on = match o.get(VIEW_ON) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => return Err(wrong_type("create.viewOn", v, "string")),
    };
    let pipeline = match o.get(PIPELINE) {
        None => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .enumerate()
            .map(|(i, s)| match s {
                Value::Document(d) => Ok(d.clone()),
                v => Err(wrong_type(&format!("create.pipeline.{i}"), v, "object")),
            })
            .collect::<Result<_, _>>()?,
        Some(v) => return Err(wrong_type("create.pipeline", v, "array")),
    };
    if let Some(v) = o.get(CAPPED)
        && !matches!(v, Value::Boolean(_))
        && integral(v).is_none()
    {
        return Err(wrong_types("create.capped", v, &CAPPED_TYPES));
    }
    for f in [SIZE, MAX, EXPIRE] {
        if let Some(v) = o.get(f) {
            number(v, &format!("create.{f}"))?;
        }
    }
    let timeseries = doc_of(o, TIMESERIES)?;
    // `storageTier` is an object; `temp` is the server's own (measured on a `refrouter` 9.0.4 on
    // 05-11-2026).
    doc_of(o, STORAGE_TIER)?;
    if o.get(TEMP).is_some() {
        return Err(CommandError::new(INVALID_OPTIONS, "the 'temp' field is an invalid option"));
    }
    let clustered = match o.get(CLUSTERED) {
        None => None,
        Some(Value::Boolean(_)) => {
            return Err(CommandError::new(
                CLUSTERED_LEGACY,
                "The 'clusteredIndex' legacy format {clusteredIndex: <bool>} is only supported for specific internal collections and vice versa",
            ));
        }
        Some(Value::Document(d)) => Some(d),
        Some(v) => return Err(wrong_type("create.clusteredIndex", v, "object")),
    };
    if o.get(PIPELINE).is_some() && view_on.is_none() {
        return Err(invalid("'pipeline' requires 'viewOn' to also be specified"));
    }
    if view_on.as_deref() == Some("") {
        return Err(bad("'viewOn' cannot be empty"));
    }
    if timeseries.is_some()
        && let Some(other) = NOT_WITH_TIMESERIES.iter().find(|f| o.get(f).is_some())
    {
        return Err(invalid(format!("{}: 'timeseries' is not allowed with '{other}'", full(ns))));
    }
    let capped = o.get(CAPPED).and_then(crate::types::truthy) == Some(true);
    if clustered.is_some() {
        if view_on.is_some() {
            return Err(CommandError::new(
                CLUSTERED_ON_VIEW,
                "The 'clusteredIndex' option is not supported with views",
            ));
        }
        if capped && o.get(SIZE).is_some() {
            return Err(CommandError::new(
                CLUSTERED_CAPPED_SIZE,
                "'size' field for capped collections is not allowed on clustered collections. Did you mean 'capped: true' with 'expireAfterSeconds'?",
            ));
        }
        if capped {
            return Err(CommandError::new(
                CLUSTERED_CAPPED_TEST,
                "Clustered capped collection only available with 'enableTestCommands' server parameter",
            ));
        }
    }
    if capped && o.get(SIZE).is_none() {
        return Err(invalid("specify size:<n> when capped is true"));
    }
    if !capped && (o.get(SIZE).is_some() || o.get(MAX).is_some()) {
        return Err(invalid(
            "the 'capped' field needs to be true when either the 'size' or 'max' fields are present",
        ));
    }
    if o.get(EXPIRE).is_some() && timeseries.is_none() && clustered.is_none() {
        return Err(invalid(EXPIRE_NOT_SUPPORTED));
    }
    if let Some(view_on) = view_on {
        return check_view(o, view_on, pipeline);
    }
    let expire = o.get(EXPIRE).map(|v| number(v, "create.expireAfterSeconds")).transpose()?;
    if let Some(ts) = timeseries {
        return check_timeseries(ns, ts, expire, now_secs);
    }
    if let Some(c) = clustered {
        return check_clustered(o, c, expire, now_secs);
    }
    if capped {
        return check_capped(o);
    }
    Ok(Asked::Normal)
}

fn check_view(
    o: &Document,
    view_on: String,
    pipeline: Vec<Document>,
) -> Result<Asked, CommandError> {
    if let Some(index) = o.get(ID_INDEX) {
        return Err(invalid(format!("'idIndex' is not allowed with 'viewOn': {}", display(index))));
    }
    if o.get(CHANGE_STREAM_IMAGES).is_some() {
        return Err(invalid(format!("option not supported on a view: {CHANGE_STREAM_IMAGES}")));
    }
    Ok(Asked::View { view_on, pipeline })
}

fn check_capped(o: &Document) -> Result<Asked, CommandError> {
    let size = o.get(SIZE).map(|v| number(v, "create.size")).transpose()?.unwrap_or(0);
    bounded(SIZE, size, 1, MAX_SIZE, true)?;
    let max = match o.get(MAX).map(|v| number(v, "create.max")).transpose()? {
        None => None,
        Some(n) if n <= 0 => Some(i32::MAX),
        Some(n) => {
            bounded(MAX, n, 1, MAX_DOCS, false)?;
            // Below 2³¹: the bound above.
            Some(n as i32)
        }
    };
    // At most 1024⁵: the bound above.
    let size = size as i64;
    Ok(Asked::Capped { size, max })
}

/// `expireAfterSeconds` of a time series or a clustered collection, checked: not negative, its
/// milliseconds within an `int64` and at most the seconds since 1970 at `now_secs` (measured on a
/// `refnode` 9.0.4 on 02-11-2026, also by `collMod`).
pub(crate) fn checked_expire(
    expire: Option<i128>,
    prefix: &str,
    now_secs: i64,
) -> Result<Option<i64>, CommandError> {
    let Some(n) = expire else { return Ok(None) };
    if n < 0 {
        return Err(invalid(format!(
            "{prefix}TTL index 'expireAfterSeconds' option cannot be less than 0"
        )));
    }
    // `integral` keeps every value within `i64`.
    let seconds = n as i64;
    if seconds.checked_mul(1000).is_none() {
        return Err(invalid(format!(
            "{prefix}TTL index 'expireAfterSeconds' option cannot overflow int64_t when cast as milliseconds, found {seconds}"
        )));
    }
    if seconds > now_secs {
        return Err(invalid(format!(
            "{prefix}TTL index 'expireAfterSeconds' option cannot exceed time since last epoch ({now_secs}s) found {seconds}"
        )));
    }
    Ok(Some(seconds))
}

fn ts_string(ts: &Document, field: &str) -> Result<Option<String>, CommandError> {
    match ts.get(field) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(v) => Err(wrong_type(&format!("create.timeseries.{field}"), v, "string")),
    }
}

fn check_timeseries(
    ns: &Namespace,
    ts: &Document,
    expire: Option<i128>,
    now_secs: i64,
) -> Result<Asked, CommandError> {
    if let Some((k, _)) = ts.iter().find(|(k, _)| !TIMESERIES_FIELDS.contains(k)) {
        return Err(CommandError::new(
            IDL_UNKNOWN_FIELD,
            format!("BSON field 'create.timeseries.{k}' is an unknown field."),
        ));
    }
    let time = ts_string(ts, TIME_FIELD)?.ok_or_else(|| {
        CommandError::new(
            IDL_FAILED_TO_PARSE,
            "BSON field 'create.timeseries.timeField' is missing but a required field",
        )
    })?;
    let meta = ts_string(ts, META_FIELD)?;
    let granularity = match ts_string(ts, GRANULARITY)? {
        None => None,
        Some(g) => Some(Granularity::of(&g).ok_or_else(|| {
            bad(format!(
                "Enumeration value '{g}' for field 'create.timeseries.granularity' is not a valid value."
            ))
        })?),
    };
    let span_of =
        |f: &str| ts.get(f).map(|v| number(v, &format!("create.timeseries.{f}"))).transpose();
    let (span, rounding) = (span_of(MAX_SPAN)?, span_of(ROUNDING)?);
    for (f, n) in [(MAX_SPAN, span), (ROUNDING, rounding)] {
        if let Some(n) = n {
            bounded(f, n, 1, MAX_BUCKET_SPAN, true)?;
        }
    }
    for field in std::iter::once(&time).chain(meta.as_ref()) {
        if field.starts_with('$') {
            return Err(bad("The 'timeField' or 'metaField' cannot start with '$'"));
        }
    }
    for (name, field) in [(TIME_FIELD, Some(&time)), (META_FIELD, meta.as_ref())] {
        if field.is_some_and(|f| f.contains('.')) {
            return Err(invalid(format!(
                "{}: '{name}' must be a top-level field and not contain a '.'",
                full(ns)
            )));
        }
    }
    if meta.as_deref() == Some(time.as_str()) {
        return Err(invalid("'metaField' cannot be the same as 'timeField'"));
    }
    if meta.as_deref() == Some("_id") {
        return Err(invalid("'metaField' cannot be \"_id\""));
    }
    let failed = |why: String| invalid(format!("{TIMESERIES_FAILED}Time-series {why}"));
    let mut out = Document::new().with(TIME_FIELD, time);
    if let Some(m) = meta {
        out.push(META_FIELD, m);
    }
    match (granularity, span, rounding) {
        (Some(g), span, rounding) => {
            // The span is checked before the rounding (measured).
            let default = g.max_span_seconds();
            if span.is_some_and(|s| s != i128::from(default)) {
                return Err(failed(format!(
                    "'bucketMaxSpanSeconds' cannot be set to a value other than the default of {default} for the provided granularity"
                )));
            }
            if rounding.is_some() {
                return Err(failed(
                    "'bucketRoundingSeconds' cannot be set alongside the granularity parameter"
                        .to_owned(),
                ));
            }
            out.push(GRANULARITY, g.name());
            out.push(MAX_SPAN, span_value(i128::from(default)));
        }
        (None, None, None) => {
            let g = Granularity::Seconds;
            out.push(GRANULARITY, g.name());
            out.push(MAX_SPAN, span_value(i128::from(g.max_span_seconds())));
        }
        (None, Some(s), Some(r)) if s == r => {
            out.push(ROUNDING, span_value(r));
            out.push(MAX_SPAN, span_value(s));
        }
        (None, _, _) => {
            return Err(failed(
                "'bucketRoundingSeconds' must be equal to 'bucketMaxSpanSeconds'".to_owned(),
            ));
        }
    }
    out.push(FIXED_BUCKETING, true);
    let expire = checked_expire(expire, TIMESERIES_FAILED, now_secs)?;
    Ok(Asked::TimeSeries { options: out, expire })
}

/// A bucket span, at most a year in seconds: the bound checked before.
fn span_value(n: i128) -> i32 {
    n as i32
}

fn check_clustered(
    o: &Document,
    c: &Document,
    expire: Option<i128>,
    now_secs: i64,
) -> Result<Asked, CommandError> {
    if o.get(ID_INDEX).is_some() {
        return Err(invalid(
            "The 'clusteredIndex' option is not supported with the 'idIndex' option",
        ));
    }
    if let Some((k, _)) = c.iter().find(|(k, _)| !CLUSTERED_FIELDS.contains(k)) {
        return Err(CommandError::new(
            IDL_UNKNOWN_FIELD,
            format!("BSON field 'create.clusteredIndex.{k}' is an unknown field."),
        ));
    }
    let required = |f: &str| {
        CommandError::new(
            IDL_FAILED_TO_PARSE,
            format!("BSON field 'create.clusteredIndex.{f}' is missing but a required field"),
        )
    };
    let key = c.get(KEY).ok_or_else(|| required(KEY))?;
    let unique = c.get(UNIQUE).ok_or_else(|| required(UNIQUE))?;
    let name = match c.get(NAME) {
        None => ID_INDEX_NAME.to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => return Err(wrong_type("create.clusteredIndex.name", v, "string")),
    };
    let version = match c.get(VERSION) {
        None => i128::from(CLUSTERED_INDEX_VERSION),
        Some(v) => number(v, "create.clusteredIndex.v")?,
    };
    let id_key = Document::new().with("_id", 1);
    let is_id_key = match key {
        Value::Document(k) => k.len() == 1 && k.get("_id").and_then(integral) == Some(1),
        _ => false,
    };
    if !is_id_key {
        return Err(CommandError::new(
            INVALID_INDEX_SPECIFICATION_OPTION,
            "The clusteredIndex option is only supported for key: {_id: 1}",
        ));
    }
    if crate::types::truthy(unique) != Some(true) {
        return Err(CommandError::new(
            CLUSTERED_UNIQUE,
            "The clusteredIndex option requires unique: true to be specified",
        ));
    }
    let index = |v: Value| {
        Document::new()
            .with(VERSION, v)
            .with(KEY, id_key.clone())
            .with(NAME, name.clone())
            .with(UNIQUE, true)
    };
    if version != i128::from(CLUSTERED_INDEX_VERSION) {
        let shown = index(c.get(VERSION).cloned().unwrap_or(Value::Null));
        return Err(CommandError::new(
            CLUSTERED_VERSION,
            format!(
                "Invalid clusteredIndex specification {}; cannot create a clusteredIndex with v={version}",
                display(&Value::Document(shown))
            ),
        ));
    }
    if name.is_empty() {
        return Err(CommandError::new(
            CANNOT_CREATE_INDEX,
            "The clusteredIndex name cannot be empty",
        ));
    }
    let expire = checked_expire(expire, "", now_secs)?;
    Ok(Asked::Clustered { index: index(Value::Int32(CLUSTERED_INDEX_VERSION)), expire })
}

/// The options a `create` of `asked` stores, from its options `o` (collation already complete),
/// in the order a `refrouter` 9.0.4 lists them.
pub fn stored(asked: &Asked, o: &Document) -> Document {
    let collation = o.get(COLLATION).cloned();
    let mut out = Document::new();
    match asked {
        Asked::Normal => return o.clone(),
        Asked::View { view_on, pipeline } => {
            out.push(VIEW_ON, view_on.as_str());
            out.push(PIPELINE, pipeline.iter().cloned().map(Value::Document).collect::<Vec<_>>());
            if let Some(c) = collation {
                out.push(COLLATION, c);
            }
        }
        Asked::Capped { size, max } => {
            out.push(CAPPED, true);
            match i32::try_from(*size) {
                Ok(s) => out.push(SIZE, s),
                Err(_) => out.push(SIZE, *size),
            }
            if let Some(m) = max {
                out.push(MAX, *m);
            }
            for (k, v) in o.iter().filter(|(k, _)| ![CAPPED, SIZE, MAX].contains(k)) {
                out.push(k, v.clone());
            }
        }
        Asked::TimeSeries { options, expire } => {
            if let Some(c) = collation {
                out.push(COLLATION, c);
            }
            if let Some(e) = expire {
                out.push(EXPIRE, *e);
            }
            out.push(TIMESERIES, options.clone());
        }
        Asked::Clustered { index, expire } => {
            for (k, v) in o.iter().filter(|(k, _)| ![COLLATION, CLUSTERED, EXPIRE].contains(k)) {
                out.push(k, v.clone());
            }
            if let Some(c) = collation {
                out.push(COLLATION, c);
            }
            out.push(CLUSTERED, index.clone());
            if let Some(e) = expire {
                out.push(EXPIRE, *e);
            }
        }
    }
    out
}

/// The kind of collection `c`.
pub fn kind_of(c: &CollectionEntry) -> Result<CollectionKind, CommandError> {
    CollectionKind::of(&c.options)
        .map_err(|e| CommandError::new(crate::error::INTERNAL_ERROR, e.0.to_owned()))
}

/// Refuses a command that needs a collection on view `ns` (`CommandNotSupportedOnView`).
pub fn not_a_view(ns: &Namespace, c: &CollectionEntry) -> Result<(), CommandError> {
    match kind_of(c)? {
        CollectionKind::View { .. } => Err(CommandError::new(
            COMMAND_NOT_SUPPORTED_ON_VIEW,
            format!("Namespace {} is a view, not a collection", full(ns)),
        )),
        _ => Ok(()),
    }
}

fn bytes_of(d: &Document) -> Result<Vec<u8>, CommandError> {
    Ok(d.to_bytes()?)
}

/// Checks the definition of view `ns` against the views of its database.
pub fn check_view_definition<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    stored: &Document,
) -> Result<(), CommandError> {
    let (view_on, pipeline, collation) = match CollectionKind::of(&bytes_of(stored)?) {
        Ok(CollectionKind::View { view_on, pipeline, collation }) => (view_on, pipeline, collation),
        _ => return Ok(()),
    };
    let kinds: Vec<(String, CollectionKind)> = backend
        .collections()?
        .into_iter()
        .filter(|(n, _)| n.db == ns.db)
        .filter_map(|(n, c)| CollectionKind::of(&c.options).ok().map(|k| (n.coll, k)))
        .collect();
    let def = Definition {
        name: &ns.coll,
        view_on: &view_on,
        pipeline: &pipeline,
        collation: collation.as_deref(),
    };
    let lookup = |name: &str| kinds.iter().find(|(n, _)| n == name).map(|(_, k)| k.clone());
    views::check_definition(&ns.db, &def, lookup).map_err(CommandError::from)
}

/// Creates `system.views` of `db` with the first view of the database.
pub fn ensure_system_views<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
) -> Result<(), CommandError> {
    let ns = super::names::ns(db, SYSTEM_VIEWS);
    if backend.collection(&ns)?.is_some() {
        return Ok(());
    }
    let uuid = super::new_uuid(backend);
    let options = Document::new().to_bytes()?;
    backend.catalog_write(CatalogCommand::Create { ns, uuid, options, if_absent: true })?;
    Ok(())
}

/// The views of `db`.
pub fn views_of<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
) -> Result<Vec<Namespace>, CommandError> {
    let mut out = Vec::new();
    for (ns, c) in backend.collections()?.into_iter().filter(|(n, _)| n.db == db) {
        if matches!(kind_of(&c)?, CollectionKind::View { .. }) {
            out.push(ns);
        }
    }
    Ok(out)
}

/// The documents of `system.views` of `db`: one per view, `_id` its namespace.
pub fn view_documents<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
) -> Result<Vec<Document>, CommandError> {
    let mut docs = Vec::new();
    for (ns, c) in backend.collections()?.into_iter().filter(|(n, _)| n.db == db) {
        if !matches!(kind_of(&c)?, CollectionKind::View { .. }) {
            continue;
        }
        let mut d = Document::new().with("_id", full(&ns));
        for (k, v) in Document::from(RawDoc::from_bytes(&c.options)?).iter() {
            d.push(k, v.clone());
        }
        docs.push(d);
    }
    Ok(docs)
}

/// A pipeline as RefDb shows it in the errors of a repeated `create`: a document of the
/// stages by position.
fn pipeline_shown(p: Option<&Value>) -> String {
    let mut d = Document::new();
    if let Some(Value::Array(stages)) = p {
        for (i, s) in stages.iter().enumerate() {
            d.push(i.to_string(), s.clone());
        }
    }
    display(&Value::Document(d))
}

/// The answer to a repeated `create` of `ns` when the kinds differ or a view is involved, as a
/// `refrouter` 9.0.4 gives it; `None` when the options are compared as a whole.
pub fn existing(
    ns: &Namespace,
    have: &Document,
    asked: &Document,
    asked_collation: Option<&Document>,
) -> Option<Result<Document, CommandError>> {
    let exists = |why: String| {
        Some(Err(CommandError::new(
            NAMESPACE_EXISTS,
            format!("namespace '{}' already exists, but {why}", full(ns)),
        )))
    };
    let have_view = have.get(VIEW_ON);
    let asked_view = asked.get(VIEW_ON);
    match (have_view, asked_view) {
        (None, None) => None,
        (Some(_), None) => exists("is a view".to_owned()),
        (None, Some(_)) => exists("is a collection rather than a view".to_owned()),
        (Some(h), Some(a)) => {
            if h != a {
                let name = |v: &Value| match v {
                    Value::String(s) => format!("{}.{s}", ns.db),
                    other => display(other),
                };
                return exists(format!("is a view on '{}' rather than '{}'", name(h), name(a)));
            }
            if have.get(PIPELINE) != asked.get(PIPELINE) {
                return exists(format!(
                    "with pipeline {} rather than {}",
                    pipeline_shown(have.get(PIPELINE)),
                    pipeline_shown(asked.get(PIPELINE))
                ));
            }
            if have.get(COLLATION) != asked.get(COLLATION) {
                let shown = |c: Option<&Document>| {
                    display(&Value::Document(c.cloned().unwrap_or_default()))
                };
                let have_c = match have.get(COLLATION) {
                    Some(Value::Document(d)) => Some(d),
                    _ => None,
                };
                return exists(format!(
                    "with collation: {} rather than {}",
                    shown(have_c),
                    shown(asked_collation)
                ));
            }
            Some(Ok(Document::new().with("ok", 1.0)))
        }
    }
}

/// The existing options a repeated `create` shows when they differ: a time series is shown
/// without its UUID to another time series, and with the clustering it keeps its buckets in
/// to any other kind (measured).
pub fn shown_existing(have: &Document, asked: &Document, uuid: &str) -> String {
    let mut parts = Vec::new();
    let have_ts = have.get(TIMESERIES).is_some();
    if !(have_ts && asked.get(TIMESERIES).is_some()) {
        parts.push(format!("uuid: UUID(\"{uuid}\")"));
        if have_ts {
            parts.push(format!("{CLUSTERED}: true"));
        }
    }
    for (k, v) in have.iter() {
        parts.push(format!("{k}: {}", display(v)));
    }
    format!("{{ {} }}", parts.join(", "))
}
