//! The tree of a parsed query as the reference server prints it in a planner error (`Tree: …`):
//! each `parsedQuery` (`explain` on a reference router 9.0.4, 02-11-2026) beside the tree of the
//! error of a `find` with a hint to an index that does not exist.

use teseladb_bson::extjson;
use teseladb_mql::debug_tree;

const CASES: &[(&str, &str)] = &[
    (
        r##"{"$and":[{"$or":[{"e":{"$size":{"$numberInt":"2"}}},{"c":{"$exists":true}}]},{"h":{"$elemMatch":{"k":{"$eq":{"$numberInt":"1"}}}}},{"g":{"$elemMatch":{"$gt":{"$numberInt":"1"}}}},{"a":{"$eq":{"$numberInt":"1"}}},{"b":{"$regex":"^x","$options":"i"}},{"f":{"$not":{"$gt":{"$numberInt":"3"}}}},{"t":{"$type":[{"$numberInt":"2"},{"$numberInt":"16"}]}},{"$expr":{"$eq":["$a",{"$const":{"$numberInt":"1"}}]}},{"a":{"$_internalExprEq":{"$numberInt":"1"}}}]}"##,
        "$and\n    $or\n        e $size : 2\n        c exists\n    h $elemMatch (obj)\n        k $eq 1\n    g $elemMatch (value)\n         $gt 1\n    a $eq 1\n    b regex /^x/i\n    $not\n        f $gt 3\n    t $type: { 0: 2, 1: 16 }\n    $expr {$eq: [\"$a\", {$const: 1}]}\n    a $_internalExprEq 1\n",
    ),
    (
        r##"{"$nor":[{"a":{"$eq":{"$numberInt":"1"}}},{"b":{"$eq":{"$numberInt":"2"}}}]}"##,
        "$nor\n    a $eq 1\n    b $eq 2\n",
    ),
    (
        r##"{"$and":[{"a":{"$lt":{"$numberInt":"5"}}},{"a":{"$gte":{"$numberInt":"1"}}}]}"##,
        "$and\n    a $lt 5\n    a $gte 1\n",
    ),
];

#[test]
fn the_tree_of_a_parsed_query_is_the_reference_one() {
    for (parsed, tree) in CASES {
        let parsed = extjson::parse(parsed).expect("a parsed query");
        assert_eq!(debug_tree(&parsed), *tree);
    }
}
