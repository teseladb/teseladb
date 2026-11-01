//! The kinds of collection (015 FR-001, FR-003, FR-006, FR-009, data model): a view, a capped
//! collection, a time series or a clustered collection, read from the options the catalog keeps
//! for it (`create` checked and normalized them, as a `refrouter` 9.0.4 stores them).

use teseladb_bson::{RawDoc, RawValue};

/// Options of the kinds.
pub(crate) const VIEW_ON: &str = "viewOn";
const PIPELINE: &str = "pipeline";
const COLLATION: &str = "collation";
const CAPPED: &str = "capped";
const SIZE: &str = "size";
const MAX: &str = "max";
const TIMESERIES: &str = "timeseries";
const TIME_FIELD: &str = "timeField";
const META_FIELD: &str = "metaField";
const GRANULARITY: &str = "granularity";
const MAX_SPAN: &str = "bucketMaxSpanSeconds";
const ROUNDING: &str = "bucketRoundingSeconds";
const EXPIRE_AFTER_SECONDS: &str = "expireAfterSeconds";
const CLUSTERED_INDEX: &str = "clusteredIndex";
const NAME: &str = "name";

/// Options the catalog should never hold: their reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KindError(pub &'static str);

/// `granularity` of a time series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Granularity {
    /// `seconds`.
    Seconds,
    /// `minutes`.
    Minutes,
    /// `hours`.
    Hours,
}

impl Granularity {
    const ALL: [Granularity; 3] = [Granularity::Seconds, Granularity::Minutes, Granularity::Hours];

    /// Its name.
    pub fn name(self) -> &'static str {
        match self {
            Granularity::Seconds => "seconds",
            Granularity::Minutes => "minutes",
            Granularity::Hours => "hours",
        }
    }

    /// The granularity named `name`.
    pub fn of(name: &str) -> Option<Granularity> {
        Granularity::ALL.into_iter().find(|g| g.name() == name)
    }

    /// The `bucketMaxSpanSeconds` it gives (measured on a `refrouter` 9.0.4).
    pub fn max_span_seconds(self) -> i64 {
        match self {
            Granularity::Seconds => 3_600,
            Granularity::Minutes => 86_400,
            Granularity::Hours => 2_592_000,
        }
    }
}

/// The options of a time series.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeSeries {
    /// `timeField`.
    pub time_field: String,
    /// `metaField`.
    pub meta_field: Option<String>,
    /// `granularity` (none with custom bucketing).
    pub granularity: Option<Granularity>,
    /// `bucketMaxSpanSeconds`.
    pub bucket_max_span_seconds: i64,
    /// `bucketRoundingSeconds` (custom bucketing).
    pub bucket_rounding_seconds: Option<i64>,
    /// `expireAfterSeconds` of the collection.
    pub expire_after_seconds: Option<i64>,
}

/// The kind of a collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollectionKind {
    /// A collection without special options.
    Normal,
    /// A view: the collection or view it reads, its pipeline (BSON documents) and collation.
    View {
        /// `viewOn`.
        view_on: String,
        /// `pipeline`, each stage as BSON.
        pipeline: Vec<Vec<u8>>,
        /// `collation`, as BSON.
        collation: Option<Vec<u8>>,
    },
    /// A capped collection: its bytes and documents at most (`None`: no limit of documents).
    Capped {
        /// `size`.
        size: u64,
        /// `max`.
        max: Option<u64>,
    },
    /// A time series.
    TimeSeries(TimeSeries),
    /// A collection clustered by `_id`.
    Clustered {
        /// The name of its clustered index.
        name: String,
        /// `expireAfterSeconds`.
        expire_after_seconds: Option<i64>,
    },
}

fn integer(v: Option<RawValue<'_>>) -> Option<i64> {
    match v? {
        RawValue::Int32(n) => Some(i64::from(n)),
        RawValue::Int64(n) => Some(n),
        RawValue::Double(n) if n.fract() == 0.0 => Some(n as i64),
        _ => None,
    }
}

fn string(v: Option<RawValue<'_>>) -> Option<String> {
    match v? {
        RawValue::String(s) => Some(s.to_string()),
        _ => None,
    }
}

impl CollectionKind {
    /// The kind of a collection whose stored options are `options` (BSON).
    pub fn of(options: &[u8]) -> Result<CollectionKind, KindError> {
        let d = RawDoc::from_bytes(options).map_err(|_| KindError("options that are not BSON"))?;
        if let Some(on) = d.get(VIEW_ON) {
            let RawValue::String(view_on) = on else {
                return Err(KindError("viewOn not a string"));
            };
            let pipeline = match d.get(PIPELINE) {
                Some(RawValue::Array(stages)) => stages
                    .iter()
                    .map(|s| match s {
                        RawValue::Document(s) => Ok(s.as_bytes().to_vec()),
                        _ => Err(KindError("a stage that is not a document")),
                    })
                    .collect::<Result<_, _>>()?,
                None => Vec::new(),
                Some(_) => return Err(KindError("pipeline not an array")),
            };
            let collation = match d.get(COLLATION) {
                Some(RawValue::Document(c)) => Some(c.as_bytes().to_vec()),
                _ => None,
            };
            return Ok(CollectionKind::View { view_on: view_on.to_string(), pipeline, collation });
        }
        let expire = integer(d.get(EXPIRE_AFTER_SECONDS));
        if let Some(ts) = d.get(TIMESERIES) {
            let RawValue::Document(ts) = ts else {
                return Err(KindError("timeseries not a document"));
            };
            let time_field = string(ts.get(TIME_FIELD)).ok_or(KindError("no timeField"))?;
            let granularity = match ts.get(GRANULARITY) {
                Some(RawValue::String(g)) => {
                    Some(Granularity::of(g).ok_or(KindError("an unknown granularity"))?)
                }
                _ => None,
            };
            let rounding = integer(ts.get(ROUNDING));
            let span = integer(ts.get(MAX_SPAN))
                .or_else(|| granularity.map(Granularity::max_span_seconds))
                .unwrap_or(Granularity::Seconds.max_span_seconds());
            return Ok(CollectionKind::TimeSeries(TimeSeries {
                time_field,
                meta_field: string(ts.get(META_FIELD)),
                granularity,
                bucket_max_span_seconds: span,
                bucket_rounding_seconds: rounding,
                expire_after_seconds: expire,
            }));
        }
        if let Some(c) = d.get(CLUSTERED_INDEX) {
            let RawValue::Document(c) = c else {
                return Err(KindError("clusteredIndex not a document"));
            };
            let name = string(c.get(NAME)).ok_or(KindError("a clustered index without name"))?;
            return Ok(CollectionKind::Clustered { name, expire_after_seconds: expire });
        }
        if matches!(d.get(CAPPED), Some(RawValue::Boolean(true))) {
            let size = integer(d.get(SIZE))
                .and_then(|n| u64::try_from(n).ok())
                .ok_or(KindError("a capped collection without size"))?;
            let max = integer(d.get(MAX)).and_then(|n| u64::try_from(n).ok());
            return Ok(CollectionKind::Capped { size, max });
        }
        Ok(CollectionKind::Normal)
    }

    /// The `type` `listCollections` gives it.
    pub fn type_name(&self) -> &'static str {
        match self {
            CollectionKind::View { .. } => "view",
            CollectionKind::TimeSeries(_) => "timeseries",
            CollectionKind::Normal
            | CollectionKind::Capped { .. }
            | CollectionKind::Clustered { .. } => "collection",
        }
    }
}
