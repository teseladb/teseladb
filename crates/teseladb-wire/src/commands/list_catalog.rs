//! `$listCatalog` (010 T037): the node's catalog entry of each collection and view, as a
//! `refrouter` 9.0.4 gives its shard's (measured 01-11-2026): `db`, `name`, `type` and `shard`;
//! for a collection, `md` with its namespace, its options after its `uuid` and each ready index
//! with `ready`, `multikey` and `multikeyPaths` (one byte per component of each field's path, 1
//! where an array was met); for a view, its `_id`, `viewOn` and `pipeline`; and `ns` last.
//! `system.views` comes as the collection it is in TeselaDB's catalog, as in 9.0.4. TeselaDB has
//! no storage engine idents (`idxIdent`, `ident`), and lists in namespace order (DV-078).

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_meta::collection_types::CollectionKind;
use teseladb_meta::indexes::IndexState;
use teseladb_range::read::ReadConcern;

use super::Backend;
use super::index_maintenance::MULTIKEY_MARKS;
use super::names;
use crate::error::CommandError;

/// The entries of collection `only` of database `db`, or of every collection of every database.
pub fn entries<B: Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    only: Option<&str>,
) -> Result<Vec<Document>, CommandError> {
    let shard = super::shard_name(backend.node());
    let mut found = backend.collections()?;
    found.retain(|(ns, _)| only.is_none_or(|c| ns.db == db && ns.coll == c));
    found.sort_by_key(|(ns, _)| names::full(ns));
    let mut out = Vec::new();
    for (ns, c) in found {
        let kind = super::kinds::kind_of(&c)?;
        let mut d = Document::new()
            .with("db", ns.db.as_str())
            .with("name", ns.coll.as_str())
            .with("type", kind.type_name())
            .with("shard", shard.as_str());
        match &kind {
            CollectionKind::View { view_on, pipeline, .. } => {
                d.push("_id", names::full(&ns));
                d.push("viewOn", view_on.as_str());
                let stages = pipeline
                    .iter()
                    .map(|s| Ok(Value::Document(Document::from(RawDoc::from_bytes(s)?))))
                    .collect::<Result<Vec<_>, CommandError>>()?;
                d.push("pipeline", Value::Array(stages));
            }
            _ => d.push("md", metadata(backend, &ns, &c, &kind)?),
        }
        d.push("ns", names::full(&ns));
        out.push(d);
    }
    Ok(out)
}

/// `md` of collection `c` of kind `kind`. A time series is its buckets' collection, clustered
/// by `_id`, with their indexes, and says whether its buckets may mix schemas (measured on a
/// `refnode` and a `refrouter` 9.0.4 on 03-11-2026).
fn metadata<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    c: &CollectionEntry,
    kind: &CollectionKind,
) -> Result<Document, CommandError> {
    let series = matches!(kind, CollectionKind::TimeSeries(_));
    let mut options =
        Document::new().with("uuid", Value::Binary { subtype: 4, bytes: c.uuid.to_vec() });
    if series {
        options.push("clusteredIndex", true);
    }
    for (k, v) in super::catalog::options_of(c)?.iter() {
        options.push(k, v.clone());
    }
    let mut indexes = Vec::new();
    if super::catalog::has_id_index(ns) && !series {
        let spec = super::catalog::id_index(c)?;
        indexes.push(index(spec, &[]));
    }
    for e in backend.indexes(c.id)? {
        if e.state != IndexState::Ready {
            continue;
        }
        let spec = Document::from(RawDoc::from_bytes(&e.spec)?);
        let paths = marks(backend, c, e.id)?;
        indexes.push(index(spec, &paths));
    }
    let mut md = Document::new()
        .with("ns", names::full(ns))
        .with("options", options)
        .with("indexes", Value::Array(indexes));
    if series {
        md.push("timeseriesBucketsMayHaveMixedSchemaData", false);
    }
    Ok(md)
}

/// An index of `md`, whose fields went through arrays at `paths` (`(field, path)`).
fn index(spec: Document, paths: &[(String, String)]) -> Value {
    let mut multikey_paths = Document::new();
    // The node stored the specification with its key.
    let key = match spec.get("key") {
        Some(Value::Document(key)) => key.clone(),
        _ => Document::new(),
    };
    for (field, _) in key.iter() {
        let mut prefix = String::new();
        let bytes: Vec<u8> = field
            .split('.')
            .map(|part| {
                if !prefix.is_empty() {
                    prefix.push('.');
                }
                prefix.push_str(part);
                u8::from(paths.iter().any(|(f, p)| f == field && *p == prefix))
            })
            .collect();
        multikey_paths.push(field, Value::Binary { subtype: 0, bytes });
    }
    Value::Document(
        Document::new()
            .with("spec", spec)
            .with("ready", true)
            .with("multikey", !paths.is_empty())
            .with("multikeyPaths", multikey_paths),
    )
}

/// The multikey marks of index `id` of `c`: `(field, path)`.
fn marks<B: Backend + ?Sized>(
    backend: &mut B,
    c: &CollectionEntry,
    id: u32,
) -> Result<Vec<(String, String)>, CommandError> {
    let start = KeyBuilder::index_prefix(c.tenant, c.id, MULTIKEY_MARKS + id);
    let end = KeyBuilder::index_prefix(c.tenant, c.id, MULTIKEY_MARKS + id + 1);
    let head = start.as_bytes().len();
    let rows = backend.scan(start.as_bytes(), end.as_bytes(), usize::MAX, ReadConcern::Local)?;
    Ok(rows
        .into_iter()
        .filter_map(|(key, _)| {
            let rest = key.get(head..)?;
            let cut = rest.iter().position(|b| *b == 0)?;
            let field = String::from_utf8_lossy(&rest[..cut]).into_owned();
            Some((field, String::from_utf8_lossy(&rest[cut + 1..]).into_owned()))
        })
        .collect())
}
