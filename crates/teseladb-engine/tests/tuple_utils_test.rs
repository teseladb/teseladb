//! Tests of the tuple utilities: mapping values and types, iterating over the elements, and
//! filtering elements by type.

#![cfg(target_os = "linux")]

use std::any::TypeId;
use std::fmt::{self, Write as _};

use teseladb::engine::util::tuple_utils::{
    FalseType, FilterClass, TrueType, TupleFunction, TupleMapFunction, TupleMapTypes, TypeMap,
    tuple_filter_by_type, tuple_for_each, tuple_map,
};

fn is_same<A: 'static, B: 'static>() -> bool {
    TypeId::of::<A>() == TypeId::of::<B>()
}

/// Maps each element `e` to the pair `(e, e)`.
struct Pair;

impl<T: Clone> TupleMapFunction<T> for Pair {
    type Output = (T, T);
    fn call(&mut self, e: T) -> (T, T) {
        (e.clone(), e)
    }
}

#[test]
fn map() {
    let pairs = tuple_map((10, 5.5, true), Pair);

    assert!(pairs == ((10, 10), (5.5, 5.5), (true, true)));
}

/// Writes each element to the output string.
struct Print<'a> {
    os: &'a mut String,
}

impl<T: fmt::Display> TupleFunction<T> for Print<'_> {
    fn call(&mut self, e: T) {
        let _ = write!(self.os, "{}", e);
    }
}

#[test]
fn for_each() {
    let mut os = String::new();

    tuple_for_each(('a', 10, false, 5.4), Print { os: &mut os });

    // `false` prints as 0.
    assert_eq!(os.replace("false", "0"), "a1005.4");
}

/// A type map: identity except for the two types it maps to others.
struct TransformType;

impl TypeMap<TransformType> for bool {
    type Type = i32;
}
impl TypeMap<TransformType> for f64 {
    type Type = u8;
}
impl TypeMap<TransformType> for &'static str {
    type Type = &'static str;
}

#[test]
fn map_types() {
    type BeforeTuple = (f64, bool, &'static str);
    type AfterTuple = <BeforeTuple as TupleMapTypes<TransformType>>::Type;

    assert!(is_same::<AfterTuple, (u8, i32, &'static str)>());
}

//
// Strip all `bool` fields.
//

struct KeepType;

impl FilterClass<bool> for KeepType {
    type Value = FalseType;
}
impl FilterClass<i32> for KeepType {
    type Value = TrueType;
}
impl FilterClass<f64> for KeepType {
    type Value = TrueType;
}
impl FilterClass<char> for KeepType {
    type Value = TrueType;
}

#[test]
fn filter_by_type() {
    type BeforeTuple = (bool, i32, bool, f64, bool, char);

    let t = tuple_filter_by_type::<KeepType, BeforeTuple>((true, 10, false, 5.5, true, 'a'));

    fn type_of<T: 'static>(_: &T) -> TypeId {
        TypeId::of::<T>()
    }
    assert!(type_of(&t) == TypeId::of::<(i32, f64, char)>());
    assert!(t == (10, 5.5, 'a'));
}
