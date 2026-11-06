//! `tenants` workload (007 T059, FR-031, FR-034, SC-009): two tenants with a storage quota in
//! Group 0, written through every node while the fault plan crashes nodes, partitions the network
//! and freezes cores. Each node runs the server's rules over its own replica of Group 0: whether
//! the tenant's users are served ([`TenantState::admits_users`]) and its part of the quota's margin
//! ([`QuotaWindow`]) by its readings of the two epochs before. Each node reads the data each tenant uses once per
//! epoch on its own clock, as the server's `tenant-usage` thread does, if it reaches a majority; a
//! crash loses what it read and counted, and the node counts again from the window after its
//! restart. Writers go quiet now and then, as a client's traffic comes in bursts. An admin
//! suspends and reactivates the second tenant now and then.
//!
//! The checker `tenant_limits` fails if a tenant's data passes its quota by more than one write in
//! flight per node, or if a node serves a command of a tenant its replica of Group 0 has applied
//! as suspended.

use std::collections::BTreeMap;

use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::{GROUP0, Group0Sm};
use teseladb_meta::tenant_quota::{
    EPOCH_MS, GUARD_MS, NodeReading, QuotaRefusal, QuotaWindow, Reading, base_epoch, note,
    reading_at, window_of,
};
use teseladb_meta::tenants::{TenantLimits, TenantState};
use teseladb_raft::Role;
use teseladb_raftlog::Completions;
use teseladb_types::Nanos;

use crate::checkers::Violation;
use crate::node::NodeSm;
use crate::workloads::Workload;
use crate::world::{Event, World};

/// The checker of this workload.
pub const CHECKER: &str = "tenant_limits";
/// The tenants; the admin suspends and reactivates the second one.
const NAMES: [&str; 2] = ["acme", "beta"];
/// Each tenant's storage quota, in bytes.
const QUOTA: u64 = 1 << 20;
/// The bytes of a write, in KiB.
const WRITE_KIB: (u64, u64) = (1, 16);
/// The largest write: the most one node can have in flight for a tenant.
const MAX_WRITE: u64 = WRITE_KIB.1 << 10;
/// How long a write takes to commit, and the pause of a writer after a refusal, in ms.
const WRITE_TAKES: (u64, u64) = (1, 40);
const PAUSE: (u64, u64) = (1, 20);
/// A writer goes quiet after one write in [`IDLE_ONE_IN`], for this long in ms, as a client's
/// traffic comes in bursts: its node keeps the part it worked out before.
const IDLE: (u64, u64) = (1_000, 9_000);
const IDLE_ONE_IN: u64 = 40;
/// How long a reading takes, in ms, and how soon a node that could not read tries again.
const READ_TAKES: (u64, u64) = (20, 600);
const READ_RETRY: (u64, u64) = (50, 150);
/// The readings a node keeps, as the server's: those of its epoch and the two before.
const KEPT_READINGS: usize = 3;
/// The admin's turns, in ms.
const ADMIN_EVERY: (u64, u64) = (300, 1_500);
/// How long a run must last before a write of a tenant can be served: a node lets the first one through once
/// the base of its epoch comes after its start (`base_epoch(now) >= window_of(start) + 1`), some 5.75 s after it
/// starts, as the server does (007 FR-031); one epoch more covers the clocks' skew and the writers' pauses.
const SERVED_BY_MS: u64 = 4 * EPOCH_MS;
/// Client numbers: the admin, then a writer per node and tenant, then a reader per node, then
/// writes and readings in flight.
const ADMIN: u32 = 0;
const FIRST_READER: u32 = 500;
const FIRST_MESSAGE: u32 = 1_000;

/// What a node keeps in memory of the tenants, as the server's `TenantLimiter`: its last three
/// readings, what it let each one write, the first window it counts whole and the last epoch it
/// read.
#[derive(Clone, Debug, Default)]
struct NodeView {
    readings: Vec<NodeReading>,
    windows: BTreeMap<u32, QuotaWindow>,
    since: u64,
    read: Option<u64>,
}

/// A reading on its way: the node, its life and what it gave.
type InFlight = (u32, u64, NodeReading);

/// A write or a reading on its way.
#[derive(Clone, Debug)]
enum Pending {
    /// A write of `bytes` of `tenant` by writer `writer`.
    Write { tenant: u32, bytes: u64, writer: u32 },
    /// A reading.
    Read(InFlight),
}

/// The `tenants` workload.
pub struct Tenants {
    nodes: u32,
    views: Vec<NodeView>,
    /// The bytes of each tenant committed in the cluster.
    data: BTreeMap<u32, u64>,
    pending: BTreeMap<u32, Pending>,
    next_message: u32,
    /// Writes served, and commands refused because the tenant is suspended.
    served: u64,
    refused_suspended: u64,
    violation: Option<Violation>,
}

impl Tenants {
    /// The workload as a [`crate::registry::WorkloadFactory`].
    pub fn boxed(nodes: u32, _cores: u16) -> Box<dyn Workload> {
        Box::new(Tenants::new(nodes))
    }

    /// A workload over `nodes` nodes.
    pub fn new(nodes: u32) -> Tenants {
        Tenants {
            nodes,
            views: vec![NodeView::default(); nodes as usize],
            data: BTreeMap::new(),
            pending: BTreeMap::new(),
            next_message: FIRST_MESSAGE,
            served: 0,
            refused_suspended: 0,
            violation: None,
        }
    }

    fn up(w: &World, n: u32) -> bool {
        w.nodes[n as usize].alive && !w.frozen(n, 0)
    }

    /// What node `n`'s replica of Group 0 knows of tenant `name`: its number and state.
    fn tenant(w: &mut World, n: u32, name: &str) -> Option<(u32, TenantState)> {
        let core = w.paused_core(n, 0)?;
        let sm: &Group0Sm = core.state_machine(GROUP0).and_then(NodeSm::group0)?;
        let id = sm.tenants.id_of(name)?;
        Some((id, sm.tenants.state(id)?.clone()))
    }

    fn after(w: &mut World, ms: (u64, u64), client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(ms.0, ms.1 + 1));
        w.schedule(at, Event::Client { client });
    }

    fn message(&mut self, w: &mut World, ms: (u64, u64), p: Pending) {
        let id = self.next_message;
        self.next_message += 1;
        self.pending.insert(id, p);
        Self::after(w, ms, id);
    }

    /// The admin's turn: creates the tenants and their quotas, then suspends or reactivates the
    /// second one, through Group 0's leader.
    fn admin(&mut self, w: &mut World) {
        let leader = (0..self.nodes).find(|&n| {
            Self::up(w, n)
                && w.cluster_core(n, 0)
                    .and_then(|c| c.status(GROUP0))
                    .is_some_and(|s| s.role == Role::Leader)
        });
        let Some(n) = leader else { return };
        let mut cmd = None;
        for name in NAMES {
            cmd = match Self::tenant(w, n, name) {
                None => Some(Group0Command::CreateTenant { name: name.to_owned() }),
                Some((tenant, s)) if s.limits.storage_bytes != QUOTA => {
                    let limits = TenantLimits { storage_bytes: QUOTA, ..TenantLimits::default() };
                    Some(Group0Command::SetTenantLimits { tenant, limits })
                }
                Some(_) => continue,
            };
            break;
        }
        if cmd.is_none()
            && let Some((tenant, s)) = Self::tenant(w, n, NAMES[1])
        {
            cmd = Some(Group0Command::SetTenantSuspended { tenant, suspended: !s.suspended });
        }
        let Some(cmd) = cmd else { return };
        let now = w.local(n);
        // A refusal (the leader just stepped down) is tried again at the next turn.
        let _ = w.cluster_core(n, 0).map(|c| c.propose(GROUP0, &cmd.encode(), now));
        w.settle(n, 0);
    }

    /// Writer `writer` of tenant `NAMES[t]` on node `n`: a write if the node serves the tenant
    /// and the write fits in its part of the quota.
    fn write(&mut self, w: &mut World, writer: u32, n: u32, t: usize) {
        let Some((tenant, state)) = Self::up(w, n).then(|| Self::tenant(w, n, NAMES[t])).flatten()
        else {
            return Self::after(w, PAUSE, writer);
        };
        if !state.admits_users() {
            self.refused_suspended += 1;
            return Self::after(w, PAUSE, writer);
        }
        if state.suspended {
            let detail = format!(
                "node {n} serves tenant {} after applying its suspension ({} ms)",
                NAMES[t],
                w.now().as_millis()
            );
            self.violation.get_or_insert(Violation::safety(CHECKER, detail));
        }
        if state.limits.storage_bytes == 0 {
            return Self::after(w, PAUSE, writer);
        }
        let bytes = w.rng.range(WRITE_KIB.0, WRITE_KIB.1 + 1) << 10;
        let now = w.local(n).as_millis();
        let (limit, nodes) = (state.limits.storage_bytes, self.nodes as usize);
        let mut r = self.admit(n, tenant, (limit, nodes, now), bytes);
        // A tenant the node has no reading of is read at once, as the server does.
        let base = base_epoch(now);
        if r == Err(QuotaRefusal::Unread) && base >= self.views[n as usize].since {
            let used = self.data.get(&tenant).copied().unwrap_or(0);
            note(&mut self.views[n as usize].readings, base, tenant, used, KEPT_READINGS);
            r = self.admit(n, tenant, (limit, nodes, now), bytes);
        }
        match r {
            Ok(()) => self.message(w, WRITE_TAKES, Pending::Write { tenant, bytes, writer }),
            Err(_) => Self::after(w, PAUSE, writer),
        }
    }

    /// Node `n`'s rule of FR-031 for a write of `bytes` of `tenant` at its wall-clock `now` under quota `limit`.
    fn admit(
        &mut self,
        n: u32,
        tenant: u32,
        (limit, nodes, now): (u64, usize, u64),
        bytes: u64,
    ) -> Result<(), QuotaRefusal> {
        let view = &mut self.views[n as usize];
        let readings: Vec<Reading> = view.readings.iter().map(|r| r.of(tenant)).collect();
        let since = view.since;
        view.windows.entry(tenant).or_default().admit(limit, nodes, since, now, &readings, bytes)
    }

    /// A write commits: in the cluster even if its node crashed meanwhile, as a Raft entry may.
    fn committed(&mut self, w: &mut World, tenant: u32, bytes: u64, writer: u32) {
        self.served += 1;
        let total = self.data.entry(tenant).or_default();
        *total += bytes;
        let bound = QUOTA + u64::from(self.nodes) * MAX_WRITE;
        if *total > bound {
            let detail = format!(
                "tenant {tenant} holds {total} bytes: past its quota of {QUOTA} by more than one \
                 write in flight per node ({} ms)",
                w.now().as_millis()
            );
            self.violation.get_or_insert(Violation::safety(CHECKER, detail));
        }
        let idle = w.rng.range(0, IDLE_ONE_IN) == 0;
        Self::after(w, if idle { IDLE } else { PAUSE }, writer);
    }

    /// Node `n`'s reader, as the server's thread: once in each epoch, as soon as its reading may
    /// start on the node's clock, if the node reaches a majority.
    fn read(&mut self, w: &mut World, n: u32) {
        let client = FIRST_READER + n;
        let now = w.local(n).as_millis();
        let epoch = now.saturating_sub(GUARD_MS) / EPOCH_MS;
        if self.views[n as usize].read == Some(epoch) {
            let wait = reading_at(epoch + 1).saturating_sub(now);
            return Self::after(w, (wait, wait + 1), client);
        }
        let reached = (0..self.nodes).filter(|&m| m == n || w.net.reachable(n, m)).count();
        if !Self::up(w, n) || reached * 2 <= self.nodes as usize {
            return Self::after(w, READ_RETRY, client);
        }
        // Every tenant the node's replica knows, as the server's reading: none written is 0.
        let known: Vec<u32> =
            NAMES.iter().filter_map(|name| Self::tenant(w, n, name)).map(|(t, _)| t).collect();
        let used = known.iter().map(|t| (*t, self.data.get(t).copied().unwrap_or(0))).collect();
        let reading = NodeReading { epoch, used };
        self.views[n as usize].read = Some(epoch);
        let life = w.nodes[n as usize].life;
        self.message(w, READ_TAKES, Pending::Read((n, life, reading)));
        Self::after(w, (1, 1), client);
    }

    /// A reading ends: the node keeps it among its last three. A reading of a node that crashed
    /// meanwhile is lost with it.
    fn read_done(&mut self, w: &World, (n, life, reading): InFlight) {
        let node = &w.nodes[n as usize];
        if !node.alive || node.life != life {
            return;
        }
        let readings = &mut self.views[n as usize].readings;
        for (tenant, used) in reading.used {
            note(readings, reading.epoch, tenant, used, KEPT_READINGS);
        }
    }

    /// Node `n` starts (again): it counts from the window after this one.
    fn started(&mut self, w: &World, n: u32) {
        let since = window_of(w.local(n).as_millis()) + 1;
        self.views[n as usize] = NodeView { since, ..NodeView::default() };
    }
}

impl Workload for Tenants {
    fn name(&self) -> &'static str {
        "tenants"
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        Self::after(w, (10, 10), ADMIN);
        for n in 0..self.nodes {
            self.started(w, n);
            for t in 0..NAMES.len() as u32 {
                Self::after(w, PAUSE, 1 + n * 2 + t);
            }
            Self::after(w, (1, 1), FIRST_READER + n);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if client >= FIRST_MESSAGE {
            // Every message turn was scheduled with its message.
            match self.pending.remove(&client).expect("a message in flight") {
                Pending::Write { tenant, bytes, writer } => {
                    self.committed(w, tenant, bytes, writer);
                }
                Pending::Read(reading) => self.read_done(w, reading),
            }
        } else if client >= FIRST_READER {
            self.read(w, client - FIRST_READER);
        } else if client == ADMIN {
            self.admin(w);
            Self::after(w, ADMIN_EVERY, ADMIN);
        } else {
            let (n, t) = ((client - 1) / 2, ((client - 1) % 2) as usize);
            self.write(w, client, n, t);
        }
    }

    fn on_completions(&mut self, w: &mut World, _node: u32, _core: u16, _c: Completions) {
        // The admin's entries need no answer: the writers read each node's replica.
        w.events.clear();
    }

    fn on_crash(&mut self, _w: &mut World, node: u32) {
        self.views[node as usize] = NodeView::default();
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.started(w, node);
        for core in 0..w.nodes[node as usize].cores.len() as u16 {
            w.settle(node, core);
        }
        Ok(())
    }

    fn on_heal(&mut self, _w: &mut World) {}

    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        if let Some(v) = self.violation.take() {
            return Err(v);
        }
        // A shorter run ends before any node could have read the tenants' data, so no write is owed yet.
        if self.served == 0 && w.now().as_millis() >= SERVED_BY_MS {
            return Err(Violation::liveness(CHECKER, "no write of a tenant was served"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn world(seed: u64, secs: u64) -> (World, Tenants) {
        let mut wl = Tenants::new(3);
        let mut w = World::with_clocks(seed, 3, 1, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, &mut wl, Nanos::from_secs(secs), |_| false);
        (w, wl)
    }

    #[test]
    fn the_tenants_write_up_to_their_quota_and_the_suspension_holds() {
        let (mut w, mut wl) = world(5, 40);
        assert!(wl.served > 50, "{}", wl.served);
        assert!(wl.refused_suspended > 0, "the second tenant was suspended at some point");
        let filled = wl.data.values().copied().max().unwrap_or(0);
        assert!(filled > QUOTA / 2, "the writes reached the quota: {filled}");
        assert!(wl.finish(&mut w).is_ok());
    }

    #[test]
    fn a_tenant_past_its_quota_or_without_writes_breaks_the_checker() {
        let (mut w, mut wl) = world(6, 10);
        wl.data.insert(1, QUOTA + 3 * MAX_WRITE);
        wl.committed(&mut w, 1, 1, 1);
        let v = wl.violation.take().expect("a violation");
        assert!(v.detail.contains("past its quota"), "{v:?}");
        wl.served = 0;
        assert!(wl.finish(&mut w).is_err());
    }

    #[test]
    fn a_run_too_short_for_a_first_write_owes_none() {
        let (mut w, mut wl) = world(1, 1);
        assert_eq!(wl.served, 0, "no node can let a write through in the first second");
        assert!(wl.finish(&mut w).is_ok());
    }
}
