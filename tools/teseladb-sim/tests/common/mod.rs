//! Helpers shared by the simulator's integration tests: a workload wrapped with hooks before it
//! starts and when it finishes, and a loop that runs a cluster world's events.

#![allow(dead_code)]

use teseladb_sim::checkers::Violation;
use teseladb_sim::clock::ClockLimits;
use teseladb_sim::workloads::Workload;
use teseladb_sim::world::{Event, World};
use teseladb_raftlog::Completions;

/// Hook on the world.
pub type Hook = Box<dyn FnMut(&mut World)>;

/// `inner` with `before` run when it starts, `crashing` when a node is about to crash, and `after`
/// when it finishes (after the final crash and recovery of every node), on clocks drawn within
/// `clocks`.
pub struct Around {
    pub inner: Box<dyn Workload>,
    pub before: Hook,
    pub crashing: Box<dyn FnMut(&mut World, u32)>,
    pub after: Hook,
    pub clocks: ClockLimits,
}

impl Around {
    /// `inner` with no hooks and the default clocks.
    pub fn new(inner: Box<dyn Workload>) -> Around {
        Around {
            inner,
            before: Box::new(|_| {}),
            crashing: Box::new(|_, _| {}),
            after: Box::new(|_| {}),
            clocks: ClockLimits::default(),
        }
    }
}

impl Workload for Around {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn cluster(&self) -> bool {
        self.inner.cluster()
    }
    fn clocks(&self) -> ClockLimits {
        self.clocks
    }
    fn start(&mut self, w: &mut World) {
        (self.before)(w);
        self.inner.start(w);
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        self.inner.on_client(w, client);
    }
    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        self.inner.on_completions(w, node, core, c);
    }
    fn on_crash(&mut self, w: &mut World, node: u32) {
        (self.crashing)(w, node);
        self.inner.on_crash(w, node);
    }
    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.inner.on_restart(w, node)
    }
    fn on_heal(&mut self, w: &mut World) {
        self.inner.on_heal(w);
    }
    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        (self.after)(w);
        self.inner.finish(w)
    }
}

/// Runs a cluster world's events (disk, ticks, network and restarts) until `done` holds; `false`
/// if the events end or 200,000 go by first.
pub fn drive(w: &mut World, done: impl FnMut(&mut World) -> bool) -> bool {
    drive_seeing(w, |_| {}, done)
}

/// [`drive`], showing `seen` every event before it runs.
pub fn drive_seeing(
    w: &mut World,
    mut seen: impl FnMut(&Event),
    mut done: impl FnMut(&mut World) -> bool,
) -> bool {
    for _ in 0..200_000 {
        if done(w) {
            return true;
        }
        let Some((_, _, ev)) = w.pop() else { return false };
        seen(&ev);
        match ev {
            Event::Disk { node, core, life } => {
                w.on_disk(node, core, life);
            }
            Event::LogFlush { node, life, sync } => w.on_log_flush(node, life, sync),
            Event::LogDone { node, core, life, done } => {
                w.on_log_done(node, core, life, done);
            }
            Event::Tick { node, core, life } => {
                w.on_tick(node, core, life);
            }
            Event::Deliver { from, to, core, epoch, frame } => {
                w.on_deliver(from, to, core, epoch, &frame)
            }
            Event::Restart { node } => match w.start_recheck(node) {
                Some(at) => w.schedule(at, Event::Restart { node }),
                None => w.start_node(node).unwrap(),
            },
            _ => {}
        }
    }
    false
}
