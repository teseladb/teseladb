//! Server-side JavaScript on each kind of thread the node runs it on (016 A016-3-18, 023 FR-018):
//! an engine thread of a shard, a thread already on a stack of its own, and a plain thread. With
//! the node's runner of deep stacks, recursion and the nesting of values end where a reference
//! router 9.0.4 ends them (measured on 02-11), whatever the stack of the thread. Its own test
//! binary: the runner is set once for the process.
#![cfg(target_os = "linux")]

use teseladb_bson::Value;
use teseladb_exec::expr::js::JsEngine;
use teseladb_wire::commands::js::{COMMAND_STACK, NodeJs, set_stack};

/// Each `$function` body and what the reference router gives for it.
const CASES: &[(&str, &str)] = &[
    // 51 000 frames: the program's function and 50 999 nested calls.
    (
        "function(){ let n = 0; function f(){ n++; f() } try { f() } catch (e) { return n + ' ' + e.message } }",
        "50999 too much recursion",
    ),
    // The shell prints an object nested deeper than 100 levels with `[Object]` below them.
    (
        "function(){ let o = {}; for (let i = 0; i < 300; i++) { o = {a: o} } return String(tojson(o).length) }",
        "11219",
    ),
    (
        "function(){ const t = (d) => { let a = []; for (let i = 0; i < d; i++) a = [a]; try { return JSON.stringify(a).length } catch (e) { return e.message } }; return t(346) + ' ' + t(347) }",
        "694 too much recursion",
    ),
];

fn answers() -> Vec<String> {
    CASES
        .iter()
        .map(|(body, _)| match NodeJs.call(&Value::String((*body).into()), &[]) {
            Ok(Value::String(s)) => s,
            other => format!("{other:?}"),
        })
        .collect()
}

fn want() -> Vec<String> {
    CASES.iter().map(|(_, w)| (*w).to_owned()).collect()
}

fn with_runner() {
    teseladb_server::client_listener::set_deep_stack();
}

#[test]
fn on_an_engine_thread_of_a_shard_calls_go_as_deep_as_on_the_reference() {
    use teseladb_io::engine::stacks;
    use teseladb_server::client_listener::ENGINE_COMMAND_STACK;
    use teseladb_server::{engine_jobs, shard_client::block_on};

    with_runner();
    stacks::set_stack_size(ENGINE_COMMAND_STACK);
    let got = block_on(|| {
        engine_jobs::run(|| {
            // As a client session does before it runs a command.
            set_stack(stacks::stack_size());
            answers()
        })
    })
    .expect("the engine ran the job");
    assert_eq!(got, want());
}

#[test]
fn on_a_stack_of_its_own_a_call_has_the_room_left_there() {
    with_runner();
    let got = std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(|| {
            let mut got = None;
            teseladb_sys::deep_stack::run(COMMAND_STACK, &mut |_| {
                // The calls run where they are, on this stack, not on a new one.
                got = Some(answers());
            });
            got
        })
        .expect("spawned")
        .join()
        .expect("joined");
    assert_eq!(got, Some(want()));
}

#[test]
fn on_a_plain_thread_with_a_small_stack_calls_go_as_deep_as_on_the_reference() {
    with_runner();
    let got = std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(answers)
        .expect("spawned")
        .join()
        .expect("joined");
    assert_eq!(got, want());
}
