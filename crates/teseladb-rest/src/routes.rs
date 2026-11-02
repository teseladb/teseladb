//! The route table of contract `rest-api.md` (FR-005, FR-014): each route's method, path
//! template and operation. The server matches requests against it and the OpenAPI document
//! ([`crate::openapi`]) is generated from it, so the two cannot drift apart.

/// The prefix of every route.
pub const BASE: &str = "/v1";

/// What a route does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `listDatabases`.
    ListDatabases,
    /// `listCollections`.
    ListCollections,
    /// `create`.
    CreateCollection,
    /// `drop`.
    DropCollection,
    /// `find`.
    Find,
    /// `aggregate`.
    Aggregate,
    /// `aggregate` with `$match` and `$group`.
    CountDocuments,
    /// `count` without a filter.
    EstimatedDocumentCount,
    /// `distinct`.
    Distinct,
    /// `find` by `_id` with `limit: 1`.
    GetDocument,
    /// `update` replacing by `_id`, with `upsert`.
    PutDocument,
    /// `update` by `_id`.
    PatchDocument,
    /// `delete` by `_id`.
    DeleteDocument,
    /// `insert` of one document.
    InsertOne,
    /// `insert` of several.
    InsertMany,
    /// `update` of one document.
    UpdateOne,
    /// `update` with `multi`.
    UpdateMany,
    /// `update` with a replacement.
    ReplaceOne,
    /// `delete` with `limit: 1`.
    DeleteOne,
    /// `delete` with `limit: 0`.
    DeleteMany,
    /// `findAndModify` with an update.
    FindOneAndUpdate,
    /// `findAndModify` with a replacement.
    FindOneAndReplace,
    /// `findAndModify` with `remove`.
    FindOneAndDelete,
    /// `bulkWrite`.
    BulkWrite,
    /// `listIndexes`.
    ListIndexes,
    /// `createIndexes`.
    CreateIndexes,
    /// `dropIndexes`.
    DropIndex,
    /// `$listSearchIndexes`.
    ListSearchIndexes,
    /// `$listSearchIndexes` of one index.
    GetSearchIndex,
    /// `createSearchIndexes`.
    CreateSearchIndexes,
    /// `updateSearchIndex`.
    UpdateSearchIndex,
    /// `dropSearchIndex`.
    DropSearchIndex,
    /// Any command of the body.
    Command,
    /// `getMore`.
    GetMore,
    /// `killCursors`.
    KillCursor,
    /// The OpenAPI document.
    OpenApi,
}

/// A route of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    /// Method.
    pub method: &'static str,
    /// Path template under [`BASE`], with `{db}`, `{coll}`, `{id}` and `{name}` parameters.
    pub path: &'static str,
    /// What it does.
    pub op: Op,
    /// The command it builds, for the OpenAPI document.
    pub command: &'static str,
    /// Whether it takes a body.
    pub body: bool,
}

const fn r(method: &'static str, path: &'static str, op: Op, command: &'static str) -> Route {
    let body = matches!(method.as_bytes(), b"POST" | b"PUT" | b"PATCH");
    Route { method, path, op, command, body }
}

const COLL: &str = "/databases/{db}/collections/{coll}";

macro_rules! coll {
    ($tail:literal) => {
        concat!("/databases/{db}/collections/{coll}", $tail)
    };
}

/// Every route.
pub const ROUTES: &[Route] = &[
    r("GET", "/databases", Op::ListDatabases, "listDatabases"),
    r("GET", "/databases/{db}/collections", Op::ListCollections, "listCollections"),
    r("POST", "/databases/{db}/collections", Op::CreateCollection, "create"),
    r("DELETE", COLL, Op::DropCollection, "drop"),
    r("POST", coll!("/find"), Op::Find, "find"),
    r("POST", coll!("/aggregate"), Op::Aggregate, "aggregate"),
    r("POST", coll!("/countDocuments"), Op::CountDocuments, "aggregate"),
    r("GET", coll!("/estimatedDocumentCount"), Op::EstimatedDocumentCount, "count"),
    r("POST", coll!("/distinct"), Op::Distinct, "distinct"),
    r("GET", coll!("/documents/{id}"), Op::GetDocument, "find"),
    r("PUT", coll!("/documents/{id}"), Op::PutDocument, "update"),
    r("PATCH", coll!("/documents/{id}"), Op::PatchDocument, "update"),
    r("DELETE", coll!("/documents/{id}"), Op::DeleteDocument, "delete"),
    r("POST", coll!("/insertOne"), Op::InsertOne, "insert"),
    r("POST", coll!("/insertMany"), Op::InsertMany, "insert"),
    r("POST", coll!("/updateOne"), Op::UpdateOne, "update"),
    r("POST", coll!("/updateMany"), Op::UpdateMany, "update"),
    r("POST", coll!("/replaceOne"), Op::ReplaceOne, "update"),
    r("POST", coll!("/deleteOne"), Op::DeleteOne, "delete"),
    r("POST", coll!("/deleteMany"), Op::DeleteMany, "delete"),
    r("POST", coll!("/findOneAndUpdate"), Op::FindOneAndUpdate, "findAndModify"),
    r("POST", coll!("/findOneAndReplace"), Op::FindOneAndReplace, "findAndModify"),
    r("POST", coll!("/findOneAndDelete"), Op::FindOneAndDelete, "findAndModify"),
    r("POST", coll!("/bulkWrite"), Op::BulkWrite, "bulkWrite"),
    r("GET", coll!("/indexes"), Op::ListIndexes, "listIndexes"),
    r("POST", coll!("/indexes"), Op::CreateIndexes, "createIndexes"),
    r("DELETE", coll!("/indexes/{name}"), Op::DropIndex, "dropIndexes"),
    r("GET", coll!("/searchIndexes"), Op::ListSearchIndexes, "aggregate"),
    r("GET", coll!("/searchIndexes/{name}"), Op::GetSearchIndex, "aggregate"),
    r("POST", coll!("/searchIndexes"), Op::CreateSearchIndexes, "createSearchIndexes"),
    r("PATCH", coll!("/searchIndexes/{name}"), Op::UpdateSearchIndex, "updateSearchIndex"),
    r("DELETE", coll!("/searchIndexes/{name}"), Op::DropSearchIndex, "dropSearchIndex"),
    r("POST", "/databases/{db}/command", Op::Command, "command"),
    r("GET", "/cursors/{id}", Op::GetMore, "getMore"),
    r("DELETE", "/cursors/{id}", Op::KillCursor, "killCursors"),
    r("GET", "/openapi.json", Op::OpenApi, ""),
];

/// The path parameters of a matched route, percent-decoded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    /// `{db}`.
    pub db: String,
    /// `{coll}`.
    pub coll: String,
    /// `{id}`.
    pub id: String,
    /// `{name}`.
    pub name: String,
}

/// Why no route takes a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Miss {
    /// No route has the path: `404`.
    NotFound,
    /// Routes have the path but not the method: `405` with `Allow`.
    Method(&'static [Route]),
}

/// Percent-decodes `s` (RFC 3986, section 2.1); `None` when an escape is malformed or the result
/// is not UTF-8.
pub fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Matches the segments of `template` against those of `path`.
fn bind(template: &str, path: &str) -> Option<Params> {
    let mut t = template.split('/');
    let mut p = path.split('/');
    let mut params = Params::default();
    loop {
        match (t.next(), p.next()) {
            (None, None) => return Some(params),
            (Some(ts), Some(ps)) => {
                let slot = match ts {
                    "{db}" => &mut params.db,
                    "{coll}" => &mut params.coll,
                    "{id}" => &mut params.id,
                    "{name}" => &mut params.name,
                    literal if literal == ps => continue,
                    _ => return None,
                };
                if ps.is_empty() {
                    return None;
                }
                *slot = percent_decode(ps)?;
            }
            _ => return None,
        }
    }
}

/// The route of `method` and `path` (without the query) and its parameters.
pub fn find(method: &str, path: &str) -> Result<(&'static Route, Params), Miss> {
    let Some(rest) = path.strip_prefix(BASE) else { return Err(Miss::NotFound) };
    let mut start = None;
    let mut end = 0;
    for (k, route) in ROUTES.iter().enumerate() {
        if let Some(params) = bind(route.path, rest) {
            let method = if method == "HEAD" { "GET" } else { method };
            if route.method == method {
                return Ok((route, params));
            }
            start.get_or_insert(k);
            end = k + 1;
        }
    }
    match start {
        // Routes of one path are next to each other in the table, except the path of a
        // collection, whose `DELETE` stands apart; the methods listed are those that match.
        Some(k) => Err(Miss::Method(&ROUTES[k..end])),
        None => Err(Miss::NotFound),
    }
}

/// The value of query parameter `name` of `query` (the part after `?`), percent-decoded with `+`
/// as a space.
pub fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = percent_decode(&k.replace('+', " "))?;
        (k == name).then(|| percent_decode(&v.replace('+', " "))).flatten()
    })
}
