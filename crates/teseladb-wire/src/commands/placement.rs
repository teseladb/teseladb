//! `untrackUnshardedCollection` and `resetPlacementHistory` (018), as a `refrouter` 9.0.4 answers
//! them (measured on the native `refrouter` on 09-11-2026, scenario `sharding-moves`). The first
//! forgets the tracking a `moveCollection` or an `unshardCollection` left on a collection without
//! shard key, whose documents stay where they are; the second resets RefDb's log of namespace
//! placements, which TeselaDB does not keep (its ranges are in Group 0), so it has nothing to do.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::CatalogCommand;

use super::admin::admin_only;
use super::move_collection::unsplittable_by;
use super::names::parse_full;
use super::reshard::UNSPLITTABLE;
use super::sharding::shard_key_of;
use super::{Backend, Ctx};
use crate::error::{CommandError, INVALID_NAMESPACE, NAMESPACE_NOT_FOUND};
use crate::types::wrong_type;

/// `untrackUnshardedCollection`.
pub const UNTRACK_UNSHARDED_COLLECTION: &str = "untrackUnshardedCollection";
/// `resetPlacementHistory`.
pub const RESET_PLACEMENT_HISTORY: &str = "resetPlacementHistory";

/// `untrackUnshardedCollection`: a collection without shard key stops being tracked; one that is
/// not tracked, or does not exist, is answered as done.
pub fn untrack_unsharded_collection<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let name = UNTRACK_UNSHARDED_COLLECTION;
    let text = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(wrong_type(&format!("{name}.{name}"), v, "string")),
    };
    admin_only(name, ctx.db)?;
    let ns =
        parse_full(&text).filter(|n| !n.db.is_empty() && !n.coll.is_empty()).ok_or_else(|| {
            CommandError::new(INVALID_NAMESPACE, format!("Invalid namespace specified: {text}"))
        })?;
    if !ctx.backend.databases()?.contains(&ns.db) {
        let msg = format!("database {} not found", ns.db);
        return Err(CommandError::new(NAMESPACE_NOT_FOUND, msg));
    }
    let Some(c) = ctx.backend.collection(&ns)? else { return Ok(done()) };
    if shard_key_of(&c)?.is_some() {
        return Err(CommandError::new(
            INVALID_NAMESPACE,
            format!("Collection {text} must not be sharded"),
        ));
    }
    if unsplittable_by(&c).is_none() {
        return Ok(done());
    }
    let mut options = Document::from(RawDoc::from_bytes(&c.options)?);
    options.remove(UNSPLITTABLE);
    ctx.backend.catalog_write(CatalogCommand::Modify {
        ns,
        options: options.to_bytes()?,
        description: Document::new().to_bytes()?,
    })?;
    Ok(done())
}

/// `resetPlacementHistory`: there is no log of placements to reset.
pub fn reset_placement_history<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(RESET_PLACEMENT_HISTORY, ctx.db)?;
    Ok(done())
}

fn done() -> Document {
    Document::new().with("ok", 1.0)
}
