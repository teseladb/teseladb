//! Gate and weak pointer details: the messages of the closed-gate errors (plain, named,
//! unnamed), a default holder, copy-assigning holders between gates and from an empty
//! holder; weak pointers compared, printed and dereferenced, and moved, converted or copied
//! from a null weak pointer.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::pin::pin;

use teseladb::engine::core::gate::{Gate, GateClosedException, Holder, NamedGateClosedException};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::weak_ptr::{WeakPtr, WeaklyReferencable, weak_from_this};
use teseladb::weakly_referencable;

#[test]
fn closed_gate_error_messages() {
    assert_eq!(GateClosedException.to_string(), "gate closed");
    assert_eq!(NamedGateClosedException::new(&Sstring::from("db")).to_string(), "db gate closed");
    assert_eq!(NamedGateClosedException::new(&Sstring::new()).to_string(), "named gate closed");
    assert_eq!(NamedGateClosedException::default().to_string(), "named gate closed");
}

#[test]
fn holders_copy_assigned_between_gates() {
    let a = pin!(Gate::new());
    let b = pin!(Gate::new());
    let ha = a.as_ref().hold().unwrap();
    let mut h = Holder::default();
    // From a holder of `a`: enters `a`.
    h.clone_from(&ha);
    assert_eq!(a.get_count(), 2);
    // The same gate again: nothing changes.
    h.clone_from(&ha);
    assert_eq!(a.get_count(), 2);
    // A holder of `b`: leaves `a`, enters `b`.
    let hb = b.as_ref().hold().unwrap();
    h.clone_from(&hb);
    assert_eq!((a.get_count(), b.get_count()), (1, 2));
    // An empty holder: leaves `b` and holds nothing.
    h.clone_from(&Holder::default());
    assert_eq!(b.get_count(), 1);
    drop((ha, hb, h));
    assert_eq!((a.get_count(), b.get_count()), (0, 0));
}

#[derive(Default)]
struct Obj {
    v: u32,
    weak: WeaklyReferencable<Obj>,
}

weakly_referencable!(Obj { weak });

#[test]
fn weak_pointers_compared_printed_and_moved_from_null() {
    let mut o = Box::pin(Obj { v: 7, ..Obj::default() });
    let mut w1 = pin!(WeakPtr::<Obj>::new());
    let mut w2 = pin!(WeakPtr::<Obj>::new());
    weak_from_this(o.as_mut(), w1.as_mut());
    weak_from_this(o.as_mut(), w2.as_mut());
    assert!(*w1 == *w2);
    let null = WeakPtr::<Obj>::new();
    assert!(*w1 != null);
    assert!(format!("{:?}", *w1).starts_with("WeakPtr("));
    // SAFETY: the object is alive and not otherwise borrowed.
    unsafe {
        assert_eq!(WeakPtr::as_ref(&w1).map(|r| r.v), Some(7));
        WeakPtr::as_mut(&w1).expect("live").v = 8;
        assert!(WeakPtr::as_ref(&null).is_none());
    }
    assert_eq!(o.v, 8);
    // Moving, converting or copying a null weak pointer empties the target.
    let mut empty = pin!(WeakPtr::<Obj>::new());
    w1.as_mut().assign_from(empty.as_mut());
    assert!(!w1.is_some());
    w2.as_mut().take_from(empty.as_mut(), |p| p);
    assert!(!w2.is_some());
    let mut w3 = pin!(WeakPtr::<Obj>::new());
    weak_from_this(o.as_mut(), w3.as_mut());
    w3.as_mut().assign(&empty);
    assert!(!w3.is_some());
}
