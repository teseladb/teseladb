//! The OpenAPI 3.1 document of `GET /v1/openapi.json` (FR-014), generated from
//! [`crate::routes::ROUTES`], the same table the server matches requests against.

use std::fmt::Write;

use crate::media::{BSON, CANONICAL_JSON, JSON};
use crate::problem::PROBLEM_JSON;
use crate::routes::{BASE, Op, ROUTES, Route};
use teseladb_http::sse::EVENT_STREAM;

/// The OpenAPI version of the document.
pub const OPENAPI_VERSION: &str = "3.1.0";

/// The `operationId` of `route`: its method and path, unique in the table.
pub fn operation_id(route: &Route) -> String {
    let mut id = route.method.to_ascii_lowercase();
    for seg in route.path.split('/').filter(|s| !s.is_empty()) {
        let seg = seg.trim_start_matches('{').trim_end_matches('}');
        let mut chars = seg.chars().filter(char::is_ascii_alphanumeric);
        if let Some(first) = chars.next() {
            id.push(first.to_ascii_uppercase());
            id.extend(chars);
        }
    }
    id
}

/// The path parameters of a template.
fn parameters(path: &str) -> Vec<&str> {
    path.split('/').filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}'))).collect()
}

fn content(schema: &str) -> String {
    [JSON, CANONICAL_JSON, BSON]
        .iter()
        .map(|t| format!("\"{t}\":{{\"schema\":{schema}}}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn operation(route: &Route) -> String {
    let mut o = format!("\"operationId\":\"{}\"", operation_id(route));
    if !route.command.is_empty() {
        let _ = write!(o, ",\"summary\":\"{}\"", route.command);
    }
    let params: Vec<String> = parameters(route.path)
        .iter()
        .map(|p| format!("{{\"name\":\"{p}\",\"in\":\"path\",\"required\":true,\"schema\":{{\"type\":\"string\"}}}}"))
        .collect();
    if !params.is_empty() {
        let _ = write!(o, ",\"parameters\":[{}]", params.join(","));
    }
    if route.body {
        let _ = write!(
            o,
            ",\"requestBody\":{{\"required\":false,\"content\":{{{}}}}}",
            content("{\"type\":\"object\"}")
        );
    }
    let ok = if matches!(route.op, Op::WatchCollection | Op::WatchDatabase | Op::WatchCluster) {
        format!("\"{EVENT_STREAM}\":{{\"schema\":{{\"type\":\"string\"}}}}")
    } else {
        content("{\"type\":\"object\"}")
    };
    let _ = write!(
        o,
        ",\"responses\":{{\"200\":{{\"description\":\"The command's result\",\"content\":{{{ok}}}}},\
         \"default\":{{\"description\":\"An error\",\"content\":{{\"{PROBLEM_JSON}\":{{\"schema\":{{\"$ref\":\"#/components/schemas/Problem\"}}}}}}}}}}"
    );
    format!("{{{o}}}")
}

/// The document.
pub fn document() -> String {
    let mut paths: Vec<(&str, Vec<&Route>)> = Vec::new();
    for route in ROUTES {
        match paths.iter_mut().find(|(p, _)| *p == route.path) {
            Some((_, rs)) => rs.push(route),
            None => paths.push((route.path, vec![route])),
        }
    }
    let paths: Vec<String> = paths
        .iter()
        .map(|(path, routes)| {
            let ops: Vec<String> = routes
                .iter()
                .map(|r| format!("\"{}\":{}", r.method.to_ascii_lowercase(), operation(r)))
                .collect();
            format!("\"{path}\":{{{}}}", ops.join(","))
        })
        .collect();
    let problem = "{\"type\":\"object\",\"required\":[\"type\",\"title\",\"status\"],\"properties\":{\
        \"type\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"},\"status\":{\"type\":\"integer\"},\
        \"code\":{\"type\":\"integer\"},\"codeName\":{\"type\":\"string\"},\"errmsg\":{\"type\":\"string\"},\
        \"errorLabels\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}}}";
    format!(
        "{{\"openapi\":\"{OPENAPI_VERSION}\",\"info\":{{\"title\":\"TeselaDB REST API\",\"version\":\"1\"}},\
         \"servers\":[{{\"url\":\"{BASE}\"}}],\"paths\":{{{}}},\"components\":{{\"schemas\":{{\"Problem\":{problem}}}}}}}",
        paths.join(",")
    )
}
