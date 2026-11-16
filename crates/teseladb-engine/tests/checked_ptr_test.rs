//! Tests of `CheckedPtr`: empty when default- or null-initialized, engaged after assignment,
//! for raw and weak pointers.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::{Pin, pin};

use teseladb::engine::core::checked_ptr::CheckedPtr;
use teseladb::engine::core::weak_ptr::{WeakPtr, WeaklyReferencable, weak_from_this};

// Default construction (`Default`) and every move cannot fail; this function only checks
// that the pointer types implement `Default`.
fn _static_checks() {
    fn is_default<T: Default>() {}
    is_default::<CheckedPtr<*mut i32>>();
    is_default::<CheckedPtr<WeakPtr<i32>>>();
}

struct MySt {
    weak: WeaklyReferencable<MySt>,
    a: i32,
}

impl MySt {
    fn new(a: i32) -> Self {
        MySt { weak: WeaklyReferencable::new(), a }
    }
}

teseladb::weakly_referencable!(MySt { weak });

/// The field `a` read through the checked `get()`.
fn a_of<P>(cp: &CheckedPtr<P>) -> i32
where
    P: teseladb::engine::core::checked_ptr::CheckedPtrTarget<Element = MySt>,
{
    // SAFETY: the tests only read through engaged pointers to live objects.
    unsafe { (*cp.get().expect("checked_ptr_is_null_exception")).a }
}

fn const_ref_check_naked(cp: &CheckedPtr<*mut MySt>) {
    assert!(cp.is_engaged());
    assert!(a_of(cp) == 3);
    assert!(a_of(cp) == 3);
    assert!(a_of(cp) == 3);
}

fn const_ref_check_smart(cp: &CheckedPtr<WeakPtr<MySt>>) {
    assert!(cp.is_engaged());
    assert!(a_of(cp) == 3);
    assert!(a_of(cp) == 3);
    assert!(a_of(cp) == 3);
}

#[test]
fn test_checked_ptr_is_empty_when_default_initialized() {
    let cp = CheckedPtr::<*mut i32>::default();
    assert!(!cp.is_engaged());
}

#[test]
fn test_checked_ptr_is_empty_when_nullptr_initialized_nakes_ptr() {
    let cp = CheckedPtr::<*mut i32>::new(std::ptr::null_mut());
    assert!(!cp.is_engaged());
}

#[test]
fn test_checked_ptr_is_empty_when_nullptr_initialized_smart_ptr() {
    let cp = CheckedPtr::<WeakPtr<MySt>>::new(WeakPtr::default());
    assert!(!cp.is_engaged());
}

#[test]
fn test_checked_ptr_is_initialized_after_assignment_naked_ptr() {
    let mut cp = CheckedPtr::<*mut MySt>::new(std::ptr::null_mut());
    assert!(!cp.is_engaged());
    let mut i = MySt::new(3);
    let mut k = MySt::new(3);
    cp = CheckedPtr::new(&mut i);
    let cp1 = CheckedPtr::<*mut MySt>::new(&mut i);
    let cp2 = CheckedPtr::<*mut MySt>::new(&mut k);
    assert!(cp.is_engaged());
    assert!(cp == cp1);
    assert!(cp != cp2);
    assert!(a_of(&cp) == 3);
    assert!(a_of(&cp) == 3);
    assert!(a_of(&cp) == 3);

    const_ref_check_naked(&cp);

    cp = CheckedPtr::new(std::ptr::null_mut());
    assert!(!cp.is_engaged());
}

#[test]
fn test_checked_ptr_is_initialized_after_assignment_smart_ptr() {
    // A linked weak pointer does not move, so each checked weak pointer is pinned
    // and assigning `i.weak_from_this()` fills its pointer in place.
    let mut cp = pin!(CheckedPtr::<WeakPtr<MySt>>::new(WeakPtr::default()));
    assert!(!cp.is_engaged());
    let mut i: Option<Pin<Box<MySt>>> = Some(Box::pin(MySt::new(3)));
    weak_from_this(i.as_mut().expect("live").as_mut(), cp.as_mut().ptr_pin());
    let mut cp1 = pin!(CheckedPtr::<WeakPtr<MySt>>::default());
    weak_from_this(i.as_mut().expect("live").as_mut(), cp1.as_mut().ptr_pin());
    let cp2 = CheckedPtr::<WeakPtr<MySt>>::default();
    assert!(cp.is_engaged());
    assert!(*cp == *cp1);
    assert!(*cp != cp2);
    assert!(a_of(&cp) == 3);
    assert!(a_of(&cp) == 3);
    assert!(a_of(&cp) == 3);

    const_ref_check_smart(&cp);

    i = None;
    assert!(i.is_none());
    assert!(!cp.is_engaged());
    assert!(!cp1.is_engaged());
    assert!(!cp2.is_engaged());
}
