//! The estimate of what a filter's conditions let through from a collection's statistics (010
//! FR-029, FR-031): each equality, comparison and `$in` on a field with a histogram takes the
//! fraction of the field's values inside its intervals, which orders the conditions of the
//! register machine's `$and`s and `$or`s; any other predicate, or a field without statistics,
//! takes the default estimate.

use teseladb_bson::Value;
use teseladb_mql::vm::{DefaultSelectivity, Selectivity};
use teseladb_mql::{CmpOp, Filter, InItem, Pred};
use teseladb_plan::logical::intervals;
use teseladb_plan::stats::{RangeStats, collection_stats};

/// The estimate from statistics `stats`.
pub struct StatsSelectivity<'s>(pub &'s RangeStats);

/// The operator of a comparison.
fn operator(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Gt => "$gt",
        CmpOp::Gte => "$gte",
        CmpOp::Lt => "$lt",
        CmpOp::Lte => "$lte",
    }
}

impl Selectivity for StatsSelectivity<'_> {
    fn fraction(&self, path: &str, pred: &Pred) -> f64 {
        let clause = match pred {
            Pred::Eq(t) => Some(("$eq", t.to_value())),
            Pred::Cmp(op, t) => Some((operator(*op), t.to_value())),
            Pred::In(items) => items
                .iter()
                .map(|i| match i {
                    InItem::Value(t) => Some(t.to_value()),
                    InItem::Regex(_) => None,
                })
                .collect::<Option<Vec<Value>>>()
                .map(|v| ("$in", Value::Array(v))),
            Pred::Not(inner) => return 1.0 - self.fraction(path, inner),
            Pred::And(items) => return items.iter().map(|p| self.fraction(path, p)).product(),
            _ => None,
        };
        clause
            .and_then(|(op, arg)| intervals(op, &arg))
            .and_then(|(bounds, _)| self.0.fraction(path, &bounds))
            .unwrap_or_else(|| DefaultSelectivity.fraction(path, pred))
    }
}

/// `filter` with its conditions ordered by the statistics of the collection whose keys start
/// with `collection`, when a compaction published some.
pub fn ordered_by_statistics(filter: Filter, collection: &[u8]) -> Filter {
    match collection_stats(collection) {
        Some((epoch, stats)) => filter.with_selectivity(&StatsSelectivity(&stats), epoch),
        None => filter,
    }
}

#[cfg(test)]
mod tests {
    use teseladb_bson::{Document, RawDoc};
    use teseladb_mql::FilterUse;
    use teseladb_plan::stats::{forget_range, recompute_at_compaction};

    use super::*;

    fn filter(d: &Document) -> Filter {
        let bytes = d.to_bytes().expect("small");
        Filter::parse(RawDoc::from_bytes(&bytes).expect("valid"), FilterUse::Query).expect("ok")
    }

    #[test]
    fn statistics_put_the_rarest_condition_first() {
        // `common` is 1 in 99 % of the documents, `rare` is 1 in 1 %.
        let coll = b"\x01selectivity-test";
        let values = |ones: i32| -> Vec<Value> {
            (0..1000).map(|i| Value::Int32(if i < ones { 1 } else { 2 })).collect()
        };
        recompute_at_compaction(
            coll,
            7,
            1000,
            vec![("common".to_owned(), values(990)), ("rare".to_owned(), values(10))],
        );
        let f = Document::new().with("common", 1).with("rare", 1);
        // A document that fails `rare`: the steps it takes show which condition ran first.
        let doc = Document::new().with("common", 1).with("rare", 2).to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&doc).expect("valid");
        let ext = teseladb_mql::NoExtensions;
        let ctx = teseladb_mql::MatchContext { collation: None, ext: &ext, vars: None };
        let steps =
            |p: &Filter| p.program().run_counted(p.run_tree(), raw, &ctx).expect("no error");
        // Without statistics both are equalities: they keep their order.
        assert_eq!(steps(&filter(&f)), (false, 7));
        let ordered = ordered_by_statistics(filter(&f), coll);
        assert_eq!(steps(&ordered), (false, 4), "{:#?}", ordered.program());
        let s = collection_stats(coll).expect("published").1;
        let sel = StatsSelectivity(&s);
        let eq = |path: &str| match filter(&Document::new().with(path, 1)).run_tree() {
            teseladb_mql::Expr::Field { pred, .. } => sel.fraction(path, pred),
            _ => unreachable!("one field"),
        };
        assert!((eq("rare") - 0.01).abs() < 0.005, "{}", eq("rare"));
        assert!((eq("common") - 0.99).abs() < 0.005, "{}", eq("common"));
        forget_range(coll, 7);
        assert_eq!(steps(&ordered_by_statistics(filter(&f), coll)), (false, 7));
    }
}
