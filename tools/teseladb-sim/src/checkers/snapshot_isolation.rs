//! Snapshot isolation over histories of transactions on append-only lists (008 FR-016, T003,
//! research R5): each key is a list, a transaction appends unique values to keys and reads whole
//! lists. The order of a key's versions is its longest read (every read of it is a prefix), which
//! gives the dependencies between committed transactions: `ww` (the next append), `wr` (the last
//! value a read saw) and `rw` (a read whose list the next append extends). Anomalies, after Adya
//! and the snapshot isolation of Elle (Kingsbury and Alvaro, VLDB 2020):
//! - incompatible order: two reads of a key that are not prefixes of one another;
//! - G1a: a committed transaction reads a value of an aborted one;
//! - G1b: a committed transaction reads an append that is not its writer's last to that key;
//! - G0: a cycle of `ww`;
//! - G1c: a cycle of `ww` and `wr`;
//! - G-SI: a cycle with `rw` edges and no two of them in a row (two in a row, write skew, is what
//!   snapshot isolation allows).
//!
//! Then the real-time order (008 FR-008, the strong snapshot isolation of Elle): a transaction
//! that began after another's commit was acknowledged comes after it (`rt`), so it sees it. With
//! those edges too, G1c-realtime is a cycle of `ww`, `wr` and `rt`, and G-SI-realtime a G-SI cycle;
//! a stale read is the cycle of the reader's `rw` to the transaction it missed and that one's `rt`
//! to the reader. Only the transactions with times take part: the workload leaves them out once
//! the clocks are further apart than the uncertainty window, where no order is promised.

use std::collections::{BTreeMap, BTreeSet};

use crate::checkers::Violation;

/// The name of the checker in its violations.
pub const NAME: &str = "snapshot-isolation";

/// One operation of a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MicroOp {
    /// Appends `value` (unique per key) to `key`.
    Append {
        /// Key.
        key: String,
        /// Value.
        value: i64,
    },
    /// Reads `key`'s whole list.
    Read {
        /// Key.
        key: String,
        /// The list, oldest first.
        list: Vec<i64>,
    },
}

/// How a transaction ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Committed.
    Committed,
    /// Aborted.
    Aborted,
    /// No answer: it may have committed or not.
    Unknown,
}

/// A transaction of the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Txn {
    /// Identifier.
    pub id: u64,
    /// Its operations, in order.
    pub ops: Vec<MicroOp>,
    /// How it ended.
    pub outcome: Outcome,
    /// When it ran, if it takes part in the real-time order.
    pub realtime: Option<RealTime>,
}

/// When a transaction ran, in the run's real time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealTime {
    /// When it began (it took its read mark then).
    pub began: u64,
    /// When its commit was acknowledged (`None`: it did not commit, or nobody knows).
    pub acked: Option<u64>,
}

/// The kinds of dependency edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Dep {
    Ww,
    Wr,
    Rw,
    Rt,
}

/// The edges between the transactions of the graph: `(from, to, kind)`.
type Edges = BTreeSet<(usize, usize, Dep)>;

/// Checks `history`: the first anomaly found, or none.
pub fn check(history: &[Txn]) -> Result<(), Violation> {
    let orders = version_orders(history)?;
    let writers = writers(history);
    check_aborted_and_intermediate_reads(history, &writers)?;
    let mut edges = dependencies(history, &orders, &writers);
    if let Some(c) = cycle_of(history.len(), &edges, &[Dep::Ww]) {
        return Err(anomaly("G0", history, &c));
    }
    if let Some(c) = cycle_of(history.len(), &edges, &[Dep::Ww, Dep::Wr]) {
        return Err(anomaly("G1c", history, &c));
    }
    if let Some(c) = si_cycle(history.len(), &edges) {
        return Err(anomaly("G-SI", history, &c));
    }
    edges.extend(realtime_order(history));
    if let Some(c) = cycle_of(history.len(), &edges, &[Dep::Ww, Dep::Wr, Dep::Rt]) {
        return Err(anomaly("G1c-realtime", history, &c));
    }
    if let Some(c) = si_cycle(history.len(), &edges) {
        return Err(anomaly("G-SI-realtime", history, &c));
    }
    Ok(())
}

/// The `rt` edges: from each committed transaction with an acknowledgement to each one with times
/// that may count and began after it. Only the edges no chain of others implies: from `a` to `b`
/// when no transaction acknowledged before `b` began also began after `a` was acknowledged.
fn realtime_order(history: &[Txn]) -> Edges {
    let mut acked: Vec<(u64, u64, usize)> = history
        .iter()
        .enumerate()
        .filter(|(_, t)| t.outcome == Outcome::Committed)
        .filter_map(|(i, t)| t.realtime.and_then(|r| r.acked.map(|a| (a, r.began, i))))
        .collect();
    acked.sort_unstable();
    // The latest begin among the first `n` acknowledged.
    let latest_begin: Vec<u64> = acked
        .iter()
        .scan(0, |m, &(_, began, _)| {
            *m = began.max(*m);
            Some(*m)
        })
        .collect();
    let mut edges = Edges::new();
    for (b, t) in history.iter().enumerate().filter(|(_, t)| may_count(t)) {
        let Some(r) = t.realtime else { continue };
        let before = acked.partition_point(|&(a, _, _)| a < r.began);
        let Some(&floor) = before.checked_sub(1).and_then(|n| latest_begin.get(n)) else {
            continue;
        };
        for &(_, _, i) in acked[..before].iter().rev().take_while(|(a, _, _)| *a >= floor) {
            if i != b {
                edges.insert((i, b, Dep::Rt));
            }
        }
    }
    edges
}

fn anomaly(kind: &str, history: &[Txn], cycle: &[usize]) -> Violation {
    let ids: Vec<String> = cycle.iter().map(|&i| history[i].id.to_string()).collect();
    Violation::safety(NAME, format!("{kind}: transactions {}", ids.join(" -> ")))
}

/// Whether a transaction's effects may be in the history: it committed, or nobody knows.
fn may_count(t: &Txn) -> bool {
    t.outcome != Outcome::Aborted
}

/// The order of each key's versions: its longest read, after checking that every read of the key
/// is a prefix of it.
fn version_orders(history: &[Txn]) -> Result<BTreeMap<&str, &[i64]>, Violation> {
    let mut longest: BTreeMap<&str, &[i64]> = BTreeMap::new();
    for t in history.iter().filter(|t| t.outcome == Outcome::Committed) {
        for op in &t.ops {
            if let MicroOp::Read { key, list } = op {
                let cur = longest.entry(key.as_str()).or_insert(&[]);
                if list.len() > cur.len() {
                    *cur = list.as_slice();
                }
            }
        }
    }
    for t in history.iter().filter(|t| t.outcome == Outcome::Committed) {
        for op in &t.ops {
            if let MicroOp::Read { key, list } = op {
                let full = longest.get(key.as_str()).copied().unwrap_or(&[]);
                if !full.starts_with(list) {
                    return Err(Violation::safety(
                        NAME,
                        format!(
                            "incompatible order: transaction {} read {key} as {list:?}, another \
                             read gives {full:?}",
                            t.id
                        ),
                    ));
                }
            }
        }
    }
    Ok(longest)
}

/// The transaction (index) and position among its appends to that key of each appended value.
fn writers(history: &[Txn]) -> BTreeMap<(&str, i64), (usize, bool)> {
    let mut out = BTreeMap::new();
    for (i, t) in history.iter().enumerate() {
        let mut last: BTreeMap<&str, i64> = BTreeMap::new();
        for op in &t.ops {
            if let MicroOp::Append { key, value } = op {
                last.insert(key.as_str(), *value);
            }
        }
        for op in &t.ops {
            if let MicroOp::Append { key, value } = op {
                let is_last = last.get(key.as_str()) == Some(value);
                out.insert((key.as_str(), *value), (i, is_last));
            }
        }
    }
    out
}

/// G1a and G1b on the last value of each read of a committed transaction.
fn check_aborted_and_intermediate_reads(
    history: &[Txn],
    writers: &BTreeMap<(&str, i64), (usize, bool)>,
) -> Result<(), Violation> {
    for t in history.iter().filter(|t| t.outcome == Outcome::Committed) {
        for op in &t.ops {
            let MicroOp::Read { key, list } = op else { continue };
            for (pos, v) in list.iter().enumerate() {
                let Some(&(w, is_last)) = writers.get(&(key.as_str(), *v)) else { continue };
                let writer = &history[w];
                if writer.outcome == Outcome::Aborted {
                    return Err(Violation::safety(
                        NAME,
                        format!("G1a: transaction {} read {v} of aborted {}", t.id, writer.id),
                    ));
                }
                let next_by_same =
                    list.get(pos + 1).and_then(|n| writers.get(&(key.as_str(), *n))).map(|x| x.0);
                if !is_last && writer.id != t.id && next_by_same != Some(w) {
                    return Err(Violation::safety(
                        NAME,
                        format!(
                            "G1b: transaction {} read {v}, an intermediate append of {}",
                            t.id, writer.id
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The `ww`, `wr` and `rw` edges between transactions that may count.
fn dependencies(
    history: &[Txn],
    orders: &BTreeMap<&str, &[i64]>,
    writers: &BTreeMap<(&str, i64), (usize, bool)>,
) -> Edges {
    let mut edges = Edges::new();
    let writer = |key: &str, v: i64| writers.get(&(key, v)).map(|x| x.0);
    for (key, order) in orders {
        for pair in order.windows(2) {
            if let (Some(a), Some(b)) = (writer(key, pair[0]), writer(key, pair[1]))
                && a != b
            {
                edges.insert((a, b, Dep::Ww));
            }
        }
    }
    for (i, t) in history.iter().enumerate().filter(|(_, t)| may_count(t)) {
        for op in &t.ops {
            let MicroOp::Read { key, list } = op else { continue };
            let order = orders.get(key.as_str()).copied().unwrap_or(&[]);
            if let Some(w) = list.last().and_then(|v| writer(key, *v))
                && w != i
            {
                edges.insert((w, i, Dep::Wr));
            }
            // The next version after what the read saw: its writer comes after the reader.
            if let Some(w) = order.get(list.len()).and_then(|v| writer(key, *v))
                && w != i
            {
                edges.insert((i, w, Dep::Rw));
            }
        }
    }
    edges.retain(|(a, b, _)| may_count(&history[*a]) && may_count(&history[*b]));
    edges
}

/// Tarjan's strongly connected components of a graph of `n` nodes.
fn components(n: usize, adj: &[Vec<usize>]) -> Vec<Vec<usize>> {
    struct State<'a> {
        adj: &'a [Vec<usize>],
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        out: Vec<Vec<usize>>,
    }
    fn visit(s: &mut State<'_>, v: usize) {
        s.index[v] = Some(s.next);
        s.low[v] = s.next;
        s.next += 1;
        s.stack.push(v);
        s.on_stack[v] = true;
        for &w in &s.adj[v] {
            match s.index[w] {
                None => {
                    visit(s, w);
                    s.low[v] = s.low[v].min(s.low[w]);
                }
                Some(iw) if s.on_stack[w] => s.low[v] = s.low[v].min(iw),
                Some(_) => {}
            }
        }
        if Some(s.low[v]) == s.index[v] {
            let mut comp = Vec::new();
            while let Some(w) = s.stack.pop() {
                s.on_stack[w] = false;
                comp.push(w);
                if w == v {
                    break;
                }
            }
            s.out.push(comp);
        }
    }
    let mut s = State {
        adj,
        index: vec![None; n],
        low: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        next: 0,
        out: Vec::new(),
    };
    for v in 0..n {
        if s.index[v].is_none() {
            visit(&mut s, v);
        }
    }
    s.out
}

/// A cycle using only edges of `kinds`, as the transactions of a component that has one.
fn cycle_of(n: usize, edges: &Edges, kinds: &[Dep]) -> Option<Vec<usize>> {
    let mut adj = vec![Vec::new(); n];
    for (a, b, k) in edges {
        if kinds.contains(k) {
            adj[*a].push(*b);
        }
    }
    components(n, &adj).into_iter().find(|c| c.len() > 1).map(|mut c| {
        c.sort_unstable();
        c
    })
}

/// A cycle with at least one `rw` edge and no two `rw` edges in a row: a component of the graph
/// whose nodes are `(transaction, the edge into it was rw)`, where an `rw` edge may only leave a
/// node entered by another kind, that contains an `rw` edge.
fn si_cycle(n: usize, edges: &Edges) -> Option<Vec<usize>> {
    let node = |t: usize, after_rw: bool| t * 2 + usize::from(after_rw);
    let mut adj = vec![Vec::new(); n * 2];
    for (a, b, k) in edges {
        for after_rw in [false, true] {
            match k {
                Dep::Rw if after_rw => {}
                Dep::Rw => adj[node(*a, after_rw)].push(node(*b, true)),
                _ => adj[node(*a, after_rw)].push(node(*b, false)),
            }
        }
    }
    let comps = components(n * 2, &adj);
    let mut of = vec![usize::MAX; n * 2];
    for (i, c) in comps.iter().enumerate() {
        for &v in c {
            of[v] = i;
        }
    }
    for (a, b, k) in edges {
        if *k == Dep::Rw && of[node(*a, false)] == of[node(*b, true)] {
            let mut txns: Vec<usize> = comps[of[node(*a, false)]].iter().map(|v| v / 2).collect();
            txns.sort_unstable();
            txns.dedup();
            return Some(txns);
        }
    }
    None
}
