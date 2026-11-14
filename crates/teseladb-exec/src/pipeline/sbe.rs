//! Which stage specifications the reference server's slot-based engine (SBE) can run, measured on a reference router
//! 9.0.4 by the `explainVersion` a shard gives (`2` for SBE, `1` for the classic engine): a
//! pipeline goes to the classic engine from the first stage that names one of the operators
//! below, whatever stage or position holds it. What a pipeline runs in SBE decides how `$group`
//! and its accumulators break ties and which errors some operators give.

use teseladb_bson::{RawDoc, RawValue};

/// The expression operators SBE does not run (each measured with field arguments inside a
/// `$project`, a `$group` key, an accumulator's argument and `$expr`).
const CLASSIC_EXPRESSIONS: [&str; 58] = [
    "$allElementsTrue",
    "$arrayElemAt",
    "$avg",
    "$binarySize",
    "$bitAnd",
    "$bitNot",
    "$bitOr",
    "$bitXor",
    "$convert",
    "$deserializeEJSON",
    "$filter",
    "$first",
    "$firstN",
    "$getField",
    "$hash",
    "$hexHash",
    "$indexOfArray",
    "$last",
    "$lastN",
    "$log",
    "$map",
    "$max",
    "$maxN",
    "$median",
    "$mergeObjects",
    "$meta",
    "$min",
    "$minN",
    "$percentile",
    "$reduce",
    "$replaceAll",
    "$replaceOne",
    "$serializeEJSON",
    "$setField",
    "$similarityCosine",
    "$similarityDotProduct",
    "$similarityEuclidean",
    "$size",
    "$slice",
    "$split",
    "$stdDevPop",
    "$stdDevSamp",
    "$subtype",
    "$sum",
    "$toArray",
    "$toBool",
    "$toDate",
    "$toDecimal",
    "$toDouble",
    "$toHashedIndexKey",
    "$toInt",
    "$toLong",
    "$toObject",
    "$toObjectId",
    "$toString",
    "$toUUID",
    "$type",
    "$unsetField",
];

/// The accumulators SBE does not run.
const CLASSIC_ACCUMULATORS: [&str; 2] = ["$median", "$percentile"];

/// The query operators SBE does not run in `$match`.
const CLASSIC_QUERY_OPERATORS: [&str; 12] = [
    "$jsonSchema",
    "$alwaysFalse",
    "$where",
    "$text",
    "$bitsAllSet",
    "$bitsAnySet",
    "$bitsAllClear",
    "$bitsAnyClear",
    "$geoWithin",
    "$geoIntersects",
    "$near",
    "$nearSphere",
];

/// Whether the specification `arg` of stage `name` names only operators SBE runs (the stage
/// itself is judged apart).
pub(super) fn spec_runs_in_sbe(name: &str, arg: RawValue<'_>) -> bool {
    match (name, arg) {
        ("$match", RawValue::Document(d)) => filter_in_sbe(d),
        ("$group", RawValue::Document(d)) => d.iter().all(|(k, v)| match k {
            "_id" => expression_in_sbe(v),
            _ => accumulator_in_sbe(v),
        }),
        ("$bucket", RawValue::Document(d)) => d.iter().all(|(k, v)| match k {
            "output" => match v {
                RawValue::Document(o) => o.iter().all(|(_, acc)| accumulator_in_sbe(acc)),
                _ => true,
            },
            _ => expression_in_sbe(v),
        }),
        ("$lookup", RawValue::Document(d)) => d.get("let").is_none_or(expression_in_sbe),
        (
            "$project" | "$addFields" | "$set" | "$replaceRoot" | "$replaceWith" | "$sortByCount"
            | "$fill",
            arg,
        ) => expression_in_sbe(arg),
        _ => true,
    }
}

/// Whether expression `v` names only operators SBE runs; a `$literal`'s value is no
/// expression.
fn expression_in_sbe(v: RawValue<'_>) -> bool {
    match v {
        RawValue::Document(d) => match d.iter().next() {
            Some(("$literal", _)) => true,
            Some((op, arg)) if op.starts_with('$') => {
                !CLASSIC_EXPRESSIONS.contains(&op) && expression_in_sbe(arg)
            }
            _ => d.iter().all(|(_, x)| expression_in_sbe(x)),
        },
        RawValue::Array(a) => a.iter().all(expression_in_sbe),
        _ => true,
    }
}

/// Whether the accumulator `{"$op": argument}` of a `$group` or `$bucket` runs in SBE.
fn accumulator_in_sbe(v: RawValue<'_>) -> bool {
    let RawValue::Document(d) = v else { return true };
    d.iter().all(|(op, arg)| !CLASSIC_ACCUMULATORS.contains(&op) && expression_in_sbe(arg))
}

/// Whether filter `d` uses only query operators SBE runs (its `$expr`, only such expressions).
fn filter_in_sbe(d: RawDoc<'_>) -> bool {
    d.iter().all(|(k, v)| match k {
        "$expr" => expression_in_sbe(v),
        "$and" | "$or" | "$nor" => match v {
            RawValue::Array(a) => a.iter().all(|x| match x {
                RawValue::Document(f) => filter_in_sbe(f),
                _ => true,
            }),
            _ => true,
        },
        k if CLASSIC_QUERY_OPERATORS.contains(&k) => false,
        _ => condition_in_sbe(v),
    })
}

/// Whether the condition on a field (a value, or a document of query operators) runs in SBE.
fn condition_in_sbe(v: RawValue<'_>) -> bool {
    let RawValue::Document(d) = v else { return true };
    d.iter().all(|(op, x)| match op {
        op if CLASSIC_QUERY_OPERATORS.contains(&op) => false,
        "$not" | "$elemMatch" => match x {
            RawValue::Document(f) => filter_in_sbe(f) && condition_in_sbe(x),
            _ => true,
        },
        _ => true,
    })
}
