//! The runner of a process's deep stacks (016 A016-3-18): a call of JavaScript runs through it,
//! and on the stack it gives the call has all of it, whatever stack the command's thread says it
//! has. Its own test binary: the runner is set once for the process.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Value, extjson};
use teseladb_wire::commands::js::{COMMAND_STACK, set_deep_stack, set_stack};

/// Stands for the process's runner: this test's thread already has `COMMAND_STACK`, so the call
/// runs where it is and is told it has all of it.
fn runner(call: &mut dyn FnMut(Option<usize>)) {
    call(Some(COMMAND_STACK));
}

#[test]
fn a_call_takes_the_depth_of_the_stack_the_runner_gives() {
    let reached = std::thread::Builder::new()
        .stack_size(COMMAND_STACK)
        .spawn(|| {
            set_deep_stack(runner);
            // The command's thread says it has a small stack, as an engine thread has.
            set_stack(16 << 20);
            let mut m = Mem::new();
            let one = Value::Document(doc(&[("_id", Value::Int32(0))]));
            let insert =
                doc(&[("insert", Value::String("c".into())), ("documents", Value::Array(vec![one]))]);
            let r = m.run("test", insert);
            assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
            let r = m.run(
                "test",
                extjson::parse(
                    r#"{"aggregate":"c","pipeline":[{"$project":{"_id":0,"v":{"$function":{"body":"function(){ let n = 0; function f(){ n++; f() } try { f() } catch (e) { return n + ' ' + e.message } }","args":[],"lang":"js"}}}}],"cursor":{}}"#,
                )
                .expect("command"),
            );
            match field(&r, "cursor.firstBatch.0.v") {
                Value::String(s) => s.clone(),
                other => panic!("{other:?} {r:?}"),
            }
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(reached, "50999 too much recursion");
}
