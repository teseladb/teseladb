//! Wildcard indexes (FR-013), measured on the reference server 9.0.4
//! (`tests/data/wildcard-keys.json`, 009 research R11).
//!
//! A wildcard field (`$**`, or `a.$**` for one subtree) gives one key per value it reaches, as the
//! pair (path, value): the path without positions and the value of a leaf. Documents are walked
//! with the `wildcardProjection` applied as a `find` projection does: an empty document (or one
//! whose fields the projection removes) is a value `{}`; an array marks its path as multikey and
//! gives its elements, with the documents among them walked at the same path and the other
//! elements (an array inside it stays one value) kept only where the projection includes the
//! path; an array that keeps nothing gives `undefined`. `_id` is left out unless the projection
//! names it. In a compound wildcard index the other fields cannot hold arrays, and a document
//! without wildcard values still has a key, with `MinKey` as path and value.

use std::collections::BTreeSet;

use teseladb_bson::{Decimal, RawDoc, RawValue};

use super::KeygenError;

/// The key of a wildcard field over the whole document.
pub const WILDCARD_FIELD: &str = "$**";
/// The end of the key of a wildcard field over one subtree (`a.$**`).
pub const WILDCARD_SUFFIX: &str = ".$**";
/// The document's identifier, left out unless the projection names it.
const ID: &str = "_id";

/// The reference server's code of a regular field of a compound wildcard index that holds an array.
pub const WILDCARD_COMPOUND_ARRAY: i32 = 7_246_301;
/// The reference server's `FailedToParse`.
pub const FAILED_TO_PARSE: i32 = 9;
/// The reference server's `CannotCreateIndex`.
pub const CANNOT_CREATE_INDEX: i32 = 67;
/// The reference server's code of an exclusion inside an inclusion projection.
pub const EXCLUSION_IN_INCLUSION: i32 = 31_254;
/// The reference server's code of an inclusion inside an exclusion projection.
pub const INCLUSION_IN_EXCLUSION: i32 = 31_253;
/// The reference server's code of a projection value that is an unknown expression.
pub const UNKNOWN_EXPRESSION: i32 = 31_325;
/// The reference server's code of a projection value that is not a number, a boolean or a document.
pub const PROJECTION_COMPUTED: i32 = 51_271;
/// The reference server's code of a projection path with a name that starts with `$`.
pub const PROJECTION_DOLLAR: i32 = 16_410;
/// The reference server's code of a key with two wildcard fields.
pub const TWO_WILDCARDS: i32 = 7_246_201;
/// The reference server's code of an inclusion projection that names a regular field of the key.
pub const PROJECTION_OVERLAPS_KEY: i32 = 7_246_208;
/// The reference server's code of an exclusion projection that leaves a regular field of the key
/// in.
pub const PROJECTION_KEEPS_KEY: i32 = 7_246_209;

/// Why a wildcard index definition is refused: the reference server's code and the reason it gives
/// after `Error in specification { … } :: caused by ::`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecError {
    /// Code.
    pub code: i32,
    /// Reason.
    pub reason: String,
}

fn spec_error(code: i32, reason: impl Into<String>) -> SpecError {
    SpecError { code, reason: reason.into() }
}

/// The start of every projection parsing reason.
const PARSE_PREFIX: &str = "Failed to parse projection: wildcardProjection :: caused by :: ";

/// Which paths a wildcard field indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WildcardProjection {
    /// Whether `paths` are the paths it keeps (otherwise, the ones it removes).
    include: bool,
    /// The paths named, other than `_id`.
    paths: Vec<String>,
    /// Whether `_id` is indexed.
    id: bool,
}

/// Whether `p` is `q` or under it.
fn under(p: &str, q: &str) -> bool {
    p == q || p.strip_prefix(q).is_some_and(|r| r.starts_with('.'))
}

/// Whether `p` names `_id` or something under it.
fn is_id(p: &str) -> bool {
    under(p, ID)
}

impl WildcardProjection {
    /// Every field but `_id` (`{"$**": 1}` without a projection).
    pub fn all() -> WildcardProjection {
        WildcardProjection { include: false, paths: Vec::new(), id: false }
    }

    /// The subtree of the key `path.$**`.
    pub fn subtree(path: &str) -> WildcardProjection {
        WildcardProjection { include: true, paths: vec![path.to_string()], id: false }
    }

    /// The projection of a key: its subtree for `path.$**`, `projection` (or every field) for
    /// `$**`. `expression` says whether a name is an aggregation expression (`$literal`), which a
    /// projection value may not be.
    pub fn of_key(
        key: &str,
        projection: Option<RawDoc<'_>>,
        expression: &dyn Fn(&str) -> bool,
    ) -> Result<WildcardProjection, SpecError> {
        match (key.strip_suffix(WILDCARD_SUFFIX), projection) {
            (Some(_), Some(_)) => Err(spec_error(
                FAILED_TO_PARSE,
                "The field 'wildcardProjection' is only allowed when 'key' is {\"$**\": ±1}",
            )),
            (Some(path), None) => Ok(WildcardProjection::subtree(path)),
            (None, Some(p)) => WildcardProjection::parse(p, expression),
            (None, None) => Ok(WildcardProjection::all()),
        }
    }

    /// A `wildcardProjection` as written: paths, possibly nested in documents, with a number or a
    /// boolean (0 or false removes, anything else keeps); `expression` as in [`Self::of_key`].
    pub fn parse(
        spec: RawDoc<'_>,
        expression: &dyn Fn(&str) -> bool,
    ) -> Result<WildcardProjection, SpecError> {
        let mut named = Vec::new();
        flatten(spec, "", expression, &mut named)?;
        if named.is_empty() {
            return Err(spec_error(
                FAILED_TO_PARSE,
                "The 'wildcardProjection' field can't be an empty object",
            ));
        }
        let mut id = None;
        let mut mode: Option<bool> = None;
        let mut paths = Vec::new();
        for (path, keep) in named {
            if path == ID {
                id = Some(keep);
                continue;
            }
            match mode {
                Some(m) if m != keep => {
                    let (code, what, kind) = if keep {
                        (INCLUSION_IN_EXCLUSION, "inclusion", "exclusion")
                    } else {
                        (EXCLUSION_IN_INCLUSION, "exclusion", "inclusion")
                    };
                    return Err(spec_error(
                        code,
                        format!(
                            "{PARSE_PREFIX}Cannot do {what} on field {path} in {kind} projection"
                        ),
                    ));
                }
                _ => mode = Some(keep),
            }
            paths.push(path);
        }
        let include = mode.unwrap_or(id == Some(true));
        Ok(WildcardProjection { include, paths, id: id == Some(true) })
    }

    /// Whether the walk goes into `p`.
    fn descends(&self, p: &str) -> bool {
        if is_id(p) && !self.paths.iter().any(|q| is_id(q)) {
            return self.id;
        }
        if self.include {
            self.paths.iter().any(|q| under(p, q) || under(q, p))
        } else {
            !self.paths.iter().any(|q| under(p, q))
        }
    }

    /// Whether a leaf at `p` is kept.
    fn keeps(&self, p: &str) -> bool {
        if is_id(p) && !self.paths.iter().any(|q| is_id(q)) {
            return self.id;
        }
        self.paths.iter().any(|q| under(p, q)) == self.include
    }

    /// Whether the projection keeps `path` of a regular field (only inclusion projections may
    /// keep one out, and exclusion ones must name it).
    fn names(&self, path: &str) -> bool {
        self.paths.iter().any(|q| under(path, q) || under(q, path))
    }
}

/// The reason for a value the reference server reads as a computed field.
fn computed() -> SpecError {
    spec_error(
        PROJECTION_COMPUTED,
        format!(
            "{PARSE_PREFIX}Bad projection specification, cannot use computed fields when parsing a spec in kBanComputedFields mode"
        ),
    )
}

/// The paths of a projection document and whether each is kept.
fn flatten(
    d: RawDoc<'_>,
    prefix: &str,
    expression: &dyn Fn(&str) -> bool,
    out: &mut Vec<(String, bool)>,
) -> Result<(), SpecError> {
    for (name, v) in d.iter() {
        let path = if prefix.is_empty() { name.to_string() } else { format!("{prefix}.{name}") };
        if let Some(segment) = path.split('.').find(|s| s.starts_with('$')) {
            return Err(spec_error(
                PROJECTION_DOLLAR,
                format!(
                    "{PARSE_PREFIX}Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not start with '$', given '{segment}'."
                ),
            ));
        }
        let keep = match v {
            RawValue::Document(inner) => {
                // A document whose first name starts with `$` is an expression (measured).
                if let Some((op, _)) = inner.iter().next().filter(|(n, _)| n.starts_with('$')) {
                    return Err(if expression(op) {
                        computed()
                    } else {
                        spec_error(
                            UNKNOWN_EXPRESSION,
                            format!("{PARSE_PREFIX}Unknown expression {op}"),
                        )
                    });
                }
                flatten(inner, &path, expression, out)?;
                continue;
            }
            RawValue::Boolean(b) => b,
            RawValue::Int32(n) => n != 0,
            RawValue::Int64(n) => n != 0,
            RawValue::Double(n) => n != 0.0,
            RawValue::Decimal128(n) => {
                !matches!(n.classify(), Decimal::Finite { coefficient: 0, .. })
            }
            _ => return Err(computed()),
        };
        out.push((path, keep));
    }
    Ok(())
}

/// The options a wildcard index refuses, in the order the reference server checks them (measured:
/// `sparse`, then `unique`, then `expireAfterSeconds`).
pub fn check_options(unique: bool, sparse: bool, ttl: bool) -> Result<(), SpecError> {
    let refuse =
        |option: &str| format!("Index type 'wildcard' does not support the '{option}' option");
    if sparse {
        return Err(spec_error(CANNOT_CREATE_INDEX, refuse("sparse")));
    }
    if unique {
        return Err(spec_error(CANNOT_CREATE_INDEX, refuse("unique")));
    }
    if ttl {
        let reason =
            format!("cannot make a TTL index :: caused by :: {}", refuse("expireAfterSeconds"));
        return Err(spec_error(CANNOT_CREATE_INDEX, reason));
    }
    Ok(())
}

/// The reason of a wildcard field that is not ascending or descending (`"$**": "hashed"`).
pub const WILDCARD_NOT_ORDERED: &str =
    "Index key contains an illegal field name: field name starts with '$'.";

/// The restrictions the reference server puts on a key with a wildcard field, given its fields as
/// written (`(path, is_wildcard)`), whether another field is hashed or geographic, and the
/// projection.
pub fn check_key(
    fields: &[(&str, bool)],
    plugin: bool,
    projection: Option<&WildcardProjection>,
) -> Result<(), SpecError> {
    let wildcards = fields.iter().filter(|(_, w)| *w).count();
    if wildcards > 1 {
        return Err(spec_error(
            TWO_WILDCARDS,
            "Index Key can contain only one wildcard index field",
        ));
    }
    if wildcards == 0 || fields.len() == 1 {
        return Ok(());
    }
    if plugin {
        return Err(spec_error(
            CANNOT_CREATE_INDEX,
            "Can't use more than one index plugin for a single index.",
        ));
    }
    let all = fields.iter().any(|(p, w)| *w && *p == WILDCARD_FIELD);
    let Some(projection) = projection.filter(|_| all) else {
        return if all {
            Err(spec_error(
                CANNOT_CREATE_INDEX,
                "Compound wildcard indexes on all fields must also specify 'wildcardProjection' option",
            ))
        } else {
            Ok(())
        };
    };
    for (path, _) in fields.iter().filter(|(_, w)| !*w) {
        if projection.include
            && let Some(q) = projection.paths.iter().find(|q| under(path, q) || under(q, path))
        {
            return Err(spec_error(
                PROJECTION_OVERLAPS_KEY,
                format!(
                    "Index Key and Wildcard Projection cannot contain overlapping fields, however '{path}' index field is overlapping with '{q}' wildcardProjection path"
                ),
            ));
        }
        if !projection.include && !projection.names(path) {
            return Err(spec_error(
                PROJECTION_KEEPS_KEY,
                format!(
                    "wildcardProjection paths must exclude all regular index fields, however '{path}'is not excluded"
                ),
            ));
        }
    }
    Ok(())
}

/// The wildcard values of a document: (path, value) pairs without repeats, in the order found,
/// and the paths that went through arrays.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WildcardValues<'a> {
    /// Path and value of each key.
    pub keys: Vec<(String, RawValue<'a>)>,
    /// Paths of the arrays walked (the reference server's multikey paths of the index).
    pub multikey_paths: BTreeSet<String>,
}

impl<'a> WildcardValues<'a> {
    fn push(&mut self, path: &str, v: RawValue<'a>) {
        if !self.keys.iter().any(|(p, w)| p == path && *w == v) {
            self.keys.push((path.to_string(), v));
        }
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() { name.to_string() } else { format!("{prefix}.{name}") }
}

/// The wildcard values of `doc` under `projection`.
pub fn wildcard_values<'a>(doc: RawDoc<'a>, projection: &WildcardProjection) -> WildcardValues<'a> {
    let mut out = WildcardValues::default();
    walk_doc(doc, "", projection, &mut out);
    out
}

/// The fields of document `d` at `prefix`; a document left without fields is a value `{}`.
fn walk_doc<'a>(
    d: RawDoc<'a>,
    prefix: &str,
    projection: &WildcardProjection,
    out: &mut WildcardValues<'a>,
) {
    let mut kept = false;
    for (name, v) in d.iter() {
        let path = join(prefix, name);
        if !projection.descends(&path) {
            continue;
        }
        match v {
            RawValue::Document(inner) => walk_doc(inner, &path, projection, out),
            RawValue::Array(a) => {
                out.multikey_paths.insert(path.clone());
                let mut any = false;
                for e in a.iter() {
                    match e {
                        RawValue::Document(inner) => {
                            any = true;
                            walk_doc(inner, &path, projection, out);
                        }
                        other if projection.keeps(&path) => {
                            any = true;
                            out.push(&path, other);
                        }
                        _ => {}
                    }
                }
                if !any {
                    out.push(&path, RawValue::Undefined);
                }
            }
            leaf if projection.keeps(&path) => out.push(&path, leaf),
            _ => continue,
        }
        kept = true;
    }
    if !kept && !prefix.is_empty() {
        out.push(prefix, RawValue::Document(RawDoc::EMPTY));
    }
}

/// The error of a regular field of a compound wildcard index that holds an array.
pub(crate) fn compound_array(path: &str) -> KeygenError {
    KeygenError::WildcardCompoundArray(path.to_string())
}
