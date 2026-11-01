//! The plans a query may take (010 FR-001, T014), by the rule RefDb's planner follows
//! (measured on a `refrouter` 9.0.4, 01-11-2026): an index whose first field the filter constrains
//! scans its leading fields' intervals, equalities first and one range after them; an index that
//! gives the order of the sort is a plan even when the filter does not constrain it; a `$or`
//! whose every clause has an index scans each and joins them; without any, the collection is
//! scanned. The clauses an index decides alone leave the filter; the others filter what it
//! finds.

use teseladb_bson::{Document, Value};

use crate::logical::{Bound, conjunction, conjuncts, intersect, intervals, on_path};

/// What the planner knows of an index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexMeta {
    /// Name.
    pub name: String,
    /// Fields and directions (`1`, `-1`).
    pub key: Vec<(String, i32)>,
    /// Some document went through an array on one of its fields.
    pub multikey: bool,
    /// Whether the planner may scan it: an ordered index, not hidden, not partial or sparse.
    pub plannable: bool,
}

/// How a plan reads the collection.
#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    /// Every document, with the filter.
    Collscan,
    /// The keys of an index within intervals, then the documents with what is left of the
    /// filter.
    Index(IndexScan),
    /// Each clause of a `$or` with its own scan, the documents found once.
    Or(Vec<IndexScan>),
    /// Two index scans whose entries meet by primary key (`AND_HASH`): the first one's keys held,
    /// the last one's streamed in its order, then the documents with the whole filter.
    AndHash {
        /// The scans, the streamed one last.
        scans: Vec<IndexScan>,
        /// The filter of the documents found, its clauses in RefDb's order.
        filter: Option<Document>,
    },
}

/// A scan of an index.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexScan {
    /// Its position in the catalog.
    pub index: usize,
    /// The intervals of each field, in the index's order.
    pub bounds: Vec<(String, Vec<Bound>)>,
    /// Whether it reads the keys from the last.
    pub backward: bool,
    /// The clauses left for the documents found.
    pub residual: Option<Document>,
    /// Whether its order is that of the sort.
    pub sorts: bool,
    /// The clauses that gave it intervals.
    pub assigned: usize,
    /// Their positions among the filter's clauses.
    pub preds: Vec<usize>,
}

/// What a query asks of the planner.
#[derive(Clone, Copy, Debug, Default)]
pub struct Request<'a> {
    /// The filter as RefDb writes it back after parsing it.
    pub parsed: Option<&'a Document>,
    /// The sort, as fields and directions.
    pub sort: &'a [(String, i32)],
    /// The index `hint` names, by its position in the catalog.
    pub hint: Option<usize>,
    /// Whether plans may intersect index scans (`internalQueryPlannerEnableHashIntersection`).
    pub intersect: bool,
}

/// The plans of `request` over `indexes`, in the order RefDb makes them; the collection scan
/// alone when no index serves.
pub fn candidates(request: &Request<'_>, indexes: &[IndexMeta]) -> Vec<Access> {
    let empty = Document::new();
    let parsed = request.parsed.unwrap_or(&empty);
    if let Some(branches) = branch_scans(request, indexes) {
        return match branches.into_iter().map(|b| b.into_iter().next()).collect() {
            Some(scans) => vec![Access::Or(scans)],
            None => vec![Access::Collscan],
        };
    }
    let clauses = conjuncts(parsed);
    let found = scans_of(&clauses, request, indexes, true);
    if found.is_empty() {
        return vec![Access::Collscan];
    }
    let pairs = if request.intersect && request.hint.is_none() {
        intersections(&clauses, &found, !request.sort.is_empty())
    } else {
        Vec::new()
    };
    found.into_iter().map(Access::Index).chain(pairs).collect()
}

/// The scans each clause of a rooted `$or` may take, in the order RefDb makes them (RefDb
/// plans each clause on its own, by trial, `SUBPLAN`); `None` for a filter that is no rooted
/// `$or`, or with a `hint`. A clause no index serves has no scans.
pub fn branch_scans(request: &Request<'_>, indexes: &[IndexMeta]) -> Option<Vec<Vec<IndexScan>>> {
    let parsed = request.parsed?;
    let Some(Value::Array(branches)) = parsed.get("$or").filter(|_| parsed.len() == 1) else {
        return None;
    };
    if request.hint.is_some() {
        return None;
    }
    let mut out = Vec::new();
    for b in branches {
        let found = match b {
            Value::Document(branch) => scans_of(&conjuncts(branch), request, indexes, false),
            _ => Vec::new(),
        };
        out.push(found);
    }
    Some(out)
}

/// The intersections of two scans RefDb makes when hash intersection is on (measured on a
/// `refrouter` 9.0.4 with `internalQueryPlannerEnableHashIntersection`): every pair of scans that
/// the filter constrains on different clauses, unless one index alone takes those clauses; the
/// later index of the catalog first, or last with a sort; the filter of the documents with the
/// streamed scan's clauses first, then the other's, then the rest.
fn intersections(clauses: &[Document], scans: &[IndexScan], sorted: bool) -> Vec<Access> {
    let mut out = Vec::new();
    for a in scans.iter().filter(|s| !s.preds.is_empty()) {
        for b in scans.iter().filter(|s| !s.preds.is_empty() && s.index < a.index) {
            if a.preds.iter().any(|p| b.preds.contains(p)) {
                continue;
            }
            let mut union: Vec<usize> = a.preds.iter().chain(&b.preds).copied().collect();
            union.sort_unstable();
            let alone = scans.iter().any(|s| {
                let mut own = s.preds.clone();
                own.sort_unstable();
                own == union
            });
            if alone {
                continue;
            }
            let mut order: Vec<usize> = b.preds.clone();
            order.extend(&a.preds);
            order.extend((0..clauses.len()).filter(|i| !union.contains(i)));
            let filter: Vec<Document> = order.iter().map(|i| clauses[*i].clone()).collect();
            let mut pair = vec![a.clone(), b.clone()];
            if sorted {
                pair.reverse();
            }
            out.push(Access::AndHash { scans: pair, filter: conjunction(&filter) });
        }
    }
    out
}

/// The scans of the indexes that serve `clauses` (or the sort, with `sorting`).
fn scans_of(
    clauses: &[Document],
    request: &Request<'_>,
    indexes: &[IndexMeta],
    sorting: bool,
) -> Vec<IndexScan> {
    let mut out = Vec::new();
    let mut sorting_only = Vec::new();
    for (pos, index) in indexes.iter().enumerate() {
        if request.hint.is_some_and(|h| h != pos) || (!index.plannable && request.hint.is_none()) {
            continue;
        }
        let scan = scan(pos, index, clauses, if sorting { request.sort } else { &[] });
        let constrained = scan.bounds.first().is_some_and(|(_, b)| b != &vec![Bound::all()]);
        if constrained || request.hint == Some(pos) {
            out.push(scan);
        } else if scan.sorts {
            sorting_only.push(scan);
        }
    }
    order(&mut out);
    // An index that only gives the order is a plan when no other plan gives it.
    if !out.iter().any(|s| s.sorts) {
        out.extend(sorting_only);
    }
    out
}

/// The scans in the order RefDb makes their plans, which keeps them when their trials tie
/// (measured): those that take more clauses first, then those of fewer fields, then those
/// nearer the first in the catalog.
fn order(scans: &mut [IndexScan]) {
    let Some(top) = scans.iter().max_by_key(|s| {
        (s.assigned, std::cmp::Reverse(s.bounds.len()), std::cmp::Reverse(s.index))
    }) else {
        return;
    };
    let anchor = top.index;
    scans.sort_by_key(|s| {
        (std::cmp::Reverse(s.assigned), s.bounds.len(), s.index.abs_diff(anchor), s.index)
    });
}

/// The scan of `index` for `clauses`: the intervals of its leading fields, the clauses it
/// decides alone removed.
fn scan(pos: usize, index: &IndexMeta, clauses: &[Document], sort: &[(String, i32)]) -> IndexScan {
    let mut used = vec![false; clauses.len()];
    let mut assigned = 0;
    let mut preds = Vec::new();
    let mut bounds = Vec::new();
    let mut open = true;
    for (field, _) in &index.key {
        if !open {
            bounds.push((field.clone(), vec![Bound::all()]));
            continue;
        }
        let mut field_bounds: Option<Vec<Bound>> = None;
        let mut taken = Vec::new();
        for (i, c) in clauses.iter().enumerate() {
            let Some((path, op, arg)) = on_path(c) else { continue };
            if path != field {
                continue;
            }
            let Some((b, exact)) = intervals(op, arg) else { continue };
            if index.multikey && field_bounds.is_some() {
                continue;
            }
            field_bounds = Some(match field_bounds {
                Some(prev) => intersect(&prev, &b),
                None => b,
            });
            assigned += 1;
            preds.push(i);
            if exact {
                taken.push(i);
            }
        }
        match field_bounds {
            Some(b) => {
                open = b.iter().all(Bound::is_point);
                for i in taken {
                    used[i] = true;
                }
                bounds.push((field.clone(), b));
            }
            None => {
                open = false;
                bounds.push((field.clone(), vec![Bound::all()]));
            }
        }
    }
    let (sorts, backward) = sort_order(index, &bounds, sort);
    // What is left filters the documents by path (measured).
    let mut left: Vec<Document> =
        clauses.iter().zip(&used).filter(|(_, u)| !**u).map(|(c, _)| c.clone()).collect();
    left.sort_by(|a, b| match (on_path(a), on_path(b)) {
        (Some((x, _, _)), Some((y, _, _))) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    });
    IndexScan { index: pos, bounds, backward, residual: conjunction(&left), sorts, assigned, preds }
}

/// Whether `index` with `bounds` reads in the order of `sort`, and backward: the sort's fields
/// follow the index's after those fixed by a point, all in its direction or all against it.
fn sort_order(
    index: &IndexMeta,
    bounds: &[(String, Vec<Bound>)],
    sort: &[(String, i32)],
) -> (bool, bool) {
    if sort.is_empty() || index.multikey {
        return (false, false);
    }
    let fixed = bounds.iter().take_while(|(_, b)| b.len() == 1 && b[0].is_point()).count();
    for start in 0..=fixed {
        let keys = &index.key[start.min(index.key.len())..];
        if keys.len() < sort.len() {
            continue;
        }
        let same =
            sort.iter().zip(keys).all(|((f, d), (k, kd))| f == k && d.signum() == kd.signum());
        let opposite =
            sort.iter().zip(keys).all(|((f, d), (k, kd))| f == k && d.signum() == -kd.signum());
        if same || opposite {
            return (true, opposite);
        }
    }
    (false, false)
}
