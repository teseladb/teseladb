//! The nodes a scenario starts with a configuration of its own (a TLS HTTP port, a root user,
//! small memtables), each in a process of its own (one node per process, D4, 023 FR-034): this
//! binary with [`NODE_PROCESS`] and the scenario's recipe runs the node; the scenario holds its
//! [`NodeProc`]. The local clusters without a recipe are `teseladb dev`'s members
//! ([`crate::client::Target`]).

use std::path::Path;
use std::time::Duration;

use teseladb_server::node::NodeConfig;
use teseladb_server::procs::{NodeArgs, NodeProc, log_path, run_node_process};

use crate::client::{program_dir, text};
use crate::scenarios::{disk_usage, noisy_tenant, rest};

pub use teseladb_server::procs::NODE_PROCESS;

/// This program's name: the node processes are this binary.
const PROGRAM: &str = "teseladb-bench";
/// Exit code of a node process with a recipe it does not know or cannot build.
const USAGE: i32 = 2;

/// The recipes, by name: what each makes of its arguments.
const RECIPES: &[(&str, fn(&NodeArgs) -> Result<NodeConfig, String>)] = &[
    (rest::RECIPE, rest::node_config),
    (noisy_tenant::RECIPE, noisy_tenant::node_config),
    (disk_usage::RECIPE, disk_usage::node_config),
];

/// The configuration of recipe `a.recipe`.
///
/// # Errors
///
/// An unknown recipe, or one that cannot be built.
pub fn config(a: &NodeArgs) -> Result<NodeConfig, String> {
    let (_, make) = RECIPES
        .iter()
        .find(|(name, _)| *name == a.recipe)
        .ok_or_else(|| format!("unknown recipe {}", a.recipe))?;
    make(a)
}

/// Runs a node process: `args` follow [`NODE_PROCESS`]. Returns its exit code.
pub fn run_process(args: &[String]) -> i32 {
    let made = NodeArgs::parse(args).and_then(|a| config(&a));
    match made {
        Ok(cfg) => run_node_process(args, &|_: &NodeArgs| cfg.clone()),
        Err(e) => {
            println!("error usage {e}");
            USAGE
        }
    }
}

/// Starts the node of `recipe` in a process of its own, joining `join` (none: it creates its
/// cluster), on the files of `dir` (none: in memory), with `params`; it serves within `ready` or
/// it is stopped and the error is `failure`.
///
/// # Errors
///
/// When it does not start or does not serve in time.
pub fn start(
    recipe: &str,
    join: &[String],
    dir: Option<&Path>,
    params: &[String],
    ready: Duration,
    failure: &str,
) -> Result<NodeProc, String> {
    let a = NodeArgs {
        recipe: recipe.to_owned(),
        join: join.to_vec(),
        dir: dir.map(Path::to_owned),
        params: params.to_vec(),
        ports: None,
    };
    let program = program_dir()?.join(PROGRAM);
    let node = NodeProc::spawn(&program, &a.to_args(), Some(&log_path(recipe))).map_err(text)?;
    if node.wait_ready(ready) {
        return Ok(node);
    }
    node.stop();
    Err(failure.to_owned())
}

/// A recipe's parameter `name`, which it needs.
///
/// # Errors
///
/// When it is missing.
pub fn param<'a>(a: &'a NodeArgs, name: &str) -> Result<&'a str, String> {
    a.param(name).ok_or_else(|| format!("recipe {} needs --param {name}=…", a.recipe))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn args(recipe: &str, params: &[&str]) -> NodeArgs {
        NodeArgs {
            recipe: recipe.to_owned(),
            params: params.iter().map(|p| (*p).to_owned()).collect(),
            ..NodeArgs::default()
        }
    }

    #[test]
    fn every_recipe_makes_a_node_of_its_scenario() {
        let rest = config(&args(rest::RECIPE, &["now=1800000000"])).unwrap();
        assert!(rest.http.as_ref().is_some_and(|h| h.tls.is_some()), "HTTP over TLS");
        assert!(rest.client.is_some());
        let noisy = config(&args(noisy_tenant::RECIPE, &[])).unwrap();
        assert!(noisy.root_user.is_some(), "a root user");
        assert!(noisy.client.as_ref().is_some_and(|c| c.auth), "authentication");
        let disk = config(&args(disk_usage::RECIPE, &[])).unwrap();
        assert_eq!(disk.cores, Some(1));
    }

    #[test]
    fn an_unknown_recipe_or_a_missing_parameter_is_refused() {
        assert_eq!(config(&args("none", &[])).unwrap_err(), "unknown recipe none");
        let missing = config(&args(rest::RECIPE, &[])).unwrap_err();
        assert!(missing.contains("--param now="), "{missing}");
        assert_eq!(run_process(&["none".to_owned()]), USAGE);
        assert_eq!(run_process(&[]), USAGE);
    }

    #[test]
    fn a_node_not_ready_in_time_is_stopped_with_the_error() {
        let err = start(disk_usage::RECIPE, &[], None, &[], Duration::ZERO, "late").unwrap_err();
        assert_eq!(err, "late");
        let node =
            start(disk_usage::RECIPE, &[], None, &[], Duration::from_secs(30), "late").unwrap();
        assert!(!node.client_addrs().is_empty());
        node.stop();
    }
}
