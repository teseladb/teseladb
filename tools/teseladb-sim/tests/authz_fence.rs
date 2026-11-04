//! `authz-fence` (013 FR-012, D-073): the fence's messages that reach a node restarted, cut off or
//! frozen since they left, and the check after the heal with a node down; and whole seeds under
//! the fault plans.

use teseladb_raftlog::Completions;
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::Workload;
use teseladb_sim::workloads::authz_fence::AuthzFence;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_seed};
use teseladb_types::Nanos;

/// The first client number that carries a message (the admin and one turn per node come first).
const FIRST_MESSAGE: u32 = 1_000;

/// What the test does to the world just before a message arrives.
type Before = Box<dyn FnMut(&mut World, &mut AuthzFence)>;

/// Runs the world's events until `until`; before each of the first messages to arrive, the next
/// action of `before`. Returns how many actions ran.
fn run(w: &mut World, wl: &mut AuthzFence, until: Nanos, mut before: Vec<Before>) -> usize {
    before.reverse();
    let mut ran = 0;
    while let Some((at, _, ev)) = w.pop() {
        if at > until {
            break;
        }
        match ev {
            Event::Client { client } => {
                if client >= FIRST_MESSAGE
                    && let Some(mut act) = before.pop()
                {
                    act(w, wl);
                    ran += 1;
                }
                wl.on_client(w, client);
            }
            Event::Disk { node, core, life } => {
                if let Some(c) = w.on_disk(node, core, life) {
                    wl.on_completions(w, node, core, c);
                }
            }
            Event::Tick { node, core, life } => {
                if let Some(c) = w.on_tick(node, core, life) {
                    wl.on_completions(w, node, core, c);
                }
            }
            Event::Deliver { from, to, core, epoch, frame } => {
                w.on_deliver(from, to, core, epoch, &frame);
                wl.on_completions(w, to, core, Completions::default());
            }
            Event::Restart { node } => match w.start_recheck(node) {
                Some(at) => w.schedule(at, Event::Restart { node }),
                None => w.start_node(node).expect("a healthy disk"),
            },
            _ => {}
        }
    }
    ran
}

fn crash_and_restart(w: &mut World, wl: &mut AuthzFence) {
    for n in 0..3 {
        wl.on_crash(w, n);
        w.crash_node(n);
        w.start_node(n).expect("a healthy disk");
        wl.on_restart(w, n).expect("nothing to check on a restart");
    }
}

fn cut_everyone(w: &mut World, _: &mut AuthzFence) {
    w.net.partition(&[vec![0], vec![1], vec![2]]);
}

fn heal(w: &mut World, _: &mut AuthzFence) {
    w.net.heal();
}

fn freeze_everyone(w: &mut World, _: &mut AuthzFence) {
    let until = w.now() + Nanos::from_millis(100);
    for n in 0..3 {
        w.nodes[n].cores[0].frozen_until = until;
    }
}

/// Messages that reach a node restarted or cut off since they left are dropped, one that reaches
/// a frozen core waits for it, and the fence holds through all of them.
#[test]
fn messages_to_a_node_restarted_cut_off_or_frozen() {
    let mut wl = AuthzFence::new(3);
    let mut w = World::with_clocks(7, 3, 1, false, true, wl.clocks());
    wl.start(&mut w);
    let acts: Vec<Before> = vec![
        Box::new(crash_and_restart),
        Box::new(cut_everyone),
        Box::new(heal),
        Box::new(freeze_everyone),
    ];
    let ran = run(&mut w, &mut wl, Nanos::from_secs(10), acts);
    assert_eq!(ran, 4, "every action found a message on its way");
    assert!(wl.finish(&mut w).is_ok());
}

/// After the heal a node that is down is not asked to hold a lease; the others are.
#[test]
fn the_check_after_the_heal_leaves_out_a_node_that_is_down() {
    let mut wl = AuthzFence::new(3);
    let mut w = World::with_clocks(8, 3, 1, false, true, wl.clocks());
    wl.start(&mut w);
    run(&mut w, &mut wl, Nanos::from_secs(9), Vec::new());
    wl.on_heal(&mut w);
    wl.on_crash(&mut w, 2);
    w.crash_node(2);
    let end = w.now() + Nanos::from_secs(5);
    run(&mut w, &mut wl, end, Vec::new());
    assert!(wl.finish(&mut w).is_ok());
}

#[test]
fn seeds_pass_under_the_fault_plans() {
    for plan in [FaultPlan::None, FaultPlan::Default, FaultPlan::Heavy] {
        let cfg =
            SimConfig { workload: "authz-fence".into(), faults: plan, ..SimConfig::default() };
        for seed in 1..=4 {
            assert_eq!(run_seed(&cfg, seed).verdict, Verdict::Pass, "seed {seed} with {plan:?}");
        }
    }
}
