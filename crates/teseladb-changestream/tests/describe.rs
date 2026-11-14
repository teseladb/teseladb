//! The description of operator updates against what a reference replica set member 9.0.4 logs for
//! them (measured on 31-10-2026 through a change stream with `showExpandedEvents`): each case is
//! the document before, the operators' touches, and the `updateDescription` of its event.

use teseladb_bson::{Document, extjson};
use teseladb_changestream::describe::{Touch, describe};

fn doc(text: &str) -> Document {
    extjson::parse(text).unwrap()
}

fn s(p: &str) -> String {
    p.to_owned()
}

/// Checks the description of `touches` taking `old` to `new` against the measured one.
fn check(old: &str, new: &str, touches: &[Touch], position: Option<usize>, want: &str) {
    let got = describe(&doc(old), &doc(new), touches, position).to_document(true);
    assert_eq!(got, doc(want), "{old} -> {new} with {touches:?}");
}

const NONE: &str = r#"{"updatedFields": {}, "removedFields": [], "truncatedArrays": [], "disambiguatedPaths": {}}"#;

fn upd(fields: &str) -> String {
    format!(
        r#"{{"updatedFields": {fields}, "removedFields": [], "truncatedArrays": [], "disambiguatedPaths": {{}}}}"#
    )
}

#[test]
fn values_set_give_their_paths_or_the_first_new_field() {
    use Touch::*;
    check(
        r#"{"_id": 1, "a": 1, "s": "x"}"#,
        r#"{"_id": 1, "a": 2, "s": "x"}"#,
        &[Assign(s("a"))],
        None,
        &upd(r#"{"a": 2}"#),
    );
    check(
        r#"{"_id": 2, "b": {"c": 1, "d": 2}}"#,
        r#"{"_id": 2, "b": {"c": 5, "d": 2}}"#,
        &[Assign(s("b.c"))],
        None,
        &upd(r#"{"b.c": 5}"#),
    );
    check(
        r#"{"_id": 3, "b": {"c": 1}}"#,
        r#"{"_id": 3, "b": {"z": 1}}"#,
        &[Assign(s("b"))],
        None,
        &upd(r#"{"b": {"z": 1}}"#),
    );
    check(
        r#"{"_id": 1, "a": 1}"#,
        r#"{"_id": 1, "a": 1, "x": {"y": 1}}"#,
        &[Assign(s("x.y"))],
        None,
        &upd(r#"{"x": {"y": 1}}"#),
    );
    check(
        r#"{"_id": 9, "a": {"b": 1}}"#,
        r#"{"_id": 9, "a": {"b": 1, "c": {"d": 1}}}"#,
        &[Assign(s("a.c.d"))],
        None,
        &upd(r#"{"a.c": {"d": 1}}"#),
    );
    check(
        r#"{"_id": 20, "a": {"b": {"c": 1}}}"#,
        r#"{"_id": 20, "a": {"b": {"c": 1, "d": 2}}}"#,
        &[Assign(s("a.b.d"))],
        None,
        &upd(r#"{"a.b.d": 2}"#),
    );
    check(
        r#"{"_id": 17, "a": 1}"#,
        r#"{"_id": 17, "a": 1, "n": 1}"#,
        &[Assign(s("n"))],
        None,
        &upd(r#"{"n": 1}"#),
    );
    check(
        r#"{"_id": 11, "arr": [1, 2, 3]}"#,
        r#"{"_id": 11, "arr": [1, 9, 3]}"#,
        &[Assign(s("arr.1"))],
        None,
        &upd(r#"{"arr.1": 9}"#),
    );
    check(
        r#"{"_id": 12, "arr": [1, 2, 3]}"#,
        r#"{"_id": 12, "arr": [1, 2, 3, null, null, 9]}"#,
        &[Assign(s("arr.5"))],
        None,
        &upd(r#"{"arr.5": 9}"#),
    );
    check(
        r#"{"_id": 14, "arr": [{"x": 1}, {"x": 2}]}"#,
        r#"{"_id": 14, "arr": [{"x": 1}, {"x": 5}]}"#,
        &[Assign(s("arr.1.x"))],
        None,
        &upd(r#"{"arr.1.x": 5}"#),
    );
    check(
        r#"{"_id": 4, "arr": [1, 2, 3]}"#,
        r#"{"_id": 4, "arr": [1, 7, 3]}"#,
        &[Assign(s("arr.$"))],
        Some(1),
        &upd(r#"{"arr.1": 7}"#),
    );
    check(
        r#"{"_id": 14, "a": {"b": 1, "c": 2}}"#,
        r#"{"_id": 14, "a": {"c": 2, "b": 1}}"#,
        &[Assign(s("a"))],
        None,
        &upd(r#"{"a": {"c": 2, "b": 1}}"#),
    );
    // A value that does not change is not in it.
    check(r#"{"_id": 23, "a": 1}"#, r#"{"_id": 23, "a": 1}"#, &[Assign(s("a"))], None, NONE);
}

#[test]
fn existing_fields_come_sorted_then_the_new_ones() {
    use Touch::*;
    let touches = [Assign(s("z")), Assign(s("a")), Assign(s("b")), Unset(s("m"))];
    check(
        r#"{"_id": 5, "z": 1, "a": 1, "m": 1}"#,
        r#"{"_id": 5, "z": 2, "a": 2, "b": 3}"#,
        &touches,
        None,
        r#"{"updatedFields": {"a": 2, "z": 2, "b": 3}, "removedFields": ["m"], "truncatedArrays": [], "disambiguatedPaths": {}}"#,
    );
}

#[test]
fn removed_and_renamed_fields() {
    use Touch::*;
    check(
        r#"{"_id": 4, "b": {"c": 1, "d": 2}}"#,
        r#"{"_id": 4, "b": {"c": 1}}"#,
        &[Unset(s("b.d"))],
        None,
        r#"{"updatedFields": {}, "removedFields": ["b.d"], "truncatedArrays": [], "disambiguatedPaths": {}}"#,
    );
    check(
        r#"{"_id": 5, "a": 1}"#,
        r#"{"_id": 5}"#,
        &[Unset(s("a"))],
        None,
        r#"{"updatedFields": {}, "removedFields": ["a"], "truncatedArrays": [], "disambiguatedPaths": {}}"#,
    );
    check(
        r#"{"_id": 15, "a": 1, "b": 2}"#,
        r#"{"_id": 15, "b": 2, "z": 1}"#,
        &[Rename(s("a"), s("z"))],
        None,
        r#"{"updatedFields": {"z": 1}, "removedFields": ["a"], "truncatedArrays": [], "disambiguatedPaths": {}}"#,
    );
    // In an array, the element becomes null.
    check(
        r#"{"_id": 13, "arr": [1, 2, 3]}"#,
        r#"{"_id": 13, "arr": [1, null, 3]}"#,
        &[Unset(s("arr.1"))],
        None,
        &upd(r#"{"arr.1": null}"#),
    );
}

#[test]
fn appended_elements_give_their_positions_and_rewritten_arrays_the_whole_array() {
    use Touch::*;
    check(
        r#"{"_id": 6, "arr": [1, 2, 3]}"#,
        r#"{"_id": 6, "arr": [1, 2, 3, 4]}"#,
        &[Append(s("arr"))],
        None,
        &upd(r#"{"arr.3": 4}"#),
    );
    check(
        r#"{"_id": 7, "arr": [1, 2, 3]}"#,
        r#"{"_id": 7, "arr": [1, 2, 3, 4, 5]}"#,
        &[Append(s("arr"))],
        None,
        &upd(r#"{"arr.3": 4, "arr.4": 5}"#),
    );
    check(
        r#"{"_id": 18, "arr": [1, 2]}"#,
        r#"{"_id": 18, "arr": [1, 2, 3]}"#,
        &[Append(s("arr"))],
        None,
        &upd(r#"{"arr.2": 3}"#),
    );
    check(
        r#"{"_id": 22, "arr": [[1, 2], [3]]}"#,
        r#"{"_id": 22, "arr": [[1, 2, 7], [3]]}"#,
        &[Append(s("arr.0"))],
        None,
        &upd(r#"{"arr.0.2": 7}"#),
    );
    check(
        r#"{"_id": 8, "arr": [1, 2, 3, 4]}"#,
        r#"{"_id": 8, "arr": [1, 2, 3]}"#,
        &[Rewrite(s("arr"))],
        None,
        &upd(r#"{"arr": [1, 2, 3]}"#),
    );
    check(
        r#"{"_id": 9, "arr": [1, 2, 3, 4]}"#,
        r#"{"_id": 9, "arr": [2, 3, 4]}"#,
        &[Rewrite(s("arr"))],
        None,
        &upd(r#"{"arr": [2, 3, 4]}"#),
    );
    check(
        r#"{"_id": 10, "arr": [1, 2, 3, 2]}"#,
        r#"{"_id": 10, "arr": [1, 3]}"#,
        &[Rewrite(s("arr"))],
        None,
        &upd(r#"{"arr": [1, 3]}"#),
    );
    check(
        r#"{"_id": 6, "arr": [3, 1, 2]}"#,
        r#"{"_id": 6, "arr": [0, 1, 2, 3]}"#,
        &[Rewrite(s("arr"))],
        None,
        &upd(r#"{"arr": [0, 1, 2, 3]}"#),
    );
    check(
        r#"{"_id": 21, "arr": [1, 2, 3, 4, 5]}"#,
        r#"{"_id": 21, "arr": [1, 2]}"#,
        &[Assign(s("arr"))],
        None,
        &upd(r#"{"arr": [1, 2]}"#),
    );
    // All the elements, or those of an array filter: the whole array.
    check(
        r#"{"_id": 2, "arr": [1, 2, 3]}"#,
        r#"{"_id": 2, "arr": [11, 12, 13]}"#,
        &[Assign(s("arr.$[]"))],
        None,
        &upd(r#"{"arr": [11, 12, 13]}"#),
    );
    check(
        r#"{"_id": 3, "arr": [1, 2, 3]}"#,
        r#"{"_id": 3, "arr": [1, 0, 0]}"#,
        &[Assign(s("arr.$[e]"))],
        None,
        &upd(r#"{"arr": [1, 0, 0]}"#),
    );
}

#[test]
fn paths_through_fields_named_like_numbers_are_disambiguated() {
    use Touch::*;
    check(
        r#"{"_id": 13, "a": {"1": {"x": 1}}}"#,
        r#"{"_id": 13, "a": {"1": {"x": 2}}}"#,
        &[Assign(s("a.1.x"))],
        None,
        r#"{"updatedFields": {"a.1.x": 2}, "removedFields": [], "truncatedArrays": [], "disambiguatedPaths": {"a.1.x": ["a", "1", "x"]}}"#,
    );
    check(
        r#"{"_id": 10, "a": {"0": 1}}"#,
        r#"{"_id": 10, "a": {}}"#,
        &[Unset(s("a.0"))],
        None,
        r#"{"updatedFields": {}, "removedFields": ["a.0"], "truncatedArrays": [], "disambiguatedPaths": {"a.0": ["a", "0"]}}"#,
    );
    check(
        r#"{"_id": 11, "arr": [{"0": 1}]}"#,
        r#"{"_id": 11, "arr": [{"0": 2}]}"#,
        &[Assign(s("arr.0.0"))],
        None,
        r#"{"updatedFields": {"arr.0.0": 2}, "removedFields": [], "truncatedArrays": [], "disambiguatedPaths": {"arr.0.0": ["arr", {"$numberInt": "0"}, "0"]}}"#,
    );
}

#[test]
fn touches_that_change_nothing_or_overlap_give_each_path_once() {
    use Touch::*;
    // `$unset` and `$rename` of a missing field, `$addToSet` of an element already there.
    check(r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 1}"#, &[Unset(s("x"))], None, NONE);
    check(r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 1}"#, &[Rename(s("x"), s("y"))], None, NONE);
    check(
        r#"{"_id": 1, "t": [1, 2]}"#,
        r#"{"_id": 1, "t": [1, 2]}"#,
        &[Append(s("t"))],
        None,
        NONE,
    );
    // A path that an array filter stops at its first part names nothing.
    check(r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 1}"#, &[Assign(s("$[]"))], None, NONE);
    check(r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 1}"#, &[Unset(s("$[e]"))], None, NONE);
    // A field given whole hides the paths inside it, and a path touched twice is named once.
    check(
        r#"{"_id": 1, "a": {}}"#,
        r#"{"_id": 1, "a": {"b": 1, "c": 2}}"#,
        &[Assign(s("a")), Assign(s("a.b")), Assign(s("a"))],
        None,
        &upd(r#"{"a": {"b": 1, "c": 2}}"#),
    );
    // A path the new document no longer has is named by the field that replaced it.
    check(
        r#"{"_id": 1, "a": {"b": 1}}"#,
        r#"{"_id": 1, "a": 5}"#,
        &[Assign(s("a.b")), Assign(s("a"))],
        None,
        &upd(r#"{"a": 5}"#),
    );
    // Elements that are not added at the end make the whole array.
    check(
        r#"{"_id": 1, "t": [1, 2]}"#,
        r#"{"_id": 1, "t": [3, 1, 2]}"#,
        &[Append(s("t"))],
        None,
        &upd(r#"{"t": [3, 1, 2]}"#),
    );
    check(
        r#"{"_id": 1, "t": [1, 2]}"#,
        r#"{"_id": 1, "t": [1]}"#,
        &[Append(s("t"))],
        None,
        &upd(r#"{"t": [1]}"#),
    );
}
