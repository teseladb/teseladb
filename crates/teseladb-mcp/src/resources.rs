//! The resources of contract `mcp-server.md` (FR-022): the options in force, the connection,
//! the cluster's status, the exports and the schema and indexes of a collection.

use teseladb_bson::Document;
use teseladb_json::{Object, Value, to_string};
use teseladb_rest::api::Executor;

use crate::calls::{Ctx, call, to_json};
use crate::jsonrpc::{ErrorObject, INVALID_PARAMS};
use crate::server::Options;
use crate::versions::Revision;

/// The options in force.
pub const CONFIG: &str = "config://config";
/// The connection the server fixed.
pub const DEBUG: &str = "debug://refdb";
/// The cluster's status.
pub const CLUSTER_STATUS: &str = "teseladb://cluster/status";
/// The prefix of an export.
pub const EXPORT_PREFIX: &str = "exported-data://";

fn entry(uri: &str, name: &str, description: &str) -> Value {
    let mut o = Object::new();
    o.insert("uri", uri);
    o.insert("name", name);
    o.insert("description", description);
    o.insert("mimeType", "application/json");
    Value::Object(o)
}

/// `resources/list`: the fixed resources and each export.
pub fn list<'a>(exports: impl Iterator<Item = &'a String>) -> Object {
    let mut all = vec![
        entry(CONFIG, "config", "The MCP options in force and the protocol revision"),
        entry(DEBUG, "debug-refdb", "The connection the server fixed"),
        entry(CLUSTER_STATUS, "cluster-status", "The cluster's nodes and health"),
    ];
    all.extend(exports.map(|e| entry(&format!("{EXPORT_PREFIX}{e}"), e, "Exported data")));
    let mut o = Object::new();
    o.insert("resources", Value::Array(all));
    o
}

fn template(uri: &str, name: &str, description: &str) -> Value {
    let mut o = Object::new();
    o.insert("uriTemplate", uri);
    o.insert("name", name);
    o.insert("description", description);
    o.insert("mimeType", "application/json");
    Value::Object(o)
}

/// `resources/templates/list`.
pub fn templates() -> Object {
    let all = vec![
        template("exported-data://{exportName}", "exported-data", "The result of an export"),
        template(
            "teseladb://{db}/{coll}/schema",
            "collection-schema",
            "The fields of a collection",
        ),
        template(
            "teseladb://{db}/{coll}/indexes",
            "collection-indexes",
            "The indexes of a collection",
        ),
    ];
    let mut o = Object::new();
    o.insert("resourceTemplates", Value::Array(all));
    o
}

fn contents(uri: &str, v: &Value) -> Object {
    let mut c = Object::new();
    c.insert("uri", uri);
    c.insert("mimeType", "application/json");
    c.insert("text", to_string(v).unwrap_or_default());
    let mut o = Object::new();
    o.insert("contents", Value::Array(vec![Value::Object(c)]));
    o
}

fn not_found(uri: &str) -> ErrorObject {
    ErrorObject::new(INVALID_PARAMS, format!("Resource not found: {uri}"))
}

/// `resources/read` of `uri`.
pub fn read<E: Executor + ?Sized>(
    uri: &str,
    c: &mut Ctx<'_, E>,
    options: &Options,
    revision: Revision,
    export: &dyn Fn(&str) -> Option<String>,
) -> Result<Object, ErrorObject> {
    match uri {
        CONFIG => {
            let mut o = Object::new();
            o.insert("readOnly", Value::Bool(options.read_only));
            o.insert("allowDestructive", Value::Bool(options.allow_destructive));
            o.insert("maxDocuments", crate::calls::int_value(options.limits.max_documents));
            o.insert("maxBytes", crate::calls::int_value(options.limits.max_bytes));
            o.insert("maxTimeMS", crate::calls::int_value(options.limits.max_time_ms));
            o.insert("protocolVersion", revision.name());
            Ok(contents(uri, &Value::Object(o)))
        }
        DEBUG => {
            let ping = c.exec.run("admin", Document::new().with("ping", 1));
            let mut o = Object::new();
            o.insert("connectionId", c.connection_id);
            o.insert("connected", Value::Bool(crate::calls::is_ok(&ping)));
            Ok(contents(uri, &Value::Object(o)))
        }
        CLUSTER_STATUS => {
            let status =
                c.exec.run("admin", Document::new().with("serverStatus", 1).with("repl", 1));
            let mut keep = Document::new();
            for f in ["host", "version", "process", "uptime", "repl", "ok"] {
                if let Some(v) = status.get(f) {
                    keep.push(f, v.clone());
                }
            }
            Ok(contents(uri, &to_json(&keep)))
        }
        _ => {
            if let Some(name) = uri.strip_prefix(EXPORT_PREFIX) {
                let data = export(name).ok_or_else(|| not_found(uri))?;
                let mut c2 = Object::new();
                c2.insert("uri", uri);
                c2.insert("mimeType", "application/json");
                c2.insert("text", data);
                let mut o = Object::new();
                o.insert("contents", Value::Array(vec![Value::Object(c2)]));
                return Ok(o);
            }
            let rest = uri.strip_prefix("teseladb://").ok_or_else(|| not_found(uri))?;
            let parts: Vec<&str> = rest.split('/').collect();
            let [db, coll, what] = parts[..] else { return Err(not_found(uri)) };
            let tool = match what {
                "schema" => "collection-schema",
                "indexes" => "collection-indexes",
                _ => return Err(not_found(uri)),
            };
            let mut args = Object::new();
            args.insert("database", db);
            args.insert("collection", coll);
            let out = call(c, tool, &args).map_err(|m| ErrorObject::new(INVALID_PARAMS, m))?;
            Ok(contents(uri, &Value::Object(out.structured)))
        }
    }
}
