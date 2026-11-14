//! The parameters of the Stable API (006 FR-035): `apiVersion`, `apiStrict` and
//! `apiDeprecationErrors`, and what `apiStrict: true` refuses. Measured on a `refrouter` 9.0.4 on
//! 05-11-2026 (scenario `api-strict` of the battery), and the test commands of the API on a
//! `refnode` 9.0.4 with `enableTestCommands` the same day.

use teseladb_bson::{Document, Value};

use crate::concerns::canonical;
use crate::error::{
    API_DEPRECATION_ERROR, API_STRICT_ERROR, API_VERSION_ERROR, API_WITHOUT_VERSION, CommandError,
};
use crate::types::wrong_type;

/// The generic argument that names the API version.
pub const API_VERSION: &str = "apiVersion";
/// The generic argument that refuses what is not in the version.
pub const API_STRICT: &str = "apiStrict";
/// The generic argument that refuses what the version deprecates.
pub const API_DEPRECATION_ERRORS: &str = "apiDeprecationErrors";
/// The only version there is.
const VERSION_1: &str = "1";
/// Where a refused command sends the client.
const DOC_HUB: &str = teseladb_names::DOC_VERSIONED_API;

/// The API parameters of a command, as it gave them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiParams {
    /// `apiVersion`.
    pub version: Option<String>,
    /// `apiStrict`.
    pub strict: Option<bool>,
    /// `apiDeprecationErrors`.
    pub deprecation_errors: Option<bool>,
}

impl ApiParams {
    /// Whether the command asked for `apiStrict: true`.
    pub fn is_strict(&self) -> bool {
        self.strict == Some(true)
    }

    /// The parameters given, in RefDb's order, as the test commands answer them.
    pub fn echoed(&self) -> Document {
        let mut d = Document::new();
        if let Some(v) = &self.version {
            d.push(API_VERSION, v.as_str());
        }
        if let Some(s) = self.strict {
            d.push(API_STRICT, s);
        }
        if let Some(e) = self.deprecation_errors {
            d.push(API_DEPRECATION_ERRORS, e);
        }
        d
    }
}

/// Reads the API parameters of command `name`: a wrong type is 14, a version other than `"1"` is
/// 322, and `apiStrict` or `apiDeprecationErrors` without a version is 4886600.
pub fn parse(cmd: &Document, name: &str) -> Result<ApiParams, CommandError> {
    let flag = |k: &str| match cmd.get(k) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(v) => Err(wrong_type(&format!("{name}.{k}"), v, "bool")),
    };
    let version = match cmd.get(API_VERSION) {
        None => None,
        Some(Value::String(v)) => Some(v.clone()),
        Some(v) => return Err(wrong_type(&format!("{name}.{API_VERSION}"), v, "string")),
    };
    let params = ApiParams {
        version,
        strict: flag(API_STRICT)?,
        deprecation_errors: flag(API_DEPRECATION_ERRORS)?,
    };
    match &params.version {
        Some(v) if v != VERSION_1 => {
            let msg = format!("Cannot accept API version {v}");
            Err(CommandError::new(API_VERSION_ERROR, msg))
        }
        None if params.strict.is_some() || params.deprecation_errors.is_some() => {
            let msg = "Provided apiStrict and/or apiDeprecationErrors without passing apiVersion";
            Err(CommandError::new(API_WITHOUT_VERSION, msg))
        }
        _ => Ok(params),
    }
}

/// A test command of the Stable API as `listCommands` lists it on a node with
/// `enableTestCommands` (measured on a `refnode` 9.0.4 on 05-11-2026).
pub struct TestCommand {
    /// Its name.
    pub name: &'static str,
    help: &'static str,
    requires_auth: bool,
    versions: &'static [&'static str],
    deprecated: &'static [&'static str],
}

impl TestCommand {
    /// Its entry in `listCommands`.
    pub fn entry(&self) -> Document {
        let list = |v: &[&str]| v.iter().map(|s| Value::from(*s)).collect::<Vec<Value>>();
        Document::new()
            .with("help", self.help)
            .with("requiresAuth", self.requires_auth)
            .with("requiresAuthzChecks", false)
            .with("secondaryOk", true)
            .with("adminOnly", false)
            .with("apiVersions", list(self.versions))
            .with("deprecatedApiVersions", list(self.deprecated))
    }
}

/// The help of a command that has none.
const NO_HELP: &str = "no help defined";
/// The test commands of the API that a node with `enableTestCommands` has.
pub const TEST_COMMANDS: [TestCommand; 4] = [
    TestCommand {
        name: "testDeprecation",
        help: "replies with the values of the OperationContext's API parameters",
        requires_auth: false,
        versions: &["1"],
        deprecated: &["1"],
    },
    TestCommand {
        name: "testDeprecationInVersion2",
        help: NO_HELP,
        requires_auth: true,
        versions: &["1", "2"],
        deprecated: &["2"],
    },
    TestCommand {
        name: "testRemoval",
        help: NO_HELP,
        requires_auth: true,
        versions: &["1"],
        deprecated: &[],
    },
    TestCommand {
        name: "testVersion2",
        help: NO_HELP,
        requires_auth: true,
        versions: &["2"],
        deprecated: &[],
    },
];

impl TestCommand {
    /// Whether version 1 has it and whether it deprecates it.
    fn in_v1(&self) -> (bool, bool) {
        (self.versions.contains(&VERSION_1), self.deprecated.contains(&VERSION_1))
    }
}

/// Whether `name` is one of the test commands of the API.
pub fn is_test_command(name: &str) -> bool {
    TEST_COMMANDS.iter().any(|t| t.name == name)
}

/// Whether command `name` is in version 1 and whether version 1 deprecates it.
fn versions_of(name: &str) -> (bool, bool) {
    match TEST_COMMANDS.iter().find(|t| t.name == name) {
        Some(t) => t.in_v1(),
        None => (crate::commands::in_api_v1(canonical(name)), false),
    }
}

/// Refuses command `name` with `apiStrict: true` when it is not in version 1 (323), and with
/// `apiDeprecationErrors: true` when version 1 deprecates it (324).
pub fn check_command(name: &str, params: &ApiParams) -> Result<(), CommandError> {
    let (in_v1, deprecated) = versions_of(name);
    if params.is_strict() && !in_v1 {
        let msg = format!(
            "Provided apiStrict:true, but the command {name} is not in API Version 1. \
             Information on supported commands and migrations in API Version 1 can be found at \
             {DOC_HUB}."
        );
        return Err(CommandError::new(API_STRICT_ERROR, msg));
    }
    if params.deprecation_errors == Some(true) && deprecated {
        let msg = format!(
            "Provided apiDeprecationErrors:true, but the command {name} is deprecated in API \
             Version 1."
        );
        return Err(CommandError::new(API_DEPRECATION_ERROR, msg));
    }
    Ok(())
}

/// The type a field must have for `apiStrict` to look at it: with another type the command's own
/// parse answers first (measured: `getMore.term` as an `int` is 14, as a `long` 323).
#[derive(Clone, Copy)]
enum Kind {
    Bool,
    Object,
    Number,
    Long,
    Uuid,
}

impl Kind {
    fn matches(self, v: &Value) -> bool {
        match self {
            Kind::Bool => matches!(v, Value::Boolean(_)),
            Kind::Object => matches!(v, Value::Document(_)),
            Kind::Number => {
                matches!(
                    v,
                    Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_)
                )
            }
            Kind::Long => matches!(v, Value::Int64(_)),
            Kind::Uuid => matches!(v, Value::Binary { subtype: 4, .. }),
        }
    }
}

/// The fields of version 1's commands that are not in version 1 (measured one by one).
const UNSTABLE_FIELDS: [(&str, &[(&str, Kind)]); 13] = [
    (
        "aggregate",
        &[
            ("explain", Kind::Bool),
            ("isMapReduceCommand", Kind::Bool),
            ("collectionUUID", Kind::Uuid),
            ("$_requestResumeToken", Kind::Bool),
            ("$_requestReshardingResumeToken", Kind::Bool),
        ],
    ),
    (
        "find",
        &[
            ("min", Kind::Object),
            ("max", Kind::Object),
            ("returnKey", Kind::Bool),
            ("showRecordId", Kind::Bool),
            ("tailable", Kind::Bool),
            ("oplogReplay", Kind::Bool),
            ("noCursorTimeout", Kind::Bool),
            ("awaitData", Kind::Bool),
            ("term", Kind::Number),
            ("readOnce", Kind::Bool),
            ("$_requestResumeToken", Kind::Bool),
            ("collectionUUID", Kind::Uuid),
            ("mirrored", Kind::Bool),
        ],
    ),
    ("count", &[("fields", Kind::Object), ("mirrored", Kind::Bool)]),
    ("insert", &[("collectionUUID", Kind::Uuid), ("bypassEmptyTsReplacement", Kind::Bool)]),
    ("update", &[("collectionUUID", Kind::Uuid), ("bypassEmptyTsReplacement", Kind::Bool)]),
    ("delete", &[("collectionUUID", Kind::Uuid), ("bypassEmptyTsReplacement", Kind::Bool)]),
    ("findAndModify", &[("bypassEmptyTsReplacement", Kind::Bool)]),
    (
        "create",
        &[
            ("size", Kind::Number),
            ("max", Kind::Number),
            ("storageEngine", Kind::Object),
            ("indexOptionDefaults", Kind::Object),
            ("changeStreamPreAndPostImages", Kind::Object),
            ("flags", Kind::Number),
        ],
    ),
    ("drop", &[("collectionUUID", Kind::Uuid)]),
    (
        "collMod",
        &[
            ("changeStreamPreAndPostImages", Kind::Object),
            ("timeseriesBucketsMayHaveMixedSchemaData", Kind::Bool),
            ("_removeLegacyTimeseriesBucketingParametersHaveChanged", Kind::Bool),
            ("dryRun", Kind::Bool),
            ("collectionUUID", Kind::Uuid),
            ("recordIdsReplicated", Kind::Bool),
        ],
    ),
    ("listCollections", &[("includePendingDrops", Kind::Bool)]),
    ("listIndexes", &[("includeBuildUUIDs", Kind::Bool), ("includeIndexBuildInfo", Kind::Bool)]),
    ("getMore", &[("term", Kind::Long), ("lastKnownCommittedOpTime", Kind::Object)]),
];

/// The fields of `createIndexes` that are not in version 1.
const UNSTABLE_CREATE_INDEXES: [(&str, Kind); 1] = [("ignoreUnknownIndexOptions", Kind::Bool)];

fn field_refused(k: &str) -> CommandError {
    CommandError::new(
        API_STRICT_ERROR,
        format!("BSON field '{k}' is not allowed with apiStrict:true."),
    )
}

/// The last field of `d` among `unstable` with its type, in the order of `d`: the one a
/// `refrouter` 9.0.4 names (measured: `{min, max}` names `max`, `{max, min}` names `min`).
fn last_unstable<'a>(d: &'a Document, unstable: &[(&str, Kind)]) -> Option<&'a str> {
    d.iter()
        .filter(|(k, v)| unstable.iter().any(|(n, kind)| n == k && kind.matches(v)))
        .map(|(k, _)| k)
        .last()
}

/// With `apiStrict: true`, refuses the fields of command `cmd` (named `name`) that are not in
/// version 1: its own, the `sort` of an `update` statement, `encryptedFields.fields` of a
/// `create` and the stages of an `aggregate`. The text indexes of a `createIndexes` are the
/// shard's to refuse ([`check_text_indexes`]).
pub fn check_strict(cmd: &Document, name: &str, params: &ApiParams) -> Result<(), CommandError> {
    if !params.is_strict() {
        return Ok(());
    }
    let name = canonical(name);
    let own: &[(&str, Kind)] = match name {
        "createIndexes" => &UNSTABLE_CREATE_INDEXES,
        _ => UNSTABLE_FIELDS.iter().find(|(n, _)| *n == name).map_or(&[], |(_, f)| f),
    };
    if let Some(k) = last_unstable(cmd, own) {
        return Err(field_refused(k));
    }
    match name {
        "update" => check_statement_sort(cmd),
        "create" => match cmd.get("encryptedFields") {
            Some(Value::Document(e)) if e.get("fields").is_some() => Err(field_refused("fields")),
            _ => Ok(()),
        },
        "aggregate" => match cmd.get("pipeline") {
            Some(Value::Array(stages)) => check_pipeline(stages),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

/// The `sort` of an `update` statement is not in version 1.
fn check_statement_sort(cmd: &Document) -> Result<(), CommandError> {
    let Some(Value::Array(statements)) = cmd.get("updates") else { return Ok(()) };
    let sorted =
        statements.iter().any(|s| matches!(s, Value::Document(s) if s.get("sort").is_some()));
    if sorted { Err(field_refused("sort")) } else { Ok(()) }
}

/// With `apiStrict: true` in `cmd`, a text index cannot be created: the shard refuses it, and a
/// `refrouter` 9.0.4 relays its error with `raw` (measured on 05-11-2026).
pub fn check_text_indexes(cmd: &Document) -> Result<(), CommandError> {
    if cmd.get(API_STRICT) != Some(&Value::Boolean(true)) {
        return Ok(());
    }
    let Some(Value::Array(specs)) = cmd.get("indexes") else { return Ok(()) };
    for spec in specs {
        let Value::Document(s) = spec else { continue };
        let Some(Value::Document(key)) = s.get("key") else { continue };
        if key.iter().any(|(_, v)| matches!(v, Value::String(t) if t == "text")) {
            let msg = format!(
                "Error in specification {} :: caused by :: text indexes cannot be created with \
                 apiStrict: true",
                crate::commands::display(spec)
            );
            return Err(CommandError::new(API_STRICT_ERROR, msg));
        }
    }
    Ok(())
}

/// The stages that are not in version 1; `$collStats` only with an option other than `count`.
const UNSTABLE_STAGES: [&str; 19] = [
    "$changeStreamSplitLargeEvent",
    "$currentOp",
    "$fill",
    "$indexStats",
    "$listCatalog",
    "$listClusterCatalog",
    "$listExtensions",
    "$listLocalSessions",
    "$listSearchIndexes",
    "$listSessions",
    "$planCacheStats",
    "$querySettings",
    "$queryStats",
    "$rankFusion",
    "$score",
    "$scoreFusion",
    "$search",
    "$searchMeta",
    "$vectorSearch",
];

/// Refuses the stages of `stages` that are not in version 1, also inside `$lookup`,
/// `$unionWith` and `$facet`.
fn check_pipeline(stages: &[Value]) -> Result<(), CommandError> {
    for stage in stages {
        let Value::Document(s) = stage else { continue };
        let Some((name, arg)) = s.iter().next() else { continue };
        if name == "$collStats" {
            check_coll_stats(arg)?;
        } else if UNSTABLE_STAGES.contains(&name) {
            let msg = format!("{name} is not allowed with 'apiStrict: true' in API Version 1");
            return Err(CommandError::new(API_STRICT_ERROR, msg));
        }
        for inner in sub_pipelines(name, arg) {
            check_pipeline(inner)?;
        }
    }
    Ok(())
}

/// `$collStats` with an option other than `count`.
fn check_coll_stats(arg: &Value) -> Result<(), CommandError> {
    let Value::Document(options) = arg else { return Ok(()) };
    if options.iter().all(|(k, _)| k == "count") {
        return Ok(());
    }
    let msg = "Unable to retrieve information for $collStats stage :: caused by :: only the \
               'count' option to $collStats is supported in API Version 1";
    Err(CommandError::new(API_STRICT_ERROR, msg))
}

/// The pipelines inside stage `name` with argument `arg`.
fn sub_pipelines<'a>(name: &str, arg: &'a Value) -> Vec<&'a [Value]> {
    let Value::Document(d) = arg else { return Vec::new() };
    match name {
        "$lookup" | "$unionWith" => match d.get("pipeline") {
            Some(Value::Array(p)) => vec![p.as_slice()],
            _ => Vec::new(),
        },
        "$facet" => d
            .iter()
            .filter_map(|(_, v)| match v {
                Value::Array(p) => Some(p.as_slice()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}
