//! The command each route builds (FR-005, FR-007, contract `rest-api.md`): the fields of the body
//! go as the command's options, the path names its database and collection.

use teseladb_wire::error::{BAD_VALUE, COMMAND_NOT_SUPPORTED, CommandError, FAILED_TO_PARSE};
use teseladb_bson::{Document, Value, extjson};

use crate::routes::{Op, Params};

/// Fields that tie a command to a connection or a session of the wire protocol: `/command` and
/// the bodies of every route refuse them (cursors and transactions have their routes).
pub const SESSION_FIELDS: [&str; 5] =
    ["lsid", "txnNumber", "autocommit", "startTransaction", "$clusterTime"];

/// Commands `/command` refuses with `CommandNotSupported`.
pub const REFUSED_COMMANDS: [&str; 12] = [
    "hello",
    "isMaster",
    "saslStart",
    "saslContinue",
    "authenticate",
    "logout",
    "getMore",
    "killCursors",
    "startSession",
    "endSessions",
    "commitTransaction",
    "abortTransaction",
];

/// Commands of `/command` that `TeselaDB-After-Cluster-Time` applies to: those that take
/// `readConcern.afterClusterTime`.
const READ_COMMANDS: [&str; 4] = ["find", "aggregate", "count", "distinct"];

/// How the reply of a route's command becomes its HTTP answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// The reply as it is.
    Plain,
    /// A reply with a cursor: the cursor's public id and the link to its next batch.
    Cursor,
    /// The `n` of the only document of an `aggregate` cursor, as `{count}`.
    CountDocuments,
    /// The `n` of `count`, as `{count}`.
    Count,
    /// The first document of the cursor, `404` without one.
    OneDocument,
    /// A write of `operations` operations; `404` when it matched nothing and `missing` holds.
    Write {
        /// Operations.
        operations: usize,
        /// The route names one document by `_id`.
        missing: bool,
    },
    /// An insert, with the `_id` of each document.
    Insert {
        /// The `_id`s, in order.
        ids: Vec<Value>,
        /// One document (`insertedId`) or several (`insertedIds`).
        one: bool,
    },
}

/// A route's command.
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    /// Its database.
    pub db: String,
    /// The command.
    pub command: Document,
    /// What to make of its reply.
    pub shape: Shape,
    /// It reads, so `TeselaDB-After-Cluster-Time` applies to it.
    pub reads: bool,
}

fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::new(BAD_VALUE, msg)
}

/// Refuses the fields of a session or a connection in `body`.
pub fn check_session_fields(body: &Document) -> Result<(), CommandError> {
    match body.iter().find(|(k, _)| SESSION_FIELDS.contains(k) || *k == "$db") {
        Some((k, _)) => Err(CommandError::new(
            COMMAND_NOT_SUPPORTED,
            format!("{k} is not supported over HTTP: use the routes of cursors and transactions"),
        )),
        None => Ok(()),
    }
}

/// The value of document id `id` from the path: relaxed Extended JSON (`42`, `"abc"`,
/// `{"$oid": "…"}`) when it reads as such, otherwise the text itself.
pub fn document_id(id: &str) -> Value {
    extjson::parse(&format!("{{\"_id\":{id}}}"))
        .ok()
        .and_then(|mut d| d.remove("_id"))
        .unwrap_or_else(|| Value::String(id.to_owned()))
}

/// `name: target` followed by the fields of `body` other than `skip`.
fn command(name: &str, target: impl Into<Value>, body: &Document, skip: &[&str]) -> Document {
    let mut d = Document::new().with(name, target);
    for (k, v) in body.iter().filter(|(k, _)| *k != name && !skip.contains(k)) {
        d.push(k, v.clone());
    }
    d
}

fn take_doc(body: &Document, field: &str) -> Result<Document, CommandError> {
    match body.get(field) {
        Some(Value::Document(d)) => Ok(d.clone()),
        Some(_) => Err(bad(format!("{field} must be a document"))),
        None => Err(CommandError::new(FAILED_TO_PARSE, format!("{field} is required"))),
    }
}

fn opt_doc(body: &Document, field: &str) -> Result<Document, CommandError> {
    match body.get(field) {
        None => Ok(Document::new()),
        Some(_) => take_doc(body, field),
    }
}

/// Fields of an update or delete statement, as opposed to options of the whole command.
const STATEMENT_FIELDS: [&str; 6] = ["upsert", "arrayFilters", "collation", "hint", "sort", "c"];

/// An `update` statement of `filter` and `u`, with the statement fields of `body`.
fn update(coll: &str, body: &Document, q: Document, u: Value, multi: bool) -> Document {
    let mut stmt = Document::new().with("q", q).with("u", u).with("multi", multi);
    for (k, v) in body.iter().filter(|(k, _)| STATEMENT_FIELDS.contains(k)) {
        stmt.push(k, v.clone());
    }
    let mut skip = vec!["filter", "update", "replacement"];
    skip.extend(STATEMENT_FIELDS);
    let mut d = command("update", coll, body, &skip);
    d.insert("updates", vec![Value::Document(stmt)]);
    d
}

fn delete(coll: &str, body: &Document, q: Document, limit: i32) -> Document {
    let mut stmt = Document::new().with("q", q).with("limit", limit);
    for (k, v) in body.iter().filter(|(k, _)| matches!(*k, "collation" | "hint")) {
        stmt.push(k, v.clone());
    }
    let mut d = command("delete", coll, body, &["filter", "collation", "hint"]);
    d.insert("deletes", vec![Value::Document(stmt)]);
    d
}

/// The update of a body: a document or a pipeline.
fn update_value(body: &Document, field: &str) -> Result<Value, CommandError> {
    match body.get(field) {
        Some(v @ (Value::Document(_) | Value::Array(_))) => Ok(v.clone()),
        Some(_) => Err(bad(format!("{field} must be a document or a pipeline"))),
        None => Err(CommandError::new(FAILED_TO_PARSE, format!("{field} is required"))),
    }
}

/// `document` with an `_id` first: its own, or a new ObjectId.
fn with_id(doc: Document, new_id: &mut dyn FnMut() -> [u8; 12]) -> (Value, Document) {
    if let Some(id) = doc.get("_id") {
        return (id.clone(), doc);
    }
    let id = Value::ObjectId(new_id());
    let mut d = Document::new().with("_id", id.clone());
    for (k, v) in doc.iter() {
        d.push(k, v.clone());
    }
    (id, d)
}

fn insert(
    coll: &str,
    body: &Document,
    docs: Vec<Document>,
    one: bool,
    new_id: &mut dyn FnMut() -> [u8; 12],
) -> (Document, Shape) {
    let (ids, docs): (Vec<Value>, Vec<Value>) = docs
        .into_iter()
        .map(|d| {
            let (id, d) = with_id(d, new_id);
            (id, Value::Document(d))
        })
        .unzip();
    let mut d = command("insert", coll, body, &["document", "documents"]);
    d.insert("documents", docs);
    (d, Shape::Insert { ids, one })
}

fn documents(body: &Document) -> Result<Vec<Document>, CommandError> {
    match body.get("documents") {
        Some(Value::Array(vs)) => vs
            .iter()
            .map(|v| match v {
                Value::Document(d) => Ok(d.clone()),
                _ => Err(bad("each of documents must be a document")),
            })
            .collect(),
        Some(_) => Err(bad("documents must be an array")),
        None => Err(CommandError::new(FAILED_TO_PARSE, "documents is required")),
    }
}

/// The `bulkWrite` command of the driver-style operations of `body` on namespace `ns`.
fn bulk_write(ns: &str, body: &Document) -> Result<Document, CommandError> {
    let Some(Value::Array(ops)) = body.get("operations") else {
        return Err(CommandError::new(FAILED_TO_PARSE, "operations must be an array"));
    };
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let Some((kind, Value::Document(args))) = op.as_document().and_then(|d| d.iter().next())
        else {
            return Err(bad("each operation must be a document with one field"));
        };
        let mut o = match kind {
            "insertOne" => {
                Document::new().with("insert", 0).with("document", take_doc(args, "document")?)
            }
            "updateOne" | "updateMany" => Document::new()
                .with("update", 0)
                .with("filter", take_doc(args, "filter")?)
                .with("updateMods", update_value(args, "update")?)
                .with("multi", kind == "updateMany"),
            "replaceOne" => Document::new()
                .with("update", 0)
                .with("filter", take_doc(args, "filter")?)
                .with("updateMods", take_doc(args, "replacement")?),
            "deleteOne" | "deleteMany" => Document::new()
                .with("delete", 0)
                .with("filter", take_doc(args, "filter")?)
                .with("multi", kind == "deleteMany"),
            other => return Err(bad(format!("unknown bulkWrite operation {other}"))),
        };
        for (k, v) in args.iter().filter(|(k, _)| {
            matches!(*k, "upsert" | "arrayFilters" | "collation" | "hint" | "sort")
        }) {
            o.push(k, v.clone());
        }
        out.push(Value::Document(o));
    }
    let mut d = Document::new().with("bulkWrite", 1).with("ops", out);
    d.push("nsInfo", vec![Value::Document(Document::new().with("ns", ns))]);
    for (k, v) in body.iter().filter(|(k, _)| *k != "operations") {
        d.push(k, v.clone());
    }
    Ok(d)
}

/// The command of route `op` with path `params`, query parameters `query` and `body`;
/// `new_id` draws the ObjectIds of inserted documents without `_id`.
pub fn build(
    op: Op,
    p: &Params,
    query: &str,
    body: &Document,
    new_id: &mut dyn FnMut() -> [u8; 12],
) -> Result<Built, CommandError> {
    check_session_fields(body)?;
    let c = p.coll.as_str();
    let plain = |db: &str, command: Document| Built {
        db: db.to_owned(),
        command,
        shape: Shape::Plain,
        reads: false,
    };
    let by_id = || Document::new().with("_id", document_id(&p.id));
    let built = match op {
        Op::ListDatabases => {
            plain("admin", list_query(Document::new().with("listDatabases", 1), query)?)
        }
        Op::ListCollections => Built {
            shape: Shape::Cursor,
            ..plain(&p.db, list_query(Document::new().with("listCollections", 1), query)?)
        },
        Op::CreateCollection => match body.get("name") {
            Some(Value::String(name)) => {
                plain(&p.db, command("create", name.as_str(), body, &["name"]))
            }
            _ => return Err(CommandError::new(FAILED_TO_PARSE, "name must be a string")),
        },
        Op::DropCollection => plain(&p.db, Document::new().with("drop", c)),
        Op::Find => read(&p.db, command("find", c, body, &[]), Shape::Cursor),
        Op::Aggregate => {
            let mut d = command("aggregate", c, body, &["batchSize"]);
            if d.get("cursor").is_none() {
                let mut cursor = Document::new();
                if let Some(b) = body.get("batchSize") {
                    cursor.push("batchSize", b.clone());
                }
                d.push("cursor", cursor);
            }
            read(&p.db, d, Shape::Cursor)
        }
        Op::CountDocuments => {
            let mut pipeline =
                vec![Value::Document(Document::new().with("$match", opt_doc(body, "filter")?))];
            for stage in ["skip", "limit"] {
                if let Some(v) = body.get(stage) {
                    pipeline.push(Value::Document(
                        Document::new().with(format!("${stage}"), v.clone()),
                    ));
                }
            }
            let group = Document::new().with("_id", 1).with("n", Document::new().with("$sum", 1));
            pipeline.push(Value::Document(Document::new().with("$group", group)));
            let mut d = command("aggregate", c, body, &["filter", "skip", "limit"]);
            d.insert("pipeline", pipeline);
            d.push("cursor", Document::new());
            read(&p.db, d, Shape::CountDocuments)
        }
        Op::EstimatedDocumentCount => read(&p.db, Document::new().with("count", c), Shape::Count),
        Op::Distinct => read(&p.db, command("distinct", c, body, &[]), Shape::Plain),
        Op::GetDocument => {
            let d = Document::new()
                .with("find", c)
                .with("filter", by_id())
                .with("limit", 1)
                .with("singleBatch", true);
            read(&p.db, d, Shape::OneDocument)
        }
        Op::PutDocument => {
            let upsert = Document::new().with("upsert", true);
            let d = update(c, &upsert, by_id(), Value::Document(body.clone()), false);
            write(&p.db, d, 1, false)
        }
        Op::PatchDocument => {
            // A pipeline arrives as `{"pipeline": […]}`, since a body is a document.
            let u = match body.get("pipeline") {
                Some(v @ Value::Array(_)) if body.len() == 1 => v.clone(),
                _ => Value::Document(body.clone()),
            };
            write(&p.db, update(c, &Document::new(), by_id(), u, false), 1, true)
        }
        Op::DeleteDocument => write(&p.db, delete(c, &Document::new(), by_id(), 1), 1, true),
        Op::InsertOne => {
            let (d, shape) = insert(c, body, vec![take_doc(body, "document")?], true, new_id);
            Built { db: p.db.clone(), command: d, shape, reads: false }
        }
        Op::InsertMany => {
            let (d, shape) = insert(c, body, documents(body)?, false, new_id);
            Built { db: p.db.clone(), command: d, shape, reads: false }
        }
        Op::UpdateOne | Op::UpdateMany => {
            let u = update_value(body, "update")?;
            let d = update(c, body, opt_doc(body, "filter")?, u, op == Op::UpdateMany);
            write(&p.db, d, 1, false)
        }
        Op::ReplaceOne => {
            let u = Value::Document(take_doc(body, "replacement")?);
            write(&p.db, update(c, body, opt_doc(body, "filter")?, u, false), 1, false)
        }
        Op::DeleteOne | Op::DeleteMany => {
            let limit = i32::from(op == Op::DeleteOne);
            write(&p.db, delete(c, body, opt_doc(body, "filter")?, limit), 1, false)
        }
        Op::FindOneAndUpdate | Op::FindOneAndReplace | Op::FindOneAndDelete => {
            let mut d = command("findAndModify", c, body, &["filter", "replacement", "projection"]);
            if let Some(f) = body.get("filter") {
                d.push("query", f.clone());
            }
            if let Some(f) = body.get("projection") {
                d.push("fields", f.clone());
            }
            match op {
                Op::FindOneAndDelete => d.push("remove", true),
                Op::FindOneAndReplace => d.push("update", take_doc(body, "replacement")?),
                _ => {
                    update_value(body, "update")?;
                }
            }
            plain(&p.db, d)
        }
        Op::BulkWrite => plain("admin", bulk_write(&format!("{}.{c}", p.db), body)?),
        Op::ListIndexes => {
            Built { shape: Shape::Cursor, ..plain(&p.db, Document::new().with("listIndexes", c)) }
        }
        Op::CreateIndexes => plain(&p.db, command("createIndexes", c, body, &[])),
        Op::DropIndex => {
            plain(&p.db, Document::new().with("dropIndexes", c).with("index", p.name.as_str()))
        }
        Op::ListSearchIndexes | Op::GetSearchIndex => {
            let mut stage = Document::new();
            if op == Op::GetSearchIndex {
                stage.push("name", p.name.as_str());
            }
            let pipeline = vec![Value::Document(Document::new().with("$listSearchIndexes", stage))];
            let d = Document::new()
                .with("aggregate", c)
                .with("pipeline", pipeline)
                .with("cursor", Document::new());
            Built { shape: Shape::Cursor, ..plain(&p.db, d) }
        }
        Op::CreateSearchIndexes => plain(&p.db, command("createSearchIndexes", c, body, &[])),
        Op::UpdateSearchIndex => plain(
            &p.db,
            command("updateSearchIndex", c, body, &["name"]).with("name", p.name.as_str()),
        ),
        Op::DropSearchIndex => {
            plain(&p.db, Document::new().with("dropSearchIndex", c).with("name", p.name.as_str()))
        }
        Op::Command => {
            let name = body.iter().next().map(|(k, _)| k).unwrap_or_default();
            if name.is_empty() {
                return Err(bad("the body must hold a command"));
            }
            if REFUSED_COMMANDS.iter().any(|r| r.eq_ignore_ascii_case(name)) {
                return Err(CommandError::new(
                    COMMAND_NOT_SUPPORTED,
                    format!("{name} is not supported over HTTP"),
                ));
            }
            let reads = READ_COMMANDS.contains(&name);
            Built { shape: Shape::Cursor, reads, ..plain(&p.db, body.clone()) }
        }
        Op::ListApiKeys => {
            let mut d = Document::new().with("listApiKeys", 1);
            for f in ["user", "db"] {
                if let Some(v) = crate::routes::query_param(query, f) {
                    d.push(f, v);
                }
            }
            plain("admin", d)
        }
        Op::CreateApiKey => match body.get("user") {
            Some(Value::String(user)) => {
                plain("admin", command("createApiKey", user.as_str(), body, &["user"]))
            }
            _ => return Err(CommandError::new(FAILED_TO_PARSE, "user must be a string")),
        },
        Op::RevokeApiKey => plain("admin", Document::new().with("revokeApiKey", p.id.as_str())),
        Op::GetMore
        | Op::KillCursor
        | Op::OpenApi
        | Op::Transaction
        | Op::WatchCollection
        | Op::WatchDatabase
        | Op::WatchCluster => {
            return Err(CommandError::new(COMMAND_NOT_SUPPORTED, "the route builds no command"));
        }
    };
    Ok(built)
}

fn read(db: &str, command: Document, shape: Shape) -> Built {
    Built { db: db.to_owned(), command, shape, reads: true }
}

fn write(db: &str, command: Document, operations: usize, missing: bool) -> Built {
    Built { db: db.to_owned(), command, shape: Shape::Write { operations, missing }, reads: false }
}

/// `filter` (relaxed Extended JSON) and `nameOnly` (`true` or `false`) of the query, on `d`.
fn list_query(mut d: Document, query: &str) -> Result<Document, CommandError> {
    if let Some(f) = crate::routes::query_param(query, "filter") {
        let filter = extjson::parse(&f).map_err(|e| bad(format!("filter: {e}")))?;
        d.push("filter", filter);
    }
    if let Some(n) = crate::routes::query_param(query, "nameOnly") {
        let v = match n.as_str() {
            "true" => true,
            "false" => false,
            _ => return Err(bad("nameOnly must be true or false")),
        };
        d.push("nameOnly", v);
    }
    Ok(d)
}
