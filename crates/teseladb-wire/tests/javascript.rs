//! Server-side JavaScript in the commands (016 T011, T012): `$where` in `find`, `$function` in
//! `aggregate` and `$accumulator` in `$group`, with the results and errors of a `refrouter` 9.0.4
//! (measured on 02-11).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "js";

fn load(m: &mut Mem) {
    let r = m.run(
        DB,
        extjson::parse(
            r#"{"insert":"c","documents":[{"_id":0,"k":"a","v":1,"precio":10,"coste":4},{"_id":1,"k":"a","v":2,"precio":10,"coste":6},{"_id":2,"k":"b","v":5,"precio":3,"coste":1}]}"#,
        )
        .expect("command"),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run(DB, extjson::parse(cmd).expect("command"))
}

/// The `_id`s of the first batch, in order: a collection's natural order is its key order
/// (DV-060), not RefDb's insertion order.
fn ids(r: &Document) -> Vec<i32> {
    let mut out: Vec<i32> = match field(r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|d| match d {
                Value::Document(d) => match d.get("_id") {
                    Some(Value::Int32(n)) => *n,
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?} in {r:?}"),
    };
    out.sort_unstable();
    out
}

fn first(r: &Document) -> Document {
    match field(r, "cursor.firstBatch.0") {
        Value::Document(d) => d.clone(),
        other => panic!("{other:?} in {r:?}"),
    }
}

fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("{r:?}"),
    }
}

#[test]
fn where_takes_a_string_or_a_function_and_the_document_as_this_and_obj() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"this.precio > this.coste * 2"}}"#);
    assert_eq!(ids(&r), [0, 2]);
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"$where":{"$code":"function(){ return obj._id < 2 }"},"k":"a"}}"#,
    );
    assert_eq!(ids(&r), [0, 1]);
}

#[test]
fn where_errors_are_those_of_a_refrouter() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"this._id <"}}"#);
    assert_eq!(error(&r), (6_108_306, "$where compile error".into()));
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"throw new Error('boom')"}}"#);
    let (code, msg) = error(&r);
    assert_eq!(code, 139, "{msg}");
    assert!(msg.contains("Failed to invoke JS predicate :: function id = "), "{msg}");
    assert!(msg.contains("e-runtime message : 'boom'"), "{msg}");
}

#[test]
fn function_runs_in_a_pipeline_and_keeps_bson_types() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$match":{"_id":0}},{"$project":{"_id":0,"v":{"$function":{"body":"function(a, b){ return [a * b, NumberLong(7), NumberInt(3), undefined] }","args":["$v","$precio"],"lang":"js"}}}}],"cursor":{}}"#,
    );
    assert_eq!(
        first(&r).get("v"),
        Some(&Value::Array(vec![
            Value::Double(10.0),
            Value::Int64(7),
            Value::Int32(3),
            Value::Null
        ]))
    );
}

#[test]
fn function_errors_are_those_of_a_refrouter() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$project":{"v":{"$function":{"body":"function(){ throw new Error('boom') }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    let (code, msg) = error(&r);
    assert_eq!(code, 139, "{msg}");
    assert!(msg.contains("Failed to invoke JS function :: function id = "), "{msg}");
    assert!(msg.contains("e-runtime message : 'boom'"), "{msg}");
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$project":{"v":{"$function":{"body":"function(){ return Symbol('a') }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    let (code, msg) = error(&r);
    assert_eq!(code, 16_662, "{msg}");
    assert!(
        msg.contains("unable to convert JavaScript property to ref element __returnValue"),
        "{msg}"
    );
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$project":{"v":{"$function":{"body":"function(){ return  1 + }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    let (code, msg) = error(&r);
    assert_eq!(code, 139, "{msg}");
    assert!(msg.contains("Failed to create JS function :: source = function(){ return  1 + }: e-compile message : "), "{msg}");
}

/// A `Timestamp` whose `t` or `i` is not right cannot be written (measured on a `refrouter` 9.0.4
/// on 02-11): `BadValue`, or a code of its own when the field is missing.
#[test]
fn a_malformed_timestamp_fails_with_the_codes_of_a_refrouter() {
    let mut m = Mem::new();
    load(&mut m);
    for (body, want, text) in [
        (
            "const t = Timestamp(1, 2); t.t = 'x'; return t",
            2,
            "Timestamp time (seconds) must be a number",
        ),
        (
            "const t = Timestamp(1, 2); delete t.i; return t",
            6_900_901,
            "missing increment field, 'i'",
        ),
        ("return {t: Timestamp.prototype}", 6_900_900, "missing timestamp field, 't'"),
        ("return ObjectId.prototype", 2, "Can't call getOID on OID prototype"),
    ] {
        let cmd = format!(
            r#"{{"aggregate":"c","pipeline":[{{"$project":{{"v":{{"$function":{{"body":"function(){{ {body} }}","args":[],"lang":"js"}}}}}}}}],"cursor":{{}}}}"#
        );
        let (code, msg) = error(&run(&mut m, &cmd));
        assert_eq!(code, want, "{msg}");
        assert!(msg.contains(text), "{msg}");
    }
}

#[test]
fn accumulator_groups_with_init_accumulate_merge_and_finalize() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$group":{"_id":"$k","s":{"$accumulator":{"init":"function(){ return {n: 0} }","accumulate":"function(st, v){ st.n += v; return st }","accumulateArgs":["$v"],"merge":"function(a, b){ return {n: a.n + b.n} }","finalize":"function(s){ return s.n }","lang":"js"}}}},{"$sort":{"_id":1}}],"cursor":{}}"#,
    );
    let Value::Array(groups) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let want = vec![
        Value::Document(Document::new().with("_id", "a").with("s", 3.0)),
        Value::Document(Document::new().with("_id", "b").with("s", 5.0)),
    ];
    assert_eq!(groups, &want);
}

/// Measured on a `refnode` 9.0.4 started with `--noscripting` on 02-11.
#[test]
fn without_javascript_each_use_fails_as_on_a_node_with_noscripting() {
    let mut m = Mem::new();
    load(&mut m);
    teseladb_wire::commands::lock(&m.shared).params.javascript_enabled = false;
    let r = m.run(
        DB,
        Document::new().with("find", "c").with("filter", doc(&[("$where", "true".into())])),
    );
    assert_eq!(error(&r), (6_108_304, "no globalScriptEngine in $where parsing".into()));
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$project":{"v":{"$function":{"body":"function(){ return 1 }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    assert_eq!(
        error(&r),
        (
            31_264,
            "Executor error during aggregate command on namespace: js.c :: caused by :: Cannot run server-side javascript without the javascript engine enabled".into()
        )
    );
}

fn test_clock_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    u64::try_from(START.get_or_init(std::time::Instant::now).elapsed().as_millis())
        .unwrap_or(u64::MAX)
}

/// Measured on a `refrouter` 9.0.4 on 02-11: `while(true){}` with `maxTimeMS: 100` fails with 50
/// at about 150 ms, and the node goes on serving.
#[test]
fn an_endless_where_stops_at_max_time_ms() {
    teseladb_wire::commands::js::set_clock(test_clock_ms);
    let mut m = Mem::new();
    load(&mut m);
    let start = std::time::Instant::now();
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"while(true){}"},"maxTimeMS":100}"#);
    let (code, msg) = error(&r);
    assert_eq!(code, 50, "{msg}");
    assert_eq!(msg, "operation exceeded time limit");
    assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"this._id == 2"}}"#);
    assert_eq!(ids(&r), [2]);
}

fn test_sleep(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// `sleep(ms)` in a `$where` waits on the thread, as on a `refnode` 9.0.4: drivers' tests make slow
/// operations with it (PyRef's `delay`). With `maxTimeMS` it stops at the deadline with 50.
#[test]
fn sleep_in_a_where_waits_and_stops_at_max_time_ms() {
    teseladb_wire::commands::js::set_clock(test_clock_ms);
    teseladb_wire::commands::js::set_sleeper(test_sleep);
    let mut m = Mem::new();
    load(&mut m);
    let start = std::time::Instant::now();
    let slow = r#"{"find":"c","filter":{"_id":2,"$where":"function() { sleep(0.1 * 1000); return true; }"}}"#;
    assert_eq!(ids(&run(&mut m, slow)), [2]);
    assert!(start.elapsed() >= std::time::Duration::from_millis(100), "{:?}", start.elapsed());
    let start = std::time::Instant::now();
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"sleep(5000) || true"},"maxTimeMS":50}"#);
    let (code, msg) = error(&r);
    assert_eq!(code, 50, "{msg}");
    assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
}

/// Measured on a `refrouter` 9.0.4 on 02-11: a function stored in `system.js` is a global of the
/// database's JavaScript.
#[test]
fn functions_stored_in_system_js_are_globals() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        r#"{"insert":"system.js","documents":[{"_id":"cheap","value":{"$code":"function(d){ return d.precio < 5 }"}},{"_id":"limit","value":5}]}"#,
    );
    assert_eq!(r.get("n"), Some(&Value::Int32(2)), "{r:?}");
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"cheap(this) && this.v == limit"}}"#);
    assert_eq!(ids(&r), [2]);
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$match":{"_id":2}},{"$project":{"_id":0,"v":{"$function":{"body":"function(){ return cheap({precio: 1}) }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    assert_eq!(first(&r).get("v"), Some(&Value::Boolean(true)));
}

#[test]
fn a_promise_job_ends_the_call_with_the_trap_of_a_refrouter() {
    let mut m = Mem::new();
    load(&mut m);
    for body in [
        "function(){ try { Promise.resolve(1).then(() => 1) } catch (e) { return 'caught' } }",
        "function(){ new FinalizationRegistry(() => {}); return 1 }",
    ] {
        let cmd = format!(
            r#"{{"aggregate":"c","pipeline":[{{"$project":{{"v":{{"$function":{{"body":"{body}","args":[],"lang":"js"}}}}}}}}],"cursor":{{}}}}"#
        );
        let (code, msg) = error(&run(&mut m, &cmd));
        assert_eq!(code, 11_542_340, "{msg}");
        assert_eq!(
            msg,
            "Executor error during aggregate command on namespace: js.c :: caused by :: wasm trap: uninitialized element"
        );
    }
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"$where":"Promise.resolve(1).then(()=>1); return true"}}"#,
    );
    let (code, msg) = error(&r);
    assert_eq!(code, 11_542_340, "{msg}");
    assert!(msg.ends_with(":: caused by :: wasm trap: uninitialized element"), "{msg}");
}

#[test]
fn errors_carry_the_line_column_and_stack_of_a_refrouter() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$project":{"v":{"$function":{"body":"function(){ function g(){ throw new Error('q') } function h(){ g() } h() }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    let (code, msg) = error(&r);
    assert_eq!(code, 139, "{msg}");
    assert!(
        msg.ends_with(": e-runtime message : 'q', file : 'wasm:function', line : 1, column : 34\ng@wasm:function:1:34\nh@wasm:function:1:65\n@wasm:function:1:71\n"),
        "{msg}"
    );
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$project":{"v":{"$function":{"body":"function(){ throw {a:1, b:2} }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    let (_, msg) = error(&r);
    assert!(
        msg.ends_with(": e-runtime message : 'uncaught exception: [object Object]', file : 'wasm:function', line : 1, column : 26"),
        "{msg}"
    );
    let r = run(&mut m, r#"{"find":"c","filter":{"$where":"this.a.b.c"}}"#);
    let (code, msg) = error(&r);
    assert_eq!(code, 139, "{msg}");
    assert!(
        msg.contains(": e-runtime message : 'can't access property \"b\", this.a is undefined', file : 'wasm:function', line : 1, column : 15\n@wasm:function:1:15\n"),
        "{msg}"
    );
}

/// The depth a `$function` reaches on this thread.
fn depth_reached() -> String {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$match":{"_id":0}},{"$project":{"_id":0,"v":{"$function":{"body":"function(){ let n = 0; function f(){ n++; f() } try { f() } catch (e) { return n + ' ' + e.message } }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
    );
    match field(&r, "cursor.firstBatch.0.v") {
        Value::String(s) => s.clone(),
        other => panic!("{other:?} {r:?}"),
    }
}

/// A thread with the stack of a thread that runs commands gives the calls the depth of a
/// `refrouter` 9.0.4 (016 A016-3-18); any other thread ends deep recursion within its 1 MiB.
#[test]
fn a_command_thread_gives_the_calls_the_depth_of_a_refrouter() {
    use teseladb_wire::commands::js::{COMMAND_STACK, set_stack};
    let deep = std::thread::Builder::new()
        .stack_size(COMMAND_STACK)
        .spawn(|| {
            set_stack(COMMAND_STACK);
            depth_reached()
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(deep, "50999 too much recursion");
    let shallow = std::thread::spawn(depth_reached).join().unwrap();
    let (n, message) = shallow.split_once(' ').unwrap();
    assert_eq!(message, "too much recursion");
    assert!(n.parse::<u32>().unwrap() < 5_000, "{shallow}");
}
