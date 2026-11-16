//! Tests of `SharedPtr` and `LwSharedPtr`: casts, shared-from-this, errors raised while
//! building the object, hashing and ordering through the pointee, release, and comparison
//! with null.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasherDefault, DefaultHasher, Hash, Hasher};

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::shared_ptr::{
    EnableLwSharedFromThis, EnableSharedFromThis, LwSharedPtr, SharedPtr, dynamic_pointer_cast,
    make_lw_shared, make_shared, make_shared_esft,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::util::indirect::{IndirectHash, IndirectLess, default_indirect_equal_to};

/// The error the tests raise on purpose.
#[derive(Debug)]
struct ExpectedException;

impl fmt::Display for ExpectedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected test error")
    }
}

impl std::error::Error for ExpectedException {}

thread_local! {
    /// Whether an `A` was dropped.
    static DESTROYED: Cell<bool> = const { Cell::new(false) };
}

fn destroyed() -> bool {
    DESTROYED.with(Cell::get)
}

struct A;

impl A {
    fn new() -> Self {
        DESTROYED.with(|d| d.set(false));
        A
    }
}

impl Drop for A {
    fn drop(&mut self) {
        DESTROYED.with(|d| d.set(true));
    }
}

/// An `A` that can hand out a shared pointer to itself.
struct AEsft {
    _a: A,
}

impl EnableLwSharedFromThis for AEsft {}

/// A type unrelated to `A`, for the failing cast.
struct B;

impl B {
    #[allow(dead_code)]
    fn x(&self) {}
}

#[test]
fn explot_dynamic_cast_use_after_free_problem() {
    let p: SharedPtr<A> = make_shared(A::new());
    {
        // A is not a B: the cast finds nothing.
        let p2 = dynamic_pointer_cast::<B, A>(&p, |_a: &A| None);
        assert!(!p2.is_some());
    }
    assert!(!destroyed());
}

/// An object that can hand out shared pointers to itself.
struct C {
    esft: EnableSharedFromThis,
}

teseladb::shared_from_this_impl!(C { esft });

/// The object of a shared pointer, as a method receives it: the pointer comes from the
/// shared pointer itself, as `shared_from_this` requires.
fn this_of<T>(p: &SharedPtr<T>) -> *const T {
    match p.get() {
        Some(nn) => nn.as_ptr(),
        None => panic!("member call through a null shared_ptr"),
    }
}

impl C {
    /// A new shared pointer to the object.
    fn dup(this: *const C) -> SharedPtr<C> {
        // SAFETY: every C of this test is created by make_shared_esft, and `this` comes from
        // one of its shared pointers (`this_of`).
        unsafe { SharedPtr::shared_from_this(this) }
    }

    /// A new shared pointer to the object, for read-only use.
    fn get(this: *const C) -> SharedPtr<C> {
        // SAFETY: as above.
        unsafe { SharedPtr::shared_from_this(this) }
    }
}

#[test]
fn test_const_ptr() {
    let a: SharedPtr<C> = make_shared_esft(C { esft: EnableSharedFromThis::new() });
    // A shared pointer is read-only already.
    let ca: SharedPtr<C> = a.clone();
    assert!(ca == a);
    let cca: SharedPtr<C> = C::get(this_of(&ca));
    assert!(cca == ca);
    let _ = C::dup(this_of(&a));
}

struct D;

#[test]
fn test_lw_const_ptr_1() {
    let _pd1 = make_lw_shared(D);
    let pd2 = make_lw_shared(D);
    let pd3: LwSharedPtr<D> = pd2.clone();
    assert!(pd2 == pd3);
}

/// An object that can hand out lightweight shared pointers to itself.
#[derive(Default)]
struct E;

impl EnableLwSharedFromThis for E {}

#[test]
fn test_lw_const_ptr_2() {
    let _pe1 = make_lw_shared(E);
    let pe2 = make_lw_shared(E);
    let pe3: LwSharedPtr<E> = pe2.clone();
    assert!(pe2 == pe3);
}

/// An object whose read-only method hands out a lightweight shared pointer to itself.
struct F;

impl EnableLwSharedFromThis for F {}

impl F {
    /// A new lightweight shared pointer to the object, taken from a shared reference.
    fn const_method(this: *const F) -> LwSharedPtr<F> {
        // SAFETY: the F of the test is created by make_lw_shared, and `this` comes from its
        // shared pointer (`get()`).
        unsafe { LwSharedPtr::shared_from_this(this) }
    }
}

#[test]
fn test_shared_from_this_called_on_const_object() {
    let ptr = make_lw_shared(F);
    let _ = F::const_method(ptr.get());
}

#[test]
fn test_exception_thrown_from_constructor_is_propagated() {
    struct X;
    impl X {
        /// Always fails with [`ExpectedException`].
        fn new() -> Result<Self, ExceptionPtr> {
            Err(ExceptionPtr::new(ExpectedException))
        }
    }
    match X::new().map(make_lw_shared) {
        Ok(_ptr) => panic!("Constructor should have thrown"),
        Err(e) => {
            assert!(e.is::<ExpectedException>());
            println!("Expected exception caught");
        }
    }
    match X::new().map(make_shared) {
        Ok(_ptr) => panic!("Constructor should have thrown"),
        Err(e) => {
            assert!(e.is::<ExpectedException>());
            println!("Expected exception caught");
        }
    }
}

/// A map key that hashes and compares the string a shared pointer points to.
struct IndirectKey(SharedPtr<Sstring>);

impl Hash for IndirectKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash the pointee.
        state.write_u64(
            IndirectHash::new(BuildHasherDefault::<DefaultHasher>::default()).call(&self.0),
        );
    }
}

impl PartialEq for IndirectKey {
    fn eq(&self, other: &Self) -> bool {
        default_indirect_equal_to::<Sstring>().call(&self.0, &other.0)
    }
}

impl Eq for IndirectKey {}

#[test]
fn test_indirect_functors() {
    {
        // An ordered multiset of shared pointers: an
        // ordered insertion by the pointee.
        let less = IndirectLess::new(|a: &Sstring, b: &Sstring| a < b);
        let mut a_set: Vec<SharedPtr<Sstring>> = Vec::new();
        let mut insert = |v: SharedPtr<Sstring>| {
            let pos = a_set.iter().position(|x| less.call(&v, x)).unwrap_or(a_set.len());
            a_set.insert(pos, v);
        };

        insert(make_shared(Sstring::from("k3")));
        insert(make_shared(Sstring::from("k1")));
        insert(make_shared(Sstring::from("k2")));
        insert(make_shared(Sstring::from("k4")));
        insert(make_shared(Sstring::from("k0")));

        let mut i = a_set.iter();
        assert_eq!(Sstring::from("k0"), **i.next().expect("k0"));
        assert_eq!(Sstring::from("k1"), **i.next().expect("k1"));
        assert_eq!(Sstring::from("k2"), **i.next().expect("k2"));
        assert_eq!(Sstring::from("k3"), **i.next().expect("k3"));
        assert_eq!(Sstring::from("k4"), **i.next().expect("k4"));
    }

    {
        let mut a_map: HashMap<IndirectKey, bool> = HashMap::new();

        a_map.insert(IndirectKey(make_shared(Sstring::from("k3"))), true);
        a_map.insert(IndirectKey(make_shared(Sstring::from("k1"))), true);
        a_map.insert(IndirectKey(make_shared(Sstring::from("k2"))), true);
        a_map.insert(IndirectKey(make_shared(Sstring::from("k4"))), true);
        a_map.insert(IndirectKey(make_shared(Sstring::from("k0"))), true);

        let count =
            |k: &str| usize::from(a_map.contains_key(&IndirectKey(make_shared(Sstring::from(k)))));
        assert!(count("k0") != 0);
        assert!(count("k1") != 0);
        assert!(count("k2") != 0);
        assert!(count("k3") != 0);
        assert!(count("k4") != 0);
        assert!(count("k5") == 0);
    }
}

/// What `do_test_release<T>` needs: default construction (which resets `destroyed`).
trait Releasable: Sized {
    fn make() -> Self;
}

impl Releasable for A {
    fn make() -> Self {
        A::new()
    }
}

impl Releasable for AEsft {
    fn make() -> Self {
        AEsft { _a: A::new() }
    }
}

fn do_test_release<T: Releasable>() {
    let mut ptr = make_lw_shared(T::make());
    assert!(!destroyed());

    let mut ptr2 = ptr.clone();

    assert!(ptr.release().is_none());
    assert!(!ptr.is_some());
    assert!(ptr2.use_count() == 1);

    let mut uptr2 = ptr2.release();
    assert!(uptr2.is_some());
    assert!(!ptr2.is_some());
    ptr2 = LwSharedPtr::default();
    assert!(!ptr2.is_some());

    assert!(!destroyed());
    uptr2 = None;
    assert!(uptr2.is_none());

    assert!(destroyed());

    // Check destroying via disposer
    let mut ptr3 = make_lw_shared(T::make());
    let uptr3 = ptr3.release();
    assert!(uptr3.is_some());
    assert!(!destroyed());

    let raw_ptr3 = uptr3.expect("unique").into_raw();
    // SAFETY: `raw_ptr3` was released from the only reference to a make_lw_shared object.
    unsafe { LwSharedPtr::<T>::dispose(raw_ptr3) };
    assert!(destroyed());
}

#[test]
fn test_release() {
    do_test_release::<A>();
    do_test_release::<AEsft>();
}

#[test]
fn test_const_release() {
    // There are no const-qualified types; the shared object is
    // never mutated in the test, so the same instantiations cover it.
    do_test_release::<A>();
    do_test_release::<AEsft>();
}

#[test]
fn test_nullptr_compare() {
    let mut ptr: SharedPtr<i32> = SharedPtr::default();
    assert!(!ptr.is_some());
    assert!(!ptr.is_some());
    ptr = make_shared(0);
    assert!(ptr.is_some());
    assert!(ptr.is_some());

    let mut lptr: LwSharedPtr<i32> = LwSharedPtr::default();
    assert!(!lptr.is_some());
    assert!(!lptr.is_some());
    lptr = make_lw_shared(0);
    assert!(lptr.is_some());
    assert!(lptr.is_some());
}
