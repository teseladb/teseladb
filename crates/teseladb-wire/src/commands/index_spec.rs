//! Index specifications as a `refrouter` 9.0.4 checks them (measured): the checks of any index, then
//! those of the `_id` index that `create` takes in `idIndex` (what `refrestore` sends).

use teseladb_bson::{Document, Value};

use super::{display, partial_filter};
use crate::error::{
    BAD_VALUE, CANNOT_CREATE_INDEX, CLUSTERED_INDEX_MISMATCH, CommandError, FAILED_TO_PARSE,
    INVALID_INDEX_SPECIFICATION_OPTION, TYPE_MISMATCH,
};
use crate::types::type_name;

const KEY: &str = "key";
const NAME: &str = "name";
const VERSION: &str = "v";
const COLLATION: &str = "collation";
const WEIGHTS: &str = "weights";
const WILDCARD_PROJECTION: &str = "wildcardProjection";
const CLUSTERED: &str = "clustered";
/// The option that passes settings to the storage engine.
const STORAGE_ENGINE: &str = "storageEngine";
const UNIQUE: &str = "unique";
const PARTIAL: &str = "partialFilterExpression";
const PREPARE_UNIQUE: &str = "prepareUnique";
/// The hash buckets of an index, TeselaDB's own option (D-008, DV-008).
pub const BUCKETS: &str = "teselaBuckets";
/// The options read as booleans: `true`, `false` or a number (measured on a `refrouter` 9.0.4).
const BOOLEANS: [&str; 4] = ["background", UNIQUE, "sparse", PREPARE_UNIQUE];

/// The version of an index without `v`.
pub const DEFAULT_VERSION: i32 = 2;
/// The versions an index can be created with.
const VERSIONS: [i32; 2] = [1, DEFAULT_VERSION];
/// The first version that takes a collation.
const COLLATION_VERSION: i32 = 2;

/// The fields any index specification may have.
const FIELDS: [&str; 29] = [
    KEY,
    NAME,
    "ns",
    VERSION,
    COLLATION,
    "background",
    UNIQUE,
    "hidden",
    "partialFilterExpression",
    "sparse",
    "expireAfterSeconds",
    "storageEngine",
    WEIGHTS,
    "default_language",
    "language_override",
    "textIndexVersion",
    "2dsphereIndexVersion",
    "bits",
    "min",
    "max",
    "bucketSize",
    WILDCARD_PROJECTION,
    "coarsestIndexedLevel",
    "finestIndexedLevel",
    "dropDups",
    "originalSpec",
    CLUSTERED,
    PREPARE_UNIQUE,
    BUCKETS,
];

/// The fields of an `_id` index specification.
const ID_FIELDS: [&str; 5] = [KEY, NAME, "ns", VERSION, COLLATION];

/// The type a field must have, as RefDb names it in the message.
#[derive(Clone, Copy)]
enum Kind {
    Object,
    String,
    Bool,
}

impl Kind {
    fn holds(self, v: &Value) -> bool {
        matches!(
            (self, v),
            (Self::Object, Value::Document(_))
                | (Self::String, Value::String(_))
                | (Self::Bool, Value::Boolean(_))
        )
    }

    fn article_and_name(self) -> &'static str {
        match self {
            Self::Object => "an object",
            Self::String => "a string",
            Self::Bool => "a bool",
        }
    }
}

/// The fields with a type of their own (`v` is any number, read apart).
const TYPED: [(&str, Kind); 9] = [
    (KEY, Kind::Object),
    (NAME, Kind::String),
    (COLLATION, Kind::Object),
    ("hidden", Kind::Bool),
    ("partialFilterExpression", Kind::Object),
    (WEIGHTS, Kind::Object),
    ("default_language", Kind::String),
    ("language_override", Kind::String),
    ("originalSpec", Kind::Object),
];

/// The index plugins a key pattern may name.
const PLUGINS: [&str; 5] = ["2d", "2dsphere", "2dsphere_bucket", "text", "hashed"];
/// The option of TTL indexes.
const EXPIRE_AFTER_SECONDS: &str = "expireAfterSeconds";
/// The plugin of text indexes.
const TEXT: &str = "text";
/// The field of a wildcard index (`$**` or `path.$**`).
const WILDCARD: &str = "$**";

fn spec_text(spec: &Document) -> String {
    display(&Value::Document(spec.clone()))
}

/// The specification with its version, as RefDb shows it once it has added the default one.
fn with_version(spec: &Document) -> String {
    let mut shown = spec.clone();
    if shown.get(VERSION).is_none() {
        shown.push(VERSION, DEFAULT_VERSION);
    }
    spec_text(&shown)
}

fn invalid(spec: &Document, why: &str) -> CommandError {
    let msg = format!("Invalid index specification {}; {why}", spec_text(spec));
    CommandError::new(CANNOT_CREATE_INDEX, msg)
}

/// Checks the general rules of index specification `spec` and gives its version.
pub fn check(spec: &Document) -> Result<i32, CommandError> {
    if let Some((field, _)) = spec.iter().find(|(k, _)| !FIELDS.contains(k)) {
        let msg = format!(
            "The field '{field}' is not valid for an index specification. Specification: {}",
            spec_text(spec)
        );
        return Err(CommandError::new(INVALID_INDEX_SPECIFICATION_OPTION, msg));
    }
    let mut version = DEFAULT_VERSION;
    for (field, v) in spec.iter() {
        if let Some((_, kind)) = TYPED.iter().find(|(f, _)| *f == field)
            && !kind.holds(v)
        {
            let msg = format!(
                "The field '{field}' must be {}, but got {}",
                kind.article_and_name(),
                type_name(v)
            );
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
        if BOOLEANS.contains(&field) && crate::types::truthy(v).is_none() {
            let msg = format!(
                "The field '{field} has value {field}: {}, which is not convertible to bool",
                display(v)
            );
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
        match (field, v) {
            (KEY, Value::Document(k)) => check_key_pattern(k)?,
            (VERSION, v) => version = check_version(spec, v)?,
            (BUCKETS, v) => {
                buckets(v)?;
            }
            (COLLATION, Value::Document(c)) if c.is_empty() => {
                let msg = "The field 'collation' cannot be an empty object.";
                return Err(CommandError::new(BAD_VALUE, msg));
            }
            _ => {}
        }
    }
    for required in [KEY, NAME] {
        if spec.get(required).is_none() {
            let msg =
                format!("The '{required}' field is a required property of an index specification");
            return Err(CommandError::new(FAILED_TO_PARSE, msg));
        }
    }
    check_options(spec, version)?;
    check_wildcard(spec, version)?;
    Ok(version)
}

/// The key, the `wildcardProjection` and the options of wildcard specification `spec`, as
/// `teseladb-index` reads them.
fn wildcard_rules(
    spec: &Document,
) -> Result<Result<(), teseladb_index::keygen::wildcard::SpecError>, CommandError> {
    use teseladb_index::keygen::wildcard::{WildcardProjection, check_key, check_options};
    let bytes = spec.to_bytes()?;
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes)?;
    if let Err(e) = teseladb_index::spec::index_def(raw, 0) {
        return Ok(Err(e));
    }
    let Some(Value::Document(key)) = spec.get(KEY) else { return Ok(Ok(())) };
    let is_wildcard = |f: &str| f == "$**" || f.ends_with(".$**");
    let fields: Vec<(&str, bool)> = key.iter().map(|(f, _)| (f, is_wildcard(f))).collect();
    let plugin = key.iter().any(|(_, v)| matches!(v, Value::String(_)));
    let projection = match raw.get("wildcardProjection") {
        Some(teseladb_bson::RawValue::Document(p)) => {
            match WildcardProjection::parse(p, &|_| false) {
                Ok(p) => Some(p),
                Err(e) => return Ok(Err(e)),
            }
        }
        _ => None,
    };
    if let Err(e) = check_key(&fields, plugin, projection.as_ref()) {
        return Ok(Err(e));
    }
    let flag = |k: &str| spec.get(k).and_then(crate::types::truthy).unwrap_or(false);
    Ok(check_options(flag("unique"), flag("sparse"), spec.get("expireAfterSeconds").is_some()))
}

/// The rules of a wildcard index (key, `wildcardProjection` and the options it does not take),
/// checked with the specification as RefDb does, before any build.
fn check_wildcard(spec: &Document, version: i32) -> Result<(), CommandError> {
    let wildcard = match spec.get(KEY) {
        Some(Value::Document(k)) => k.iter().any(|(f, _)| f == "$**" || f.ends_with(".$**")),
        _ => false,
    };
    if !wildcard {
        return Ok(());
    }
    let Err(e) = wildcard_rules(spec)? else { return Ok(()) };
    let code = crate::error::code(e.code).unwrap_or(crate::error::BAD_VALUE);
    // An option the index does not take is found once the specification has its version.
    let options = ["does not support the", "cannot make a TTL index"];
    if !options.iter().any(|o| e.reason.contains(o)) || spec.get(VERSION).is_some() {
        return Err(CommandError::new(code, e.reason));
    }
    let shown = display(&Value::Document(spec.clone().with(VERSION, version)));
    Err(CommandError::new(
        code,
        format!("Error in specification {shown} :: caused by :: {}", e.reason),
    ))
}

/// The rules between the fields of a specification whose fields have their types.
fn check_options(spec: &Document, version: i32) -> Result<(), CommandError> {
    if spec.get(COLLATION).is_some() && version < COLLATION_VERSION {
        let why = format!("cannot create an index with the '{COLLATION}' option and v={version}");
        return Err(invalid(spec, &why));
    }
    if spec.get(NAME) == Some(&Value::from("")) {
        let msg = format!(
            "Error in specification {} :: caused by :: index name cannot be empty",
            with_version(spec)
        );
        return Err(CommandError::new(CANNOT_CREATE_INDEX, msg));
    }
    // Measured on a `refrouter` 9.0.4 on 05-11-2026.
    if matches!(spec.get(STORAGE_ENGINE), Some(Value::Document(d)) if d.is_empty()) {
        let msg = format!(
            "Error in specification {} :: caused by :: Empty \"storageEngine\" options are invalid. \
             Please remove the field or include valid options.",
            with_version(spec)
        );
        return Err(CommandError::new(CANNOT_CREATE_INDEX, msg));
    }
    let key = match spec.get(KEY) {
        Some(Value::Document(k)) => k.clone(),
        _ => Document::new(),
    };
    let set = |f: &str| spec.get(f).and_then(crate::types::truthy) == Some(true);
    if set(UNIQUE) && set(PREPARE_UNIQUE) {
        let why = "cannot create an index with the 'unique' option and the 'prepareUnique' option";
        return Err(invalid(spec, why));
    }
    if spec.get(WEIGHTS).is_some() && !key.iter().any(|(_, v)| v == &Value::from(TEXT)) {
        return Err(invalid(spec, "the field 'weights' can only be specified with text indexes"));
    }
    if spec.get(EXPIRE_AFTER_SECONDS).is_some() && key.len() > 1 {
        let msg = format!(
            "TTL indexes are single-field indexes, compound indexes do not support TTL. Index \
             spec: {}",
            spec_text(spec)
        );
        return Err(CommandError::new(CANNOT_CREATE_INDEX, msg));
    }
    if let Some(Value::Document(filter)) = spec.get(PARTIAL) {
        check_partial(spec, filter)?;
    }
    let wildcard = key.iter().any(|(k, _)| k == WILDCARD || k.ends_with(".$**"));
    if spec.get(WILDCARD_PROJECTION).is_some() && !wildcard {
        let msg = "The field 'wildcardProjection' is only allowed in 'wildcard' indexes";
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    if spec.get(CLUSTERED).is_some() {
        if spec.get(UNIQUE).is_none() {
            let msg = "The 'unique' field is required when 'clustered' is specified";
            return Err(CommandError::new(CANNOT_CREATE_INDEX, msg));
        }
        // No collection of TeselaDB is clustered.
        let msg = format!(
            "Error in specification {} :: caused by :: Cannot create index with option 'clustered' that does not match an existing clustered index",
            with_version(spec)
        );
        return Err(CommandError::new(CLUSTERED_INDEX_MISMATCH, msg));
    }
    Ok(())
}

fn check_key_pattern(key: &Document) -> Result<(), CommandError> {
    if key.is_empty() {
        return Err(CommandError::new(CANNOT_CREATE_INDEX, "Index keys cannot be empty."));
    }
    for (_, v) in key.iter() {
        if let Value::String(plugin) = v
            && !PLUGINS.contains(&plugin.as_str())
        {
            let msg = format!("Unknown index plugin '{plugin}'");
            return Err(CommandError::new(CANNOT_CREATE_INDEX, msg));
        }
    }
    Ok(())
}

/// The filter of a partial index (009 FR-008), with the errors of a `refrouter` 9.0.4 (measured):
/// a parse error shows the specification as given, the others with its version.
fn check_partial(spec: &Document, filter: &Document) -> Result<(), CommandError> {
    let wrapped = |e: CommandError, shown: String| CommandError {
        errmsg: format!("Error in specification {shown} :: caused by :: {}", e.errmsg),
        ..e
    };
    if spec.get("sparse").and_then(crate::types::truthy) == Some(true) {
        let e = CommandError::new(
            CANNOT_CREATE_INDEX,
            "cannot mix \"partialFilterExpression\" and \"sparse\" options",
        );
        return Err(wrapped(e, with_version(spec)));
    }
    partial_filter::parse_error(filter).map_err(|e| wrapped(e, spec_text(spec)))?;
    match partial_filter::unsupported(filter) {
        Some(why) => {
            let reason = format!("{}{why}", partial_filter::NOT_SUPPORTED);
            Err(wrapped(CommandError::new(CANNOT_CREATE_INDEX, reason), with_version(spec)))
        }
        None => Ok(()),
    }
}

/// The hash buckets `v` asks for: an integer from 0 (none) to 255.
pub fn buckets(v: &Value) -> Result<u8, CommandError> {
    crate::types::number(v)
        .filter(|n| n.fract() == 0.0 && (0.0..=f64::from(u8::MAX)).contains(n))
        .map(|n| n as u8)
        .ok_or_else(|| {
            let msg = format!("{BUCKETS} must be an integer from 0 to 255, but got {}", display(v));
            CommandError::new(BAD_VALUE, msg)
        })
}

fn check_version(spec: &Document, v: &Value) -> Result<i32, CommandError> {
    let Some(n) = crate::types::number(v) else {
        let msg = format!("The field 'v' must be a number, but got {}", type_name(v));
        return Err(CommandError::new(TYPE_MISMATCH, msg));
    };
    let as_int = (n.fract() == 0.0 && n >= f64::from(i32::MIN) && n <= f64::from(i32::MAX))
        .then_some(n as i32);
    let Some(version) = as_int else {
        let msg = format!(
            "Index version must be representable as a 32-bit integer, but got {}",
            display(v)
        );
        return Err(CommandError::new(BAD_VALUE, msg));
    };
    if !VERSIONS.contains(&version) {
        return Err(invalid(spec, &format!("cannot create an index with v={version}")));
    }
    Ok(version)
}

/// Checks that a specification of the `_id` index (key `{_id: 1}`) has only the fields such an
/// index takes (measured on a `refrouter` 9.0.4: `unique: false` is refused with 197).
pub fn check_id_fields(spec: &Document) -> Result<(), CommandError> {
    if let Some((field, _)) = spec.iter().find(|(k, _)| !ID_FIELDS.contains(k)) {
        let msg = format!(
            "The field '{field}' is not valid for an _id index specification. Specification: {}",
            with_version(spec)
        );
        return Err(CommandError::new(INVALID_INDEX_SPECIFICATION_OPTION, msg));
    }
    Ok(())
}

/// Checks the `idIndex` of `create`: the general rules, then those of the `_id` index, with no
/// other option and `{_id: 1}`; `collation` is the collection's (`None` = simple). Gives its version.
pub fn check_id_index(
    spec: &Document,
    collation: Option<&Value>,
    collation_of: impl Fn(&Document) -> Result<Option<Value>, CommandError>,
) -> Result<i32, CommandError> {
    let version = check(spec)?;
    check_id_fields(spec)?;
    if let Some(key) = spec.get(KEY) {
        let id_key = matches!(key, Value::Document(k) if k.len() == 1
            && k.get("_id").and_then(crate::types::number) == Some(1.0));
        if !id_key {
            let msg = format!(
                "The field 'key' for an _id index must be {{_id: 1}}, but got {}",
                display(key)
            );
            return Err(CommandError::new(BAD_VALUE, msg));
        }
    }
    let wanted = match spec.get(COLLATION) {
        Some(Value::Document(c)) => collation_of(c)?,
        // Without one, a version that takes a collation takes the collection's (measured).
        _ if version >= COLLATION_VERSION => collation.cloned(),
        _ => None,
    };
    if wanted.as_ref() != collation {
        let msg = "'idIndex' must have the same collation as the collection.";
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    Ok(version)
}
