#![allow(unsafe_code)]
//! Shared-pointer behaviour not reached by shared_ptr_test: the counts seen from the object
//! (`EnableLwSharedFromThis::use_count`, `EnableSharedFromThis::use_count`), `owned`, the
//! orderings, hashing and printing (`null` for a null pointer, as the original's operator<<),
//! the custom deleter, the unique owner left by `release`, and the casts.
#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use teseladb::engine::core::shared_ptr::{
    EnableLwSharedFromThis, EnableSharedFromThis, LwSharedPtr, LwSharedPtrDeleter, SharedFromThis,
    SharedPtr, const_pointer_cast, dynamic_pointer_cast, fmt_ptr, fmt_ptr_lw, make_lw_shared,
    make_shared, make_shared_esft,
};
use teseladb::engine::util::indirect::NullablePointer;

fn hash_of<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

struct Node {
    v: i32,
}
impl EnableLwSharedFromThis for Node {}

#[test]
fn lw_counts_seen_from_the_object_and_owned() {
    let a = make_lw_shared(Node { v: 7 });
    assert!(a.owned());
    // SAFETY: the pointer comes from the LwSharedPtr that owns the object.
    assert_eq!(unsafe { <Node as EnableLwSharedFromThis>::use_count(a.get()) }, 1);
    let b = a.clone();
    assert!(!a.owned() && !b.owned());
    // SAFETY: as above.
    assert_eq!(unsafe { <Node as EnableLwSharedFromThis>::use_count(a.get()) }, 2);
    drop(b);
    assert!(a.owned());
    assert_eq!(a.v, 7);
    let null = LwSharedPtr::<Node>::null();
    assert!(!null.owned(), "a null pointer owns nothing");
    assert_eq!(null.use_count(), 0);
}

#[test]
fn lw_order_hash_and_printing() {
    let a = make_lw_shared(1);
    let b = make_lw_shared(2);
    let null = LwSharedPtr::<i32>::null();
    // Ordered by address, null first (the original compares the raw pointers).
    assert!(null < a && null < b);
    assert_eq!(a.cmp(&a.clone()), std::cmp::Ordering::Equal);
    assert_eq!(a < b, a.get() < b.get());
    assert_eq!(a.partial_cmp(&b), Some(a.cmp(&b)));
    // Hashed by the object address: copies hash alike, other objects differently.
    assert_eq!(hash_of(&a), hash_of(&a.clone()));
    assert_eq!(hash_of(&a), hash_of(&a.get()));
    assert_ne!(hash_of(&a), hash_of(&b));
    assert_eq!(format!("{a}"), "1");
    assert_eq!(format!("{b:?}"), "2");
    assert_eq!(format!("{null}"), "null");
    assert_eq!(format!("{null:?}"), "null");
    assert_eq!(fmt_ptr_lw(&a), a.get() as *const ());
    assert!(fmt_ptr_lw(&null).is_null());
    assert_eq!(NullablePointer::get(&a), Some(&1));
    assert_eq!(NullablePointer::get(&null), None);
}

#[test]
#[should_panic(expected = "dereferencing a null shared pointer")]
fn lw_null_deref_panics() {
    let null = LwSharedPtr::<i32>::null();
    let v: i32 = *null;
    std::hint::black_box(v);
}

#[test]
#[should_panic(expected = "dereferencing a null shared pointer")]
fn shared_null_deref_panics() {
    let null = SharedPtr::<i32>::null();
    let v: i32 = *null;
    std::hint::black_box(v);
}

thread_local! {
    static DISPOSED: Cell<i32> = const { Cell::new(0) };
}

struct CountingDeleter;
impl LwSharedPtrDeleter<Rc<i32>> for CountingDeleter {
    fn dispose(p: *mut Rc<i32>) {
        DISPOSED.with(|d| d.set(d.get() + 1));
        // SAFETY: `p` was released from its last owner and is disposed once.
        unsafe { LwSharedPtr::<Rc<i32>>::dispose(p) };
    }
}

#[test]
fn release_hands_over_the_last_owner_and_a_custom_deleter_disposes() {
    let witness = Rc::new(5);
    let mut a = make_lw_shared(witness.clone());
    let mut b = a.clone();
    assert!(b.release().is_none(), "another owner remains");
    assert!(!b.is_some());
    assert_eq!(a.use_count(), 1);
    let mut unique = a.release().expect("last owner");
    assert!(!a.is_some());
    // The unique owner reads and writes the object.
    assert_eq!(**unique, 5);
    *unique = Rc::new(6);
    assert_eq!(Rc::strong_count(&witness), 1, "the old value was dropped by the write");
    let raw = unique.into_raw();
    LwSharedPtr::<Rc<i32>>::dispose_with::<CountingDeleter>(raw);
    assert_eq!(DISPOSED.with(Cell::get), 1);
}

#[test]
fn dropping_the_unique_owner_destroys_the_object() {
    let witness = Rc::new(1);
    let mut a = make_lw_shared(witness.clone());
    assert_eq!(Rc::strong_count(&witness), 2);
    let unique = a.release().expect("last owner");
    assert_eq!(Rc::strong_count(&witness), 2, "not yet destroyed");
    drop(unique);
    assert_eq!(Rc::strong_count(&witness), 1);
}

struct Esft {
    _esft: EnableSharedFromThis,
    v: i32,
}
teseladb::shared_from_this_impl!(Esft { _esft });

#[test]
fn shared_from_this_counts_through_the_embedded_counter() {
    let p = make_shared_esft(Esft { _esft: EnableSharedFromThis::default(), v: 3 });
    assert_eq!(p.esft().use_count(), 1);
    // SAFETY: the pointer comes from a SharedPtr of an object made by make_shared_esft.
    let q = unsafe { SharedPtr::shared_from_this(p.get().unwrap().as_ptr()) };
    assert_eq!(p.esft().use_count(), 2);
    assert_eq!(q.use_count(), 2);
    assert_eq!(q.v, 3);
    assert!(p == q);
    drop(q);
    assert_eq!(p.use_count(), 1);
}

trait Shape {
    fn area(&self) -> i32;
    fn as_square(&self) -> Option<&Square>;
}
#[derive(Debug)]
struct Square(i32);
impl Shape for Square {
    fn area(&self) -> i32 {
        self.0 * self.0
    }
    fn as_square(&self) -> Option<&Square> {
        Some(self)
    }
}
struct Circle;
impl Shape for Circle {
    fn area(&self) -> i32 {
        3
    }
    fn as_square(&self) -> Option<&Square> {
        None
    }
}

#[test]
fn casts_share_the_count_and_failed_downcasts_are_null() {
    let sq = make_shared(Square(4));
    // SAFETY: the cast returns the same object as a trait object.
    let shape: SharedPtr<dyn Shape> = unsafe {
        teseladb::engine::core::shared_ptr::static_pointer_cast(&sq, |p| p as *mut dyn Shape)
    };
    assert_eq!(shape.area(), 16);
    assert_eq!(sq.use_count(), 2);
    let back = dynamic_pointer_cast(&shape, |s| s.as_square());
    assert!(back.is_some());
    assert_eq!(back.use_count(), 3);
    assert_eq!(format!("{back:?}"), "Square(4)");
    let copy = const_pointer_cast(&back);
    assert_eq!(copy.use_count(), 4);
    assert!(copy == sq);
    let circle = make_shared(Circle);
    // SAFETY: as above.
    let cshape: SharedPtr<dyn Shape> = unsafe {
        teseladb::engine::core::shared_ptr::static_pointer_cast(&circle, |p| p as *mut dyn Shape)
    };
    let none = dynamic_pointer_cast(&cshape, |s| s.as_square());
    assert!(!none.is_some());
    assert_eq!(none.use_count(), 0, "a failed cast does not share the count");
    assert_eq!(circle.use_count(), 2);
    // A null source gives a null result through every cast.
    let null = SharedPtr::<Square>::null();
    let n2 = dynamic_pointer_cast(&null, |s: &Square| Some(s));
    assert!(!n2.is_some());
    // SAFETY: as above.
    let n3: SharedPtr<dyn Shape> = unsafe {
        teseladb::engine::core::shared_ptr::static_pointer_cast(&null, |p| p as *mut dyn Shape)
    };
    assert!(!n3.is_some());
    assert_eq!(n3.use_count(), 0);
}

#[test]
fn shared_order_hash_and_printing() {
    let a = make_shared(10);
    let b = make_shared(20);
    let null = SharedPtr::<i32>::null();
    assert!(null < a);
    assert_eq!(a.partial_cmp(&b), Some(a.cmp(&b)));
    assert_eq!(a.cmp(&a.clone()), std::cmp::Ordering::Equal);
    assert_eq!(hash_of(&a), hash_of(&a.clone()));
    assert_ne!(hash_of(&a), hash_of(&b));
    assert_eq!(format!("{a}"), "10");
    assert_eq!(format!("{b:?}"), "20");
    assert_eq!(format!("{null}"), "null");
    assert_eq!(format!("{null:?}"), "null");
    assert_eq!(fmt_ptr(&a), a.get().unwrap().as_ptr() as *const ());
    assert!(fmt_ptr(&null).is_null());
    assert_eq!(null.use_count(), 0);
    assert_eq!(NullablePointer::get(&a), Some(&10));
    assert_eq!(NullablePointer::get(&null), None);
}
