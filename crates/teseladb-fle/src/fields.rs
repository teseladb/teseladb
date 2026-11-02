//! The `encryptedFields` option of a collection (FR-001, FR-002): read with the IDL rules and
//! messages of `create` on a `refnode` 9.0.4, checked, and kept as `listCollections` shows it, with
//! the names of the metadata collections and the default contention filled in.

use teseladb_bson::types as t;
use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::types::{TypeSpec, by_alias, name_of};

use crate::error::*;
use crate::idl::{Fields, wrong_type};

/// The prefix of the metadata collections' names.
pub const METADATA_PREFIX: &str = "enxcol_.";
/// The suffix of the encrypted state collection.
pub const ESC_SUFFIX: &str = ".esc";
/// The suffix of the encrypted compaction collection.
pub const ECOC_SUFFIX: &str = ".ecoc";
/// The array of tags every encrypted document has.
pub const SAFE_CONTENT: &str = "__safeContent__";
/// The contention factor of a query that does not give one.
pub const DEFAULT_CONTENTION: i64 = 8;
/// The highest sparsity.
pub const MAX_SPARSITY: i64 = 8;
/// The sparsity of a range field that does not give one.
pub const DEFAULT_SPARSITY: i64 = 2;
/// The domains of more bits than this are checked for the size of their queries.
const LARGE_DOMAIN_BITS: u32 = 18;
/// From this trim factor plus sparsity a query of a large domain could pass the BSON limit.
const QUERY_TOO_LARGE_FROM: i64 = 20;
/// From this trim factor plus sparsity, with the bounds, a query of a large domain could pass it.
const QUERY_TOO_LARGE_BOUNDS_FROM: i64 = 19;
/// The highest contention factor.
pub const MAX_CONTENTION: i64 = 200_000;
/// The most tags one value may give.
pub const MAX_TAGS: u64 = 84_000;
/// The shortest substring query.
pub const MIN_SUBSTRING_QUERY: i32 = 2;
/// The longest substring query.
pub const MAX_SUBSTRING_QUERY: i32 = 6;
/// The longest string a substring field indexes.
pub const MAX_SUBSTRING_LENGTH: i32 = 50;

/// The query types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryType {
    /// Equality.
    Equality,
    /// Range.
    Range,
    /// Prefix of a string.
    Prefix,
    /// Suffix of a string.
    Suffix,
    /// Substring of a string.
    Substring,
}

impl QueryType {
    /// Its name in `queryType`.
    pub fn name(self) -> &'static str {
        match self {
            QueryType::Equality => EQUALITY,
            QueryType::Range => RANGE,
            QueryType::Prefix => PREFIX,
            QueryType::Suffix => SUFFIX,
            QueryType::Substring => SUBSTRING,
        }
    }

    /// Whether it is one of the three text types.
    pub fn is_text(self) -> bool {
        matches!(self, QueryType::Prefix | QueryType::Suffix | QueryType::Substring)
    }
}

const EQUALITY: &str = "equality";
const RANGE: &str = "range";
const PREFIX: &str = "prefix";
const SUFFIX: &str = "suffix";
const SUBSTRING: &str = "substring";
const RANGE_PREVIEW: &str = "rangePreview";
const TEXT_PREVIEWS: [&str; 3] = ["prefixPreview", "suffixPreview", "substringPreview"];

/// One element of `queries`.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// `queryType`.
    pub kind: QueryType,
    /// `contention`.
    pub contention: i64,
    /// `min`.
    pub min: Option<Value>,
    /// `max`.
    pub max: Option<Value>,
    /// `sparsity`.
    pub sparsity: Option<i64>,
    /// `precision`.
    pub precision: Option<i32>,
    /// `trimFactor`.
    pub trim_factor: Option<i32>,
    /// `strMaxLength`.
    pub str_max_length: Option<i32>,
    /// `strMinQueryLength`.
    pub str_min_query_length: Option<i32>,
    /// `strMaxQueryLength`.
    pub str_max_query_length: Option<i32>,
    /// `caseSensitive`.
    pub case_sensitive: Option<bool>,
    /// `diacriticSensitive`.
    pub diacritic_sensitive: Option<bool>,
}

/// One encrypted field.
#[derive(Clone, Debug, PartialEq)]
pub struct EncryptedField {
    /// `keyId`.
    pub key_id: [u8; 16],
    /// `path`.
    pub path: String,
    /// `bsonType`, as the element type and its alias.
    pub bson_type: Option<(u8, String)>,
    /// `queries`, and whether it was given as an array.
    pub queries: Option<(Vec<Query>, bool)>,
}

impl EncryptedField {
    /// The first query, which says how the field is indexed.
    pub fn query(&self) -> Option<&Query> {
        self.queries.as_ref().and_then(|(q, _)| q.first())
    }

    /// Whether a query of `kind` is configured on the field.
    pub fn has(&self, kind: QueryType) -> bool {
        self.queries.as_ref().is_some_and(|(q, _)| q.iter().any(|x| x.kind == kind))
    }
}

/// An `encryptedFields` option.
#[derive(Clone, Debug, PartialEq)]
pub struct EncryptedFieldConfig {
    /// `escCollection`.
    pub esc: String,
    /// `eccCollection`, kept as given (version 1 only).
    pub ecc: Option<String>,
    /// `ecocCollection`.
    pub ecoc: String,
    /// `fields`.
    pub fields: Vec<EncryptedField>,
    /// `strEncodeVersion`.
    pub str_encode_version: Option<i32>,
}

/// The default name of the encrypted state collection of `coll`.
pub fn esc_name(coll: &str) -> String {
    format!("{METADATA_PREFIX}{coll}{ESC_SUFFIX}")
}

/// The default name of the encrypted compaction collection of `coll`.
pub fn ecoc_name(coll: &str) -> String {
    format!("{METADATA_PREFIX}{coll}{ECOC_SUFFIX}")
}

/// Whether `coll` is the name of a metadata collection.
pub fn is_metadata(coll: &str) -> bool {
    coll.starts_with(METADATA_PREFIX)
}

const TOP: [&str; 5] =
    ["escCollection", "eccCollection", "ecocCollection", "fields", "strEncodeVersion"];
const FIELD: [&str; 4] = ["keyId", "path", "bsonType", "queries"];
const QUERY: [&str; 12] = [
    "queryType",
    "contention",
    "min",
    "max",
    "sparsity",
    "precision",
    "trimFactor",
    "strMaxLength",
    "strMinQueryLength",
    "strMaxQueryLength",
    "caseSensitive",
    "diacriticSensitive",
];

/// A string field.
fn string<'a>(f: &Fields<'a>, k: &str) -> Result<Option<&'a str>, FleError> {
    match f.get(k) {
        None => Ok(None),
        Some(RawValue::String(s)) => Ok(Some(s)),
        Some(v) => Err(wrong_type(&f.path(k), &v, &[t::STRING])),
    }
}

/// A number the IDL type `safeInt64` or `safeInt` takes: any number with an integral value.
fn safe_int(f: &Fields<'_>, k: &str) -> Result<Option<i64>, FleError> {
    let v = match f.get(k) {
        None => return Ok(None),
        Some(v) => v,
    };
    let fractional =
        |shown: String| Err(err(FAILED_TO_PARSE, format!("Expected an integer: {k}: {shown}")));
    match v {
        RawValue::Int32(n) => Ok(Some(i64::from(n))),
        RawValue::Int64(n) => Ok(Some(n)),
        RawValue::Double(d) if d.fract() == 0.0 && d.is_finite() => Ok(Some(d as i64)),
        RawValue::Double(d) => fractional(format!("{d}")),
        RawValue::Decimal128(d) => match crate::values::decimal_to_i64(d) {
            Some(n) => Ok(Some(n)),
            None => fractional(String::from("decimal")),
        },
        other => {
            Err(wrong_type(&f.path(k), &other, &[t::INT64, t::INT32, t::DOUBLE, t::DECIMAL128]))
        }
    }
}

/// A `safeInt` that must be above (`strict`) or at least `floor`.
fn bounded(f: &Fields<'_>, k: &str, floor: i64, strict: bool) -> Result<Option<i64>, FleError> {
    let Some(n) = safe_int(f, k)? else { return Ok(None) };
    let fails = if strict { n <= floor } else { n < floor };
    if fails {
        let op = if strict { ">" } else { ">=" };
        return Err(err(
            BAD_VALUE,
            format!("BSON field '{k}' value must be {op} {floor}, actual value '{n}'"),
        ));
    }
    Ok(Some(n))
}

/// `n`, if it is at most `ceiling`.
fn at_most(n: Option<i64>, k: &str, ceiling: i64) -> Result<Option<i64>, FleError> {
    match n {
        Some(n) if n > ceiling => Err(err(
            BAD_VALUE,
            format!("BSON field '{k}' value must be <= {ceiling}, actual value '{n}'"),
        )),
        n => Ok(n),
    }
}

fn int32(n: Option<i64>) -> Option<i32> {
    n.map(|n| i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX }))
}

fn read_query(doc: RawDoc<'_>, ctx: &str) -> Result<Query, FleError> {
    let f = Fields::read(doc, ctx, &QUERY)?;
    let kind = match string(&f, "queryType")? {
        None => return Err(crate::idl::missing(&f.path("queryType"))),
        Some(EQUALITY) => QueryType::Equality,
        Some(RANGE) => QueryType::Range,
        Some(PREFIX) => QueryType::Prefix,
        Some(SUFFIX) => QueryType::Suffix,
        Some(SUBSTRING) => QueryType::Substring,
        Some(RANGE_PREVIEW) => {
            return Err(err(
                crate::error::RANGE_PREVIEW,
                "Cannot create a collection with an encrypted field with query type rangePreview, \
                 as it is deprecated",
            ));
        }
        Some(p) if TEXT_PREVIEWS.contains(&p) => {
            return Err(err(
                TEXT_PREVIEW,
                format!(
                    "Cannot create a collection with an encrypted field with query type {p}, as \
                     it is deprecated"
                ),
            ));
        }
        Some(other) => {
            return Err(err(
                BAD_VALUE,
                format!(
                    "Enumeration value '{other}' for field '{}' is not a valid value.",
                    f.path("queryType")
                ),
            ));
        }
    };
    let contention = bounded(&f, "contention", 0, false)?.unwrap_or(DEFAULT_CONTENTION);
    if contention > MAX_CONTENTION {
        return Err(err(
            BAD_VALUE,
            format!(
                "contention factor ({contention}) exceeds the maximum allowed value \
                 ({MAX_CONTENTION})"
            ),
        ));
    }
    Ok(Query {
        kind,
        contention,
        min: f.get("min").map(Value::from),
        max: f.get("max").map(Value::from),
        sparsity: at_most(bounded(&f, "sparsity", 1, false)?, "sparsity", MAX_SPARSITY)?,
        precision: int32(bounded(&f, "precision", 0, false)?),
        trim_factor: int32(bounded(&f, "trimFactor", 0, false)?),
        str_max_length: int32(bounded(&f, "strMaxLength", 0, true)?),
        str_min_query_length: int32(bounded(&f, "strMinQueryLength", 0, true)?),
        str_max_query_length: int32(bounded(&f, "strMaxQueryLength", 0, true)?),
        case_sensitive: f.boolean("caseSensitive")?,
        diacritic_sensitive: f.boolean("diacriticSensitive")?,
    })
}

fn read_field(doc: RawDoc<'_>, ctx: &str) -> Result<EncryptedField, FleError> {
    let f = Fields::read(doc, ctx, &FIELD)?;
    let key_id = match f.get("keyId") {
        None => None,
        Some(RawValue::Binary { subtype: t::BINARY_UUID, bytes }) if bytes.len() == 16 => {
            let mut k = [0u8; 16];
            k.copy_from_slice(bytes);
            Some(k)
        }
        Some(RawValue::Binary { subtype, .. }) => {
            return Err(err(
                TYPE_MISMATCH,
                format!(
                    "BSON field '{}' is the wrong binData type '{}', expected type 'UUID'",
                    f.path("keyId"),
                    crate::idl::subtype_name(subtype)
                ),
            ));
        }
        Some(v) => return Err(wrong_type(&f.path("keyId"), &v, &[t::BINARY])),
    };
    let path = string(&f, "path")?;
    let bson_type = string(&f, "bsonType")?;
    let queries_ctx = f.path("queries");
    let queries = match f.get("queries") {
        None => None,
        Some(RawValue::Document(d)) => Some((vec![read_query(d, &queries_ctx)?], false)),
        Some(RawValue::Array(a)) => {
            let mut list = Vec::new();
            for v in a.iter() {
                match v {
                    RawValue::Document(d) => list.push(read_query(d, &queries_ctx)?),
                    other => return Err(wrong_type(&queries_ctx, &other, &[t::DOCUMENT])),
                }
            }
            Some((list, true))
        }
        Some(v) => {
            return Err(err(
                TYPE_MISMATCH,
                format!(
                    "BSON field '{queries_ctx}' is the wrong type '{}', expected types '[]'",
                    name_of(v.element_type())
                ),
            ));
        }
    };
    f.require(&["keyId", "path"])?;
    let bson_type = match bson_type {
        None => None,
        Some(alias) => match by_alias(alias) {
            Some(TypeSpec::Element(e)) => Some((e, alias.to_string())),
            _ => return Err(err(BAD_VALUE, format!("Unknown type name: {alias}"))),
        },
    };
    Ok(EncryptedField {
        key_id: key_id.unwrap_or_default(),
        path: path.unwrap_or_default().to_string(),
        bson_type,
        queries,
    })
}

/// Reads `encryptedFields` of `create` (`ctx` is `create.encryptedFields`).
pub fn read(doc: RawDoc<'_>, ctx: &str) -> Result<EncryptedFieldConfig, FleError> {
    let f = Fields::read(doc, ctx, &TOP)?;
    let esc = string(&f, "escCollection")?.map(str::to_string);
    let ecc = string(&f, "eccCollection")?.map(str::to_string);
    let ecoc = string(&f, "ecocCollection")?.map(str::to_string);
    let fields_ctx = f.path("fields");
    let fields = match f.get("fields") {
        None => None,
        Some(RawValue::Array(a)) => {
            let mut list = Vec::new();
            for (i, v) in a.iter().enumerate() {
                match v {
                    RawValue::Document(d) => list.push(read_field(d, &fields_ctx)?),
                    other => {
                        return Err(wrong_type(
                            &format!("{fields_ctx}.{i}"),
                            &other,
                            &[t::DOCUMENT],
                        ));
                    }
                }
            }
            Some(list)
        }
        Some(v) => return Err(wrong_type(&fields_ctx, &v, &[t::ARRAY])),
    };
    let str_encode_version = match f.get("strEncodeVersion") {
        None => None,
        Some(RawValue::Int32(n)) if n > 0 => Some(n),
        Some(RawValue::Int32(n)) => {
            return Err(err(
                BAD_VALUE,
                format!("BSON field 'strEncodeVersion' value must be > 0, actual value '{n}'"),
            ));
        }
        Some(v) => return Err(wrong_type(&f.path("strEncodeVersion"), &v, &[t::INT32])),
    };
    f.require(&["fields"])?;
    Ok(EncryptedFieldConfig {
        esc: esc.unwrap_or_default(),
        ecc,
        ecoc: ecoc.unwrap_or_default(),
        fields: fields.unwrap_or_default(),
        str_encode_version,
    })
}

/// Reads, checks and fills in the `encryptedFields` of `create` on collection `coll`: what the
/// catalog keeps.
pub fn for_create(coll: &str, doc: RawDoc<'_>) -> Result<EncryptedFieldConfig, FleError> {
    let mut c = read(doc, "create.encryptedFields")?;
    if c.esc.is_empty() {
        c.esc = esc_name(coll);
    } else if !c.esc.starts_with(METADATA_PREFIX) || !c.esc.ends_with(ESC_SUFFIX) {
        return Err(err(
            ESC_NAME,
            "Encrypted State Collection name should follow enxcol_.<collection>.esc naming pattern",
        ));
    }
    if c.ecoc.is_empty() {
        c.ecoc = ecoc_name(coll);
    } else if !c.ecoc.starts_with(METADATA_PREFIX) || !c.ecoc.ends_with(ECOC_SUFFIX) {
        return Err(err(
            ECOC_NAME,
            "Encrypted Compaction Collection name should follow enxcol_.<collection>.ecoc naming \
             pattern",
        ));
    }
    check(&c)?;
    Ok(c)
}

/// The checks of a configuration beyond its IDL, in the order of a `refnode` 9.0.4: first each
/// field's type and queries, all fields, then for each field its key id, its path and how it
/// meets the paths before it (measured with fields that break two rules, `create-two-rules.json`).
pub fn check(c: &EncryptedFieldConfig) -> Result<(), FleError> {
    for field in &c.fields {
        check_field(field)?;
    }
    let mut keys: Vec<[u8; 16]> = Vec::new();
    let mut paths: Vec<&str> = Vec::new();
    for field in &c.fields {
        if keys.contains(&field.key_id) {
            return Err(err(DUPLICATE_KEY_IDS, "Duplicate key ids are not allowed"));
        }
        keys.push(field.key_id);
        if field.path.is_empty() {
            return Err(err(EMPTY_PATH, "Encrypted field must have a non-empty path"));
        }
        if field.path == "_id" || field.path.starts_with("_id.") {
            return Err(err(ENCRYPTED_ID, "Cannot encrypt _id or its subfields"));
        }
        for other in &paths {
            if *other == field.path {
                return Err(err(DUPLICATE_PATHS, "Duplicate paths are not allowed"));
            }
            let (short, long) = if other.len() < field.path.len() {
                (*other, field.path.as_str())
            } else {
                (field.path.as_str(), *other)
            };
            if long.starts_with(short) && long.as_bytes().get(short.len()) == Some(&b'.') {
                return Err(err(
                    CONFLICTING_PATHS,
                    format!(
                        "Conflicting index paths found as one is a prefix of another '{}' and '{}'",
                        field.path, other
                    ),
                ));
            }
        }
        paths.push(&field.path);
    }
    Ok(())
}

/// The types an unindexed field cannot have.
const NOT_UNINDEXED: [u8; 4] = [t::MIN_KEY, t::MAX_KEY, t::NULL, t::UNDEFINED];
/// The types an equality field can have.
const EQUALITY_TYPES: [u8; 13] = [
    t::STRING,
    t::BINARY,
    t::OBJECT_ID,
    t::BOOLEAN,
    t::DATE_TIME,
    t::REGEX,
    t::DB_POINTER,
    t::JAVASCRIPT,
    t::SYMBOL,
    t::INT32,
    t::TIMESTAMP,
    t::INT64,
    t::JAVASCRIPT_WITH_SCOPE,
];
/// The types a range field can have.
const RANGE_TYPES: [u8; 5] = [t::DOUBLE, t::DATE_TIME, t::INT32, t::INT64, t::DECIMAL128];

fn check_field(field: &EncryptedField) -> Result<(), FleError> {
    let Some((queries, _)) = &field.queries else {
        if let Some((ty, alias)) = &field.bson_type
            && NOT_UNINDEXED.contains(ty)
        {
            return Err(err(
                UNINDEXED_TYPE,
                format!("Type '{alias}' is not a supported unindexed type"),
            ));
        }
        return Ok(());
    };
    let Some((ty, alias)) = &field.bson_type else {
        return Err(err(
            INDEXED_WITHOUT_TYPE,
            "BSON type needs to be specified for an indexed field",
        ));
    };
    if queries.is_empty() {
        return Err(err(NO_QUERY_TYPE, "At least one query type should be specified per field"));
    }
    if queries.len() > 1
        && !queries.iter().all(|q| matches!(q.kind, QueryType::Prefix | QueryType::Suffix))
    {
        return Err(err(
            MULTIPLE_QUERY_TYPES,
            "Multiple query types may only include the suffix and prefix query types",
        ));
    }
    if queries.len() > 1 && queries[0].kind == queries[1].kind {
        return Err(err(
            MULTIPLE_QUERY_TYPES,
            "Multiple query types may only include the suffix and prefix query types",
        ));
    }
    for q in queries {
        match q.kind {
            QueryType::Equality => check_equality(q, *ty, alias)?,
            QueryType::Range => check_range(q, *ty, alias, &field.path)?,
            _ => check_text(q, *ty, alias, &field.path)?,
        }
    }
    check_text_family(queries, &field.path)
}

fn not_allowed(code: (i32, &'static str), name: &str, index: &str) -> FleError {
    err(code, format!("The field '{name}' is not allowed for {index} index but is present"))
}

fn check_equality(q: &Query, ty: u8, alias: &str) -> Result<(), FleError> {
    // The type first, then the options (measured: `create-two-rules.json`).
    if !EQUALITY_TYPES.contains(&ty) {
        return Err(err(
            EQUALITY_TYPE,
            format!("Type '{alias}' is not a supported equality indexed type"),
        ));
    }
    for (name, present) in [("min", q.min.is_some()), ("max", q.max.is_some())] {
        if present {
            return Err(not_allowed(EQUALITY_BOUND, name, "equality"));
        }
    }
    for (name, present) in [
        ("sparsity", q.sparsity.is_some()),
        ("precision", q.precision.is_some()),
        ("trimFactor", q.trim_factor.is_some()),
    ] {
        if present {
            return Err(not_allowed(EQUALITY_OPTION, name, "equality"));
        }
    }
    if let Some(name) = text_option(q) {
        return Err(not_allowed(EQUALITY_TEXT_OPTION, name, "equality"));
    }
    Ok(())
}

fn text_option(q: &Query) -> Option<&'static str> {
    [
        ("strMaxLength", q.str_max_length.is_some()),
        ("strMinQueryLength", q.str_min_query_length.is_some()),
        ("strMaxQueryLength", q.str_max_query_length.is_some()),
        ("caseSensitive", q.case_sensitive.is_some()),
        ("diacriticSensitive", q.diacritic_sensitive.is_some()),
    ]
    .into_iter()
    .find_map(|(n, p)| p.then_some(n))
}

fn check_range(q: &Query, ty: u8, alias: &str, path: &str) -> Result<(), FleError> {
    if let Some(name) = text_option(q) {
        return Err(not_allowed(RANGE_TEXT_OPTION, name, "range"));
    }
    if !RANGE_TYPES.contains(&ty) {
        return Err(err(
            RANGE_TYPE,
            format!("Type '{alias}' is not a supported range indexed type"),
        ));
    }
    for (name, bound) in [("min", &q.min), ("max", &q.max)] {
        if let Some(v) = bound
            && v.element_type() != ty
        {
            return Err(err(
                RANGE_BOUND_TYPE,
                format!(
                    "Range field type '{alias}' does not match the {name} value type '{}'",
                    name_of(v.element_type())
                ),
            ));
        }
    }
    let floating = matches!(ty, t::DOUBLE | t::DECIMAL128);
    if floating && q.precision.is_some() != (q.min.is_some() && q.max.is_some())
        || floating && q.precision.is_none() && (q.min.is_some() || q.max.is_some())
    {
        return Err(err(
            RANGE_PRECISION_TRIO,
            "Precision, min, and max must all be specified together for floating point fields",
        ));
    }
    if let (Some(min), Some(max)) = (&q.min, &q.max)
        && crate::values::compare(min, max) != std::cmp::Ordering::Less
    {
        return Err(err(MIN_NOT_BELOW_MAX, "Min must be less than max."));
    }
    if let (Some(p), Some(min), Some(max)) = (q.precision, &q.min, &q.max) {
        crate::range::check_precision(ty, (min, max), p, path)?;
    }
    if let Some(trim) = q.trim_factor {
        let bits = crate::range::domain_bits(ty, q.min.as_ref(), q.max.as_ref(), q.precision);
        if u32::try_from(trim).is_ok_and(|t| t >= bits) {
            return Err(err(
                TRIM_FACTOR,
                format!(
                    "The field 'trimFactor' must be >= 0 and less than the total number of bits \
                     needed to represent elements in the domain ({bits})"
                ),
            ));
        }
        let sum = i64::from(trim) + q.sparsity.unwrap_or(DEFAULT_SPARSITY);
        if bits > LARGE_DOMAIN_BITS && sum >= QUERY_TOO_LARGE_FROM {
            return Err(err(
                QUERY_TOO_LARGE,
                "Sparsity and trimFactor together are too large and could create queries that \
                 exceed the BSON size limit",
            ));
        }
        if bits > LARGE_DOMAIN_BITS && sum >= QUERY_TOO_LARGE_BOUNDS_FROM {
            return Err(err(
                QUERY_TOO_LARGE_BOUNDS,
                "Sparsity, trimFactor, min, and max together are too large and could create \
                 queries that exceed the BSON size limit",
            ));
        }
    }
    Ok(())
}

fn check_text(q: &Query, ty: u8, alias: &str, path: &str) -> Result<(), FleError> {
    for (name, present) in [
        ("min", q.min.is_some()),
        ("max", q.max.is_some()),
        ("sparsity", q.sparsity.is_some()),
        ("precision", q.precision.is_some()),
        ("trimFactor", q.trim_factor.is_some()),
    ] {
        if present {
            return Err(not_allowed(TEXT_RANGE_OPTION, name, "text-based"));
        }
    }
    if ty != t::STRING {
        return Err(err(
            TEXT_TYPE,
            format!(
                "Type '{alias}' is not a supported type for text search indexed encrypted field {path}"
            ),
        ));
    }
    let kind = q.kind.name();
    let required = |name: &str, code: (i32, &'static str)| {
        err(code, format!("{name} parameter is required for {kind} query type of field {path}"))
    };
    let lb = q
        .str_min_query_length
        .ok_or_else(|| required("strMinQueryLength", TEXT_PARAMETER_MISSING))?;
    let ub =
        q.str_max_query_length.ok_or_else(|| required("strMaxQueryLength", TEXT_MAX_MISSING))?;
    if q.case_sensitive.is_none() {
        return Err(required("caseSensitive", TEXT_CASE_MISSING));
    }
    if q.diacritic_sensitive.is_none() {
        return Err(required("diacriticSensitive", TEXT_DIACRITIC_MISSING));
    }
    if lb > ub {
        return Err(err(
            MIN_ABOVE_MAX_QUERY,
            "strMinQueryLength cannot be greater than strMaxQueryLength",
        ));
    }
    if q.kind != QueryType::Substring {
        return Ok(());
    }
    let Some(len) = q.str_max_length else {
        return Err(required("strMaxLength", SUBSTRING_LENGTH_MISSING));
    };
    if ub > len {
        return Err(err(
            QUERY_ABOVE_LENGTH,
            "strMaxQueryLength cannot be greater than strMaxLength",
        ));
    }
    Ok(())
}

/// The worst case number of tags of a text field: the exact one, one per prefix and suffix
/// length, and one per substring of each length.
fn worst_tags(queries: &[Query]) -> u64 {
    let mut n = 1u64;
    for q in queries {
        let lb = u64::try_from(q.str_min_query_length.unwrap_or(1)).unwrap_or(1);
        let ub = u64::try_from(q.str_max_query_length.unwrap_or(1)).unwrap_or(1);
        match q.kind {
            QueryType::Prefix | QueryType::Suffix => n += ub.saturating_sub(lb) + 1,
            QueryType::Substring => {
                let len = u64::try_from(q.str_max_length.unwrap_or(0)).unwrap_or(0);
                for k in lb..=ub.min(len) {
                    n = n.saturating_add(len - k + 1);
                }
            }
            QueryType::Equality | QueryType::Range => {}
        }
    }
    n
}

fn check_text_family(queries: &[Query], path: &str) -> Result<(), FleError> {
    if !queries.first().is_some_and(|q| q.kind.is_text()) {
        return Ok(());
    }
    let first = &queries[0];
    for q in &queries[1..] {
        let differs = |name: &str, code: (i32, &'static str)| {
            err(
                code,
                format!("{name} parameter must be the same for all query types of field {path}"),
            )
        };
        if q.case_sensitive != first.case_sensitive {
            return Err(differs("caseSensitive", CASE_DIFFERS));
        }
        if q.diacritic_sensitive != first.diacritic_sensitive {
            return Err(differs("diacriticSensitive", DIACRITIC_DIFFERS));
        }
        if q.contention != first.contention {
            return Err(differs("contention", CONTENTION_DIFFERS));
        }
    }
    let worst = worst_tags(queries);
    if worst > MAX_TAGS {
        return Err(err(
            TAG_LIMIT,
            format!(
                "Queryable Encryption tag limit exceeded for field '{path}'. Worst case tag count \
                 is {worst}"
            ),
        ));
    }
    if first.kind == QueryType::Substring {
        let (lb, ub) =
            (first.str_min_query_length.unwrap_or(0), first.str_max_query_length.unwrap_or(0));
        let len = first.str_max_length.unwrap_or(0);
        let kind = SUBSTRING;
        if lb < MIN_SUBSTRING_QUERY {
            return Err(err(
                SUBSTRING_MIN,
                format!(
                    "strMinQueryLength ({lb}) must be >= {MIN_SUBSTRING_QUERY} for {kind} query \
                     type of field {path}."
                ),
            ));
        }
        if ub > MAX_SUBSTRING_QUERY {
            return Err(err(
                SUBSTRING_MAX,
                format!(
                    "strMaxQueryLength ({ub}) must be <= {MAX_SUBSTRING_QUERY} for {kind} query \
                     type of field {path}."
                ),
            ));
        }
        if len > MAX_SUBSTRING_LENGTH {
            return Err(err(
                SUBSTRING_LENGTH,
                format!(
                    "strMaxLength ({len}) must be <= {MAX_SUBSTRING_LENGTH} for {kind} query type \
                     of field {path}."
                ),
            ));
        }
    }
    Ok(())
}

impl Query {
    fn to_document(&self) -> Document {
        let mut d = Document::new().with("queryType", self.kind.name());
        d.push("contention", self.contention);
        let opt = |d: &mut Document, k: &str, v: Option<Value>| {
            if let Some(v) = v {
                d.push(k, v);
            }
        };
        opt(&mut d, "min", self.min.clone());
        opt(&mut d, "max", self.max.clone());
        opt(&mut d, "sparsity", self.sparsity.map(Value::Int64));
        opt(&mut d, "precision", self.precision.map(Value::Int32));
        opt(&mut d, "trimFactor", self.trim_factor.map(Value::Int32));
        opt(&mut d, "strMaxLength", self.str_max_length.map(Value::Int32));
        opt(&mut d, "strMinQueryLength", self.str_min_query_length.map(Value::Int32));
        opt(&mut d, "strMaxQueryLength", self.str_max_query_length.map(Value::Int32));
        opt(&mut d, "caseSensitive", self.case_sensitive.map(Value::Boolean));
        opt(&mut d, "diacriticSensitive", self.diacritic_sensitive.map(Value::Boolean));
        d
    }
}

impl EncryptedFieldConfig {
    /// The option as the catalog keeps it and `listCollections` shows it.
    pub fn to_document(&self) -> Document {
        let mut d = Document::new().with("escCollection", self.esc.as_str());
        if let Some(ecc) = &self.ecc {
            d.push("eccCollection", ecc.as_str());
        }
        d.push("ecocCollection", self.ecoc.as_str());
        let fields: Vec<Value> = self
            .fields
            .iter()
            .map(|f| {
                let mut fd = Document::new()
                    .with(
                        "keyId",
                        Value::Binary { subtype: t::BINARY_UUID, bytes: f.key_id.to_vec() },
                    )
                    .with("path", f.path.as_str());
                if let Some((_, alias)) = &f.bson_type {
                    fd.push("bsonType", alias.as_str());
                }
                if let Some((queries, array)) = &f.queries {
                    let docs: Vec<Value> =
                        queries.iter().map(|q| Value::Document(q.to_document())).collect();
                    match (array, docs.as_slice()) {
                        (false, [one]) => fd.push("queries", one.clone()),
                        _ => fd.push("queries", docs),
                    }
                }
                Value::Document(fd)
            })
            .collect();
        d.push("fields", fields);
        if let Some(v) = self.str_encode_version {
            d.push("strEncodeVersion", v);
        }
        d
    }

    /// The encrypted field at `path`, if any.
    pub fn field(&self, path: &str) -> Option<&EncryptedField> {
        self.fields.iter().find(|f| f.path == path)
    }
}
