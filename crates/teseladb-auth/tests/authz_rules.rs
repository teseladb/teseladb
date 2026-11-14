//! 013 FR-011: what each command needs, row by row of the table and rule by rule for the commands
//! whose needs depend on their fields (`contracts/privileges.md` of 013).

use teseladb_auth::actions::ActionType as A;
use teseladb_auth::authz::{Check, Need, Requirement, Target, needs};
use teseladb_auth::authz_rules::{Subject, check, has_rule};
use teseladb_auth::users::{RoleName, UserName};
use teseladb_bson::{Document, Value};

const DB: &str = "app";

fn doc(fields: &[(&str, Value)]) -> Document {
    fields.iter().fold(Document::new(), |d, (k, v)| d.with(*k, v.clone()))
}

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

/// What `cmd` needs on `$db` `db` from user `ann` of `app` with role `reader` of `app`.
fn on(db: &str, cmd: &Document) -> Check {
    let user = UserName::new("ann", DB);
    let roles = [RoleName::new("reader", DB)];
    let name = cmd.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default();
    check(&name, db, cmd, &Subject { user: &user, roles: &roles })
}

fn of(cmd: &Document) -> Check {
    on(DB, cmd)
}

fn all(target: &Target, actions: &[A]) -> Check {
    Check::Needs(vec![Requirement::all(target, actions)])
}

/// One option of every need in `list`, sorted and without repeats, as the write rules give it.
fn set(mut list: Vec<Need>) -> Check {
    list.sort();
    list.dedup();
    Check::Needs(vec![Requirement { options: vec![list] }])
}

fn coll(c: &str) -> Target {
    Target::collection(DB, c)
}

#[test]
fn each_kind_of_row_names_its_target() {
    assert_eq!(of(&doc(&[("ping", 1.into())])), Check::Open);
    assert_eq!(of(&doc(&[("serverStatus", 1.into())])), all(&Target::Cluster, &[A::ServerStatus]));
    assert_eq!(of(&doc(&[("dbStats", 1.into())])), all(&Target::database(DB), &[A::DbStats]));
    let named = doc(&[("enableSharding", "other".into())]);
    assert_eq!(of(&named), all(&Target::database("other"), &[A::EnableSharding]));
    let unnamed = doc(&[("enableSharding", 1.into())]);
    assert_eq!(of(&unnamed), all(&Target::database(DB), &[A::EnableSharding]));
    assert_eq!(of(&doc(&[("find", "c".into())])), all(&coll("c"), &[A::Find]));
    assert_eq!(of(&doc(&[("find", 1.into())])), all(&Target::database(DB), &[A::Find]));
    let full = doc(&[("split", "other.c".into())]);
    assert_eq!(of(&full), all(&Target::collection("other", "c"), &[A::SplitChunk]));
    for bad in [Value::from("nodot"), Value::Int32(1)] {
        let cmd = doc(&[("split", bad)]);
        assert_eq!(of(&cmd), all(&Target::database(DB), &[A::SplitChunk]));
    }
    let any = doc(&[("validateDBMetadata", 1.into())]);
    assert_eq!(of(&any), all(&Target::AnyNormal, &[A::Validate]));
    let config = doc(&[("balancerStatus", 1.into())]);
    assert_eq!(of(&config), all(&Target::database("config"), &[A::Find]));
    // `moveChunk` on `admin`, whatever `$db` the command names.
    let placement = doc(&[("resetPlacementHistory", 1.into())]);
    assert_eq!(on("other", &placement), all(&Target::database("admin"), &[A::MoveChunk]));
    // A command without a row needs everything.
    let unknown = doc(&[("noSuchCommand", 1.into())]);
    assert!(!has_rule("noSuchCommand") && has_rule("find") && has_rule("insert"));
    assert_eq!(of(&unknown), all(&Target::Any, &[A::AnyAction]));
}

#[test]
fn writes_need_their_actions_and_the_bypass_when_asked() {
    let insert = doc(&[("insert", "c".into()), ("bypassDocumentValidation", true.into())]);
    let c = coll("c");
    assert_eq!(
        of(&insert),
        set(vec![
            Need::new(c.clone(), A::Insert),
            Need::new(c.clone(), A::BypassDocumentValidation)
        ])
    );
    let upsert = doc(&[
        ("update", "c".into()),
        ("updates", arr(vec![Value::Document(doc(&[("upsert", Value::Int32(1))]))])),
    ]);
    assert_eq!(of(&upsert), set(needs(&c, &[A::Update, A::Insert])));
    let plain = doc(&[("update", "c".into()), ("updates", arr(vec![Value::Int32(1)]))]);
    assert_eq!(of(&plain), set(needs(&c, &[A::Update])));
    let remove = doc(&[("findAndModify", "c".into()), ("remove", Value::Double(1.0))]);
    assert_eq!(of(&remove), set(needs(&c, &[A::Find, A::Remove])));
    let upserted = doc(&[("findAndModify", "c".into()), ("upsert", Value::Int64(1))]);
    assert_eq!(of(&upserted), set(needs(&c, &[A::Find, A::Update, A::Insert])));
    let neither = doc(&[("findAndModify", "c".into()), ("upsert", "yes".into())]);
    assert_eq!(of(&neither), set(needs(&c, &[A::Find, A::Update])));
}

#[test]
fn a_bulk_write_needs_each_operation_on_its_namespace() {
    let ns = |n: &str| Value::Document(doc(&[("ns", n.into())]));
    let op = |kind: &str, i: Value, upsert: bool| {
        let mut d = doc(&[(kind, i)]);
        if upsert {
            d = d.with("upsert", true);
        }
        Value::Document(d)
    };
    let cmd = doc(&[
        ("bulkWrite", 1.into()),
        (
            "ops",
            arr(vec![
                op("insert", Value::Int32(0), false),
                op("delete", Value::Int64(1), false),
                op("update", Value::Int32(0), true),
                op("update", Value::Int32(1), false),
                op("insert", Value::Int32(9), false),
                op("insert", "x".into(), false),
                Value::Document(Document::new()),
            ]),
        ),
        ("nsInfo", arr(vec![ns("a.x"), ns("b.y")])),
    ]);
    let (x, y) = (Target::collection("a", "x"), Target::collection("b", "y"));
    let mut want = needs(&x, &[A::Insert, A::Update]);
    want.extend(needs(&y, &[A::Remove, A::Update]));
    assert_eq!(of(&cmd), set(want));
}

#[test]
fn cursor_commands_name_their_cursor_and_collection() {
    let more = doc(&[("getMore", Value::Int64(7)), ("collection", "c".into())]);
    assert_eq!(of(&more), Check::GetMore { target: coll("c"), id: 7 });
    let small = doc(&[("getMore", Value::Int32(8))]);
    assert_eq!(of(&small), Check::GetMore { target: Target::database(DB), id: 8 });
    let odd = doc(&[("getMore", "x".into())]);
    assert_eq!(of(&odd), Check::GetMore { target: Target::database(DB), id: 0 });
    let kill = doc(&[
        ("killCursors", "c".into()),
        ("cursors", arr(vec![Value::Int64(5), Value::Int32(6)])),
    ]);
    assert_eq!(of(&kill), Check::KillCursors { target: coll("c"), ids: vec![5] });
    let none = doc(&[("killCursors", "c".into())]);
    assert_eq!(of(&none), Check::KillCursors { target: coll("c"), ids: Vec::new() });
}

fn pipeline(source: Value, stages: Vec<Document>) -> Document {
    let stages = stages.into_iter().map(Value::Document).collect();
    doc(&[("aggregate", source), ("pipeline", Value::Array(stages))])
}

#[test]
fn an_aggregation_needs_its_first_stage_or_find_and_what_its_stages_touch() {
    let c = coll("c");
    let stage = |name: &str, spec: Value| doc(&[(name, spec)]);
    let empty = || Value::Document(Document::new());
    let first = [
        ("$collStats", needs(&c, &[A::CollStats])),
        ("$indexStats", needs(&c, &[A::IndexStats])),
        ("$planCacheStats", needs(&c, &[A::PlanCacheRead])),
        ("$listSearchIndexes", needs(&Target::database(DB), &[A::ListSearchIndexes])),
        ("$changeStream", needs(&c, &[A::ChangeStream, A::Find])),
        ("$currentOp", needs(&Target::Cluster, &[A::Inprog])),
        ("$listSessions", Vec::new()),
        ("$documents", Vec::new()),
        ("$shardedDataDistribution", needs(&Target::Cluster, &[A::ShardedDataDistribution])),
        ("$listClusterCatalog", needs(&Target::Cluster, &[A::ListClusterCatalog])),
        ("$querySettings", needs(&Target::Cluster, &[A::QuerySettings])),
        (
            "$listCatalog",
            needs(&Target::Any, &[A::ListDatabases, A::ListCollections, A::ListIndexes]),
        ),
        ("$match", needs(&c, &[A::Find])),
    ];
    for (name, want) in first {
        assert_eq!(of(&pipeline("c".into(), vec![stage(name, empty())])), set(want), "{name}");
    }
    let all_users = Value::Document(doc(&[("allUsers", true.into())]));
    let sessions = pipeline(Value::Int32(1), vec![stage("$listLocalSessions", all_users)]);
    assert_eq!(of(&sessions), set(needs(&Target::Cluster, &[A::ListSessions])));
    let cluster = Value::Document(doc(&[("allChangesForCluster", true.into())]));
    let everything = pipeline(Value::Int32(1), vec![stage("$changeStream", cluster)]);
    assert_eq!(of(&everything), set(needs(&Target::AnyNormal, &[A::ChangeStream, A::Find])));
    let database = pipeline(Value::Int32(1), vec![stage("$changeStream", empty())]);
    assert_eq!(of(&database), set(needs(&Target::database(DB), &[A::ChangeStream, A::Find])));
    // Without a collection and with an ordinary first stage it needs nothing more.
    assert_eq!(of(&pipeline(Value::Int32(1), vec![stage("$match", empty())])), set(Vec::new()));
    let bare = doc(&[("aggregate", "c".into()), ("pipeline", arr(vec![Value::Int32(1)]))]);
    assert_eq!(of(&bare), set(needs(&c, &[A::Find])));
    let missing = doc(&[("aggregate", "c".into())]);
    assert_eq!(of(&missing), set(needs(&c, &[A::Find])));
}

#[test]
fn the_stages_that_read_or_write_other_collections_need_them() {
    let c = coll("c");
    let d = |f: &[(&str, Value)]| Value::Document(doc(f));
    let inner = arr(vec![d(&[("$lookup", d(&[("from", "deep".into())]))])]);
    let stages = vec![
        doc(&[("$lookup", d(&[("from", "l".into()), ("pipeline", inner)]))]),
        doc(&[("$graphLookup", d(&[("from", d(&[("db", "o".into()), ("coll", "g".into())]))]))]),
        doc(&[("$unionWith", "u".into())]),
        doc(&[("$unionWith", d(&[("coll", "v".into())]))]),
        doc(&[("$lookup", d(&[("from", d(&[("db", "o".into())]))]))]),
        doc(&[("$lookup", d(&[("from", Value::Int32(1))]))]),
        doc(&[("$facet", d(&[("f", arr(vec![d(&[("$unionWith", "w".into())])]))]))]),
        doc(&[("$out", "o1".into())]),
        doc(&[("$out", d(&[("db", "x".into()), ("coll", "o2".into())]))]),
        doc(&[("$merge", "m1".into())]),
        doc(&[("$merge", d(&[("into", "m2".into())]))]),
        doc(&[("$merge", d(&[("whenMatched", "keep".into())]))]),
        doc(&[("$out", Value::Int32(1))]),
    ];
    let mut cmd = pipeline("c".into(), stages);
    cmd.push("bypassDocumentValidation", true);
    let mut want = needs(&c, &[A::Find]);
    for t in ["l", "deep", "u", "v", "w"] {
        want.extend(needs(&coll(t), &[A::Find]));
    }
    want.extend(needs(&Target::collection("o", "g"), &[A::Find]));
    for (t, write) in [(coll("o1"), A::Remove), (Target::collection("x", "o2"), A::Remove)] {
        want.extend(needs(&t, &[A::Insert, write, A::BypassDocumentValidation]));
    }
    for t in [coll("m1"), coll("m2")] {
        want.extend(needs(&t, &[A::Insert, A::Update, A::BypassDocumentValidation]));
    }
    assert_eq!(of(&cmd), set(want));
}

#[test]
fn explain_needs_what_the_explained_command_needs() {
    let find = Value::Document(doc(&[("find", "c".into())]));
    assert_eq!(of(&doc(&[("explain", find)])), all(&coll("c"), &[A::Find]));
    assert_eq!(of(&doc(&[("explain", Value::Document(Document::new()))])), Check::Open);
    assert_eq!(of(&doc(&[("explain", Value::Int32(1))])), Check::Open);
}

#[test]
fn apply_ops_needs_its_own_action_and_each_entry_until_one_that_does_not_parse() {
    let d = |f: &[(&str, Value)]| Value::Document(doc(f));
    let o = || d(&[("x", 1.into())]);
    let entry = |op: &str, ns: &str| d(&[("op", op.into()), ("ns", ns.into()), ("o", o())]);
    let applied = |entries: Vec<Value>| of(&doc(&[("applyOps", arr(entries))]));
    assert_eq!(of(&doc(&[("applyOps", 1.into())])), Check::Open);
    assert_eq!(applied(vec![d(&[("x", 1.into())])]), Check::Open);
    assert_eq!(applied(Vec::new()), all(&Target::Any, &[A::AnyAction]));
    let with_ui = d(&[("op", "i".into()), ("ns", "a.c".into()), ("o", o()), ("ui", 1.into())]);
    let create_inner = d(&[("create", "n".into())]);
    let ping = d(&[("ping", 1.into())]);
    let more = d(&[("getMore", Value::Int64(1))]);
    let got = applied(vec![
        with_ui,
        entry("u", "a.c"),
        entry("d", "nodot"),
        entry("n", ""),
        d(&[("op", "c".into()), ("ns", "b.$cmd".into()), ("o", create_inner)]),
        d(&[("op", "c".into()), ("ns", "".into()), ("o", ping)]),
        d(&[("op", "c".into()), ("ns", "".into()), ("o", more)]),
        entry("x", "a.c"),
        entry("i", "never.read"),
    ]);
    let a = Target::collection("a", "c");
    let want = vec![
        Requirement::all(&Target::Cluster, &[A::ApplyOps]),
        Requirement::all(&a, &[A::Insert]),
        Requirement::all(&Target::Cluster, &[A::UseUUID]),
        Requirement::all(&a, &[A::Update]),
        Requirement::all(&Target::collection("nodot", ""), &[A::Remove]),
        Requirement::all(&Target::Cluster, &[A::AppendOplogNote]),
        Requirement::either(
            needs(&Target::collection("b", "n"), &[A::CreateCollection]),
            needs(&Target::collection("b", "n"), &[A::Insert]),
        ),
        Requirement::all(&Target::Any, &[A::AnyAction]),
    ];
    assert_eq!(got, Check::Needs(want));
    // An entry without `ns` or without `o` ends the list too.
    let short = applied(vec![d(&[("op", "i".into())]), entry("i", "a.c")]);
    assert_eq!(short, Check::Needs(vec![Requirement::all(&Target::Cluster, &[A::ApplyOps])]));
    let no_o = applied(vec![d(&[("op", "i".into()), ("ns", "a.c".into())])]);
    assert_eq!(no_o, Check::Needs(vec![Requirement::all(&Target::Cluster, &[A::ApplyOps])]));
    let empty_cmd = applied(vec![d(&[("op", "c".into()), ("ns", "a".into()), ("o", d(&[]))])]);
    assert_eq!(empty_cmd, Check::Needs(vec![Requirement::all(&Target::Cluster, &[A::ApplyOps])]));
}

#[test]
fn map_reduce_create_and_rename_need_their_sources_and_targets() {
    let c = coll("c");
    let mut out = needs(&c, &[A::Find]);
    out.extend(needs(&coll("o"), &[A::Insert, A::Remove]));
    let to_coll = doc(&[("mapReduce", "c".into()), ("out", "o".into())]);
    assert_eq!(of(&to_coll), Check::Needs(vec![Requirement { options: vec![out] }]));
    let mut merged = needs(&c, &[A::Find]);
    merged.extend(needs(&Target::collection("x", "m"), &[A::Insert, A::Remove]));
    let out_doc = Value::Document(doc(&[("merge", "m".into()), ("db", "x".into())]));
    let to_db = doc(&[("mapReduce", "c".into()), ("out", out_doc)]);
    assert_eq!(of(&to_db), Check::Needs(vec![Requirement { options: vec![merged] }]));
    let inline = doc(&[("mapReduce", "c".into()), ("out", Value::Int32(1))]);
    assert_eq!(
        of(&inline),
        Check::Needs(vec![Requirement { options: vec![needs(&c, &[A::Find])] }])
    );

    let view = doc(&[("create", "v".into()), ("viewOn", "c".into())]);
    assert_eq!(of(&view), all(&coll("v"), &[A::CreateCollection]));
    let capped = doc(&[("create", "k".into()), ("capped", true.into())]);
    let k = coll("k");
    assert_eq!(
        of(&capped),
        Check::Needs(vec![Requirement::either(
            needs(&k, &[A::CreateCollection, A::ConvertToCapped]),
            needs(&k, &[A::Insert, A::ConvertToCapped]),
        )])
    );
    let plain = doc(&[("create", "p".into())]);
    let p = coll("p");
    assert_eq!(
        of(&plain),
        Check::Needs(vec![Requirement::either(
            needs(&p, &[A::CreateCollection]),
            needs(&p, &[A::Insert]),
        )])
    );

    let (from, to) = (Target::collection("a", "x"), Target::collection("a", "y"));
    let mut copy = needs(&from, &[A::Find, A::DropCollection]);
    copy.extend(needs(&to, &[A::Insert, A::CreateIndex]));
    let same = doc(&[
        ("renameCollection", "a.x".into()),
        ("to", "a.y".into()),
        ("dropTarget", true.into()),
    ]);
    assert_eq!(
        on("admin", &same),
        Check::Needs(vec![
            Requirement::either(needs(&Target::database("a"), &[A::RenameCollectionSameDB]), copy),
            Requirement::all(&to, &[A::DropCollection]),
        ])
    );
    let other = Target::collection("b", "y");
    let mut across = needs(&from, &[A::Find, A::DropCollection]);
    across.extend(needs(&other, &[A::Insert, A::CreateIndex]));
    let between = doc(&[("renameCollection", "a.x".into()), ("to", "b.y".into())]);
    assert_eq!(on("admin", &between), Check::Needs(vec![Requirement { options: vec![across] }]));
}

#[test]
fn listings_profiles_and_diagnostics_need_their_privileges() {
    let all_dbs = doc(&[("listDatabases", 1.into()), ("authorizedDatabases", false.into())]);
    assert_eq!(of(&all_dbs), all(&Target::Cluster, &[A::ListDatabases]));
    assert_eq!(of(&doc(&[("listDatabases", 1.into())])), Check::Open);
    let names = doc(&[
        ("listCollections", 1.into()),
        ("nameOnly", true.into()),
        ("authorizedCollections", true.into()),
    ]);
    assert_eq!(of(&names), Check::InDatabase(DB.into()));
    let listed = doc(&[("listCollections", 1.into()), ("nameOnly", true.into())]);
    assert_eq!(of(&listed), all(&Target::database(DB), &[A::ListCollections]));

    let enable = needs(&Target::database(DB), &[A::EnableProfiler]);
    let read = needs(&coll("system.profile"), &[A::Find]);
    let read_only = Check::Needs(vec![Requirement::either(enable.clone(), read)]);
    for level in [Value::Int32(-1), Value::Int64(-1), Value::Double(-1.0)] {
        assert_eq!(of(&doc(&[("profile", level)])), read_only);
    }
    for level in [Value::Int32(1), Value::from("x")] {
        let set_level = Check::Needs(vec![Requirement { options: vec![enable.clone()] }]);
        assert_eq!(of(&doc(&[("profile", level)])), set_level);
    }

    let clone = doc(&[("cloneCollectionAsCapped", "c".into()), ("toCollection", "k".into())]);
    assert_eq!(
        of(&clone),
        Check::Needs(vec![
            Requirement::all(&coll("c"), &[A::Find]),
            Requirement::all(&coll("k"), &[A::Insert, A::CreateIndex, A::ConvertToCapped]),
        ])
    );
    let meta = |db: &str, first: Value| on(db, &doc(&[("checkMetadataConsistency", first)]));
    assert_eq!(meta(DB, "c".into()), all(&coll("c"), &[A::CheckMetadataConsistency]));
    assert_eq!(meta("admin", 1.into()), all(&Target::Cluster, &[A::CheckMetadataConsistency]));
    assert_eq!(
        meta(DB, 1.into()),
        Check::Needs(vec![Requirement::either(
            needs(&Target::Cluster, &[A::CheckMetadataConsistency]),
            needs(&Target::database(DB), &[A::CheckMetadataConsistency]),
        )])
    );
    let local = Requirement::all(&Target::database("local"), &[A::CollStats]);
    let diag = Requirement::all(
        &Target::Cluster,
        &[A::ServerStatus, A::ReplSetGetStatus, A::ConnPoolStats],
    );
    assert_eq!(
        of(&doc(&[("getDiagnosticData", 1.into())])),
        Check::Needs(vec![diag, local.clone()])
    );
    let ftdc = Requirement::all(&Target::Cluster, &[A::ServerStatus, A::ReplSetGetStatus]);
    assert_eq!(of(&doc(&[("rotateFTDC", 1.into())])), Check::Needs(vec![ftdc, local]));
    let zone = |other: Vec<Need>| {
        Check::Needs(vec![Requirement::either(
            needs(&Target::Cluster, &[A::EnableSharding]),
            other,
        )])
    };
    assert_eq!(
        of(&doc(&[("addShardToZone", "s".into())])),
        zone(needs(&Target::database("config"), &[A::Update]))
    );
    assert_eq!(
        of(&doc(&[("removeShardFromZone", "s".into())])),
        zone(needs(&Target::Any, &[A::Find, A::Update]))
    );
    assert_eq!(
        of(&doc(&[("updateZoneKeyRange", "a.c".into())])),
        zone(needs(&Target::Any, &[A::Find, A::Update, A::Remove]))
    );
}

#[test]
fn user_and_role_commands_need_grant_role_on_what_they_grant() {
    let d = |f: &[(&str, Value)]| Value::Document(doc(f));
    let roles =
        arr(vec!["r1".into(), d(&[("role", "r2".into()), ("db", "other".into())]), 1.into()]);
    let privileges = arr(vec![
        d(&[("resource", d(&[("db", "p".into()), ("collection", "".into())]))]),
        d(&[("resource", d(&[("cluster", true.into())]))]),
        d(&[("resource", d(&[("db", "".into())]))]),
        d(&[("actions", arr(Vec::new()))]),
        1.into(),
    ]);
    let user = doc(&[
        ("createUser", "u".into()),
        ("roles", roles.clone()),
        ("authenticationRestrictions", arr(Vec::new())),
    ]);
    let here = Target::database(DB);
    assert_eq!(
        of(&user),
        set(vec![
            Need::new(here.clone(), A::CreateUser),
            Need::new(here.clone(), A::GrantRole),
            Need::new(Target::database("other"), A::GrantRole),
            Need::new(here.clone(), A::SetAuthenticationRestriction),
        ])
    );
    let role = doc(&[
        ("createRole", "r".into()),
        ("privileges", privileges.clone()),
        ("roles", arr(Vec::new())),
    ]);
    assert_eq!(
        of(&role),
        set(vec![
            Need::new(here.clone(), A::CreateRole),
            Need::new(Target::database("admin"), A::GrantRole),
            Need::new(Target::database("p"), A::GrantRole),
        ])
    );
    let update =
        doc(&[("updateRole", "r".into()), ("authenticationRestrictions", arr(Vec::new()))]);
    assert_eq!(
        of(&update),
        set(vec![
            Need::new(Target::AnyNormal, A::RevokeRole),
            Need::new(here.clone(), A::SetAuthenticationRestriction),
        ])
    );
    let plain_update = doc(&[("updateRole", "r".into())]);
    assert_eq!(of(&plain_update), set(vec![Need::new(Target::AnyNormal, A::RevokeRole)]));
    for (name, action) in [
        ("grantRolesToUser", A::GrantRole),
        ("grantRolesToRole", A::GrantRole),
        ("revokeRolesFromUser", A::RevokeRole),
        ("revokeRolesFromRole", A::RevokeRole),
    ] {
        let cmd = doc(&[(name, "u".into()), ("roles", roles.clone())]);
        let want = set(vec![
            Need::new(here.clone(), action),
            Need::new(Target::database("other"), action),
        ]);
        assert_eq!(of(&cmd), want, "{name}");
    }
    for (name, action) in
        [("grantPrivilegesToRole", A::GrantRole), ("revokePrivilegesFromRole", A::RevokeRole)]
    {
        let cmd = doc(&[(name, "r".into()), ("privileges", privileges.clone())]);
        let want = set(vec![
            Need::new(Target::database("admin"), action),
            Need::new(Target::database("p"), action),
        ]);
        assert_eq!(of(&cmd), want, "{name}");
    }
    let no_privileges = doc(&[("grantPrivilegesToRole", "r".into())]);
    assert_eq!(of(&no_privileges), set(Vec::new()));
}

#[test]
fn a_user_may_change_its_own_password_and_custom_data() {
    let here = Target::database(DB);
    let changes = |name: &str| {
        doc(&[
            ("updateUser", name.into()),
            ("pwd", "x".into()),
            ("customData", Value::Document(Document::new())),
            ("roles", arr(Vec::new())),
            ("authenticationRestrictions", arr(Vec::new())),
        ])
    };
    let rest = [
        Requirement::all(&Target::AnyNormal, &[A::GrantRole, A::RevokeRole]),
        Requirement::all(&here, &[A::SetAuthenticationRestriction]),
    ];
    let mut mine = vec![
        Requirement::either(
            needs(&here, &[A::ChangePassword]),
            needs(&here, &[A::ChangeOwnPassword]),
        ),
        Requirement::either(
            needs(&here, &[A::ChangeCustomData]),
            needs(&here, &[A::ChangeOwnCustomData]),
        ),
    ];
    mine.extend(rest.clone());
    assert_eq!(of(&changes("ann")), Check::Needs(mine));
    let mut theirs = vec![
        Requirement::all(&here, &[A::ChangePassword]),
        Requirement::all(&here, &[A::ChangeCustomData]),
    ];
    theirs.extend(rest);
    assert_eq!(of(&changes("bob")), Check::Needs(theirs));
    assert_eq!(of(&doc(&[("updateUser", "bob".into())])), Check::Needs(Vec::new()));
}

#[test]
fn users_info_and_roles_info_are_open_for_oneself_and_need_view_otherwise() {
    let d = |f: &[(&str, Value)]| Value::Document(doc(f));
    let here = Target::database(DB);
    let view = |db: &str| set(vec![Need::new(Target::database(db), A::ViewUser)]);
    assert_eq!(of(&doc(&[("usersInfo", "ann".into())])), Check::Open);
    assert_eq!(of(&doc(&[("usersInfo", "bob".into())])), view(DB));
    let other = d(&[("user", "bob".into()), ("db", "x".into())]);
    assert_eq!(of(&doc(&[("usersInfo", other)])), view("x"));
    // A list that names someone else needs `viewUser` on the database of every name, its own too.
    let both = arr(vec!["ann".into(), d(&[("user", "bob".into()), ("db", "x".into())])]);
    assert_eq!(
        of(&doc(&[("usersInfo", both)])),
        set(vec![
            Need::new(here.clone(), A::ViewUser),
            Need::new(Target::database("x"), A::ViewUser)
        ])
    );
    assert_eq!(of(&doc(&[("usersInfo", arr(Vec::new()))])), set(Vec::new()));
    assert_eq!(of(&doc(&[("usersInfo", 1.into())])), all(&here, &[A::ViewUser]));
    let broken = arr(vec![d(&[("user", "bob".into())])]);
    assert_eq!(of(&doc(&[("usersInfo", broken)])), all(&here, &[A::ViewUser]));
    let every = d(&[("forAllDBs", true.into())]);
    assert_eq!(of(&doc(&[("usersInfo", every)])), all(&Target::Cluster, &[A::ViewUser]));
    // With a mechanism, `internal` on the cluster too; with a name that is no mechanism, nothing more.
    let internal = Requirement::all(&Target::Cluster, &[A::Internal]);
    let mine =
        doc(&[("usersInfo", "ann".into()), ("authenticatedMechanism", "SCRAM-SHA-256".into())]);
    assert_eq!(of(&mine), Check::Needs(vec![internal.clone()]));
    let theirs =
        doc(&[("usersInfo", "bob".into()), ("authenticatedMechanism", "SCRAM-SHA-256".into())]);
    let Check::Needs(reqs) = of(&theirs) else { panic!("needs") };
    assert_eq!(reqs.last(), Some(&internal));
    let odd = doc(&[("usersInfo", "bob".into()), ("authenticatedMechanism", "NOPE".into())]);
    assert_eq!(of(&odd), view(DB));
    // A role of the user's own is open to it.
    assert_eq!(of(&doc(&[("rolesInfo", "reader".into())])), Check::Open);
    let writer = d(&[("role", "writer".into()), ("db", DB.into())]);
    assert_eq!(of(&doc(&[("rolesInfo", writer)])), set(vec![Need::new(here.clone(), A::ViewRole)]));
    assert_eq!(of(&doc(&[("rolesInfo", 1.into())])), all(&here, &[A::ViewRole]));
}

#[test]
fn merging_the_authorization_collections_needs_every_user_and_role_action() {
    let mut list =
        needs(&Target::AnyNormal, &[A::CreateUser, A::CreateRole, A::GrantRole, A::RevokeRole]);
    let plain = doc(&[("_mergeAuthzCollections", 1.into())]);
    assert_eq!(of(&plain), set(list.clone()));
    list.extend(needs(&Target::AnyNormal, &[A::DropUser, A::DropRole]));
    list.extend(needs(&Target::collection("admin", "tu"), &[A::Find]));
    list.extend(needs(&Target::collection("admin", "tr"), &[A::Find]));
    let dropping = doc(&[
        ("_mergeAuthzCollections", 1.into()),
        ("drop", true.into()),
        ("tempUsersCollection", "admin.tu".into()),
        ("tempRolesCollection", "admin.tr".into()),
    ]);
    assert_eq!(of(&dropping), set(list));
}
