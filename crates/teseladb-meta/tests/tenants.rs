//! Tenants in Group 0 (007 FR-025, D-014, contract `catalog.md`): `CreateTenant` and
//! `DropTenant`, one catalog per tenant with the collection numbers shared by all, and the users
//! of a tenant.

use teseladb_bson::Document;
use teseladb_meta::catalog::{CatalogCommand, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_meta::tenants::{DEFAULT_TENANT, TENANT_FIELD, qualified_db, split_db};
use teseladb_multiraft::StateMachine;

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: NodeAddrs { name: b"n1".to_vec(), cores: 2, ..NodeAddrs::default() },
        cert_fingerprint: [0; 32],
    };
    sm.apply_command(1, &boot);
    sm
}

fn apply(sm: &mut Group0Sm, index: u64, cmd: Group0Command) -> Group0Result {
    // Every command reads back from its encoding, as a replica gets it.
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd.clone()));
    sm.apply_command(index, &cmd)
}

fn create_tenant(name: &str) -> Group0Command {
    Group0Command::CreateTenant { name: name.into() }
}

fn drop_tenant(name: &str) -> Group0Command {
    Group0Command::DropTenant { name: name.into() }
}

fn create(tenant: u32, db: &str, coll: &str) -> Group0Command {
    let ns = Namespace { db: qualified_db(tenant, db), coll: coll.into() };
    Group0Command::Catalog(CatalogCommand::Create {
        ns,
        uuid: [1; 16],
        options: Vec::new(),
        if_absent: false,
    })
}

#[test]
fn a_tenant_gets_a_number_never_reused_and_a_unique_name() {
    let mut sm = cluster();
    assert_eq!(apply(&mut sm, 2, create_tenant("acme")), Group0Result::Tenant(1));
    assert_eq!(apply(&mut sm, 3, create_tenant("beta")), Group0Result::Tenant(2));
    let taken = Group0Result::Err(Group0Error::TenantExists);
    assert_eq!(apply(&mut sm, 4, create_tenant("acme")), taken);
    // A name with nothing in it, too long or with a zero byte is no name.
    let invalid = Group0Result::Err(Group0Error::InvalidTenantName);
    for name in ["", &"x".repeat(65), "a\0b"] {
        assert_eq!(apply(&mut sm, 5, create_tenant(name)), invalid, "{name:?}");
    }
    assert_eq!(sm.tenants.list(), vec![(1, "acme"), (2, "beta")]);
    assert_eq!(apply(&mut sm, 6, drop_tenant("acme")), Group0Result::Ok);
    let missing = Group0Result::Err(Group0Error::TenantNotFound);
    assert_eq!(apply(&mut sm, 7, drop_tenant("acme")), missing);
    // The default tenant is no tenant of the table and cannot go.
    assert_eq!(apply(&mut sm, 8, drop_tenant("")), invalid);
    assert_eq!(apply(&mut sm, 9, create_tenant("acme")), Group0Result::Tenant(3));
    assert_eq!(sm.tenants.id_of("acme"), Some(3));
    assert_eq!(sm.tenants.id_of("beta"), Some(2));
}

#[test]
fn each_tenant_has_its_databases_and_the_collection_numbers_are_shared() {
    let mut sm = cluster();
    assert_eq!(apply(&mut sm, 2, create_tenant("acme")), Group0Result::Tenant(1));
    let Group0Result::Collection(a) = apply(&mut sm, 3, create(DEFAULT_TENANT, "app", "c")) else {
        panic!("the default tenant's collection")
    };
    // The same database and collection name in another tenant: another collection.
    let Group0Result::Collection(b) = apply(&mut sm, 4, create(1, "app", "c")) else {
        panic!("the tenant's collection")
    };
    assert_eq!((a.id, a.tenant, b.id, b.tenant), (1, DEFAULT_TENANT, 2, 1));
    // A database differing in case clashes only within its tenant.
    let Group0Result::Collection(c) = apply(&mut sm, 5, create(1, "Other", "c")) else {
        panic!("the tenant's other database")
    };
    assert_eq!(c.tenant, 1);
    let clash = Group0Result::Err(Group0Error::DatabaseDifferCase);
    assert_eq!(apply(&mut sm, 6, create(1, "other", "c")), clash);
    let Group0Result::Collection(d) = apply(&mut sm, 7, create(DEFAULT_TENANT, "other", "c"))
    else {
        panic!("the default tenant's database with that name")
    };
    assert_eq!(d.id, 4);
    // Dropping a tenant's database leaves the other tenant's.
    let drop = CatalogCommand::DropDatabase { db: qualified_db(1, "app") };
    assert_eq!(apply(&mut sm, 8, Group0Command::Catalog(drop)), Group0Result::Ok);
    let app = Namespace { db: "app".into(), coll: "c".into() };
    assert_eq!(sm.catalog.get(&app).map(|e| e.id), Some(1));
    // Read back from a snapshot, every collection keeps its tenant.
    let mut back = Group0Sm::default();
    back.restore(&sm.snapshot()).unwrap();
    let mut tenants: Vec<(u32, u32)> =
        back.catalog.collections.values().map(|e| (e.id, e.tenant)).collect();
    tenants.sort_unstable();
    assert_eq!(tenants, vec![(1, DEFAULT_TENANT), (3, 1), (4, DEFAULT_TENANT)]);
    assert_eq!(back.tenants, sm.tenants);
}

#[test]
fn a_tenant_with_databases_users_or_roles_cannot_be_dropped() {
    let mut sm = cluster();
    apply(&mut sm, 2, create_tenant("acme"));
    apply(&mut sm, 3, create(1, "app", "c"));
    let busy = Group0Result::Err(Group0Error::NotEmpty);
    assert_eq!(apply(&mut sm, 4, drop_tenant("acme")), busy);
    let drop = CatalogCommand::DropDatabase { db: qualified_db(1, "app") };
    apply(&mut sm, 5, Group0Command::Catalog(drop));
    // A user of the tenant keeps it too.
    let user = Document::new()
        .with("_id", "admin.ann")
        .with("user", "ann")
        .with("db", "admin")
        .with(TENANT_FIELD, 1_i64);
    let op = SecurityOp::put(Principal::User, Expect::Absent, user.to_bytes().unwrap()).unwrap();
    let r = apply(&mut sm, 6, Group0Command::Security(vec![op]));
    assert_eq!(r, Group0Result::Ok);
    assert_eq!(apply(&mut sm, 7, drop_tenant("acme")), busy);
    let gone = sm.security.get(Principal::User, "admin", "ann").map(<[u8]>::to_vec).unwrap();
    let op = SecurityOp::remove(Principal::User, gone).unwrap();
    apply(&mut sm, 8, Group0Command::Security(vec![op]));
    // And a role of the tenant (013 FR-024).
    let role = Document::new()
        .with("_id", "admin.reader")
        .with("role", "reader")
        .with("db", "admin")
        .with(TENANT_FIELD, 1_i64);
    let op = SecurityOp::put(Principal::Role, Expect::Absent, role.to_bytes().unwrap()).unwrap();
    assert_eq!(apply(&mut sm, 9, Group0Command::Security(vec![op])), Group0Result::Ok);
    assert_eq!(apply(&mut sm, 10, drop_tenant("acme")), busy);
    let gone = sm.security.get(Principal::Role, "admin", "reader").map(<[u8]>::to_vec).unwrap();
    let op = SecurityOp::remove(Principal::Role, gone).unwrap();
    apply(&mut sm, 11, Group0Command::Security(vec![op]));
    assert_eq!(apply(&mut sm, 12, drop_tenant("acme")), Group0Result::Ok);
}

#[test]
fn a_qualified_database_names_its_tenant() {
    assert_eq!(qualified_db(DEFAULT_TENANT, "app"), "app");
    assert_eq!(split_db("app"), (DEFAULT_TENANT, "app"));
    let q = qualified_db(7, "app");
    assert_ne!(q, "app");
    assert_eq!(split_db(&q), (7, "app"));
    assert_eq!(split_db(&qualified_db(u32::MAX, "")), (u32::MAX, ""));
    // A name with the mark but no number after it stays the default tenant's.
    assert_eq!(split_db("\0x\0app"), (DEFAULT_TENANT, "\0x\0app"));
    assert_eq!(split_db("\0"), (DEFAULT_TENANT, "\0"));
}

#[test]
fn a_snapshot_of_the_version_before_tenants_still_reads() {
    let mut sm = cluster();
    apply(&mut sm, 2, create(DEFAULT_TENANT, "app", "c"));
    let old = sm.encode_snapshot_as(teseladb_meta::group0::SNAPSHOT_VERSION_WITHOUT_TENANTS);
    let mut back = Group0Sm::default();
    back.restore(&old).unwrap();
    assert_eq!(back.catalog, sm.catalog);
    assert!(back.tenants.list().is_empty());
}
