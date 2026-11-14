//! 013 FR-011: the privileges of every built-in role, where each one exists, and how a tenant's
//! user, an inherited role and a revoked privilege change a set of privileges.

use teseladb_auth::actions::ActionType as A;
use teseladb_auth::authz::{Target, allows};
use teseladb_auth::privileges::{Privilege, Resource, builtin, merge, of_tenant, subtract};
use teseladb_auth::roles::{
    ADMIN_DB, ADMIN_ROLES, DATABASE_ROLES, Role, is_builtin, is_builtin_name,
};
use teseladb_auth::users::RoleName;

const DB: &str = "app";

#[test]
fn every_built_in_role_has_privileges_where_it_exists() {
    for name in DATABASE_ROLES {
        for db in [DB, ADMIN_DB] {
            let role = RoleName::new(name, db);
            assert!(is_builtin(&role) && is_builtin_name(name), "{name} on {db}");
            let privileges = builtin(&role).unwrap_or_else(|| panic!("{name} on {db}"));
            assert!(!privileges.is_empty(), "{name} on {db}");
            assert!(Role::builtin(&role).is_some(), "{name} on {db}");
        }
        assert!(!is_builtin(&RoleName::new(name, "$external")), "{name} on $external");
    }
    for name in ADMIN_ROLES {
        let role = RoleName::new(name, ADMIN_DB);
        assert!(is_builtin(&role) && is_builtin_name(name), "{name}");
        assert!(builtin(&role).is_some(), "{name}");
        assert!(!is_builtin(&RoleName::new(name, DB)), "{name} outside admin");
        assert_eq!(builtin(&RoleName::new(name, DB)), None, "{name} outside admin");
    }
    assert_eq!(builtin(&RoleName::new("custom", DB)), None);
    assert!(!is_builtin_name("custom"));
}

#[test]
fn the_roles_give_the_actions_their_names_say() {
    let of = |name: &str, db: &str| builtin(&RoleName::new(name, db)).expect("a built-in role");
    let read = of("read", DB);
    assert!(allows(&read, &Target::collection(DB, "c"), A::Find));
    assert!(!allows(&read, &Target::collection(DB, "c"), A::Insert));
    assert!(!allows(&read, &Target::collection("other", "c"), A::Find));
    let write = of("readWrite", DB);
    assert!(allows(&write, &Target::collection(DB, "c"), A::Insert));
    let monitor = of("clusterMonitor", ADMIN_DB);
    assert!(allows(&monitor, &Target::Cluster, A::ServerStatus));
    assert!(!allows(&monitor, &Target::Cluster, A::Shutdown));
    let any_reader = of("readAnyDatabase", ADMIN_DB);
    assert!(allows(&any_reader, &Target::collection("other", "c"), A::Find));
    let root = of("root", ADMIN_DB);
    assert!(allows(&root, &Target::Cluster, A::Shutdown));
    assert!(allows(&root, &Target::collection("other", "c"), A::DropCollection));
}

#[test]
fn a_tenant_user_keeps_only_its_own_data_and_three_cluster_actions() {
    let privs = vec![
        Privilege { resource: Resource::Any, actions: vec![A::AnyAction] },
        Privilege { resource: Resource::Cluster, actions: vec![A::Shutdown, A::ListDatabases] },
        Privilege { resource: Resource::Cluster, actions: vec![A::ServerStatus] },
        Privilege {
            resource: Resource::Ns { db: DB.into(), collection: String::new() },
            actions: vec![A::Find],
        },
    ];
    let kept = of_tenant(privs);
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert_eq!(kept[0], Privilege { resource: Resource::Cluster, actions: vec![A::ListDatabases] });
    assert!(allows(&kept, &Target::collection(DB, "c"), A::Find));
}

#[test]
fn privileges_merge_by_resource_and_go_when_no_action_is_left() {
    let app = Resource::Ns { db: DB.into(), collection: String::new() };
    let mut set = vec![Privilege { resource: app.clone(), actions: vec![A::Insert] }];
    merge(
        &mut set,
        &[
            Privilege { resource: app.clone(), actions: vec![A::Find, A::Insert] },
            Privilege { resource: Resource::Cluster, actions: vec![A::ServerStatus] },
        ],
    );
    assert_eq!(set.len(), 2);
    let mut want = vec![A::Find, A::Insert];
    want.sort();
    assert_eq!(set[0].actions, want);
    subtract(
        &mut set,
        &[
            Privilege { resource: Resource::Cluster, actions: vec![A::ServerStatus] },
            Privilege { resource: app.clone(), actions: vec![A::Find] },
            Privilege { resource: Resource::Any, actions: vec![A::Find] },
        ],
    );
    assert_eq!(set, vec![Privilege { resource: app, actions: vec![A::Insert] }]);
}
