//! The reference server's comparison order (FR-005 to FR-007): first the value's type class, then
//! the value. Numbers by exact value across their four types; strings and symbols by their bytes or
//! by a collation; documents and arrays pair by pair, the type class before the name; binaries by
//! stored length, subtype and bytes; `DBPointers` by size and bytes. A sort without an index orders
//! `DBPointers` by size, `ObjectId` and namespace instead ([`compare_for_sort`]).

use std::cmp::Ordering;

use crate::number::{self, Num};
use crate::raw::{RawDoc, RawValue};

/// An order for strings: a collation (the consumer implements it).
pub trait Collator {
    /// The sort key of a string: keys compare byte by byte as the strings do.
    fn sort_key(&self, s: &str) -> Vec<u8>;

    /// Compares two strings.
    fn compare(&self, a: &str, b: &str) -> Ordering {
        self.sort_key(a).cmp(&self.sort_key(b))
    }
}

/// The type class of a value: equal for the four numeric types and for strings and symbols.
#[must_use]
pub fn type_class(v: &RawValue<'_>) -> u8 {
    match v {
        RawValue::MinKey => 0,
        RawValue::Undefined => 1,
        RawValue::Null => 2,
        RawValue::Int32(_) | RawValue::Int64(_) | RawValue::Double(_) | RawValue::Decimal128(_) => {
            3
        }
        RawValue::String(_) | RawValue::Symbol(_) => 4,
        RawValue::Document(_) => 5,
        RawValue::Array(_) => 6,
        RawValue::Binary { .. } => 7,
        RawValue::ObjectId(_) => 8,
        RawValue::Boolean(_) => 9,
        RawValue::DateTime(_) => 10,
        RawValue::Timestamp { .. } => 11,
        RawValue::Regex { .. } => 12,
        RawValue::DbPointer { .. } => 13,
        RawValue::JavaScript(_) => 14,
        RawValue::JavaScriptWithScope { .. } => 15,
        RawValue::MaxKey => 16,
    }
}

/// Which of the reference server's two orders: the one of queries and indexes, or the one of a sort
/// without an index, which differ only for `DBPointers` (measured on a reference router 9.0.4, 003
/// T012).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Query,
    Sort,
}

/// Compares two values; `collation` orders strings and symbols, also inside documents and arrays.
#[must_use]
pub fn compare(a: RawValue<'_>, b: RawValue<'_>, collation: Option<&dyn Collator>) -> Ordering {
    values(a, b, collation, Order::Query)
}

/// Compares two values as a sort without an index does: as [`compare`], except that `DBPointers`
/// of the same size go by `ObjectId` before their namespace, also inside documents and arrays.
#[must_use]
pub fn compare_for_sort(
    a: RawValue<'_>,
    b: RawValue<'_>,
    collation: Option<&dyn Collator>,
) -> Ordering {
    values(a, b, collation, Order::Sort)
}

fn values(
    a: RawValue<'_>,
    b: RawValue<'_>,
    collation: Option<&dyn Collator>,
    o: Order,
) -> Ordering {
    type_class(&a).cmp(&type_class(&b)).then_with(|| same_class(a, b, collation, o))
}

/// Compares two documents pair by pair: the value's type class, the name, the value; the one
/// that ends first goes first.
#[must_use]
pub fn compare_docs(a: RawDoc<'_>, b: RawDoc<'_>, collation: Option<&dyn Collator>) -> Ordering {
    docs(a, b, collation, Order::Query)
}

fn docs(a: RawDoc<'_>, b: RawDoc<'_>, collation: Option<&dyn Collator>, order: Order) -> Ordering {
    let (mut ia, mut ib) = (a.iter(), b.iter());
    loop {
        let ((ka, va), (kb, vb)) = match (ia.next(), ib.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => (x, y),
        };
        let o = type_class(&va)
            .cmp(&type_class(&vb))
            .then_with(|| ka.as_bytes().cmp(kb.as_bytes()))
            .then_with(|| same_class(va, vb, collation, order));
        if o != Ordering::Equal {
            return o;
        }
    }
}

/// Two values of the same type class.
#[expect(
    clippy::many_single_char_names,
    reason = "a and b are the two values compared, as in the reference server's comparison order"
)]
fn same_class(
    a: RawValue<'_>,
    b: RawValue<'_>,
    collation: Option<&dyn Collator>,
    order: Order,
) -> Ordering {
    use RawValue as V;
    if let (Some(x), Some(y)) = (num(&a), num(&b)) {
        return number::compare(x, y);
    }
    match (a, b) {
        (V::String(x) | V::Symbol(x), V::String(y) | V::Symbol(y)) => match collation {
            Some(c) => c.compare(x, y),
            None => x.as_bytes().cmp(y.as_bytes()),
        },
        (V::Document(x), V::Document(y)) => docs(x, y, collation, order),
        (V::Array(x), V::Array(y)) => docs(x.as_doc(), y.as_doc(), collation, order),
        (V::Binary { subtype: s1, bytes: b1 }, V::Binary { subtype: s2, bytes: b2 }) => {
            stored_len(s1, b1).cmp(&stored_len(s2, b2)).then(s1.cmp(&s2)).then_with(|| b1.cmp(b2))
        }
        (V::ObjectId(x), V::ObjectId(y)) => x.cmp(&y),
        (V::Boolean(x), V::Boolean(y)) => x.cmp(&y),
        (V::DateTime(x), V::DateTime(y)) => x.cmp(&y),
        (V::Timestamp { time: t1, increment: i1 }, V::Timestamp { time: t2, increment: i2 }) => {
            (t1, i1).cmp(&(t2, i2))
        }
        (V::Regex { pattern: p1, options: o1 }, V::Regex { pattern: p2, options: o2 }) => {
            p1.as_bytes().cmp(p2.as_bytes()).then_with(|| o1.as_bytes().cmp(o2.as_bytes()))
        }
        (V::DbPointer { namespace: n1, id: i1 }, V::DbPointer { namespace: n2, id: i2 }) => {
            let by_name = || n1.as_bytes().cmp(n2.as_bytes());
            let size = n1.len().cmp(&n2.len());
            match order {
                Order::Query => size.then_with(by_name).then(i1.cmp(&i2)),
                Order::Sort => size.then(i1.cmp(&i2)).then_with(by_name),
            }
        }
        (V::JavaScript(x), V::JavaScript(y)) => x.as_bytes().cmp(y.as_bytes()),
        (
            V::JavaScriptWithScope { code: c1, scope: s1 },
            V::JavaScriptWithScope { code: c2, scope: s2 },
        ) => c1.as_bytes().cmp(c2.as_bytes()).then_with(|| docs(s1, s2, None, order)),
        // MinKey, undefined, null and MaxKey: one value each.
        _ => Ordering::Equal,
    }
}

/// The value of a number, `None` for anything else.
fn num(v: &RawValue<'_>) -> Option<Num> {
    match *v {
        RawValue::Int32(n) => Some(Num::int(i64::from(n))),
        RawValue::Int64(n) => Some(Num::int(n)),
        RawValue::Double(x) => Some(Num::double(x)),
        RawValue::Decimal128(d) => Some(Num::decimal(d)),
        _ => None,
    }
}

/// The length BSON stores for binary data: subtype 2 counts its inner length too.
fn stored_len(subtype: u8, bytes: &[u8]) -> usize {
    bytes.len() + if subtype == crate::types::BINARY_OLD { 4 } else { 0 }
}
