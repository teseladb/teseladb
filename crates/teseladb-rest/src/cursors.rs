//! Cursors over HTTP (FR-008, contract `rest-api.md`): the public id `<node>.<id>` of a cursor
//! of 006, which tells the node that opened it, the reply of a query with its first batch and the
//! link to the next one, and the `getMore` and `killCursors` of the cursor routes.

use teseladb_session::cursor::node_of;
use teseladb_wire::error::{BAD_VALUE, CURSOR_NOT_FOUND, CommandError};
use teseladb_bson::{Document, Value};

use crate::routes::{BASE, query_param};

/// The public id of cursor `id`: `"0"` for an exhausted cursor, `<node>.<id>` otherwise.
pub fn public_id(id: i64) -> String {
    if id == 0 { "0".to_owned() } else { format!("{}.{id}", node_of(id)) }
}

/// The cursor of public id `text`: the node that opened it and its id. A node that does not
/// match the id's own bits names no cursor (`CursorNotFound`).
pub fn parse_id(text: &str) -> Result<(u64, i64), CommandError> {
    let not_found = || CommandError::new(CURSOR_NOT_FOUND, format!("cursor id {text} not found"));
    let (node, id) = text.split_once('.').ok_or_else(not_found)?;
    let node: u64 = node.parse().map_err(|_| not_found())?;
    let id: i64 = id.parse().map_err(|_| not_found())?;
    if id == 0 || node_of(id) != node {
        return Err(not_found());
    }
    Ok((node, id))
}

/// The link to the next batch of cursor `id`.
pub fn next_link(id: i64) -> String {
    format!("{BASE}/cursors/{}", public_id(id))
}

/// `reply` with its cursor's `id` made public and, while it is open, `next`, which asks for
/// `batch` documents when the query set its batch size.
pub fn shape(mut reply: Document, batch: Option<i64>) -> Document {
    let Some(Value::Document(cursor)) = reply.get_mut("cursor") else { return reply };
    let id = match cursor.get("id") {
        Some(Value::Int64(id)) => *id,
        Some(Value::Int32(id)) => i64::from(*id),
        _ => return reply,
    };
    cursor.insert("id", public_id(id));
    if id != 0 {
        let link = match batch {
            Some(b) => format!("{}?batchSize={b}", next_link(id)),
            None => next_link(id),
        };
        cursor.push("next", link);
    }
    reply
}

/// A non-negative integer query parameter.
fn int_param(query: &str, name: &str) -> Result<Option<i64>, CommandError> {
    match query_param(query, name) {
        None => Ok(None),
        Some(v) => match v.parse::<i64>() {
            Ok(n) if n >= 0 => Ok(Some(n)),
            _ => {
                Err(CommandError::new(BAD_VALUE, format!("{name} must be a non-negative integer")))
            }
        },
    }
}

/// The `batchSize` of a `getMore` query, when it is valid.
pub fn get_more_batch(query: &str) -> Option<i64> {
    int_param(query, "batchSize").ok().flatten()
}

/// `getMore` of cursor `id` over collection `coll`, with `batchSize` and `maxTimeMS` of `query`.
pub fn get_more(id: i64, coll: &str, query: &str) -> Result<Document, CommandError> {
    let mut d = Document::new().with("getMore", id).with("collection", coll);
    if let Some(b) = int_param(query, "batchSize")? {
        d.push("batchSize", b);
    }
    if let Some(m) = int_param(query, "maxTimeMS")? {
        d.push("maxTimeMS", m);
    }
    Ok(d)
}

/// `killCursors` of cursor `id` over collection `coll`.
pub fn kill(id: i64, coll: &str) -> Document {
    Document::new().with("killCursors", coll).with("cursors", vec![Value::Int64(id)])
}
