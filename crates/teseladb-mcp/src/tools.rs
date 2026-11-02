//! The tools of contract `mcp-server.md` (FR-021, FR-023): the database tools of RefDb MCP
//! Server with their arguments, schemas and annotations, each run as the command it names on the
//! executor of the wire protocol. `inputSchema`, `outputSchema` and the hints are those of the
//! version fixed in the contract (T026): `reference_tools.json`, written by
//! `scripts/mcp-reference-tools.sh` from that package's `tools/list`, without its descriptions.

use std::sync::OnceLock;

use teseladb_json::{Number, Object, Value};

/// The shape of each tool of RefDb MCP Server 3.2.2: name, annotations, `inputSchema` and
/// `outputSchema` without descriptions.
pub const REFERENCE: &str = include_str!("reference_tools.json");

/// The entry of `name` in [`REFERENCE`].
#[must_use]
pub fn reference(name: &str) -> Option<&'static Object> {
    static PARSED: OnceLock<Vec<Object>> = OnceLock::new();
    let all = PARSED.get_or_init(|| match teseladb_json::parse(REFERENCE.as_bytes()) {
        Ok(Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::Object(o) => Some(o),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    });
    all.iter().find(|o| o.get("name").and_then(Value::as_str) == Some(name))
}

fn reference_object(name: &str, field: &str) -> Option<Object> {
    match reference(name)?.get(field)? {
        Value::Object(o) => Some(o.clone()),
        _ => None,
    }
}

/// The kind of an argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A string.
    Str,
    /// A JSON object (a filter, a document…).
    Obj,
    /// An array.
    Arr,
    /// An integer, with its default.
    Int(i64),
    /// A boolean.
    Bool,
    /// One of these strings.
    Enum(&'static [&'static str]),
}

/// An argument of a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arg {
    /// Name.
    pub name: &'static str,
    /// Kind.
    pub kind: Kind,
    /// Required.
    pub required: bool,
}

const fn req(name: &'static str, kind: Kind) -> Arg {
    Arg { name, kind, required: true }
}

const fn opt(name: &'static str, kind: Kind) -> Arg {
    Arg { name, kind, required: false }
}

/// A tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tool {
    /// Name.
    pub name: &'static str,
    /// Description.
    pub description: &'static str,
    /// Arguments, `connectionId` aside.
    pub args: &'static [Arg],
    /// Fields of `structuredContent`.
    pub output: &'static [&'static str],
    /// `readOnlyHint`.
    pub read_only: bool,
    /// `destructiveHint`.
    pub destructive: bool,
}

const DB: Arg = req("database", Kind::Str);
const COLL: Arg = req("collection", Kind::Str);
const BYTES: Arg = opt("responseBytesLimit", Kind::Int(1_048_576));

const fn read(
    name: &'static str,
    description: &'static str,
    args: &'static [Arg],
    output: &'static [&'static str],
) -> Tool {
    Tool { name, description, args, output, read_only: true, destructive: false }
}

const fn write(
    name: &'static str,
    description: &'static str,
    args: &'static [Arg],
    output: &'static [&'static str],
    destructive: bool,
) -> Tool {
    Tool { name, description, args, output, read_only: false, destructive }
}

/// Every tool, in the order of `tools/list`.
pub const TOOLS: &[Tool] = &[
    read("list-databases", "List the databases you can read.", &[], &["databases", "totalCount"]),
    read(
        "list-collections",
        "List the collections of a database.",
        &[DB],
        &["collections", "totalCount"],
    ),
    read(
        "collection-schema",
        "Describe the fields of a collection from a sample of its documents.",
        &[DB, COLL, opt("sampleSize", Kind::Int(50)), BYTES],
        &["schema", "fieldsCount"],
    ),
    read(
        "collection-indexes",
        "List the indexes and search indexes of a collection.",
        &[DB, COLL],
        &["classicIndexes", "searchIndexes", "classicIndexesCount", "searchIndexesCount"],
    ),
    read(
        "collection-storage-size",
        "Give the storage size of a collection.",
        &[DB, COLL],
        &["size", "units"],
    ),
    read("db-stats", "Give the statistics of a database.", &[DB], &["stats"]),
    read(
        "find",
        "Find documents of a collection.",
        &[
            DB,
            COLL,
            opt("filter", Kind::Obj),
            opt("projection", Kind::Obj),
            opt("limit", Kind::Int(10)),
            opt("sort", Kind::Obj),
            BYTES,
        ],
        &["documents", "queryResultsCount", "appliedLimits"],
    ),
    read(
        "aggregate",
        "Run an aggregation pipeline on a collection ($out and $merge are refused).",
        &[DB, COLL, req("pipeline", Kind::Arr), BYTES],
        &["documents", "count", "appliedLimits"],
    ),
    read(
        "aggregate-db",
        "Run an aggregation pipeline on a database ($out and $merge are refused).",
        &[DB, req("pipeline", Kind::Arr), BYTES],
        &["documents", "aggResultsCount", "appliedLimits"],
    ),
    read(
        "count",
        "Count the documents of a collection that match a query.",
        &[DB, COLL, opt("query", Kind::Obj)],
        &["count"],
    ),
    read(
        "explain",
        "Explain how a find, aggregate or count runs.",
        &[
            DB,
            COLL,
            req("method", Kind::Arr),
            opt(
                "verbosity",
                Kind::Enum(&[
                    "queryPlanner",
                    "queryPlannerExtended",
                    "executionStats",
                    "allPlansExecution",
                ]),
            ),
        ],
        &["explainResult", "method", "verbosity"],
    ),
    read(
        "export",
        "Export the result of a find or an aggregation to a resource.",
        &[
            DB,
            COLL,
            req("exportTitle", Kind::Str),
            req("exportTarget", Kind::Arr),
            opt("jsonExportFormat", Kind::Enum(&["relaxed", "canonical"])),
        ],
        &[],
    ),
    read(
        "refdb-logs",
        "Show the recent lines of the server's log.",
        &[opt("type", Kind::Enum(&["global", "startupWarnings"])), opt("limit", Kind::Int(50))],
        &["logs", "totalLinesWritten", "shownCount"],
    ),
    write(
        "insert-many",
        "Insert documents into a collection.",
        &[DB, COLL, req("documents", Kind::Arr)],
        &["database", "collection", "insertedCount", "insertedIds"],
        false,
    ),
    write(
        "create-index",
        "Create an index on a collection.",
        &[DB, COLL, opt("name", Kind::Str), req("definition", Kind::Arr)],
        &["database", "collection", "indexName", "indexType"],
        false,
    ),
    write(
        "create-collection",
        "Create a collection.",
        &[DB, COLL],
        &["database", "collection", "created"],
        false,
    ),
    write(
        "update-many",
        "Update the documents of a collection that match a filter.",
        &[DB, COLL, opt("filter", Kind::Obj), req("update", Kind::Obj), opt("upsert", Kind::Bool)],
        &["database", "collection", "matchedCount", "modifiedCount", "upsertedCount", "upsertedId"],
        true,
    ),
    write(
        "rename-collection",
        "Rename a collection.",
        &[DB, COLL, req("newName", Kind::Str), opt("dropTarget", Kind::Bool)],
        &["database", "oldCollection", "newCollection", "renamed"],
        true,
    ),
    write(
        "delete-many",
        "Delete the documents of a collection that match a filter.",
        &[DB, COLL, opt("filter", Kind::Obj)],
        &["database", "collection", "deletedCount"],
        true,
    ),
    write(
        "drop-index",
        "Drop an index or a search index.",
        &[DB, COLL, req("indexName", Kind::Str), req("type", Kind::Enum(&["classic", "search"]))],
        &["database", "collection", "indexName", "dropped"],
        true,
    ),
    write(
        "drop-collection",
        "Drop a collection.",
        &[DB, COLL],
        &["database", "collection", "dropped"],
        true,
    ),
    write("drop-database", "Drop a database.", &[DB], &["database", "dropped"], true),
    read(
        "connect",
        "Return the connection the server fixed for you.",
        &[req("connectionString", Kind::Str), opt("connectionName", Kind::Str)],
        &["connectionId"],
    ),
    read("disconnect", "Leave the connection; the server keeps it.", &[], &["outcome"]),
    read("list-connections", "List your connection.", &[], &["connections"]),
];

/// Tools whose arguments have no `connectionId`.
const WITHOUT_CONNECTION: [&str; 2] = ["connect", "list-connections"];

/// The tool named `name`.
pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name)
}

/// Whether `t` is listed and runs with `read_only` and `allow_destructive` (FR-023): with
/// `read_only`, only the tools that read; otherwise the destructive ones need `allow_destructive`.
pub fn allowed(t: &Tool, read_only: bool, allow_destructive: bool) -> bool {
    if read_only && !allow_destructive {
        return t.read_only;
    }
    !t.destructive || allow_destructive
}

/// Whether `t` takes `connectionId`.
pub fn takes_connection(t: &Tool) -> bool {
    !WITHOUT_CONNECTION.contains(&t.name)
}

fn string(s: &str) -> Value {
    Value::String(s.to_owned())
}

fn schema_of(kind: Kind) -> Value {
    let mut o = Object::new();
    match kind {
        Kind::Str => o.insert("type", "string"),
        Kind::Obj => o.insert("type", "object"),
        Kind::Arr => o.insert("type", "array"),
        Kind::Int(d) => {
            o.insert("type", "integer");
            o.insert("default", Value::Number(Number::Int(d)))
        }
        Kind::Bool => o.insert("type", "boolean"),
        Kind::Enum(values) => {
            o.insert("type", "string");
            o.insert("enum", Value::Array(values.iter().map(|v| string(v)).collect()))
        }
    };
    Value::Object(o)
}

/// `inputSchema` of `t` (JSON Schema 2020-12, without other properties): the reference's.
pub fn input_schema(t: &Tool) -> Object {
    reference_object(t.name, "inputSchema").unwrap_or_else(|| own_input_schema(t))
}

/// The `inputSchema` [`Tool::args`] describe, which the reference's has to match.
#[must_use]
pub fn own_input_schema(t: &Tool) -> Object {
    let mut props = Object::new();
    let mut required = Vec::new();
    if takes_connection(t) {
        props.insert("connectionId", schema_of(Kind::Str));
        required.push(string("connectionId"));
    }
    for a in t.args {
        props.insert(a.name, schema_of(a.kind));
        if a.required {
            required.push(string(a.name));
        }
    }
    let mut o = Object::new();
    o.insert("type", "object");
    o.insert("properties", Value::Object(props));
    o.insert("required", Value::Array(required));
    o.insert("additionalProperties", Value::Bool(false));
    o
}

/// `outputSchema` of `t`, when its result has `structuredContent`: the reference's.
pub fn output_schema(t: &Tool) -> Option<Object> {
    match reference(t.name).and_then(|r| r.get("outputSchema")) {
        Some(Value::Object(o)) => return Some(satisfiable(t.name, o.clone())),
        Some(Value::Null) => return None,
        _ => {}
    }
    if t.output.is_empty() {
        return None;
    }
    let mut props = Object::new();
    for f in t.output {
        props.insert(*f, Value::Object(Object::new()));
    }
    let mut o = Object::new();
    o.insert("type", "object");
    o.insert("properties", Value::Object(props));
    Some(o)
}

/// The reference's `outputSchema` of `name`, with what no answer could meet corrected: the
/// connections of `list-connections` require `description`, which the reference leaves out of
/// their properties while it refuses any other (`additionalProperties: false`), so that every
/// SDK that validates results rejects every answer. The property is added as a string.
fn satisfiable(name: &str, schema: Object) -> Object {
    if name != "list-connections" {
        return schema;
    }
    let with = |o: &Object, key: &str, f: &dyn Fn(Object) -> Object| -> Object {
        let mut o = o.clone();
        if let Some(inner) = o.get(key).and_then(Value::as_object).cloned() {
            o.insert(key, Value::Object(f(inner)));
        }
        o
    };
    let add_description = |mut props: Object| {
        let mut text = Object::new();
        text.insert("type", "string");
        props.insert("description", Value::Object(text));
        props
    };
    let items = |i: Object| with(&i, "properties", &add_description);
    let connections = |c: Object| with(&c, "items", &items);
    let properties = |p: Object| with(&p, "connections", &connections);
    with(&schema, "properties", &properties)
}

/// The entry of `t` in `tools/list`.
pub fn listing(t: &Tool) -> Object {
    let mut annotations = Object::new();
    annotations.insert("title", t.name);
    annotations.insert("readOnlyHint", Value::Bool(t.read_only));
    annotations.insert("destructiveHint", Value::Bool(t.destructive));
    annotations.insert("openWorldHint", Value::Bool(true));
    let mut o = Object::new();
    o.insert("name", t.name);
    o.insert("description", t.description);
    o.insert("inputSchema", Value::Object(input_schema(t)));
    if let Some(out) = output_schema(t) {
        o.insert("outputSchema", Value::Object(out));
    }
    o.insert("annotations", Value::Object(annotations));
    o
}

/// Checks `args` against the schema of `t`: no unknown names, the required ones present and
/// each of its kind. The message names the first fault.
pub fn check_args(t: &Tool, args: &Object) -> Result<(), String> {
    for (name, v) in args.iter() {
        let kind = if name == "connectionId" && takes_connection(t) {
            Kind::Str
        } else {
            match t.args.iter().find(|a| a.name == name) {
                Some(a) => a.kind,
                None => return Err(format!("unknown argument {name}")),
            }
        };
        let fits = match (kind, v) {
            (Kind::Str, Value::String(_))
            | (Kind::Obj, Value::Object(_))
            | (Kind::Arr, Value::Array(_)) => true,
            (Kind::Bool, Value::Bool(_)) => true,
            (Kind::Int(_), Value::Number(n)) => n.as_i64().is_some(),
            (Kind::Enum(values), Value::String(s)) => values.contains(&s.as_str()),
            _ => false,
        };
        if !fits {
            return Err(format!("argument {name} has the wrong type"));
        }
    }
    let required = t.args.iter().filter(|a| a.required).map(|a| a.name);
    let required = required.chain(takes_connection(t).then_some("connectionId"));
    for name in required {
        if args.get(name).is_none() {
            return Err(format!("argument {name} is required"));
        }
    }
    Ok(())
}
