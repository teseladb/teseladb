//! 007 T034: `RestoreCatalog` (kind 50, plan «Copias de seguridad»): a restore puts the catalog
//! and the index definitions of a backup into Group 0 whole, with their collection and index
//! numbers (the keys of the copied data carry them), and only into a catalog without
//! collections or indexes. With the backup's tenants and security state it puts those in place
//! of the cluster's (T066, FR-037).

use std::sync::Arc;

use teseladb_meta::catalog::{CatalogCommand, Namespace};
use teseladb_meta::command::{Group0Command, NodeAddrs, RestoredSecurity, kind};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_meta::indexes::IndexCommand;
use teseladb_meta::node_versions::{FORMAT, key as node_key};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_meta::tenants::TenantLimits;
use teseladb_bson::Document;
use teseladb_types::NodeId;

fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied + 1;
    sm.applied = index;
    sm.apply_command(index, &cmd)
}

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let node = NodeAddrs {
        name: b"n1".to_vec(),
        peer_addr: b"n1:7421".to_vec(),
        cores: 1,
        ..NodeAddrs::default()
    };
    run(
        &mut sm,
        Group0Command::Bootstrap {
            cluster_id: [4; 16],
            first_node: node,
            cert_fingerprint: [1; 32],
        },
    );
    sm
}

fn create(sm: &mut Group0Sm, db: &str, coll: &str) -> u32 {
    let ns = Namespace { db: db.into(), coll: coll.into() };
    let options = Document::new().to_bytes().unwrap();
    let cmd =
        CatalogCommand::Create { ns, uuid: [coll.len() as u8; 16], options, if_absent: false };
    match run(sm, Group0Command::Catalog(cmd)) {
        Group0Result::Collection(c) => c.id,
        other => panic!("{other:?}"),
    }
}

/// A source cluster with two collections, one of them dropped first so the numbers have a gap,
/// and an index.
fn source() -> Group0Sm {
    let mut sm = cluster();
    create(&mut sm, "app", "gone");
    let ns = Namespace { db: "app".into(), coll: "gone".into() };
    run(&mut sm, Group0Command::Catalog(CatalogCommand::Drop { ns }));
    let id = create(&mut sm, "app", "users");
    create(&mut sm, "other", "c");
    let spec = Document::new().with("key", Document::new().with("a", 1)).with("name", "a_1");
    let index = IndexCommand::Create {
        collection: id,
        name: "a_1".into(),
        spec: spec.to_bytes().unwrap(),
        buckets: 1,
    };
    assert!(matches!(run(&mut sm, Group0Command::Indexes(index)), Group0Result::Index(_)));
    sm
}

fn restore_of(sm: &Group0Sm) -> Group0Command {
    Group0Command::RestoreCatalog {
        catalog: Box::new((*sm.catalog).clone()),
        indexes: Box::new(sm.indexes.clone()),
        security: None,
    }
}

#[test]
fn the_command_round_trips_with_its_kind() {
    let cmd = restore_of(&source());
    assert_eq!(cmd.kind(), kind::RESTORE_CATALOG);
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd));
}

#[test]
fn an_empty_catalog_takes_the_backups_with_its_numbers() {
    let src = source();
    let mut dst = cluster();
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Ok);
    assert_eq!(dst.catalog, src.catalog);
    assert_eq!(dst.indexes, src.indexes);
    // A collection created afterwards takes a number past every restored one.
    let next = create(&mut dst, "app", "later");
    assert!(src.catalog.collections.values().all(|c| c.id < next), "{next}");
    // The restored state travels in the snapshot.
    let mut back = Group0Sm::default();
    teseladb_multiraft::StateMachine::restore(
        &mut back,
        &teseladb_multiraft::StateMachine::snapshot(&dst),
    )
    .unwrap();
    assert_eq!(back.catalog, dst.catalog);
}

#[test]
fn a_catalog_with_a_collection_or_an_index_refuses_it() {
    let src = source();
    let mut dst = cluster();
    create(&mut dst, "app", "mine");
    let before = Arc::clone(&dst.catalog);
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Err(Group0Error::NotEmpty));
    assert_eq!(dst.catalog, before);
    // Applied twice, the second finds the first one's collections.
    let mut dst = cluster();
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Ok);
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Err(Group0Error::NotEmpty));
}

/// A user `name` of `admin`.
fn user(sm: &mut Group0Sm, name: &str) {
    let doc =
        Document::new().with("_id", format!("admin.{name}")).with("user", name).with("db", "admin");
    let op = SecurityOp::put(Principal::User, Expect::Absent, doc.to_bytes().unwrap()).unwrap();
    assert_eq!(run(sm, Group0Command::Security(vec![op])), Group0Result::Ok);
}

fn users(sm: &Group0Sm) -> Vec<Vec<u8>> {
    sm.security.users.docs().map(<[u8]>::to_vec).collect()
}

/// A source with two tenants (the first with limits), a user, and its catalog.
fn secured_source() -> Group0Sm {
    let mut sm = source();
    for name in ["acme", "beta"] {
        let r = run(&mut sm, Group0Command::CreateTenant { name: name.into() });
        assert!(matches!(r, Group0Result::Tenant(_) | Group0Result::Ok), "{r:?}");
    }
    let limits = TenantLimits { ops_per_second: 500, ..TenantLimits::default() };
    let r = run(&mut sm, Group0Command::SetTenantLimits { tenant: 1, limits });
    assert_eq!(r, Group0Result::Ok);
    user(&mut sm, "ann");
    sm
}

fn secured_restore_of(sm: &Group0Sm) -> Group0Command {
    Group0Command::RestoreCatalog {
        catalog: Box::new((*sm.catalog).clone()),
        indexes: Box::new(sm.indexes.clone()),
        security: Some(Box::new(RestoredSecurity {
            tenants: (*sm.tenants).clone(),
            security: sm.security.clone(),
        })),
    }
}

#[test]
fn the_command_with_the_security_state_round_trips() {
    let cmd = secured_restore_of(&secured_source());
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd));
}

#[test]
fn the_backups_tenants_and_users_take_the_place_of_the_clusters() {
    let src = secured_source();
    let mut dst = cluster();
    // The new cluster has its own root and had given a tenant number the backup also uses.
    user(&mut dst, "root");
    for name in ["one", "two", "three"] {
        run(&mut dst, Group0Command::CreateTenant { name: name.into() });
    }
    let version = dst.security.version;
    assert_eq!(run(&mut dst, secured_restore_of(&src)), Group0Result::Ok);
    assert_eq!(dst.tenants.list(), src.tenants.list());
    assert_eq!(dst.tenants.state(1).unwrap().limits.ops_per_second, 500);
    assert_eq!(users(&dst), users(&src));
    assert!(dst.security.version > version.max(src.security.version), "the nodes see it changed");
    // A tenant created afterwards takes a number neither cluster gave.
    run(&mut dst, Group0Command::CreateTenant { name: "later".into() });
    let later = dst.tenants.id_of("later").unwrap();
    assert!(later > 3, "{later}");
    // The restored tenants and users travel in the snapshot.
    let mut back = Group0Sm::default();
    teseladb_multiraft::StateMachine::restore(
        &mut back,
        &teseladb_multiraft::StateMachine::snapshot(&dst),
    )
    .unwrap();
    assert_eq!(back.tenants, dst.tenants);
    assert_eq!(users(&back), users(&dst));
}

#[test]
fn the_security_state_waits_until_every_node_reads_it() {
    let src = secured_source();
    let mut dst = cluster();
    let old = Document::new().with(FORMAT, 20_i64).to_bytes().unwrap();
    let r = run(&mut dst, Group0Command::SetSetting { key: node_key(NodeId(1)), value: old });
    assert_eq!(r, Group0Result::Ok);
    let refused = run(&mut dst, secured_restore_of(&src));
    assert_eq!(refused, Group0Result::Err(Group0Error::NotEveryNodeRunsIt));
    assert!(dst.catalog.collections.is_empty(), "nothing restored");
    // A backup without them still restores.
    assert_eq!(run(&mut dst, restore_of(&src)), Group0Result::Ok);
}
