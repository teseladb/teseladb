//! Tests of `WeakPtr`: empty and reset pointers, copies and moves, conversion to a base
//! type, and several weak pointers outliving or dying before the object. The owned object
//! is a pinned box, and each weak pointer is a pinned slot: `weak_from_this(x, wp)` links
//! `wp`, `assign(&o)` copies, `assign_from(o)` moves and `clear()` empties it.

#![cfg(target_os = "linux")]

use std::pin::{Pin, pin};
use std::ptr::NonNull;

use teseladb::engine::core::weak_ptr::{WeakPtr, WeaklyReferencable, weak_from_this};
use teseladb::weakly_referencable;

/// A weakly referencable object.
#[derive(Default)]
struct MyClass {
    weak: WeaklyReferencable<MyClass>,
}

weakly_referencable!(MyClass { weak });

type Owning<T> = Option<Pin<Box<T>>>;

/// A new owned object.
fn new_myclass() -> Owning<MyClass> {
    Some(Box::pin(MyClass::default()))
}

/// Links the slot `wp` to the object of `owning_ptr`.
fn wft<T: teseladb::engine::core::weak_ptr::WeaklyReferencableObject>(
    owning_ptr: &mut Owning<T>,
    wp: Pin<&mut WeakPtr<T>>,
) {
    let Some(o) = owning_ptr.as_mut() else { panic!("dereferencing a null owning_ptr") };
    weak_from_this(o.as_mut(), wp);
}

/// The address of the owned object.
fn ptr_of<T>(o: &Owning<T>) -> *mut T {
    o.as_deref().map_or(std::ptr::null_mut(), |r| std::ptr::from_ref(r).cast_mut())
}

// The object and the weak pointer have a default value, and moves cannot fail.
fn _static_checks() {
    fn is_default<T: Default>() {}
    is_default::<MyClass>();
    is_default::<WeakPtr<MyClass>>();
}

#[test]
fn test_weak_ptr_is_empty_when_default_initialized() {
    let wp = WeakPtr::<MyClass>::default();
    assert!(!wp.is_some());
}

#[test]
fn test_weak_ptr_is_reset() {
    let mut owning_ptr = new_myclass();
    let mut wp = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp.as_mut());
    assert!(wp.is_some());
    assert!(wp.get() == ptr_of(&owning_ptr));
    owning_ptr = None;
    assert!(owning_ptr.is_none());
    assert!(!wp.is_some());
}

#[test]
fn test_const_weak_ptr() {
    let mut owning_ptr = new_myclass();

    // A weak pointer that is only read through.
    let mut cwptr = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, cwptr.as_mut());
    assert!(cwptr.is_some());
    owning_ptr = None;
    assert!(owning_ptr.is_none());
    assert!(!cwptr.is_some());
}

/// The base trait of [`MyIClass`].
trait BaseClass {}

/// A weakly referencable object that converts to [`BaseClass`].
#[derive(Default)]
struct MyIClass {
    weak: WeaklyReferencable<MyIClass>,
}

weakly_referencable!(MyIClass { weak });

impl BaseClass for MyIClass {}

teseladb::weak_ptr_target!(dyn BaseClass);

#[test]
fn test_base_class_weak_ptr() {
    let mut owning_ptr: Owning<MyIClass> = Some(Box::pin(MyIClass::default()));

    // `get_checker` takes the weak pointer to the base by value and moves it into the checker;
    // a linked weak pointer does not move, so it travels in a pinned box.
    let get_checker =
        |p: Pin<Box<WeakPtr<dyn BaseClass>>>| move |v: bool| assert_eq!(p.is_some(), v);
    // The weak pointer to the object is converted to a weak pointer to its base
    // by a converting move.
    let mut wp = pin!(WeakPtr::<MyIClass>::new());
    wft(&mut owning_ptr, wp.as_mut());
    let mut p = Box::pin(WeakPtr::<dyn BaseClass>::new());
    p.as_mut().take_from(wp.as_mut(), |d| d as NonNull<dyn BaseClass>);
    let checker = get_checker(p);
    checker(true);
    owning_ptr = None;
    assert!(owning_ptr.is_none());
    checker(false);
}

#[test]
fn test_weak_ptr_can_be_moved() {
    let mut owning_ptr = new_myclass();
    let mut wp1 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp1.as_mut());
    let mut wp2 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp2.as_mut());
    let mut wp3 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp3.as_mut());

    let mut wp3_moved = pin!(WeakPtr::<MyClass>::new());
    wp3_moved.as_mut().assign_from(wp3.as_mut());
    let mut wp1_moved = pin!(WeakPtr::<MyClass>::new());
    wp1_moved.as_mut().assign_from(wp1.as_mut());
    let mut wp2_moved = pin!(WeakPtr::<MyClass>::new());
    wp2_moved.as_mut().assign_from(wp2.as_mut());
    assert!(!wp1.is_some());
    assert!(!wp2.is_some());
    assert!(!wp3.is_some());
    assert!(wp1_moved.is_some());
    assert!(wp2_moved.is_some());
    assert!(wp3_moved.is_some());
    assert!(wp1_moved.get() == ptr_of(&owning_ptr));
    assert!(wp2_moved.get() == ptr_of(&owning_ptr));
    assert!(wp3_moved.get() == ptr_of(&owning_ptr));

    owning_ptr = None;
    assert!(owning_ptr.is_none());

    assert!(!wp1_moved.is_some());
    assert!(!wp2_moved.is_some());
    assert!(!wp3_moved.is_some());
}

#[test]
fn test_weak_ptr_can_be_copied() {
    let mut owning_ptr = new_myclass();
    let mut wp1 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp1.as_mut());
    let mut wp2 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp2.as_mut());
    let mut wp3 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp3.as_mut());

    let mut wp1_copied = pin!(WeakPtr::<MyClass>::new());
    wp1_copied.as_mut().assign(&wp1);
    let mut wp2_copied = pin!(WeakPtr::<MyClass>::new());
    wp2_copied.as_mut().assign(&wp2);
    let mut wp3_copied = pin!(WeakPtr::<MyClass>::new());
    wp3_copied.as_mut().assign(&wp3);
    assert!(wp1.is_some());
    assert!(wp2.is_some());
    assert!(wp3.is_some());
    assert!(wp1_copied.is_some());
    assert!(wp2_copied.is_some());
    assert!(wp3_copied.is_some());

    assert!(wp1.get() == wp1_copied.get());
    assert!(wp2.get() == wp2_copied.get());
    assert!(wp3.get() == wp3_copied.get());

    owning_ptr = None;
    assert!(owning_ptr.is_none());

    assert!(!wp1.is_some());
    assert!(!wp2.is_some());
    assert!(!wp3.is_some());
    assert!(!wp1_copied.is_some());
    assert!(!wp2_copied.is_some());
    assert!(!wp3_copied.is_some());
}

#[test]
fn test_multipe_weak_ptrs() {
    let mut owning_ptr = new_myclass();

    let mut wp1 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp1.as_mut());
    assert!(wp1.is_some());
    assert!(wp1.get() == ptr_of(&owning_ptr));

    let mut wp2 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp2.as_mut());
    assert!(wp2.is_some());
    assert!(wp2.get() == ptr_of(&owning_ptr));

    owning_ptr = None;
    assert!(owning_ptr.is_none());
    assert!(!wp1.is_some());
    assert!(!wp2.is_some());
}

#[test]
fn test_multipe_weak_ptrs_going_away_first() {
    let mut owning_ptr = new_myclass();

    let mut wp1 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp1.as_mut());
    let mut wp2 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp2.as_mut());
    let mut wp3 = pin!(WeakPtr::<MyClass>::new());
    wft(&mut owning_ptr, wp3.as_mut());

    assert!(wp1.is_some());
    assert!(wp2.is_some());
    assert!(wp3.is_some());

    wp2.clear();

    owning_ptr = new_myclass();

    assert!(!wp1.is_some());
    assert!(!wp2.is_some());
    assert!(!wp3.is_some());

    wft(&mut owning_ptr, wp1.as_mut());
    wft(&mut owning_ptr, wp2.as_mut());
    wft(&mut owning_ptr, wp3.as_mut());

    assert!(wp1.is_some());
    assert!(wp2.is_some());
    assert!(wp3.is_some());

    wp3.clear();
    owning_ptr = new_myclass();

    assert!(!wp1.is_some());
    assert!(!wp2.is_some());
    assert!(!wp3.is_some());

    wft(&mut owning_ptr, wp1.as_mut());
    wft(&mut owning_ptr, wp2.as_mut());
    wft(&mut owning_ptr, wp3.as_mut());

    wp1.clear();
    wp3.clear();
    owning_ptr = new_myclass();

    assert!(!wp1.is_some());
    assert!(!wp2.is_some());
    assert!(!wp3.is_some());
    drop(owning_ptr);
}
