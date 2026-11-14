//! `getDiagnosticData` (010 FR-024, T041): the node's last diagnostic sample, the one the
//! server also writes each second to `<data>/diagnostic.data`. Its `data` has the parts of
//! a reference replica set member 9.0.4 that TeselaDB has (measured 01-11-2026): `start`, then
//! `serverStatus` and `connPoolStats`, each opened with its own `start` time and closed with its
//! `end`, and the sample's `end`. The reference router gives an empty `data`; TeselaDB samples the
//! node (DV-077).

use teseladb_bson::{Document, Value};

use super::{Backend, Ctx};
use crate::error::{CommandError, UNAUTHORIZED};

/// The command's name.
pub const GET_DIAGNOSTIC_DATA: &str = "getDiagnosticData";

/// `getDiagnosticData`, on `admin` only (measured: 13 elsewhere).
pub fn get_diagnostic_data<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    if ctx.db != "admin" {
        let msg = "getDiagnosticData may only be run against the admin database.";
        return Err(CommandError::new(UNAUTHORIZED, msg));
    }
    Ok(Document::new().with("data", sample(ctx)?).with("ok", 1.0))
}

/// The node's diagnostic sample now, outside any command: what the server writes each second to
/// `<data>/diagnostic.data`.
pub fn capture<B: Backend + ?Sized>(
    backend: &mut B,
    shared: &super::Shared,
) -> Result<Document, CommandError> {
    let cmd = Document::new().with(GET_DIAGNOSTIC_DATA, 1);
    sample(&mut Ctx { backend, shared, db: "admin", cmd: &cmd })
}

/// The node's diagnostic sample now.
pub fn sample<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    let now = |ctx: &mut Ctx<'_, B>| {
        Value::DateTime(i64::try_from(ctx.backend.wall_ms()).unwrap_or(i64::MAX))
    };
    let mut data = Document::new().with("start", now(ctx));
    let parts: [(&str, PartFn<B>); 2] = [
        ("serverStatus", super::diagnostics::server_status),
        ("connPoolStats", super::maintenance::conn_pool_stats),
    ];
    for (name, part) in parts {
        let mut d = Document::new().with("start", now(ctx));
        for (k, v) in part(ctx)?.iter() {
            if k != "ok" {
                d.push(k, v.clone());
            }
        }
        d.push("end", now(ctx));
        data.push(name, d);
    }
    data.push("end", now(ctx));
    Ok(data)
}

/// A command whose reply is a part of the sample.
type PartFn<B> = fn(&mut Ctx<'_, B>) -> Result<Document, CommandError>;
