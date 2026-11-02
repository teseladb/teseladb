//! The tenant of each request (007 FR-025, DV-023): the one of the user its connection
//! authenticated as, or, on a connection of the default tenant, the one its `$tenant` names (a
//! TeselaDB argument, which the command never sees). A command works with its tenant's databases
//! by their names; the node keeps them qualified by the tenant
//! ([`teseladb_meta::tenants::qualified_db`]), and these functions translate between both.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::security::{Expect, SecurityOp};
use teseladb_meta::tenants::{DEFAULT_TENANT, TENANT_FIELD, qualified_db, split_db};

use super::Backend;
use crate::error::{BAD_VALUE, CommandError, UNAUTHORIZED};

/// The argument that names the tenant a command of the default tenant runs in.
pub const TENANT_ARGUMENT: &str = "$tenant";

/// The tenant of a request of a connection in tenant `own` with command `cmd`, and the command
/// without [`TENANT_ARGUMENT`] if it had it.
pub fn of_request<B: Backend + ?Sized>(
    own: u32,
    cmd: &Document,
    backend: &mut B,
) -> Result<(u32, Option<Document>), CommandError> {
    let Some(named) = cmd.get(TENANT_ARGUMENT) else { return Ok((own, None)) };
    let Value::String(name) = named else {
        return Err(crate::types::wrong_type(TENANT_ARGUMENT, named, "string"));
    };
    if own != DEFAULT_TENANT {
        let msg = format!("a connection of a tenant cannot use {TENANT_ARGUMENT}");
        return Err(CommandError::new(UNAUTHORIZED, msg));
    }
    let Some(tenant) = backend.tenant_named(name)? else {
        return Err(CommandError::new(BAD_VALUE, format!("no tenant named '{name}'")));
    };
    let mut stripped = cmd.clone();
    stripped.remove(TENANT_ARGUMENT);
    Ok((tenant, Some(stripped)))
}

/// Namespace `ns` of tenant `tenant` as the node keeps it.
pub fn qualified(tenant: u32, ns: &Namespace) -> Namespace {
    Namespace { db: qualified_db(tenant, &ns.db), coll: ns.coll.clone() }
}

/// Collection `c` of tenant `tenant`: an answer that travelled without its tenant gets it back.
pub fn owned(tenant: u32, c: CollectionEntry) -> CollectionEntry {
    CollectionEntry { tenant, ..c }
}

/// The collections of tenant `tenant` among `all`, by the names it gives them.
pub fn own_collections(
    tenant: u32,
    all: Vec<(Namespace, CollectionEntry)>,
) -> Vec<(Namespace, CollectionEntry)> {
    all.into_iter()
        .filter_map(|(ns, c)| {
            let (t, db) = split_db(&ns.db);
            (t == tenant).then(|| (Namespace { db: db.to_owned(), coll: ns.coll.clone() }, c))
        })
        .collect()
}

/// The catalog changes of tenant `tenant` among `all`, by the names it gives its namespaces.
pub fn own_ddl(
    tenant: u32,
    all: Vec<teseladb_meta::ddl_log::DdlRecord>,
) -> Vec<teseladb_meta::ddl_log::DdlRecord> {
    use teseladb_meta::ddl_log::DdlChange;
    let own = |ns: &Namespace| {
        let (t, db) = split_db(&ns.db);
        (t == tenant).then(|| Namespace { db: db.to_owned(), coll: ns.coll.clone() })
    };
    all.into_iter()
        .filter_map(|r| {
            let change = match r.change {
                DdlChange::Create { ns, collection } => {
                    DdlChange::Create { ns: own(&ns)?, collection }
                }
                DdlChange::Drop { ns, collection } => DdlChange::Drop { ns: own(&ns)?, collection },
                DdlChange::Rename { from, to, collection, target } => {
                    DdlChange::Rename { from: own(&from)?, to: own(&to)?, collection, target }
                }
                DdlChange::Modify { ns, collection, description } => {
                    DdlChange::Modify { ns: own(&ns)?, collection, description }
                }
                DdlChange::DropDatabase { db } => {
                    let (t, name) = split_db(&db);
                    if t != tenant {
                        return None;
                    }
                    DdlChange::DropDatabase { db: name.to_owned() }
                }
                DdlChange::CreateIndex { ns, collection, spec } => {
                    DdlChange::CreateIndex { ns: own(&ns)?, collection, spec }
                }
                DdlChange::DropIndex { ns, collection, spec } => {
                    DdlChange::DropIndex { ns: own(&ns)?, collection, spec }
                }
                DdlChange::ModifyIndex { ns, collection, before, after } => {
                    DdlChange::ModifyIndex { ns: own(&ns)?, collection, before, after }
                }
            };
            Some(teseladb_meta::ddl_log::DdlRecord { change, ..r })
        })
        .collect()
}

/// The databases of tenant `tenant` among `all`, by their names.
pub fn own_databases(tenant: u32, all: Vec<String>) -> Vec<String> {
    all.iter()
        .filter_map(|db| {
            let (t, name) = split_db(db);
            (t == tenant).then(|| name.to_owned())
        })
        .collect()
}

/// Catalog command `cmd` of tenant `tenant` as the node applies it.
pub fn qualified_command(tenant: u32, cmd: &CatalogCommand) -> CatalogCommand {
    let ns = |n: &Namespace| qualified(tenant, n);
    match cmd {
        CatalogCommand::Create { ns: n, uuid, options, if_absent } => CatalogCommand::Create {
            ns: ns(n),
            uuid: *uuid,
            options: options.clone(),
            if_absent: *if_absent,
        },
        CatalogCommand::Drop { ns: n } => CatalogCommand::Drop { ns: ns(n) },
        CatalogCommand::Rename { from, to, drop_target, uuid } => CatalogCommand::Rename {
            from: ns(from),
            to: ns(to),
            drop_target: *drop_target,
            uuid: *uuid,
        },
        CatalogCommand::Modify { ns: n, options, description } => CatalogCommand::Modify {
            ns: ns(n),
            options: options.clone(),
            description: description.clone(),
        },
        CatalogCommand::DropDatabase { db } => {
            CatalogCommand::DropDatabase { db: qualified_db(tenant, db) }
        }
        CatalogCommand::CreateDatabase { db } => {
            CatalogCommand::CreateDatabase { db: qualified_db(tenant, db) }
        }
        CatalogCommand::Purged { id } => CatalogCommand::Purged { id: *id },
    }
}

/// The tenant of the stored user or role document `doc` (013 FR-024): only that tenant sees it.
pub fn of_principal(doc: &[u8]) -> u32 {
    teseladb_meta::tenants::of_user(doc)
}

/// Changes `ops` of the users and roles as tenant `tenant` writes them: each document written
/// belongs to it, whatever tenant it names (013 FR-024). `None` if one expects a document of
/// another tenant, which the tenant could not have read.
pub fn own_security_ops(
    tenant: u32,
    ops: Vec<SecurityOp>,
) -> Result<Option<Vec<SecurityOp>>, CommandError> {
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        if let Expect::Exactly(old) = &op.expect
            && of_principal(old) != tenant
        {
            return Ok(None);
        }
        let doc = match op.doc {
            Some(bytes) => Some(stamped(tenant, &bytes)?),
            None => None,
        };
        out.push(SecurityOp { doc, ..op });
    }
    Ok(Some(out))
}

/// Document `bytes` with tenant `tenant` in [`TENANT_FIELD`], which the default tenant leaves out.
fn stamped(tenant: u32, bytes: &[u8]) -> Result<Vec<u8>, CommandError> {
    let mut d = Document::from(RawDoc::from_bytes(bytes)?);
    match tenant {
        DEFAULT_TENANT => {
            d.remove(TENANT_FIELD);
        }
        t => {
            d.insert(TENANT_FIELD, i64::from(t));
        }
    }
    Ok(d.to_bytes()?)
}

/// The fields of the `config` documents that name a database or a namespace.
const NAMING_FIELDS: [&str; 3] = ["_id", "ns", "db"];

/// The documents of a `config` collection that are tenant `tenant`'s, with their database names
/// as it gives them: a document naming another tenant's database or namespace is left out.
pub fn own_config_documents(tenant: u32, all: Vec<Document>) -> Vec<Document> {
    all.into_iter()
        .filter_map(|d| {
            let mut out = d.clone();
            for f in NAMING_FIELDS {
                if let Some(Value::String(name)) = d.get(f) {
                    let (t, plain) = split_db(name);
                    if t != tenant {
                        return None;
                    }
                    out.insert(f, plain);
                }
            }
            Some(out)
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn config_documents_of_other_tenants_are_left_out() {
        let qualified = qualified_db(3, "app");
        let docs = vec![
            Document::new().with("_id", "app").with("primary", "node1"),
            Document::new().with("_id", qualified.as_str()).with("primary", "node1"),
            Document::new().with("_id", 7).with("ns", format!("{qualified}.c")),
        ];
        let mine = own_config_documents(3, docs.clone());
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].get("_id"), Some(&Value::from("app")));
        assert_eq!(mine[1].get("ns"), Some(&Value::from("app.c")));
        assert_eq!(
            own_config_documents(DEFAULT_TENANT, docs),
            vec![Document::new().with("_id", "app").with("primary", "node1")]
        );
        // Every command names the tenant's databases; a purge names none.
        let purged = CatalogCommand::Purged { id: 4 };
        assert_eq!(qualified_command(3, &purged), purged);
        let created = CatalogCommand::CreateDatabase { db: "app".into() };
        let want = CatalogCommand::CreateDatabase { db: qualified };
        assert_eq!(qualified_command(3, &created), want);
    }
}
