//! `tools/call` (FR-021, FR-024, FR-025): each tool of [`crate::tools`] builds its command, runs
//! it on the executor of the wire protocol with the request's user, and answers with a summary,
//! the data inside the untrusted block and `structuredContent`, within the documents, bytes and
//! time of `mcpOptions`.

use teseladb_bson::{Document, RawDoc, Value as Bson, extjson};
use teseladb_json::{Number, Object, Value, parse, to_string};
use teseladb_rest::api::Executor;

/// The limits of a call (`mcpOptions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Documents a result holds.
    pub max_documents: u64,
    /// Bytes a result holds.
    pub max_bytes: u64,
    /// `maxTimeMS` of each command.
    pub max_time_ms: u64,
}

/// What a call answers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    /// The summary.
    pub summary: String,
    /// The data, as relaxed Extended JSON, for the untrusted block.
    pub data: Option<String>,
    /// `structuredContent`.
    pub structured: Object,
    /// An export to keep as a resource: its name and its text.
    pub export: Option<(String, String)>,
}

/// The context of a call.
pub struct Ctx<'a, E: Executor + ?Sized> {
    /// The executor.
    pub exec: &'a mut E,
    /// The limits.
    pub limits: Limits,
    /// The `connectionId` of the session.
    pub connection_id: &'a str,
    /// The `comment` each command of the call carries, which `notifications/cancelled` finds
    /// with `currentOp` (FR-026).
    pub comment: Option<String>,
    /// Told the documents read after each batch, for `notifications/progress` (FR-026).
    pub on_batch: Option<&'a mut dyn FnMut(u64)>,
}

/// A failed call's message.
pub type Failure = String;

fn int(n: i64) -> Value {
    Value::Number(Number::Int(n))
}

/// `n` as a JSON number.
pub fn int_value(n: u64) -> Value {
    Value::Number(Number::UInt(n))
}

/// A JSON value as BSON, read as relaxed Extended JSON.
pub fn to_bson(v: &Value) -> Result<Bson, Failure> {
    let text = to_string(v).map_err(|e| format!("{e:?}"))?;
    let mut d = extjson::parse(&format!("{{\"v\":{text}}}")).map_err(|e| e.to_string())?;
    d.remove("v").ok_or_else(|| "no value".to_owned())
}

/// A BSON document as JSON, written as relaxed Extended JSON.
pub fn to_json(d: &Document) -> Value {
    let text = d
        .to_bytes()
        .ok()
        .and_then(|b| RawDoc::from_bytes(&b).ok().map(extjson::to_relaxed))
        .unwrap_or_else(|| "{}".to_owned());
    parse(text.as_bytes()).unwrap_or(Value::Null)
}

fn arg_doc(args: &Object, name: &str) -> Result<Document, Failure> {
    match args.get(name) {
        None => Ok(Document::new()),
        Some(v) => match to_bson(v)? {
            Bson::Document(d) => Ok(d),
            _ => Err(format!("{name} must be an object")),
        },
    }
}

fn arg_array(args: &Object, name: &str) -> Result<Vec<Bson>, Failure> {
    match args.get(name).map(to_bson).transpose()? {
        Some(Bson::Array(a)) => Ok(a),
        _ => Err(format!("{name} must be an array")),
    }
}

fn arg_str<'a>(args: &'a Object, name: &str) -> &'a str {
    args.get(name).and_then(Value::as_str).unwrap_or_default()
}

fn arg_int(args: &Object, name: &str, default: i64) -> i64 {
    args.get(name).and_then(Value::as_i64).unwrap_or(default)
}

/// Whether a reply says `ok: 1`, in any numeric type.
pub fn is_ok(reply: &Document) -> bool {
    match reply.get("ok") {
        Some(Bson::Double(x)) => *x == 1.0,
        Some(Bson::Int32(n)) => *n == 1,
        Some(Bson::Int64(n)) => *n == 1,
        _ => false,
    }
}

impl<E: Executor + ?Sized> Ctx<'_, E> {
    /// Runs `command` on `db` with the call's `maxTimeMS` when `timed`, and its `comment`. No
    /// tool builds a command with either field.
    fn run(&mut self, db: &str, mut command: Document, timed: bool) -> Result<Document, Failure> {
        if timed {
            command.push("maxTimeMS", self.limits.max_time_ms as i64);
        }
        if let Some(c) = &self.comment {
            command.push("comment", c.as_str());
        }
        let reply = self.exec.run(db, command);
        if is_ok(&reply) {
            return Ok(reply);
        }
        match reply.get("errmsg") {
            Some(Bson::String(m)) => Err(m.clone()),
            _ => Err("the command failed".to_owned()),
        }
    }

    /// The documents of a query's cursor, all its batches.
    fn drain(&mut self, db: &str, reply: Document, keep: usize) -> Result<Vec<Document>, Failure> {
        let mut out = Vec::new();
        let mut reply = reply;
        let mut field = "firstBatch";
        loop {
            let cursor =
                reply.get("cursor").and_then(Bson::as_document).cloned().unwrap_or_default();
            if let Some(Bson::Array(b)) = cursor.get(field) {
                out.extend(b.iter().filter_map(Bson::as_document).cloned());
            }
            if let Some(tell) = self.on_batch.as_mut() {
                tell(out.len() as u64);
            }
            let id = match cursor.get("id") {
                Some(Bson::Int64(id)) => *id,
                _ => 0,
            };
            if id == 0 {
                return Ok(out);
            }
            let ns = match cursor.get("ns") {
                Some(Bson::String(ns)) => ns.clone(),
                _ => String::new(),
            };
            let coll = ns.split_once('.').map_or("", |(_, c)| c).to_owned();
            if out.len() > keep {
                let kill = Document::new()
                    .with("killCursors", coll)
                    .with("cursors", vec![Bson::Int64(id)]);
                let _ = self.exec.run(db, kill);
                return Ok(out);
            }
            // `maxTimeMS` of a `getMore` is only the wait of a tailable cursor: the query's
            // limit applies to the command that opened it.
            reply =
                self.run(db, Document::new().with("getMore", id).with("collection", coll), false)?;
            field = "nextBatch";
        }
    }

    /// `docs` within the documents and bytes of the call: the documents kept, their text, and
    /// the limits applied.
    fn limit(&self, docs: &[Document], bytes_arg: Option<i64>) -> (Vec<Value>, String, Vec<Value>) {
        let max_docs = usize::try_from(self.limits.max_documents).unwrap_or(usize::MAX);
        let asked = bytes_arg.map(|b| b.max(0) as u64).filter(|b| *b < self.limits.max_bytes);
        let max_bytes = asked.unwrap_or(self.limits.max_bytes);
        // The names RefDb MCP Server gives the limits in `appliedLimits` (its `outputSchema`).
        let bytes_limit = if asked.is_some() { LIMIT_TOOL_BYTES } else { LIMIT_CONFIG_BYTES };
        let mut applied = Vec::new();
        let mut kept = Vec::new();
        let mut size = 2;
        for (k, d) in docs.iter().enumerate() {
            if k >= max_docs {
                applied.push(Value::String(LIMIT_CONFIG_DOCUMENTS.into()));
                break;
            }
            let v = to_json(d);
            let len = to_string(&v).map_or(0, |t| t.len() + 1) as u64;
            if size + len > max_bytes {
                applied.push(Value::String(bytes_limit.into()));
                break;
            }
            size += len;
            kept.push(v);
        }
        let text = to_string(&Value::Array(kept.clone())).unwrap_or_default();
        (kept, text, applied)
    }
}

/// Refuses `$out` and `$merge`, so the aggregation tools only read.
fn read_only_pipeline(pipeline: &[Bson]) -> Result<(), Failure> {
    let writes = pipeline
        .iter()
        .filter_map(Bson::as_document)
        .any(|s| s.get("$out").is_some() || s.get("$merge").is_some());
    if writes {
        Err("$out and $merge are not allowed: the tool only reads".to_owned())
    } else {
        Ok(())
    }
}

/// `appliedLimits` when `mcpOptions.maxDocuments` cut the documents.
const LIMIT_CONFIG_DOCUMENTS: &str = "config.maxDocumentsPerQuery";
/// `appliedLimits` when `mcpOptions.maxBytes` cut them.
const LIMIT_CONFIG_BYTES: &str = "config.maxBytesPerQuery";
/// `appliedLimits` when the call's smaller `responseBytesLimit` cut them.
const LIMIT_TOOL_BYTES: &str = "tool.responseBytesLimit";
/// The fields of a search index in `collection-indexes` (the reference's `outputSchema`).
const SEARCH_INDEX_FIELDS: &[&str] = &["name", "type", "status", "queryable", "latestDefinition"];
/// The name `list-connections` gives the server's connection.
const CONNECTION_NAME: &str = "teseladb";
/// `source` of a connection the server fixes (RefDb MCP Server's enumeration).
const PRECONFIGURED: &str = "preconfigured";
/// `state` of an open connection.
const CONNECTED: &str = "connected";
/// `outcome` of `disconnect`.
const DISCONNECTED: &str = "disconnected";

/// The fields `fields` of `d` that it has, in that order, as relaxed Extended JSON.
fn keep_fields(d: &Document, fields: &[&str]) -> Value {
    let mut kept = Document::new();
    for f in fields {
        if let Some(x) = d.get(f) {
            kept.push(*f, x.clone());
        }
    }
    to_json(&kept)
}

/// An `_id` as the text `upsertedId` carries: an ObjectId in hexadecimal, a string as it is, and
/// any other value in relaxed Extended JSON.
fn id_text(id: &Bson) -> Value {
    match id {
        Bson::ObjectId(oid) => Value::String(oid.iter().map(|b| format!("{b:02x}")).collect()),
        Bson::String(text) => Value::String(text.clone()),
        other => {
            let v = to_json(&Document::new().with("id", other.clone()));
            let inner = v.get("id").cloned().unwrap_or(Value::Null);
            Value::String(to_string(&inner).unwrap_or_default())
        }
    }
}

fn obj(fields: Vec<(&str, Value)>) -> Object {
    let mut o = Object::new();
    for (k, v) in fields {
        o.insert(k, v);
    }
    o
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

fn n_of(d: &Document, f: &str) -> i64 {
    match d.get(f) {
        Some(Bson::Int32(n)) => i64::from(*n),
        Some(Bson::Int64(n)) => *n,
        Some(Bson::Double(x)) => *x as i64,
        _ => 0,
    }
}

/// The `$type` alias of `v`.
fn type_alias(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary { .. } => "binData",
        Bson::Undefined => "undefined",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::Regex { .. } => "regex",
        Bson::DbPointer { .. } => "dbPointer",
        Bson::JavaScript(_) => "javascript",
        Bson::Symbol(_) => "symbol",
        Bson::JavaScriptWithScope { .. } => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Timestamp { .. } => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        Bson::MinKey => "minKey",
        Bson::MaxKey => "maxKey",
    }
}

/// The BSON type names of the top-level fields of `docs`.
fn schema(docs: &[Document]) -> Object {
    let mut fields: Vec<(String, Vec<&'static str>)> = Vec::new();
    for d in docs {
        for (k, v) in d.iter() {
            let t = type_alias(v);
            match fields.iter_mut().find(|(f, _)| f == k) {
                Some((_, ts)) if !ts.contains(&t) => ts.push(t),
                Some(_) => {}
                None => fields.push((k.to_owned(), vec![t])),
            }
        }
    }
    let mut o = Object::new();
    for (f, ts) in fields {
        o.insert(
            f,
            Value::Object(obj(vec![("types", Value::Array(ts.into_iter().map(s).collect()))])),
        );
    }
    o
}

/// Runs tool `name` with `args` (already checked against its schema).
pub fn call<E: Executor + ?Sized>(
    c: &mut Ctx<'_, E>,
    name: &str,
    args: &Object,
) -> Result<Output, Failure> {
    if let Some(id) = args.get("connectionId").and_then(Value::as_str)
        && id != c.connection_id
    {
        return Err(format!("Connection {id} not found"));
    }
    let db = arg_str(args, "database").to_owned();
    let coll = arg_str(args, "collection").to_owned();
    let bytes = args.get("responseBytesLimit").and_then(Value::as_i64);
    let structured = |fields: Vec<(&str, Value)>| obj(fields);
    let out = |summary: String, st: Object| Output { summary, structured: st, ..Output::default() };
    match name {
        "list-databases" => {
            let r = c.run("admin", Document::new().with("listDatabases", 1), true)?;
            let dbs: Vec<Value> = match r.get("databases") {
                Some(Bson::Array(a)) => a
                    .iter()
                    .filter_map(Bson::as_document)
                    .map(|d| {
                        Value::Object(obj(vec![
                            ("name", to_json(d).get("name").cloned().unwrap_or(Value::Null)),
                            ("size", int(n_of(d, "sizeOnDisk"))),
                        ]))
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let n = dbs.len() as i64;
            let text = to_string(&Value::Array(dbs.clone())).unwrap_or_default();
            Ok(Output {
                data: Some(text),
                ..out(
                    format!("Found {n} databases"),
                    structured(vec![("databases", Value::Array(dbs)), ("totalCount", int(n))]),
                )
            })
        }
        "list-collections" => {
            let r = c.run(
                &db,
                Document::new().with("listCollections", 1).with("nameOnly", true),
                true,
            )?;
            let docs = c.drain(&db, r, usize::MAX)?;
            let names: Vec<Value> = docs
                .iter()
                .map(|d| {
                    Value::Object(obj(vec![(
                        "name",
                        to_json(d).get("name").cloned().unwrap_or(Value::Null),
                    )]))
                })
                .collect();
            let n = names.len() as i64;
            let text = to_string(&Value::Array(names.clone())).unwrap_or_default();
            Ok(Output {
                data: Some(text),
                ..out(
                    format!("Found {n} collections in database \"{db}\""),
                    structured(vec![("collections", Value::Array(names)), ("totalCount", int(n))]),
                )
            })
        }
        "collection-schema" => {
            let size = arg_int(args, "sampleSize", 50).clamp(1, c.limits.max_documents as i64);
            let pipeline = vec![Bson::Document(
                Document::new().with("$sample", Document::new().with("size", size)),
            )];
            let r = c.run(
                &db,
                Document::new()
                    .with("aggregate", coll.as_str())
                    .with("pipeline", pipeline)
                    .with("cursor", Document::new()),
                true,
            )?;
            let docs = c.drain(&db, r, usize::MAX)?;
            let sch = schema(&docs);
            let n = sch.len() as i64;
            let text = to_string(&Value::Object(sch.clone())).unwrap_or_default();
            Ok(Output {
                data: Some(text),
                ..out(
                    format!("Found {n} fields in the schema of \"{db}.{coll}\""),
                    structured(vec![("schema", Value::Object(sch)), ("fieldsCount", int(n))]),
                )
            })
        }
        "collection-indexes" => {
            let r = c.run(&db, Document::new().with("listIndexes", coll.as_str()), true)?;
            let classic = c.drain(&db, r, usize::MAX)?;
            let stage =
                vec![Bson::Document(Document::new().with("$listSearchIndexes", Document::new()))];
            let search = match c.run(
                &db,
                Document::new()
                    .with("aggregate", coll.as_str())
                    .with("pipeline", stage)
                    .with("cursor", Document::new()),
                true,
            ) {
                Ok(r) => c.drain(&db, r, usize::MAX)?,
                Err(_) => Vec::new(),
            };
            let all: Vec<Value> = classic.iter().map(to_json).collect();
            let text = to_string(&Value::Array(all)).unwrap_or_default();
            // The structured list keeps the name and the key of each index, as the reference's
            // `outputSchema` describes it; the whole definitions go in the text.
            let cl: Vec<Value> = classic.iter().map(|i| keep_fields(i, &["name", "key"])).collect();
            let sr: Vec<Value> =
                search.iter().map(|i| keep_fields(i, SEARCH_INDEX_FIELDS)).collect();
            let summary = format!(
                "Found {} classic indexes and {} search indexes in \"{db}.{coll}\"",
                cl.len(),
                sr.len()
            );
            let st = structured(vec![
                ("classicIndexesCount", int(cl.len() as i64)),
                ("searchIndexesCount", int(sr.len() as i64)),
                ("classicIndexes", Value::Array(cl)),
                ("searchIndexes", Value::Array(sr)),
            ]);
            Ok(Output { data: Some(text), ..out(summary, st) })
        }
        "collection-storage-size" => {
            let r = c.run(&db, Document::new().with("collStats", coll.as_str()), true)?;
            let size = n_of(&r, "storageSize");
            Ok(out(
                format!("The size of \"{db}.{coll}\" is {size} bytes"),
                structured(vec![("size", int(size)), ("units", s("bytes"))]),
            ))
        }
        "db-stats" => {
            let r = c.run(&db, Document::new().with("dbStats", 1), true)?;
            let mut stats = r.clone();
            for f in ["ok", "operationTime", "$clusterTime"] {
                stats.remove(f);
            }
            let v = to_json(&stats);
            Ok(Output {
                data: to_string(&v).ok(),
                ..out(format!("Statistics of database \"{db}\""), structured(vec![("stats", v)]))
            })
        }
        "find" => {
            let filter = arg_doc(args, "filter")?;
            let limit = arg_int(args, "limit", 10).max(0).min(c.limits.max_documents as i64);
            let mut cmd = Document::new()
                .with("find", coll.as_str())
                .with("filter", filter.clone())
                .with("limit", limit);
            for opt in ["projection", "sort"] {
                if args.get(opt).is_some() {
                    cmd.push(opt, arg_doc(args, opt)?);
                }
            }
            let r = c.run(&db, cmd, true)?;
            let docs = c.drain(&db, r, limit as usize)?;
            let total = n_of(
                &c.run(
                    &db,
                    Document::new().with("count", coll.as_str()).with("query", filter),
                    true,
                )?,
                "n",
            );
            let (kept, text, applied) = c.limit(&docs, bytes);
            let summary = format!(
                "Query on collection \"{coll}\" resulted in {total} documents. Returning {} documents.",
                kept.len()
            );
            let st = structured(vec![
                ("documents", Value::Array(kept)),
                ("queryResultsCount", int(total)),
                ("appliedLimits", Value::Array(applied)),
            ]);
            Ok(Output { data: Some(text), ..out(summary, st) })
        }
        "aggregate" | "aggregate-db" => {
            let pipeline = arg_array(args, "pipeline")?;
            read_only_pipeline(&pipeline)?;
            let target: Bson =
                if name == "aggregate" { coll.as_str().into() } else { Bson::Int32(1) };
            let r = c.run(
                &db,
                Document::new()
                    .with("aggregate", target)
                    .with("pipeline", pipeline)
                    .with("cursor", Document::new()),
                true,
            )?;
            let docs = c.drain(&db, r, c.limits.max_documents as usize)?;
            let (kept, text, applied) = c.limit(&docs, bytes);
            let count = if name == "aggregate" { "count" } else { "aggResultsCount" };
            let summary = format!(
                "The aggregation resulted in {} documents. Returning {} documents.",
                docs.len(),
                kept.len()
            );
            let st = structured(vec![
                ("documents", Value::Array(kept)),
                (count, int(docs.len() as i64)),
                ("appliedLimits", Value::Array(applied)),
            ]);
            Ok(Output { data: Some(text), ..out(summary, st) })
        }
        "count" => {
            let r = c.run(
                &db,
                Document::new().with("count", coll.as_str()).with("query", arg_doc(args, "query")?),
                true,
            )?;
            let n = n_of(&r, "n");
            Ok(out(
                format!("Found {n} documents in the collection \"{coll}\""),
                structured(vec![("count", int(n))]),
            ))
        }
        "explain" => {
            let method = arg_array(args, "method")?;
            let first = method.first().and_then(Bson::as_document).cloned().unwrap_or_default();
            let m = match first.get("name") {
                Some(Bson::String(m)) => m.clone(),
                _ => return Err("method must name find, aggregate or count".to_owned()),
            };
            let a = first.get("arguments").and_then(Bson::as_document).cloned().unwrap_or_default();
            let mut inner = Document::new().with(m.as_str(), coll.as_str());
            match m.as_str() {
                "find" => inner.push(
                    "filter",
                    a.get("filter").cloned().unwrap_or(Bson::Document(Document::new())),
                ),
                "aggregate" => {
                    inner.push(
                        "pipeline",
                        a.get("pipeline").cloned().unwrap_or(Bson::Array(Vec::new())),
                    );
                    inner.push("cursor", Document::new());
                }
                "count" => inner.push(
                    "query",
                    a.get("query").cloned().unwrap_or(Bson::Document(Document::new())),
                ),
                other => return Err(format!("method {other} cannot be explained")),
            }
            let verbosity =
                args.get("verbosity").and_then(Value::as_str).unwrap_or("queryPlanner").to_owned();
            let r = c.run(
                &db,
                Document::new().with("explain", inner).with("verbosity", verbosity.as_str()),
                true,
            )?;
            let v = to_json(&r);
            Ok(Output {
                data: to_string(&v).ok(),
                ..out(
                    format!("Explain of {m} on \"{db}.{coll}\""),
                    structured(vec![
                        ("explainResult", v),
                        ("method", s(&m)),
                        ("verbosity", s(&verbosity)),
                    ]),
                )
            })
        }
        "export" => {
            let target = arg_array(args, "exportTarget")?;
            let first = target.first().and_then(Bson::as_document).cloned().unwrap_or_default();
            let a = first.get("arguments").and_then(Bson::as_document).cloned().unwrap_or_default();
            let cmd = match first.get("name") {
                Some(Bson::String(m)) if m == "find" => {
                    Document::new().with("find", coll.as_str()).with(
                        "filter",
                        a.get("filter").cloned().unwrap_or(Bson::Document(Document::new())),
                    )
                }
                Some(Bson::String(m)) if m == "aggregate" => {
                    let p = match a.get("pipeline") {
                        Some(Bson::Array(p)) => p.clone(),
                        _ => Vec::new(),
                    };
                    read_only_pipeline(&p)?;
                    Document::new()
                        .with("aggregate", coll.as_str())
                        .with("pipeline", p)
                        .with("cursor", Document::new())
                }
                _ => return Err("exportTarget must name find or aggregate".to_owned()),
            };
            let r = c.run(&db, cmd, true)?;
            let docs = c.drain(&db, r, usize::MAX)?;
            let text =
                to_string(&Value::Array(docs.iter().map(to_json).collect())).unwrap_or_default();
            let title = arg_str(args, "exportTitle").to_owned();
            Ok(Output {
                export: Some((title.clone(), text)),
                ..out(
                    format!(
                        "Data for \"{db}.{coll}\" exported as \"{title}\" with {} documents",
                        docs.len()
                    ),
                    Object::new(),
                )
            })
        }
        "refdb-logs" => {
            let kind = args.get("type").and_then(Value::as_str).unwrap_or("global").to_owned();
            let limit = arg_int(args, "limit", 50).clamp(1, 1024) as usize;
            let r = c.run("admin", Document::new().with("getLog", kind.as_str()), false)?;
            let lines: Vec<Value> = match r.get("log") {
                Some(Bson::Array(a)) => a
                    .iter()
                    .rev()
                    .take(limit)
                    .rev()
                    .filter_map(|l| match l {
                        Bson::String(t) => Some(s(t)),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let shown = lines.len() as i64;
            let text = to_string(&Value::Array(lines.clone())).unwrap_or_default();
            Ok(Output {
                data: Some(text),
                ..out(
                    format!("Found {} log lines, showing {shown}", n_of(&r, "totalLinesWritten")),
                    structured(vec![
                        ("logs", Value::Array(lines)),
                        ("totalLinesWritten", int(n_of(&r, "totalLinesWritten"))),
                        ("shownCount", int(shown)),
                    ]),
                )
            })
        }
        "insert-many" => {
            let mut ids = Vec::new();
            let docs: Vec<Bson> = arg_array(args, "documents")?
                .into_iter()
                .map(|d| match d {
                    Bson::Document(d) if d.get("_id").is_none() => {
                        let id = Bson::ObjectId(c.exec.object_id());
                        let mut with = Document::new().with("_id", id.clone());
                        for (k, v) in d.iter() {
                            with.push(k, v.clone());
                        }
                        ids.push(id);
                        Bson::Document(with)
                    }
                    Bson::Document(d) => {
                        ids.push(d.get("_id").cloned().unwrap_or(Bson::Null));
                        Bson::Document(d)
                    }
                    other => other,
                })
                .collect();
            let r = c.run(
                &db,
                Document::new().with("insert", coll.as_str()).with("documents", docs),
                true,
            )?;
            if let Some(Bson::Array(e)) = r.get("writeErrors") {
                let msg = e
                    .first()
                    .and_then(Bson::as_document)
                    .and_then(|e| e.get("errmsg"))
                    .map(|m| format!("{m:?}"));
                return Err(msg.unwrap_or_else(|| "the insert failed".to_owned()));
            }
            let n = n_of(&r, "n");
            let ids_json = to_json(&Document::new().with("ids", ids))
                .get("ids")
                .cloned()
                .unwrap_or(Value::Null);
            Ok(out(
                format!("Inserted {n} documents into \"{db}.{coll}\""),
                structured(vec![
                    ("database", s(&db)),
                    ("collection", s(&coll)),
                    ("insertedCount", int(n)),
                    ("insertedIds", ids_json),
                ]),
            ))
        }
        "create-index" => {
            let def = arg_array(args, "definition")?;
            let first = def.first().and_then(Bson::as_document).cloned().unwrap_or_default();
            let keys = first
                .get("keys")
                .and_then(Bson::as_document)
                .cloned()
                .ok_or("definition needs keys")?;
            let index_name = match args.get("name").and_then(Value::as_str) {
                Some(n) => n.to_owned(),
                None => keys
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{k}_{}",
                            match v {
                                Bson::Int32(n) => n.to_string(),
                                Bson::String(t) => t.clone(),
                                other => format!("{other:?}"),
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("_"),
            };
            let spec = Document::new().with("key", keys).with("name", index_name.as_str());
            c.run(
                &db,
                Document::new()
                    .with("createIndexes", coll.as_str())
                    .with("indexes", vec![Bson::Document(spec)]),
                true,
            )?;
            Ok(out(
                format!("Created the index \"{index_name}\" on \"{db}.{coll}\""),
                structured(vec![
                    ("database", s(&db)),
                    ("collection", s(&coll)),
                    ("indexName", s(&index_name)),
                    ("indexType", s("classic")),
                ]),
            ))
        }
        "create-collection" => {
            c.run(&db, Document::new().with("create", coll.as_str()), true)?;
            Ok(out(
                format!("Collection \"{coll}\" created in database \"{db}\""),
                structured(vec![
                    ("database", s(&db)),
                    ("collection", s(&coll)),
                    ("created", Value::Bool(true)),
                ]),
            ))
        }
        "update-many" => {
            let upsert = args.get("upsert").and_then(Value::as_bool).unwrap_or(false);
            let stmt = Document::new()
                .with("q", arg_doc(args, "filter")?)
                .with("u", arg_doc(args, "update")?)
                .with("multi", true)
                .with("upsert", upsert);
            let r = c.run(
                &db,
                Document::new()
                    .with("update", coll.as_str())
                    .with("updates", vec![Bson::Document(stmt)]),
                true,
            )?;
            let upserted = match r.get("upserted") {
                Some(Bson::Array(u)) => u.clone(),
                _ => Vec::new(),
            };
            let upserted_id = upserted
                .first()
                .and_then(Bson::as_document)
                .and_then(|d| d.get("_id"))
                .map(id_text);
            let matched = n_of(&r, "n") - upserted.len() as i64;
            let st = structured(vec![
                ("database", s(&db)),
                ("collection", s(&coll)),
                ("matchedCount", int(matched)),
                ("modifiedCount", int(n_of(&r, "nModified"))),
                ("upsertedCount", int(upserted.len() as i64)),
            ]);
            let mut st = st;
            if let Some(id) = upserted_id {
                st.insert("upsertedId", id);
            }
            Ok(out(
                format!(
                    "Matched {matched} documents in \"{db}.{coll}\", modified {}",
                    n_of(&r, "nModified")
                ),
                st,
            ))
        }
        "rename-collection" => {
            let to = arg_str(args, "newName").to_owned();
            let drop_target = args.get("dropTarget").and_then(Value::as_bool).unwrap_or(false);
            c.run(
                "admin",
                Document::new()
                    .with("renameCollection", format!("{db}.{coll}"))
                    .with("to", format!("{db}.{to}"))
                    .with("dropTarget", drop_target),
                true,
            )?;
            Ok(out(
                format!("Collection \"{coll}\" renamed to \"{to}\" in database \"{db}\""),
                structured(vec![
                    ("database", s(&db)),
                    ("oldCollection", s(&coll)),
                    ("newCollection", s(&to)),
                    ("renamed", Value::Bool(true)),
                ]),
            ))
        }
        "delete-many" => {
            let stmt = Document::new().with("q", arg_doc(args, "filter")?).with("limit", 0);
            let r = c.run(
                &db,
                Document::new()
                    .with("delete", coll.as_str())
                    .with("deletes", vec![Bson::Document(stmt)]),
                true,
            )?;
            let n = n_of(&r, "n");
            Ok(out(
                format!("Deleted {n} documents from \"{db}.{coll}\""),
                structured(vec![
                    ("database", s(&db)),
                    ("collection", s(&coll)),
                    ("deletedCount", int(n)),
                ]),
            ))
        }
        "drop-index" => {
            let index = arg_str(args, "indexName").to_owned();
            let cmd = if arg_str(args, "type") == "search" {
                Document::new().with("dropSearchIndex", coll.as_str()).with("name", index.as_str())
            } else {
                Document::new().with("dropIndexes", coll.as_str()).with("index", index.as_str())
            };
            c.run(&db, cmd, true)?;
            Ok(out(
                format!("Dropped the index \"{index}\" of \"{db}.{coll}\""),
                structured(vec![
                    ("database", s(&db)),
                    ("collection", s(&coll)),
                    ("indexName", s(&index)),
                    ("dropped", Value::Bool(true)),
                ]),
            ))
        }
        "drop-collection" => {
            c.run(&db, Document::new().with("drop", coll.as_str()), true)?;
            Ok(out(
                format!("Dropped the collection \"{coll}\" of database \"{db}\""),
                structured(vec![
                    ("database", s(&db)),
                    ("collection", s(&coll)),
                    ("dropped", Value::Bool(true)),
                ]),
            ))
        }
        "drop-database" => {
            c.run(&db, Document::new().with("dropDatabase", 1), true)?;
            Ok(out(
                format!("Dropped the database \"{db}\""),
                structured(vec![("database", s(&db)), ("dropped", Value::Bool(true))]),
            ))
        }
        "connect" => Ok(out(
            format!("The server fixes the connection: use connectionId {}", c.connection_id),
            structured(vec![("connectionId", s(c.connection_id))]),
        )),
        "disconnect" => Ok(out(
            "Disconnected; the server opens the connection again on the next call".to_owned(),
            structured(vec![("outcome", s(DISCONNECTED))]),
        )),
        "list-connections" => {
            let now = i64::try_from(c.exec.now_ms()).unwrap_or(i64::MAX);
            let at = extjson::to_log_time(now).unwrap_or_default();
            let one = Value::Object(obj(vec![
                ("connectionId", s(c.connection_id)),
                ("name", s(CONNECTION_NAME)),
                ("source", s(PRECONFIGURED)),
                ("description", s("The connection of the authenticated user, fixed by the server")),
                ("state", s(CONNECTED)),
                ("createdAt", s(&at)),
                ("lastUsedAt", s(&at)),
            ]));
            Ok(out(
                "One connection, fixed by the server".to_owned(),
                structured(vec![("connections", Value::Array(vec![one]))]),
            ))
        }
        other => Err(format!("Tool {other} not found")),
    }
}
