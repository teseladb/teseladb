//! The linearizability checker against an exhaustive search (002 T011): on random histories of a
//! CAS register small enough to try every order, the checker's answer (Wing and Gong with Lowe's
//! memoization and its pruning of writes nobody needs) must be the exhaustive one. Histories built
//! from a real sequential run must be linearizable; their mutations may or may not be.

use teseladb_sim::checkers::linearizable::check;
use teseladb_sim::history::{History, OpKind, OpRecord, OpResult};
use teseladb_types::{Nanos, Rng};

/// Operations per history: 7! orders times the subsets of writes without a response.
const MAX_OPS: u64 = 7;
const HISTORIES: u64 = 20_000;

fn record(i: u64, kind: OpKind, invoke: u64, complete: u64) -> OpRecord {
    OpRecord {
        client: i as u32,
        op_id: i,
        kind,
        group: "g".into(),
        key: "k".into(),
        expected: None,
        new: None,
        invoke: Nanos(invoke),
        complete: Some(Nanos(complete)),
        result: OpResult::Unknown,
        node: 0,
        error: None,
        ended: None,
        w1: false,
    }
}

/// A history of a register run sequentially at one point inside each operation's interval, so it
/// is linearizable; some answers are then lost (`Unknown`) or turned into refusals of operations
/// that did not take effect.
fn sequential(rng: &mut Rng) -> Vec<OpRecord> {
    let n = rng.range(1, MAX_OPS + 1);
    let mut ops: Vec<(u64, OpRecord)> = (0..n)
        .map(|i| {
            let invoke = rng.range(0, 30);
            let complete = invoke + rng.range(1, 15);
            let point = rng.range(invoke, complete);
            let kind = if rng.chance(0.4) { OpKind::Read } else { OpKind::Cas };
            (point * MAX_OPS + i, record(i, kind, invoke, complete))
        })
        .collect();
    ops.sort_by_key(|(p, _)| *p);
    let mut cur: Option<String> = None;
    let mut written: Vec<String> = Vec::new();
    for (_, o) in &mut ops {
        if o.kind == OpKind::Read {
            o.result = OpResult::Ok(cur.clone());
            continue;
        }
        // Mostly the current value; sometimes an older one or absent.
        o.expected = match rng.range(0, 4) {
            0 => None,
            1 if !written.is_empty() => {
                Some(written[rng.range(0, written.len() as u64) as usize].clone())
            }
            _ => cur.clone(),
        };
        o.new = Some(format!("v{}", o.op_id));
        if o.expected == cur {
            o.result = OpResult::Ok(None);
            cur = o.new.clone();
            written.push(cur.clone().expect("a value"));
        } else {
            o.result = OpResult::Fail;
        }
    }
    let mut ops: Vec<OpRecord> = ops.into_iter().map(|(_, o)| o).collect();
    for o in &mut ops {
        if rng.chance(0.15) {
            o.result = OpResult::Unknown;
            o.complete = None;
        } else if o.result == OpResult::Fail && rng.chance(0.2) {
            o.result = OpResult::Rejected;
        }
    }
    ops
}

/// Changes one answer: a read sees another value, or a CAS flips between success and failure.
fn mutate(rng: &mut Rng, mut ops: Vec<OpRecord>) -> Vec<OpRecord> {
    let i = rng.range(0, ops.len() as u64) as usize;
    let values: Vec<Option<String>> =
        std::iter::once(None).chain(ops.iter().filter_map(|o| o.new.clone()).map(Some)).collect();
    let o = &mut ops[i];
    o.result = match (&o.kind, &o.result) {
        (OpKind::Read, _) => {
            o.complete = o.complete.or(Some(o.invoke + Nanos(1)));
            OpResult::Ok(values[rng.range(0, values.len() as u64) as usize].clone())
        }
        (_, OpResult::Ok(_)) => OpResult::Fail,
        _ => {
            o.complete = o.complete.or(Some(o.invoke + Nanos(1)));
            OpResult::Ok(None)
        }
    };
    ops
}

/// Whether some order of the operations that took effect, respecting real time, explains every
/// answer: tries every subset of the writes without a response and every order.
fn exhaustive(ops: &[OpRecord]) -> bool {
    // A refusal and a read without an answer impose nothing.
    let ops: Vec<&OpRecord> = ops
        .iter()
        .filter(|o| o.result != OpResult::Rejected)
        .filter(|o| !(o.kind == OpKind::Read && o.result == OpResult::Unknown))
        .collect();
    let optional: Vec<usize> =
        (0..ops.len()).filter(|&i| ops[i].result == OpResult::Unknown).collect();
    (0..1u32 << optional.len()).any(|mask| {
        let chosen: Vec<&OpRecord> = (0..ops.len())
            .filter(|i| match optional.iter().position(|o| o == i) {
                Some(bit) => mask >> bit & 1 == 1,
                None => true,
            })
            .map(|i| ops[i])
            .collect();
        orders(&chosen, &mut vec![false; chosen.len()], None)
    })
}

fn orders(ops: &[&OpRecord], used: &mut Vec<bool>, cur: Option<String>) -> bool {
    if used.iter().all(|u| *u) {
        return true;
    }
    for i in 0..ops.len() {
        if used[i] {
            continue;
        }
        // Another one that ended before this one started has to go first.
        let blocked = (0..ops.len())
            .any(|j| !used[j] && j != i && ops[j].complete.is_some_and(|c| c < ops[i].invoke));
        if blocked {
            continue;
        }
        let o = ops[i];
        let next = match (&o.kind, &o.result) {
            (OpKind::Read, OpResult::Ok(v)) => (*v == cur).then(|| cur.clone()),
            (_, OpResult::Fail) => (o.expected != cur).then(|| cur.clone()),
            _ => (o.expected == cur).then(|| o.new.clone()),
        };
        if let Some(next) = next {
            used[i] = true;
            if orders(ops, used, next) {
                return true;
            }
            used[i] = false;
        }
    }
    false
}

#[test]
fn the_checker_agrees_with_an_exhaustive_search() {
    let mut rng = Rng::new(2026);
    let (mut yes, mut no) = (0, 0);
    for n in 0..HISTORIES {
        let real = sequential(&mut rng);
        assert!(exhaustive(&real), "a sequential run is linearizable: {real:#?}");
        let ops = if n % 2 == 0 { real } else { mutate(&mut rng, real) };
        let want = exhaustive(&ops);
        let got = check(&History { ops: ops.clone() }).is_ok();
        assert_eq!(got, want, "history {n}: {ops:#?}");
        if want {
            yes += 1;
        } else {
            no += 1;
        }
    }
    // Both answers come up often enough to mean something.
    assert!(yes > HISTORIES / 2 && no > HISTORIES / 10, "{yes} linearizable, {no} not");
}
