//! The buckets of a time series (015 FR-006, T012, data model §3): measurements of one value of
//! the `metaField` within one window of `bucketMaxSpanSeconds` share a bucket document,
//!
//! ```text
//! { _id: ObjectId, control: { version: 2, min: {...}, max: {...}, count }, meta: <metaField>,
//!   data: { <field>: <BSON column> } }
//! ```
//!
//! as RefDb 9.0 keeps them in the collection itself (measured on a `refrouter` 9.0.4 with
//! `rawData: true`). Each field of the measurements, the time field among them, is one column
//! (`teseladb_bson::column`), with a row per measurement; `control.min` and `control.max` hold
//! the least and greatest value of each field (subdocuments field by field), `control.min` of the
//! time field rounded down to `bucketRoundingSeconds`. A bucket holds at most
//! [`MAX_MEASUREMENTS`] measurements and [`MAX_BUCKET_BYTES`] bytes.
//!
//! The windows are aligned to `bucketMaxSpanSeconds`, and a bucket's `_id` is the window's start
//! in seconds, five bytes of a hash of the meta value and a three-byte number for the buckets of
//! one window and meta value, so a writer knows which document to read: RefDb's `_id`s of the
//! same buckets differ (TeselaDB keeps no catalog of open buckets in memory).

use std::cmp::Ordering;

use teseladb_bson::column::{self, Cell};
use teseladb_bson::{Document, RawDoc, Value, compare};

/// Most measurements of a bucket (`timeseriesBucketMaxCount`).
pub const MAX_MEASUREMENTS: usize = 1000;
/// Most bytes of a bucket's measurements (`timeseriesBucketMaxSize`, 125 KiB).
pub const MAX_BUCKET_BYTES: usize = 125 * 1024;
/// Measurements a bucket keeps open for although a large one takes it past
/// [`MAX_BUCKET_BYTES`] (`timeseriesBucketMinCount`).
pub const MIN_MEASUREMENTS: usize = 10;
/// Bytes a bucket kept open for large measurements never goes past (12 MiB).
pub const MAX_LARGE_BUCKET_BYTES: usize = 12 * 1024 * 1024;
/// `control.version` of a bucket with compressed columns.
pub const BUCKET_VERSION: i32 = 2;
/// `_id` of a bucket.
pub const ID: &str = "_id";
/// The bucket's `control` document.
pub const CONTROL: &str = "control";
/// `control.version`.
pub const VERSION: &str = "version";
/// `control.min`: the least value of each field.
pub const MIN: &str = "min";
/// `control.max`: the greatest value of each field.
pub const MAX: &str = "max";
/// `control.count`: the measurements.
pub const COUNT: &str = "count";
/// The meta value of the bucket's measurements.
pub const META: &str = "meta";
/// The columns, one per field.
pub const DATA: &str = "data";
/// Binary subtype of a column.
const COLUMN_SUBTYPE: u8 = teseladb_bson::types::BINARY_COLUMN;
/// Name of the one field of the holder documents values travel in.
const HOLDER: &str = "v";

/// Why a measurement or a bucket is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TsError {
    /// The measurement has no date in the time field: RefDb's `BadValue` text.
    NoTime(String),
    /// The bucket is full, by its count or its size: the measurement goes to the next one.
    Full(Full),
    /// A stored bucket that cannot be read.
    Malformed(String),
}

/// Why a bucket takes no more measurements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Full {
    /// It has [`MAX_MEASUREMENTS`].
    Count,
    /// The next measurement would take it past [`MAX_BUCKET_BYTES`].
    Size,
    /// The next measurement has a field of another type than the bucket's measurements
    /// (RefDb's schema change).
    Schema,
}

/// The class of a BSON element type RefDb compares schemas by (`canonicalizeBSONType`):
/// the numbers one class, strings and symbols one, `null` and `undefined` apart.
fn type_class(v: &Value) -> i32 {
    use teseladb_bson::types as t;
    match v.element_type() {
        t::MIN_KEY => -1,
        t::UNDEFINED => 0,
        t::NULL => 5,
        t::DOUBLE | t::INT32 | t::INT64 | t::DECIMAL128 => 10,
        t::STRING | t::SYMBOL => 15,
        t::DOCUMENT => 20,
        t::ARRAY => 25,
        t::BINARY => 30,
        t::OBJECT_ID => 35,
        t::BOOLEAN => 40,
        t::DATE_TIME => 45,
        t::TIMESTAMP => 47,
        t::REGEX => 50,
        t::DB_POINTER => 55,
        t::JAVASCRIPT => 60,
        t::JAVASCRIPT_WITH_SCOPE => 65,
        _ => 127,
    }
}

/// The fields of an object or the elements of an array, by name or position.
fn parts(v: &Value) -> Vec<(String, &Value)> {
    match v {
        Value::Document(d) => d.iter().map(|(k, x)| (k.to_owned(), x)).collect(),
        Value::Array(items) => items.iter().enumerate().map(|(i, x)| (i.to_string(), x)).collect(),
        _ => Vec::new(),
    }
}

/// Whether `new` has, at every path `seen` also has, a value of the same class: what a bucket
/// whose measurements gave `seen` still takes (measured on a `refnode` 9.0.4 on 02-11-2026).
fn fits(seen: &Value, new: &Value) -> bool {
    if type_class(seen) != type_class(new) {
        return false;
    }
    let inner = parts(seen);
    parts(new)
        .into_iter()
        .all(|(k, x)| inner.iter().find(|(s, _)| *s == k).is_none_or(|(_, s)| fits(s, x)))
}

/// `seen` with the paths of `new` it lacks; `new` fits it ([`fits`]).
fn merged(seen: Value, new: &Value) -> Value {
    match (seen, new) {
        (Value::Document(d), Value::Document(n)) => Value::Document(merged_schema(d, n)),
        (Value::Array(mut items), Value::Array(n)) => {
            for (i, x) in n.iter().enumerate() {
                match items.get_mut(i) {
                    Some(old) => *old = merged(std::mem::replace(old, Value::Null), x),
                    None => items.push(x.clone()),
                }
            }
            Value::Array(items)
        }
        (seen, _) => seen,
    }
}

/// The schema of a bucket's measurements: one value per path, of its class.
fn merged_schema(mut seen: Document, row: &Document) -> Document {
    for (k, x) in row.iter() {
        let next = match seen.remove(k) {
            Some(old) => merged(old, x),
            None => x.clone(),
        };
        seen.insert(k, next);
    }
    seen
}

/// Whether measurement `row` fits schema `seen`.
fn fits_schema(seen: &Document, row: &Document) -> bool {
    row.iter().all(|(k, x)| seen.get(k).is_none_or(|s| fits(s, x)))
}

/// How an append ended: why the bucket stopped before the end, and whether it stayed open past
/// its size for a large measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stop {
    /// Why it stopped taking measurements, if it did.
    pub full: Option<Full>,
    /// It took a measurement past [`MAX_BUCKET_BYTES`] because it has fewer than
    /// [`MIN_MEASUREMENTS`].
    pub kept_open: bool,
}

/// How a time series lays out its buckets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BucketSpec {
    /// `timeField`.
    pub time_field: String,
    /// `metaField`.
    pub meta_field: Option<String>,
    /// `bucketMaxSpanSeconds`.
    pub max_span_seconds: i64,
    /// `bucketRoundingSeconds` (from the granularity without custom bucketing).
    pub rounding_seconds: i64,
}

/// The error of a bucket whose bytes do not read (as BSON, as a column).
fn broken(e: impl std::fmt::Display) -> TsError {
    malformed(e.to_string())
}

fn malformed(what: impl Into<String>) -> TsError {
    TsError::Malformed(what.into())
}

/// `v` as a column cell: its type and the bytes of its value.
fn cell_of(v: &Value) -> Result<Cell, TsError> {
    let bytes = Document::new().with(HOLDER, v.clone()).to_bytes().map_err(broken)?;
    // `len · type · "v\0" · value · 00`.
    Ok(Cell { kind: bytes[4], bytes: bytes[7..bytes.len() - 1].to_vec() })
}

/// The value of a column cell.
fn value_of(c: &Cell) -> Result<Value, TsError> {
    let len = 4 + 1 + 2 + c.bytes.len() + 1;
    let mut b =
        i32::try_from(len).map_err(|_| malformed("a cell too large"))?.to_le_bytes().to_vec();
    b.push(c.kind);
    b.extend_from_slice(HOLDER.as_bytes());
    b.push(0);
    b.extend_from_slice(&c.bytes);
    b.push(0);
    let raw = RawDoc::from_bytes(&b).map_err(broken)?;
    Document::from(raw).remove(HOLDER).ok_or_else(|| malformed("an empty cell"))
}

/// RefDb's order of two values.
fn order(a: &Value, b: &Value) -> Result<Ordering, TsError> {
    let holder = Document::new().with("a", a.clone()).with("b", b.clone());
    let bytes = holder.to_bytes().map_err(broken)?;
    let raw = RawDoc::from_bytes(&bytes).map_err(broken)?;
    match (raw.get("a"), raw.get("b")) {
        (Some(x), Some(y)) => Ok(compare(x, y, None)),
        _ => Err(malformed("values that do not compare")),
    }
}

/// The least (`less`) or greatest of `current` and `v`, subdocuments field by field.
fn extreme(current: Option<&Value>, v: &Value, less: bool) -> Result<Value, TsError> {
    Ok(match (current, v) {
        (None, _) => v.clone(),
        (Some(Value::Document(c)), Value::Document(d)) => {
            let mut out = c.clone();
            for (k, x) in d.iter() {
                let next = extreme(c.get(k), x, less)?;
                out.insert(k, next);
            }
            Value::Document(out)
        }
        (Some(c), _) => {
            let keep_new = match order(v, c)? {
                Ordering::Less => less,
                Ordering::Greater => !less,
                Ordering::Equal => false,
            };
            if keep_new { v.clone() } else { c.clone() }
        }
    })
}

/// A 40-bit FNV-1a hash of the BSON of a meta value.
fn meta_hash(meta: Option<&Value>) -> Result<[u8; 5], TsError> {
    let bytes = match meta {
        Some(m) => Document::new().with(HOLDER, m.clone()).to_bytes().map_err(broken)?,
        None => Vec::new(),
    };
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let be = h.to_be_bytes();
    Ok([be[3], be[4], be[5], be[6], be[7]])
}

impl BucketSpec {
    /// The milliseconds of a measurement's time field.
    pub fn time_of(&self, m: &Document) -> Result<i64, TsError> {
        match m.get(&self.time_field) {
            Some(Value::DateTime(t)) => Ok(*t),
            _ => Err(TsError::NoTime(format!(
                "'{}' must be present and contain a valid BSON UTC datetime value",
                self.time_field
            ))),
        }
    }

    /// The meta value of a measurement.
    pub fn meta_of<'a>(&self, m: &'a Document) -> Option<&'a Value> {
        self.meta_field.as_deref().and_then(|f| m.get(f))
    }

    /// The start of the window of time `t_ms`, in seconds.
    pub fn window(&self, t_ms: i64) -> i64 {
        let span = self.max_span_seconds.max(1);
        t_ms.div_euclid(1000).div_euclid(span) * span
    }

    /// The `_id` of bucket `n` of the window of `t_ms` and meta value `meta`.
    pub fn bucket_id(&self, t_ms: i64, meta: Option<&Value>, n: u32) -> Result<[u8; 12], TsError> {
        let mut id = [0u8; 12];
        id[..4].copy_from_slice(&(self.window(t_ms) as u32).to_be_bytes());
        id[4..9].copy_from_slice(&meta_hash(meta)?);
        id[9..].copy_from_slice(&n.to_be_bytes()[1..]);
        Ok(id)
    }

    /// The bucket `id` with `bucket`'s measurements (none for a new one) and `m`; [`TsError::Full`]
    /// when it cannot take it.
    pub fn append(
        &self,
        id: [u8; 12],
        bucket: Option<RawDoc<'_>>,
        m: &Document,
    ) -> Result<Document, TsError> {
        let (b, _, _) = self.append_all(id, bucket, std::slice::from_ref(m))?;
        Ok(b)
    }

    /// The bucket `id` with `bucket`'s measurements and as many of `ms` as it takes, in order,
    /// how many it took and why it stopped before the end; [`TsError::Full`] when it takes none.
    /// The bucket is read and written once.
    pub fn append_all(
        &self,
        id: [u8; 12],
        bucket: Option<RawDoc<'_>>,
        ms: &[Document],
    ) -> Result<(Document, usize, Stop), TsError> {
        let mut rows = match bucket {
            Some(b) => self.measurements(b)?,
            None => Vec::new(),
        };
        let mut size: usize = rows.iter().map(|r| r.to_bytes().map_or(0, |b| b.len())).sum();
        let mut meta = bucket.and_then(|b| b.get(META)).map(Value::from);
        let mut schema = rows.iter().fold(Document::new(), merged_schema);
        let mut taken = 0;
        let mut stopped = None;
        let mut kept_open = false;
        for m in ms {
            self.time_of(m)?;
            if rows.len() >= MAX_MEASUREMENTS {
                stopped = Some(Full::Count);
                break;
            }
            let mut row = m.clone();
            if let Some(f) = &self.meta_field {
                meta = row.remove(f).or(meta);
            }
            if !fits_schema(&schema, &row) {
                stopped = Some(Full::Schema);
                break;
            }
            schema = merged_schema(schema, &row);
            let bytes = row.to_bytes().map_err(broken)?.len();
            // Past the size, a bucket with few measurements stays open for large ones.
            let over = size + bytes > MAX_BUCKET_BYTES;
            let few = rows.len() < MIN_MEASUREMENTS && size + bytes <= MAX_LARGE_BUCKET_BYTES;
            if over && !rows.is_empty() && !few {
                stopped = Some(Full::Size);
                break;
            }
            kept_open |= over && !rows.is_empty();
            size += bytes;
            rows.push(row);
            taken += 1;
        }
        match (taken, stopped) {
            (0, Some(why)) => Err(TsError::Full(why)),
            _ => Ok((
                self.build(id, meta.as_ref(), &rows)?,
                taken,
                Stop { full: stopped, kept_open },
            )),
        }
    }

    /// The bucket document `id` of measurements `rows` (without the meta field) of meta `meta`.
    pub fn build(
        &self,
        id: [u8; 12],
        meta: Option<&Value>,
        rows: &[Document],
    ) -> Result<Document, TsError> {
        let mut fields: Vec<String> = Vec::new();
        for r in rows {
            for (k, _) in r.iter() {
                if !fields.iter().any(|f| f == k) {
                    fields.push(k.to_owned());
                }
            }
        }
        let (mut min, mut max, mut data) = (Document::new(), Document::new(), Document::new());
        for f in &fields {
            let mut cells = Vec::with_capacity(rows.len());
            let (mut lo, mut hi): (Option<Value>, Option<Value>) = (None, None);
            for r in rows {
                match r.get(f) {
                    Some(v) => {
                        lo = Some(extreme(lo.as_ref(), v, true)?);
                        hi = Some(extreme(hi.as_ref(), v, false)?);
                        cells.push(Some(cell_of(v)?));
                    }
                    None => cells.push(None),
                }
            }
            if let (Some(lo), Some(hi)) = (lo, hi) {
                min.push(f.as_str(), self.rounded(f, lo));
                max.push(f.as_str(), hi);
            }
            data.push(
                f.as_str(),
                Value::Binary { subtype: COLUMN_SUBTYPE, bytes: column::encode(&cells) },
            );
        }
        let count = i32::try_from(rows.len()).map_err(|_| malformed("too many measurements"))?;
        let control = Document::new()
            .with(VERSION, BUCKET_VERSION)
            .with(MIN, min)
            .with(MAX, max)
            .with(COUNT, count);
        let mut bucket = Document::new().with(ID, Value::ObjectId(id)).with(CONTROL, control);
        if let Some(m) = meta {
            bucket.push(META, m.clone());
        }
        Ok(bucket.with(DATA, data))
    }

    /// `control.min` of field `f`: the time field rounded down to `bucketRoundingSeconds`.
    fn rounded(&self, f: &str, v: Value) -> Value {
        match v {
            Value::DateTime(t) if f == self.time_field => {
                let unit = self.rounding_seconds.max(1) * 1000;
                Value::DateTime(t.div_euclid(unit) * unit)
            }
            other => other,
        }
    }

    /// The measurements of bucket `b` without the meta field, in insertion order.
    pub fn measurements(&self, b: RawDoc<'_>) -> Result<Vec<Document>, TsError> {
        let bucket = Document::from(b);
        let count = match bucket.get(CONTROL) {
            Some(Value::Document(c)) => match c.get(COUNT) {
                Some(Value::Int32(n)) => usize::try_from(*n).map_err(|_| malformed("count"))?,
                _ => return Err(malformed("a bucket without its count")),
            },
            _ => return Err(malformed("a bucket without control")),
        };
        let Some(Value::Document(data)) = bucket.get(DATA) else {
            return Err(malformed("a bucket without data"));
        };
        let mut rows = vec![Document::new(); count];
        for (f, col) in data.iter() {
            let Value::Binary { subtype: COLUMN_SUBTYPE, bytes } = col else {
                return Err(malformed(format!("field {f} is no column")));
            };
            let cells = column::decode(bytes, count).map_err(broken)?;
            for (row, c) in rows.iter_mut().zip(cells) {
                if let Some(c) = c {
                    row.push(f, value_of(&c)?);
                }
            }
        }
        Ok(rows)
    }

    /// [`BucketSpec::unpack`] as each measurement's BSON bytes, built from the columns' cells
    /// without going through values (the reads of 015 FR-007).
    pub fn unpack_raw(&self, b: RawDoc<'_>) -> Result<Vec<Vec<u8>>, TsError> {
        let count = match b.get(CONTROL) {
            Some(teseladb_bson::RawValue::Document(c)) => match c.get(COUNT) {
                Some(teseladb_bson::RawValue::Int32(n)) => {
                    usize::try_from(n).map_err(|_| malformed("count"))?
                }
                _ => return Err(malformed("a bucket without its count")),
            },
            _ => return Err(malformed("a bucket without control")),
        };
        let Some(teseladb_bson::RawValue::Document(data)) = b.get(DATA) else {
            return Err(malformed("a bucket without data"));
        };
        let mut columns: Vec<(&str, Vec<Option<Cell>>)> = Vec::new();
        for (f, col) in data.iter() {
            let teseladb_bson::RawValue::Binary { subtype: COLUMN_SUBTYPE, bytes } = col else {
                return Err(malformed(format!("field {f} is no column")));
            };
            let cells = column::decode(bytes, count).map_err(broken)?;
            columns.push((f, cells));
        }
        // The time field's column first, then the others in the bucket's order.
        columns.sort_by_key(|(f, _)| *f != self.time_field);
        let meta = match (&self.meta_field, b.get(META)) {
            (Some(f), Some(v)) => Some((f.as_str(), cell_of(&Value::from(v))?)),
            _ => None,
        };
        let element = |out: &mut Vec<u8>, name: &str, c: &Cell| {
            out.push(c.kind);
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            out.extend_from_slice(&c.bytes);
        };
        let mut rows = Vec::with_capacity(count);
        for i in 0..count {
            let mut out = vec![0, 0, 0, 0];
            for (j, (f, cells)) in columns.iter().enumerate() {
                if let Some(Some(c)) = cells.get(i) {
                    element(&mut out, f, c);
                }
                if j == 0
                    && let Some((name, c)) = &meta
                {
                    element(&mut out, name, c);
                }
            }
            if columns.is_empty()
                && let Some((name, c)) = &meta
            {
                element(&mut out, name, c);
            }
            out.push(0);
            let len = i32::try_from(out.len()).map_err(|_| malformed("a measurement too large"))?;
            out[..4].copy_from_slice(&len.to_le_bytes());
            rows.push(out);
        }
        Ok(rows)
    }

    /// The measurements of bucket `b` as the collection shows them: the time field first, then
    /// the meta field, then the others in the bucket's order.
    pub fn unpack(&self, b: RawDoc<'_>) -> Result<Vec<Document>, TsError> {
        self.unpack_raw(b)?
            .iter()
            .map(|bytes| RawDoc::from_bytes(bytes).map(Document::from).map_err(broken))
            .collect()
    }
}
