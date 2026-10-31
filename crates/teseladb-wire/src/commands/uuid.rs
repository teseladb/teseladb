//! `collectionUUID`: the UUID a command expects its collection to have (`find`, the write
//! commands, `drop`, `collMod`, `renameCollection`), with the errors a `refrouter` 9.0.4 gives
//! (measured).

use teseladb_bson::{Document, Value};
use teseladb_meta::catalog::Namespace;

use super::Backend;
use crate::error::{COLLECTION_UUID_MISMATCH, CommandError, INVALID_UUID, TYPE_MISMATCH};

/// `collectionUUID`.
pub const COLLECTION_UUID: &str = "collectionUUID";
/// Subtype of a UUID in BSON binary data.
pub const UUID_SUBTYPE: u8 = 4;
/// Subtype of the old UUIDs (`UUID(deprecated)`).
const OLD_UUID_SUBTYPE: u8 = 3;

/// `InvalidUUID` for a binary that is not 16 bytes of subtype 4.
pub fn invalid_uuid() -> CommandError {
    CommandError::new(INVALID_UUID, "uuid must be a 16-byte binary field with UUID (4) subtype")
}

/// The command's `collectionUUID`, if any.
pub fn expected(cmd: &Document, name: &str) -> Result<Option<[u8; 16]>, CommandError> {
    let path = format!("{name}.{COLLECTION_UUID}");
    match cmd.get(COLLECTION_UUID) {
        None => Ok(None),
        Some(Value::Binary { subtype: UUID_SUBTYPE, bytes }) => {
            <[u8; 16]>::try_from(bytes.as_slice()).map(Some).map_err(|_| invalid_uuid())
        }
        Some(Value::Binary { subtype: OLD_UUID_SUBTYPE, .. }) => Err(CommandError::new(
            TYPE_MISMATCH,
            format!(
                "BSON field '{path}' is the wrong binData type 'UUID(deprecated)', expected type 'UUID'"
            ),
        )),
        Some(Value::Binary { .. }) => Err(invalid_uuid()),
        Some(v) => Err(crate::types::wrong_type(&path, v, "binData")),
    }
}

/// Checks that the collection of `ns` has UUID `uuid`; the error names the collection of the
/// same database that has it, if one does.
pub fn check<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    uuid: [u8; 16],
) -> Result<(), CommandError> {
    if backend.collection(ns)?.is_some_and(|c| c.uuid == uuid) {
        return Ok(());
    }
    let actual = backend
        .collections()?
        .into_iter()
        .find(|(n, c)| n.db == ns.db && c.uuid == uuid)
        .map_or(Value::Null, |(n, _)| Value::from(n.coll));
    let mut e = CommandError::new(
        COLLECTION_UUID_MISMATCH,
        "Collection UUID does not match that specified",
    );
    e.extra.push("db", ns.db.as_str());
    e.extra.push(COLLECTION_UUID, Value::Binary { subtype: UUID_SUBTYPE, bytes: uuid.to_vec() });
    e.extra.push("expectedCollection", ns.coll.as_str());
    e.extra.push("actualCollection", actual);
    Err(e)
}
