//! 007 T038 (FR-025): `teseladb admin tenant create|list|drop`, and a node that runs each request
//! in its tenant: the one `$tenant` names on a connection of the default tenant.

mod common;

use std::net::SocketAddr;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig};

fn admin(at: &str, args: &[&str]) -> (i32, String, String) {
    let argv: Vec<String> =
        ["admin", "--node", at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn with_admin() -> NodeConfig {
    NodeConfig { admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))), ..config(Vec::new()) }
}

fn found(r: &Document) -> usize {
    batch(r, "firstBatch").len()
}

#[test]
fn tenants_are_created_listed_and_dropped_and_each_sees_its_databases() {
    let node = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let at = node.admin_addr().unwrap().to_string();
    let (code, out, err) = admin(&at, &["tenant", "create", "acme"]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(out, "tenant acme: 1\n");
    let (code, _, err) = admin(&at, &["tenant", "create", "acme"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: a tenant named 'acme' exists\n"));
    let (code, out, _) = admin(&at, &["tenant", "list"]);
    assert_eq!((code, out.as_str()), (0, "1\tacme\n"));
    // Each tenant has its own `app`.
    let mut c = Client::new(&node);
    let insert = |n| Document::new().with("insert", "c").with("documents", docs(n..n + 1));
    assert!(ok(&c.run("app", insert(0))));
    assert!(ok(&c.run("app", insert(1).with("$tenant", "acme"))));
    assert!(ok(&c.run("app", insert(2).with("$tenant", "acme"))));
    let find = || Document::new().with("find", "c");
    assert_eq!(found(&c.run("app", find())), 1);
    assert_eq!(found(&c.run("app", find().with("$tenant", "acme"))), 2);
    // A tenant with databases stays.
    let (code, _, err) = admin(&at, &["tenant", "drop", "acme"]);
    assert_eq!(code, 1);
    assert_eq!(err, "teseladb admin: tenant 'acme' still has databases or users\n");
    let drop = Document::new().with("dropDatabase", 1).with("$tenant", "acme");
    assert!(ok(&c.run("app", drop)));
    let (code, out, err) = admin(&at, &["tenant", "drop", "acme"]);
    assert_eq!((code, out.as_str()), (0, "tenant acme dropped\n"), "{err}");
    let (code, _, err) = admin(&at, &["tenant", "drop", "acme"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: no tenant named 'acme'\n"));
    let (_, out, _) = admin(&at, &["tenant", "list", "--format", "json"]);
    assert_eq!(out.trim_end(), r#"{"tenants":[],"ok":1.0}"#);
    // A tenant once dropped is no tenant for the next request.
    let gone = |r: &Document| r.get("code") == Some(&Value::Int32(2));
    until("the drop on this node", || {
        gone(&c.run("app", find().with("$tenant", "acme"))).then_some(())
    });
    // The default tenant's data never moved.
    assert_eq!(found(&c.run("app", find())), 1);
    node.stop();
}

/// 007 T055 (FR-030, FR-034, FR-035): the limits, the access list and the suspension of a tenant
/// through `teseladb admin tenant limits|access|suspend|resume`, and what `tenant list` shows.
#[test]
fn a_tenant_gets_limits_an_access_list_and_a_suspension_through_the_port() {
    let node = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let at = node.admin_addr().unwrap().to_string();
    let run = |args: &[&str]| {
        let (code, out, err) = admin(&at, args);
        assert_eq!(code, 0, "{args:?}: {out}{err}");
        out
    };
    run(&["tenant", "create", "acme"]);
    // A new tenant has no limits, its switches on and any source.
    let shown = run(&["tenant", "limits", "acme"]);
    assert!(
        shown.contains("storageBytes\t0\n") && shown.contains("httpEnabled\ttrue\n"),
        "{shown}"
    );
    assert_eq!(run(&["tenant", "access", "acme"]), "any source\n");
    let changed = run(&[
        "tenant",
        "limits",
        "acme",
        "--storage-bytes",
        "1048576",
        "--max-connections",
        "5",
        "--http",
        "off",
        "--service-user",
        "svc.acme",
    ]);
    assert!(
        changed.contains("storageBytes\t1048576\n") && changed.contains("httpEnabled\tfalse\n")
    );
    // Only the limits given change.
    let again = run(&["tenant", "limits", "acme", "--ops-per-second", "100"]);
    assert!(
        again.contains("maxConnections\t5\n") && again.contains("opsPerSecond\t100\n"),
        "{again}"
    );
    assert_eq!(
        run(&["tenant", "access", "acme", "--allow", "10.0.0.0/8", "--allow", "2001:db8::/32"]),
        "10.0.0.0/8\n2001:db8::/32\n"
    );
    assert_eq!(run(&["tenant", "suspend", "acme"]), "tenant acme suspended\n");
    assert_eq!(run(&["tenant", "suspend", "acme"]), "tenant acme suspended\n", "twice is no error");
    let line = run(&["tenant", "list"]);
    assert!(
        line.starts_with("1\tacme\tstorageBytes=1048576\tmaxConnections=5\topsPerSecond=100\t"),
        "{line}"
    );
    assert!(
        line.contains("\thttpEnabled=off\t") && line.contains("\tserviceUser=svc.acme\t"),
        "{line}"
    );
    assert!(line.ends_with("\tallowedSources=10.0.0.0/8,2001:db8::/32\tsuspended\n"), "{line}");
    assert_eq!(run(&["tenant", "resume", "acme"]), "tenant acme resumed\n");
    assert_eq!(run(&["tenant", "access", "acme", "--clear"]), "any source\n");
    assert!(!run(&["tenant", "list"]).contains("suspended"));
    // What the port refuses.
    let (code, _, err) = admin(&at, &["tenant", "limits", "nobody", "--max-connections", "1"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: no tenant named 'nobody'\n"));
    let (code, _, err) = admin(&at, &["tenant", "access", "acme", "--allow", "10.0.0.0/40"]);
    assert_eq!(
        (code, err.as_str()),
        (1, "teseladb admin: not an address or a CIDR block: \"10.0.0.0/40\"\n")
    );
    let (code, _, err) = admin(&at, &["tenant", "limits", "acme", "--max-connections", "x"]);
    assert_eq!((code, err.as_str()), (2, "--max-connections: not a whole number: x\n"));
    let (code, _, err) = admin(&at, &["tenant", "suspend", "acme", "--allow", "1.2.3.4"]);
    assert_eq!((code, err.as_str()), (2, "--allow does not go with this subcommand\n"));
    node.stop();
}

/// `_mergeAuthzCollections` run in a tenant puts the users in that tenant, as `createUser` does,
/// whatever tenant their documents named (021 FR-044, a copy restored into another cluster's
/// tenant).
#[test]
fn users_merged_in_a_tenant_belong_to_it() {
    let node = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let at = node.admin_addr().unwrap().to_string();
    let (code, _, err) = admin(&at, &["tenant", "create", "acme"]);
    assert_eq!(code, 0, "{err}");
    let mut c = Client::new(&node);
    let create = Document::new()
        .with("createUser", "moved")
        .with("pwd", "moved-pw-123456")
        .with("roles", Value::Array(Vec::new()));
    assert!(ok(&c.run("admin", create)), "a user of the default tenant");
    let info = Document::new()
        .with("usersInfo", Document::new().with("user", "moved").with("db", "admin"))
        .with("showCredentials", true);
    let r = c.run("admin", info.clone());
    let Some(Value::Array(users)) = r.get("users") else { panic!("{r:?}") };
    let Some(Value::Document(user)) = users.first() else { panic!("{r:?}") };
    let mut stored = Document::new();
    for f in ["_id", "userId", "user", "db", "credentials", "roles"] {
        if let Some(v) = user.get(f) {
            stored.push(f, v.clone());
        }
    }
    // The document of another cluster's tenant.
    stored.push("tenantId", 99i64);
    assert!(ok(&c.run("admin", Document::new().with("dropUser", "moved"))));
    let temp = Document::new()
        .with("insert", "temp_users")
        .with("documents", Value::Array(vec![Value::Document(stored)]))
        .with("$tenant", "acme");
    assert!(ok(&c.run("admin", temp)), "the temporary collection");
    let merge = Document::new()
        .with("_mergeAuthzCollections", 1)
        .with("tempUsersCollection", "admin.temp_users")
        .with("db", "")
        .with("drop", false)
        .with("$tenant", "acme");
    let r = c.run("admin", merge);
    assert!(ok(&r), "{r:?}");
    let users_of = |c: &mut Client, tenant: Option<&str>| {
        let mut q = info.clone();
        if let Some(t) = tenant {
            q.push("$tenant", t);
        }
        match c.run("admin", q).get("users") {
            Some(Value::Array(u)) => u.len(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(users_of(&mut c, Some("acme")), 1, "the user is the tenant's");
    assert_eq!(users_of(&mut c, None), 0, "and not the default tenant's");
}

/// A user or role merged into a tenant whose name another tenant holds, as a copy of one tenant
/// restored into another of the same cluster brings it: names are unique in the cluster
/// (DV-023), so the merge answers that the name exists, the error of `createRole` and
/// `createUser`, at once and with nothing written (021 FR-047).
#[test]
fn a_merged_role_or_user_whose_name_another_tenant_holds_is_refused_at_once() {
    let node = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let at = node.admin_addr().unwrap().to_string();
    for t in ["acme", "beta"] {
        let (code, _, err) = admin(&at, &["tenant", "create", t]);
        assert_eq!(code, 0, "{err}");
    }
    let mut c = Client::new(&node);
    let privileges = || {
        let resource = Document::new().with("db", "shop").with("collection", "");
        let find = Document::new()
            .with("resource", resource)
            .with("actions", Value::Array(vec![Value::from("find")]));
        Value::Array(vec![Value::Document(find)])
    };
    let role = Document::new()
        .with("createRole", "reporter")
        .with("privileges", privileges())
        .with("roles", Value::Array(Vec::new()))
        .with("$tenant", "acme");
    assert!(ok(&c.run("shop", role)), "a role of acme");
    let user = Document::new()
        .with("createUser", "clerk")
        .with("pwd", "clerk-pw-123456")
        .with("roles", Value::Array(Vec::new()))
        .with("$tenant", "acme");
    assert!(ok(&c.run("shop", user)), "a user of acme");
    let role_with_id = |id: &str, name: &str| {
        Document::new()
            .with("_id", id)
            .with("role", name)
            .with("db", "shop")
            .with("privileges", privileges())
            .with("roles", Value::Array(Vec::new()))
    };
    let stored_role = |name: &str| role_with_id(&format!("shop.{name}"), name);
    let stored_user = Document::new()
        .with("_id", "shop.clerk")
        .with("user", "clerk")
        .with("db", "shop")
        .with("credentials", Document::new())
        .with("roles", Value::Array(Vec::new()));
    let fill = |c: &mut Client, coll: &str, docs: Vec<Document>| {
        // A reference router refuses to drop a collection of `admin`: its documents go instead.
        let all = Document::new().with("q", Document::new()).with("limit", 0);
        let delete = Document::new()
            .with("delete", coll)
            .with("deletes", Value::Array(vec![Value::Document(all)]))
            .with("$tenant", "beta");
        assert!(ok(&c.run("admin", delete)), "the temporary collection {coll} emptied");
        let docs = Value::Array(docs.into_iter().map(Value::Document).collect());
        let insert =
            Document::new().with("insert", coll).with("documents", docs).with("$tenant", "beta");
        assert!(ok(&c.run("admin", insert)), "the temporary collection {coll}");
    };
    let merge = |c: &mut Client, field: &str, coll: &str| {
        let cmd = Document::new()
            .with("_mergeAuthzCollections", 1)
            .with(field, format!("admin.{coll}"))
            .with("db", "")
            .with("drop", false)
            .with("$tenant", "beta");
        c.run("admin", cmd)
    };
    let refused = |r: &Document, code: i32, msg: &str| {
        assert_eq!(r.get("code"), Some(&Value::Int32(code)), "{r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(msg)), "{r:?}");
    };
    // A free role and the one acme holds: refused, and the free one is not written either.
    fill(&mut c, "temp_roles", vec![stored_role("auditor"), stored_role("reporter")]);
    let r = merge(&mut c, "tempRolesCollection", "temp_roles");
    refused(&r, 51_002, "Role \"reporter@shop\" already exists");
    let roles_of = |c: &mut Client, tenant: &str| {
        let info = Document::new().with("rolesInfo", 1).with("$tenant", tenant);
        match c.run("shop", info).get("roles") {
            Some(Value::Array(r)) => r.len(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(roles_of(&mut c, "beta"), 0, "nothing of the refused merge");
    // The name taken under another `_id`: the role with that name is the one named.
    let renamed = role_with_id("copied.reporter", "reporter");
    fill(&mut c, "temp_roles", vec![stored_role("auditor"), renamed]);
    let r = merge(&mut c, "tempRolesCollection", "temp_roles");
    refused(&r, 51_002, "Role \"reporter@shop\" already exists");
    fill(&mut c, "temp_users", vec![stored_user]);
    let r = merge(&mut c, "tempUsersCollection", "temp_users");
    refused(&r, 51_003, "User \"clerk@shop\" already exists");
    // Without the taken name the merge goes through, into beta.
    fill(&mut c, "temp_roles", vec![stored_role("auditor")]);
    let r = merge(&mut c, "tempRolesCollection", "temp_roles");
    assert!(ok(&r), "{r:?}");
    assert_eq!(roles_of(&mut c, "beta"), 1);
    assert_eq!(roles_of(&mut c, "acme"), 1);
}
