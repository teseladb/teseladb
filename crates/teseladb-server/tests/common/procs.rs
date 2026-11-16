//! The `main` of the tests whose nodes run in processes of their own (D4: one process per node).
//! Their binary has `harness = false` in `Cargo.toml`, as `cluster_local`: started with
//! [`NODE_PROCESS`] it is one node, made by one of the file's recipes
//! ([`teseladb_server::procs::run_node_process`]); otherwise it runs the file's tests as the
//! standard harness does (name filters, `--exact`, `--skip`, `--list`, `--test-threads`), each in
//! a thread of its own.
//!
//! A file declares it with `#[path = "common/procs.rs"] mod procs;`, and starts its nodes with
//! [`node`].

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use teseladb_server::node::NodeConfig;
use teseladb_server::procs::{NODE_PROCESS, NodeArgs, NodeProc, StartError, log_path};

/// A test: its name and its function.
pub type Test = (&'static str, fn());
/// A node configuration a test's processes take by name.
pub type Recipe = (&'static str, fn(&NodeArgs) -> NodeConfig);

/// Options of the standard harness that take a value.
const VALUE_OPTIONS: &[&str] =
    &["--test-threads", "--skip", "--format", "--color", "--logfile", "-Z", "--shuffle-seed"];

/// Runs this binary as a node process or as the tests of `tests`.
pub fn main(tests: &[Test], recipes: &[Recipe]) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(NODE_PROCESS) {
        let rest = &args[1..];
        let name = rest.first().map_or("", String::as_str);
        let Some((_, recipe)) = recipes.iter().find(|(n, _)| *n == name) else {
            println!("error usage no recipe `{name}`");
            std::process::exit(2);
        };
        std::process::exit(teseladb_server::procs::run_node_process(rest, recipe));
    }
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let (mut exact, mut list, mut ignored_only) = (false, false, false);
    let mut threads = thread::available_parallelism().map_or(1, usize::from);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_owned())),
            _ => (a.as_str(), None),
        };
        match flag {
            "--exact" => exact = true,
            "--list" => list = true,
            "--ignored" => ignored_only = true,
            f if VALUE_OPTIONS.contains(&f) => {
                let value = inline.or_else(|| it.next().cloned()).unwrap_or_default();
                match f {
                    "--test-threads" => threads = value.parse().unwrap_or(threads).max(1),
                    "--skip" => skips.push(value),
                    _ => {}
                }
            }
            f if f.starts_with('-') => {}
            name => filters.push(name.to_owned()),
        }
    }
    let matches = |name: &str| {
        let taken = filters.is_empty()
            || filters.iter().any(|f| if exact { name == f } else { name.contains(f.as_str()) });
        taken && !skips.iter().any(|s| name.contains(s.as_str()))
    };
    // None of these tests is ignored.
    let chosen: Vec<Test> = if ignored_only {
        Vec::new()
    } else {
        tests.iter().copied().filter(|(n, _)| matches(n)).collect()
    };
    if list {
        chosen.iter().for_each(|(n, _)| println!("{n}: test"));
        return;
    }
    println!("\nrunning {} tests", chosen.len());
    let queue = Arc::new(Mutex::new(chosen.iter().copied().collect::<VecDeque<Test>>()));
    let failed = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..threads.min(chosen.len().max(1)))
        .map(|_| {
            let (queue, failed) = (queue.clone(), failed.clone());
            thread::spawn(move || {
                while let Some((name, f)) =
                    queue.lock().unwrap_or_else(PoisonError::into_inner).pop_front()
                {
                    let ok = thread::Builder::new()
                        .name(name.to_owned())
                        .spawn(f)
                        .expect("a test thread")
                        .join()
                        .is_ok();
                    println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
                    if !ok {
                        failed.lock().unwrap_or_else(PoisonError::into_inner).push(name);
                    }
                }
            })
        })
        .collect();
    workers.into_iter().for_each(|w| w.join().expect("a worker"));
    let failed = failed.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let passed = chosen.len() - failed.len();
    if failed.is_empty() {
        println!("\ntest result: ok. {passed} passed; 0 failed; 0 ignored\n");
        return;
    }
    println!("\nfailures:");
    failed.iter().for_each(|n| println!("    {n}"));
    println!("\ntest result: FAILED. {passed} passed; {} failed; 0 ignored\n", failed.len());
    std::process::exit(101);
}

/// Starts this binary as a node process of `recipe` joining `join` (none: it creates the
/// cluster), in memory or, with `dir`, on files there.
pub fn spawn(
    recipe: &str,
    join: &[String],
    dir: Option<&Path>,
    params: &[&str],
) -> Result<NodeProc, StartError> {
    let a = NodeArgs {
        recipe: recipe.to_owned(),
        join: join.to_vec(),
        dir: dir.map(Path::to_owned),
        params: params.iter().map(|p| (*p).to_owned()).collect(),
        ports: None,
    };
    let me = std::env::current_exe().expect("this test's binary");
    NodeProc::spawn(&me, &a.to_args(), &log_path(recipe))
}

/// [`spawn`], which has to start.
pub fn node(recipe: &str, join: &[String], dir: Option<&Path>, params: &[&str]) -> NodeProc {
    spawn(recipe, join, dir, params).unwrap_or_else(|e| panic!("node `{recipe}`: {e}"))
}
