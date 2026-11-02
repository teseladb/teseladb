//! Where each core runs (012 FR-006): core `k` is pinned to the `k`-th CPU of `--cpu-list` (by
//! default the CPUs the process may use, from its affinity and its cpuset) and its thread's
//! allocations go to the NUMA node of that CPU.

use std::path::Path;

/// How the node places its cores.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Affinity {
    /// The cores are not pinned (a node embedded in tests or tools).
    #[default]
    Off,
    /// On the CPUs the process may use, in order (`teseladb start` without `--cpu-list`).
    Allowed,
    /// On these CPUs, in order (`--cpu-list`).
    List(Vec<usize>),
}

/// The CPU of each of `cores` cores, given the CPUs the process may use. With the default list,
/// more cores than CPUs share them in turn; an explicit list shorter than the cores, or with a
/// CPU the process may not use, is a usage error.
pub fn core_cpus(
    a: &Affinity,
    cores: u16,
    allowed: &[usize],
) -> Result<Vec<Option<usize>>, String> {
    let n = usize::from(cores);
    match a {
        Affinity::Off => Ok(vec![None; n]),
        Affinity::Allowed if allowed.is_empty() => Ok(vec![None; n]),
        Affinity::Allowed => Ok((0..n).map(|k| Some(allowed[k % allowed.len()])).collect()),
        Affinity::List(list) => {
            if list.len() < n {
                return Err(format!(
                    "--cpu-list names {} CPUs for {n} cores: it needs one per core",
                    list.len()
                ));
            }
            if let Some(c) = list.iter().find(|c| !allowed.contains(c)) {
                return Err(format!("--cpu-list: the process may not run on CPU {c}"));
            }
            Ok(list[..n].iter().map(|c| Some(*c)).collect())
        }
    }
}

/// Where a core ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Its CPU.
    pub cpu: usize,
    /// Whether the system pinned the thread (macOS cannot).
    pub pinned: bool,
    /// The NUMA node of the CPU, when the system has NUMA information.
    pub node: Option<usize>,
    /// Whether the thread's allocations are bound to that node.
    pub local_memory: bool,
}

/// Pins the calling thread (a core's) to `cpu` and binds its allocations to that CPU's node.
pub fn place_current(cpu: usize) -> std::io::Result<Placement> {
    let pinned = teseladb_sys::poll::pin_to_cpu(cpu)?;
    let local_memory = pinned && teseladb_sys::numa::bind_memory_local()?;
    let node = teseladb_sys::numa::cpu_node(Path::new("/sys"), cpu);
    Ok(Placement { cpu, pinned, node, local_memory })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn core_k_runs_on_the_kth_cpu_of_the_list() {
        let allowed = [0, 1, 2, 3, 8, 9];
        assert_eq!(core_cpus(&Affinity::Off, 2, &allowed).unwrap(), vec![None, None]);
        let all = core_cpus(&Affinity::Allowed, 3, &allowed).unwrap();
        assert_eq!(all, vec![Some(0), Some(1), Some(2)]);
        let wrap = core_cpus(&Affinity::Allowed, 3, &[4, 5]).unwrap();
        assert_eq!(wrap, vec![Some(4), Some(5), Some(4)]);
        assert_eq!(core_cpus(&Affinity::Allowed, 1, &[]).unwrap(), vec![None]);
        let list = Affinity::List(vec![9, 8, 3]);
        assert_eq!(core_cpus(&list, 2, &allowed).unwrap(), vec![Some(9), Some(8)]);
        let short = core_cpus(&list, 4, &allowed).unwrap_err();
        assert!(short.contains("names 3 CPUs for 4 cores"), "{short}");
        let outside = core_cpus(&Affinity::List(vec![0, 5]), 2, &allowed).unwrap_err();
        assert!(outside.contains("CPU 5"), "{outside}");
    }

    #[test]
    fn a_thread_is_placed_on_an_allowed_cpu() {
        let cpu = *teseladb_sys::numa::allowed_cpus().unwrap().last().unwrap();
        let p = std::thread::spawn(move || place_current(cpu).unwrap()).join().unwrap();
        assert_eq!(p.cpu, cpu);
        assert_eq!(p.pinned, cfg!(target_os = "linux"));
        assert!(!p.local_memory || p.pinned);
    }
}
