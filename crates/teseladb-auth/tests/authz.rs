//! 013 FR-011: what a privilege's resource covers and touches, and whether a set of privileges
//! passes each kind of check.

use teseladb_auth::actions::ActionType as A;
use teseladb_auth::authz::{
    BUCKETS_PREFIX, Check, Need, Requirement, Target, allows, holds_all, needs, touches,
};
use teseladb_auth::privileges::{Privilege, Resource};

fn ns(db: &str, coll: &str) -> Resource {
    Resource::Ns { db: db.into(), collection: coll.into() }
}

fn privilege(resource: Resource, actions: &[A]) -> Privilege {
    Privilege { resource, actions: actions.to_vec() }
}

#[test]
fn a_namespace_resource_covers_its_databases_and_collections() {
    let any_normal = ns("", "");
    assert!(any_normal.covers(&Target::AnyNormal));
    assert!(any_normal.covers(&Target::database("app")));
    assert!(!any_normal.covers(&Target::database("config")));
    assert!(!any_normal.covers(&Target::database("local")));
    assert!(any_normal.covers(&Target::collection("app", "c")));
    assert!(!any_normal.covers(&Target::collection("app", "system.views")));
    assert!(!any_normal.covers(&Target::collection("local", "c")));
    assert!(!any_normal.covers(&Target::Cluster));
    // A collection name in every database.
    let named = ns("", "c");
    assert!(named.covers(&Target::collection("other", "c")));
    assert!(!named.covers(&Target::collection("other", "d")));
    assert!(!named.covers(&Target::database("other")));
    // A database.
    let db = ns("app", "");
    assert!(db.covers(&Target::database("app")) && !db.covers(&Target::database("other")));
    assert!(db.covers(&Target::collection("app", "c")));
    assert!(!db.covers(&Target::collection("app", "system.js")));
    assert!(!db.covers(&Target::AnyNormal));
    // One collection.
    let one = ns("app", "c");
    assert!(one.covers(&Target::collection("app", "c")));
    assert!(!one.covers(&Target::collection("app", "d")));
    assert!(!one.covers(&Target::database("app")));
}

#[test]
fn the_cluster_any_and_bucket_resources_cover_their_own_targets() {
    assert!(Resource::Any.covers(&Target::Cluster) && Resource::Any.covers(&Target::Any));
    assert!(Resource::Cluster.covers(&Target::Cluster));
    assert!(!Resource::Cluster.covers(&Target::database("admin")));
    let buckets = Resource::SystemBuckets { db: Some("app".into()), bucket: "w".into() };
    let w = format!("{BUCKETS_PREFIX}w");
    assert!(buckets.covers(&Target::collection("app", &w)));
    assert!(!buckets.covers(&Target::collection("other", &w)));
    assert!(!buckets.covers(&Target::collection("app", "w")));
    assert!(!buckets.covers(&Target::database("app")));
    let everywhere = Resource::SystemBuckets { db: None, bucket: "w".into() };
    assert!(everywhere.covers(&Target::collection("other", &w)));
}

#[test]
fn a_resource_touches_the_databases_it_names() {
    assert!(Resource::Any.touches("app"));
    assert!(!Resource::Cluster.touches("app"));
    assert!(ns("", "").touches("app") && !ns("", "").touches("local"));
    assert!(ns("", "c").touches("local"), "a collection name reaches every database");
    assert!(ns("app", "c").touches("app") && !ns("app", "c").touches("other"));
    let buckets = Resource::SystemBuckets { db: Some("app".into()), bucket: "w".into() };
    assert!(buckets.touches("app") && !buckets.touches("other"));
    assert!(Resource::SystemBuckets { db: None, bucket: "w".into() }.touches("other"));
    let set = [privilege(ns("app", "c"), &[A::Find])];
    assert!(touches(&set, "app") && !touches(&set, "other"));
}

#[test]
fn a_set_of_privileges_holds_another_resource_by_resource() {
    let admin = [privilege(Resource::Any, &[A::AnyAction])];
    let required = [
        privilege(Resource::Cluster, &[A::ServerStatus]),
        privilege(Resource::Any, &[A::Find]),
        privilege(ns("", ""), &[A::Find]),
        privilege(ns("app", ""), &[A::Insert]),
        privilege(ns("app", "c"), &[A::Remove]),
        privilege(
            Resource::SystemBuckets { db: Some("app".into()), bucket: "w".into() },
            &[A::Find],
        ),
        privilege(Resource::SystemBuckets { db: None, bucket: "w".into() }, &[A::Find]),
    ];
    assert!(holds_all(&admin, &required));
    let monitor = [privilege(Resource::Cluster, &[A::ServerStatus])];
    assert!(holds_all(&monitor, &required[..1]));
    assert!(!holds_all(&monitor, &required));
    assert!(allows(&admin, &Target::collection("app", "c"), A::DropCollection));
    assert!(!allows(&monitor, &Target::Cluster, A::Shutdown));
}

#[test]
fn each_kind_of_check_passes_or_not() {
    let reader = [privilege(ns("app", ""), &[A::Find])];
    assert!(Check::Open.passes(&[], false));
    let find = Check::Needs(vec![Requirement::all(&Target::collection("app", "c"), &[A::Find])]);
    assert!(find.passes(&reader, false) && !find.passes(&[], false));
    let either = Requirement::either(
        needs(&Target::Cluster, &[A::Shutdown]),
        vec![Need::new(Target::database("app"), A::Find)],
    );
    assert!(either.holds(&reader) && !either.holds(&[]));
    assert!(Check::InDatabase("app".into()).passes(&reader, false));
    assert!(!Check::InDatabase("other".into()).passes(&reader, false));
    let more = Check::GetMore { target: Target::collection("app", "c"), id: 7 };
    assert!(more.passes(&reader, false) && !more.passes(&[], false));
    let kill = Check::KillCursors { target: Target::collection("app", "c"), ids: vec![7] };
    assert!(kill.passes(&[], true), "the user's own cursors");
    assert!(!kill.passes(&reader, false));
    let on_coll = [privilege(ns("app", ""), &[A::KillAnyCursor])];
    let on_cluster = [privilege(Resource::Cluster, &[A::KillAnyCursor])];
    assert!(kill.passes(&on_coll, false) && kill.passes(&on_cluster, false));
}

#[test]
fn a_namespace_needs_both_its_parts() {
    assert_eq!(Target::namespace("app.c"), Some(Target::collection("app", "c")));
    assert_eq!(Target::namespace("app.c.d"), Some(Target::collection("app", "c.d")));
    for bad in ["app", ".c", "app."] {
        assert_eq!(Target::namespace(bad), None, "{bad}");
    }
}
