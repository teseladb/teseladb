//! A move-only, type-erased callable with small-buffer storage.
//!
//! [`NoncopyableFunction<S>`] holds any `FnMut` closure matching the signature `S`,
//! without requiring it to be `Clone`. Closures of up to 32 bytes (and alignment up to 8)
//! are stored inline, so wrapping them does not allocate; larger ones are boxed. Calls
//! go through a static vtable (one indirect call). Continuations, tasks and callbacks
//! across the engine use it, so its size and call cost are on the hot path.
//!
//! The signature is written as a function pointer type, `NoncopyableFunction<fn(A, B) -> R>`,
//! or equivalently as `dyn FnMut(A, B) -> R` (except a single owned argument). A single
//! argument borrowed for the call is written `dyn FnMut(&A) -> R` (`RefDyn`), and a
//! borrowed first argument followed by an owned one uses [`RefFn`].
//!
//! Calling an empty function panics with "call to an empty function".

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::{MaybeUninit, align_of, size_of};

/// Storage and vtable plumbing; not part of the stable API.
pub mod internal {
    use super::*;

    /// Bytes of inline storage; closures that fit (with alignment up to 8) are not boxed.
    pub const NR_DIRECT: usize = 32;

    /// Inline bytes or a pointer to the boxed closure, 8-byte aligned.
    #[repr(C)]
    pub union Storage {
        /// The closure stored inline.
        pub direct: [MaybeUninit<u8>; NR_DIRECT],
        /// The boxed closure (`Box::into_raw`).
        pub indirect: *mut (),
        _align: [u64; 0],
    }

    /// Moves the closure stored in `from` into `to`, leaving `from` destroyed.
    pub type MoveType =
        unsafe fn(from: *mut NoncopyableFunctionBase, to: *mut NoncopyableFunctionBase);
    /// Destroys the closure stored in `func`.
    pub type DestroyType = unsafe fn(func: *mut NoncopyableFunctionBase);

    /// The storage of the function. Calls take `&mut self`, so that a function that
    /// reaches its own object again cannot hold two `&mut` to itself. The storage is an
    /// `UnsafeCell` so that the thunks, which only see a raw pointer, may write through it.
    pub struct NoncopyableFunctionBase {
        pub(crate) _storage: UnsafeCell<Storage>,
    }

    impl NoncopyableFunctionBase {
        pub(crate) const fn new() -> Self {
            NoncopyableFunctionBase {
                _storage: UnsafeCell::new(Storage { indirect: std::ptr::null_mut() }),
            }
        }

        pub(crate) unsafe fn empty_move(
            _: *mut NoncopyableFunctionBase,
            _: *mut NoncopyableFunctionBase,
        ) {
        }
        pub(crate) unsafe fn empty_destroy(_: *mut NoncopyableFunctionBase) {}

        pub(crate) unsafe fn indirect_move(
            from: *mut NoncopyableFunctionBase,
            to: *mut NoncopyableFunctionBase,
        ) {
            // SAFETY: from and to are valid function bases; the indirect pointer is copied.
            unsafe {
                (*(*to)._storage.get()).indirect = (*(*from)._storage.get()).indirect;
            }
        }

        pub(crate) unsafe fn trivial_direct_move<const N: usize>(
            from: *mut NoncopyableFunctionBase,
            to: *mut NoncopyableFunctionBase,
        ) {
            // A copy of the N bytes, since the type is erased. It also copies any padding the
            // closure left uninitialized, which is fine: nothing reads it. One
            // `copy_nonoverlapping` of a constant size, which compiles to word moves: a
            // bytewise loop through the two `UnsafeCell`s is kept as N byte moves, since the
            // compiler cannot tell that the storages do not overlap (the upstream change
            // "noncopyable_function: use memcpy instead of bytewise copy loop").
            const { assert!(N <= NR_DIRECT) };
            // SAFETY: both storages are NR_DIRECT >= N bytes, and they are the storages of two
            // distinct functions (a move is never from a function into itself).
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (*(*from)._storage.get()).direct.as_ptr(),
                    (*(*to)._storage.get()).direct.as_mut_ptr(),
                    N,
                );
            }
        }

        pub(crate) unsafe fn trivial_direct_destroy(_: *mut NoncopyableFunctionBase) {}
    }

    /// Always true: moving a closure cannot fail.
    pub const IS_NOTHROW_IF_OBJECT: bool = true;
}

use internal::{DestroyType, MoveType, NR_DIRECT, NoncopyableFunctionBase, Storage};

/// A call signature usable with [`NoncopyableFunction`], and the type of its call thunk.
pub trait Signature: 'static {
    /// `unsafe fn(*mut NoncopyableFunctionBase, args...) -> R`.
    type CallType: Copy + 'static;
}

/// The operations of one stored closure type: call, move and destroy.
pub struct Vtable<S: Signature + ?Sized> {
    /// Calls the stored closure.
    pub call: S::CallType,
    /// Moves the stored closure to another base.
    pub move_: MoveType,
    /// Destroys the stored closure.
    pub destroy: DestroyType,
    /// Marks the empty vtable (function addresses may be merged, so they cannot tell it).
    pub is_empty: bool,
}

/// A type-erased callable of signature `S` that is moved, never cloned.
///
/// See the [module documentation](self) for storage and call costs.
pub struct NoncopyableFunction<S: Signature + ?Sized> {
    base: NoncopyableFunctionBase,
    _vtable: &'static Vtable<S>,
    _s: PhantomData<S>,
}

const fn is_direct<Func>() -> bool {
    size_of::<Func>() <= NR_DIRECT && align_of::<Func>() <= align_of::<Storage>()
}

struct VtableFor<S: ?Sized, Func>(PhantomData<(*const S, Func)>);

impl<S: Signature + ?Sized, Func> VtableFor<S, Func> {
    unsafe fn direct_access(func: *mut NoncopyableFunctionBase) -> *mut Func {
        // SAFETY: the direct storage holds a Func; the pointer comes from the UnsafeCell, so
        // writing through it is allowed even when `func` was derived from a shared reference.
        unsafe {
            (&raw mut (*UnsafeCell::raw_get(&raw const (*func)._storage)).direct) as *mut Func
        }
    }
    unsafe fn indirect_access(func: *mut NoncopyableFunctionBase) -> *mut Func {
        // SAFETY: the indirect storage holds a Box<Func> pointer, read through the UnsafeCell.
        unsafe { (*UnsafeCell::raw_get(&raw const (*func)._storage)).indirect as *mut Func }
    }
    unsafe fn direct_move(from: *mut NoncopyableFunctionBase, to: *mut NoncopyableFunctionBase) {
        // SAFETY: from holds a Func; it is moved bitwise into to and from is left destroyed.
        unsafe {
            std::ptr::write(Self::direct_access(to), std::ptr::read(Self::direct_access(from)));
        }
    }
    unsafe fn direct_destroy(func: *mut NoncopyableFunctionBase) {
        // SAFETY: the direct storage holds a live Func.
        unsafe { std::ptr::drop_in_place(Self::direct_access(func)) }
    }
    unsafe fn indirect_destroy(func: *mut NoncopyableFunctionBase) {
        // SAFETY: the indirect storage holds a pointer from Box::into_raw.
        unsafe { drop(Box::from_raw(Self::indirect_access(func))) }
    }
    const fn select_move_thunk() -> MoveType {
        // Rust moves are always bytewise; a type that needs no drop is "trivial".
        let can_trivially_move = !std::mem::needs_drop::<Func>();
        if can_trivially_move {
            NoncopyableFunctionBase::trivial_direct_move::<{ NR_DIRECT }>
        } else {
            Self::direct_move
        }
    }
    const fn select_destroy_thunk() -> DestroyType {
        if !std::mem::needs_drop::<Func>() {
            NoncopyableFunctionBase::trivial_direct_destroy
        } else {
            Self::direct_destroy
        }
    }
}

impl<S: EmptyVtable + ?Sized> NoncopyableFunction<S> {
    /// Returns `true` unless the function is empty.
    pub fn is_some(&self) -> bool {
        !self._vtable.is_empty
    }

    /// Replaces the stored function with `x`, dropping the previous one.
    pub fn assign(&mut self, x: NoncopyableFunction<S>) {
        *self = x;
    }
}

/// The vtable of the empty function of a signature.
pub trait EmptyVtable: Signature {
    /// The empty vtable; its call panics with "call to an empty function".
    const EMPTY: &'static Vtable<Self>;
}

impl<S: Signature + ?Sized> Drop for NoncopyableFunction<S> {
    fn drop(&mut self) {
        // SAFETY: the vtable matches the stored function.
        unsafe { (self._vtable.destroy)(&mut self.base) }
    }
}

macro_rules! impl_signature {
    ($sig:ty; $($a:ident),*) => {
        impl<R: 'static $(, $a: 'static)*> Signature for $sig {
            type CallType = unsafe fn(*mut NoncopyableFunctionBase $(, $a)*) -> R;
        }

        #[allow(non_snake_case)]
        impl<R: 'static $(, $a: 'static)*> EmptyVtable for $sig {
            const EMPTY: &'static Vtable<Self> = &Vtable {
                call: empty_call::<R $(, $a)*>,
                move_: NoncopyableFunctionBase::empty_move,
                destroy: NoncopyableFunctionBase::empty_destroy,
                is_empty: true,
            };
        }

        #[allow(non_snake_case)]
        impl<R: 'static $(, $a: 'static)*> NoncopyableFunction<$sig> {
            /// An empty function; calling it panics.
            pub fn empty() -> Self {
                NoncopyableFunction {
                    base: NoncopyableFunctionBase::new(),
                    _vtable: <$sig as EmptyVtable>::EMPTY,
                    _s: PhantomData,
                }
            }

            /// Wraps `func`, in place when it fits in the inline storage.
            #[inline]
            pub fn new<Func: FnMut($($a),*) -> R + 'static>(func: Func) -> Self {
                let mut base = NoncopyableFunctionBase::new();
                let vt: &'static Vtable<$sig> = if is_direct::<Func>() {
                    // SAFETY: Func fits in the direct storage (size and alignment checked).
                    unsafe {
                        std::ptr::write(
                            VtableFor::<$sig, Func>::direct_access(&mut base),
                            func,
                        );
                    }
                    <Func as VtCalls<$sig>>::DIRECT
                } else {
                    base._storage.get_mut().indirect = Box::into_raw(Box::new(func)) as *mut ();
                    <Func as VtCalls<$sig>>::INDIRECT
                };
                NoncopyableFunction { base, _vtable: vt, _s: PhantomData }
            }

            /// Calls the stored function.
            ///
            /// It takes `&mut self`: the stored function is `FnMut`, and the exclusive borrow
            /// keeps a function that reaches this same object from running twice at once.
            ///
            /// # Panics
            ///
            /// Panics with "call to an empty function" if the function is empty; `is_some()` is a
            /// precondition.
            #[inline]
            pub fn call(&mut self $(, $a: $a)*) -> R {
                // SAFETY: the vtable matches the stored function, and `&mut self` makes this
                // the only access to it for the length of the call.
                unsafe { (self._vtable.call)(&mut self.base $(, $a)*) }
            }
        }

        /// The call thunk of a closure stored inline.
        #[allow(non_snake_case)]
        unsafe fn direct_call<R: 'static $(, $a: 'static)*, Func: FnMut($($a),*) -> R + 'static>(
            func: *mut NoncopyableFunctionBase $(, $a: $a)*
        ) -> R {
            // SAFETY: `call` passes the base of a function whose vtable is this one, so the
            // direct storage holds a live Func, borrowed exclusively by `call(&mut self)`.
            unsafe {
                let f = VtableFor::<$sig, Func>::direct_access(func);
                (*f)($($a),*)
            }
        }

        /// The call thunk of a boxed closure.
        #[allow(non_snake_case)]
        unsafe fn indirect_call<R: 'static $(, $a: 'static)*, Func: FnMut($($a),*) -> R + 'static>(
            func: *mut NoncopyableFunctionBase $(, $a: $a)*
        ) -> R {
            // SAFETY: as above: the indirect pointer is the Box<Func> of this function, which
            // only this call reaches while `call(&mut self)` holds the borrow.
            unsafe {
                let f = VtableFor::<$sig, Func>::indirect_access(func);
                (*f)($($a),*)
            }
        }

        #[allow(non_snake_case)]
        impl<R: 'static $(, $a: 'static)*, Func: FnMut($($a),*) -> R + 'static>
            VtCalls<$sig> for Func
        {
            const DIRECT: &'static Vtable<$sig> = &Vtable {
                call: direct_call::<R $(, $a)*, Func>,
                move_: VtableFor::<$sig, Func>::select_move_thunk(),
                destroy: VtableFor::<$sig, Func>::select_destroy_thunk(),
                is_empty: false,
            };
            const INDIRECT: &'static Vtable<$sig> = &Vtable {
                call: indirect_call::<R $(, $a)*, Func>,
                move_: NoncopyableFunctionBase::indirect_move,
                destroy: VtableFor::<$sig, Func>::indirect_destroy,
                is_empty: false,
            };
        }

        #[allow(non_snake_case)]
        unsafe fn empty_call<R $(, $a)*>(_: *mut NoncopyableFunctionBase $(, _: $a)*) -> R {
            // Calling an empty function is a precondition violation.
            panic!("call to an empty function")
        }
    };
}

/// The signature of a function whose first argument
/// is a reference borrowed for the call only (`for<'r> fn(&'r F1, A2) -> R`), as a marker type,
/// since a higher-ranked `fn` type cannot have inherent impls next to those of `fn(A1, A2)`.
pub struct RefFn<F1: ?Sized, A2, R>(RefFnMarker<F1, A2, R>);

/// The marker a [`RefFn`] holds: it owns no `F1`, `A2` or `R`.
type RefFnMarker<F1, A2, R> = PhantomData<(fn(&F1), A2, R)>;

impl<F1: ?Sized + 'static, R: 'static, A2: 'static> Signature for RefFn<F1, A2, R> {
    type CallType = for<'r> unsafe fn(*mut NoncopyableFunctionBase, &'r F1, A2) -> R;
}

#[allow(non_snake_case)]
impl<F1: ?Sized + 'static, R: 'static, A2: 'static> EmptyVtable for RefFn<F1, A2, R> {
    const EMPTY: &'static Vtable<Self> = &Vtable {
        call: empty_call_ref::<F1, A2, R>,
        move_: NoncopyableFunctionBase::empty_move,
        destroy: NoncopyableFunctionBase::empty_destroy,
        is_empty: true,
    };
}

#[allow(non_snake_case)]
impl<F1: ?Sized + 'static, R: 'static, A2: 'static> NoncopyableFunction<RefFn<F1, A2, R>> {
    /// An empty function.
    pub fn empty() -> Self {
        NoncopyableFunction {
            base: NoncopyableFunctionBase::new(),
            _vtable: <RefFn<F1, A2, R> as EmptyVtable>::EMPTY,
            _s: PhantomData,
        }
    }

    /// Wraps `func`, in place when it fits in the inline storage.
    #[inline]
    pub fn new<Func: for<'r> FnMut(&'r F1, A2) -> R + 'static>(func: Func) -> Self {
        let mut base = NoncopyableFunctionBase::new();
        let vt: &'static Vtable<RefFn<F1, A2, R>> = if is_direct::<Func>() {
            // SAFETY: Func fits in the direct storage (size and alignment checked).
            unsafe {
                std::ptr::write(
                    VtableFor::<RefFn<F1, A2, R>, Func>::direct_access(&mut base),
                    func,
                );
            }
            <Func as VtCallsRef<F1, A2, R>>::DIRECT
        } else {
            base._storage.get_mut().indirect = Box::into_raw(Box::new(func)) as *mut ();
            <Func as VtCallsRef<F1, A2, R>>::INDIRECT
        };
        NoncopyableFunction { base, _vtable: vt, _s: PhantomData }
    }

    /// Calls the stored function, with the same rules as the other signatures
    /// (`&mut self`; panics if empty).
    #[inline]
    pub fn call(&mut self, f1: &F1, a2: A2) -> R {
        // SAFETY: the vtable matches the stored function, and `&mut self` makes this the
        // only access to it for the length of the call.
        unsafe { (self._vtable.call)(&mut self.base, f1, a2) }
    }
}

/// The call thunk of a closure stored inline.
#[allow(non_snake_case)]
unsafe fn direct_call_ref<
    F1: ?Sized + 'static,
    A2: 'static,
    R: 'static,
    Func: for<'r> FnMut(&'r F1, A2) -> R + 'static,
>(
    func: *mut NoncopyableFunctionBase,
    f1: &F1,
    a2: A2,
) -> R {
    // SAFETY: as direct_call: the direct storage holds the live Func of this vtable,
    // borrowed exclusively by `call(&mut self)`.
    unsafe {
        let f = VtableFor::<RefFn<F1, A2, R>, Func>::direct_access(func);
        (*f)(f1, a2)
    }
}

/// The call thunk of a boxed closure.
#[allow(non_snake_case)]
unsafe fn indirect_call_ref<
    F1: ?Sized + 'static,
    A2: 'static,
    R: 'static,
    Func: for<'r> FnMut(&'r F1, A2) -> R + 'static,
>(
    func: *mut NoncopyableFunctionBase,
    f1: &F1,
    a2: A2,
) -> R {
    // SAFETY: as above, through the indirect Box<Func> pointer of this function.
    unsafe {
        let f = VtableFor::<RefFn<F1, A2, R>, Func>::indirect_access(func);
        (*f)(f1, a2)
    }
}

/// The vtables of a function type for this signature.
#[allow(non_snake_case)]
pub trait VtCallsRef<F1: ?Sized + 'static, A2: 'static, R: 'static> {
    /// The vtable of a closure stored inline.
    const DIRECT: &'static Vtable<RefFn<F1, A2, R>>;
    /// The vtable of a boxed closure.
    const INDIRECT: &'static Vtable<RefFn<F1, A2, R>>;
}

#[allow(non_snake_case)]
impl<F1: ?Sized + 'static, A2: 'static, R: 'static, Func: for<'r> FnMut(&'r F1, A2) -> R + 'static>
    VtCallsRef<F1, A2, R> for Func
{
    const DIRECT: &'static Vtable<RefFn<F1, A2, R>> = &Vtable {
        call: direct_call_ref::<F1, A2, R, Func>,
        move_: VtableFor::<RefFn<F1, A2, R>, Func>::select_move_thunk(),
        destroy: VtableFor::<RefFn<F1, A2, R>, Func>::select_destroy_thunk(),
        is_empty: false,
    };
    const INDIRECT: &'static Vtable<RefFn<F1, A2, R>> = &Vtable {
        call: indirect_call_ref::<F1, A2, R, Func>,
        move_: NoncopyableFunctionBase::indirect_move,
        destroy: VtableFor::<RefFn<F1, A2, R>, Func>::indirect_destroy,
        is_empty: false,
    };
}

#[allow(non_snake_case)]
unsafe fn empty_call_ref<F1: ?Sized, A2, R>(_: *mut NoncopyableFunctionBase, _: &F1, _: A2) -> R {
    // Calling an empty function is a precondition violation.
    panic!("call to an empty function")
}

/// The signature `dyn FnMut(&A1) -> R`: one argument
/// borrowed for the call only, with the same inline storage as the other signatures.
type RefDyn<A1, R> = dyn for<'r> FnMut(&'r A1) -> R + 'static;

impl<A1: ?Sized + 'static, R: 'static> Signature for RefDyn<A1, R> {
    type CallType = for<'r> unsafe fn(*mut NoncopyableFunctionBase, &'r A1) -> R;
}

impl<A1: ?Sized + 'static, R: 'static> EmptyVtable for RefDyn<A1, R> {
    const EMPTY: &'static Vtable<Self> = &Vtable {
        call: empty_call_ref1::<A1, R>,
        move_: NoncopyableFunctionBase::empty_move,
        destroy: NoncopyableFunctionBase::empty_destroy,
        is_empty: true,
    };
}

impl<A1: ?Sized + 'static, R: 'static> NoncopyableFunction<RefDyn<A1, R>> {
    /// An empty function.
    pub fn empty() -> Self {
        NoncopyableFunction {
            base: NoncopyableFunctionBase::new(),
            _vtable: <RefDyn<A1, R> as EmptyVtable>::EMPTY,
            _s: PhantomData,
        }
    }

    /// Wraps `func`, in place when it fits in the inline storage.
    #[inline]
    pub fn new<Func: for<'r> FnMut(&'r A1) -> R + 'static>(func: Func) -> Self {
        let mut base = NoncopyableFunctionBase::new();
        let vt: &'static Vtable<RefDyn<A1, R>> = if is_direct::<Func>() {
            // SAFETY: Func fits in the direct storage (size and alignment checked).
            unsafe {
                std::ptr::write(VtableFor::<RefDyn<A1, R>, Func>::direct_access(&mut base), func);
            }
            <Func as VtCallsRef1<A1, R>>::DIRECT
        } else {
            base._storage.get_mut().indirect = Box::into_raw(Box::new(func)) as *mut ();
            <Func as VtCallsRef1<A1, R>>::INDIRECT
        };
        NoncopyableFunction { base, _vtable: vt, _s: PhantomData }
    }

    /// Calls the stored function, with the same rules as the other signatures
    /// (`&mut self`; panics if empty).
    #[inline]
    pub fn call(&mut self, a1: &A1) -> R {
        // SAFETY: the vtable matches the stored function, and `&mut self` makes this the
        // only access to it for the length of the call.
        unsafe { (self._vtable.call)(&mut self.base, a1) }
    }
}

/// The call thunk of a closure stored inline.
unsafe fn direct_call_ref1<
    A1: ?Sized + 'static,
    R: 'static,
    Func: for<'r> FnMut(&'r A1) -> R + 'static,
>(
    func: *mut NoncopyableFunctionBase,
    a1: &A1,
) -> R {
    // SAFETY: the direct storage holds the live Func of this vtable, borrowed exclusively
    // by `call(&mut self)`.
    unsafe { (*VtableFor::<RefDyn<A1, R>, Func>::direct_access(func))(a1) }
}

/// The call thunk of a boxed closure.
unsafe fn indirect_call_ref1<
    A1: ?Sized + 'static,
    R: 'static,
    Func: for<'r> FnMut(&'r A1) -> R + 'static,
>(
    func: *mut NoncopyableFunctionBase,
    a1: &A1,
) -> R {
    // SAFETY: as above, through the indirect Box<Func> pointer of this function.
    unsafe { (*VtableFor::<RefDyn<A1, R>, Func>::indirect_access(func))(a1) }
}

/// The vtables of a function type for this signature.
pub trait VtCallsRef1<A1: ?Sized + 'static, R: 'static> {
    /// The vtable of a closure stored inline.
    const DIRECT: &'static Vtable<RefDyn<A1, R>>;
    /// The vtable of a boxed closure.
    const INDIRECT: &'static Vtable<RefDyn<A1, R>>;
}

impl<A1: ?Sized + 'static, R: 'static, Func: for<'r> FnMut(&'r A1) -> R + 'static>
    VtCallsRef1<A1, R> for Func
{
    const DIRECT: &'static Vtable<RefDyn<A1, R>> = &Vtable {
        call: direct_call_ref1::<A1, R, Func>,
        move_: VtableFor::<RefDyn<A1, R>, Func>::select_move_thunk(),
        destroy: VtableFor::<RefDyn<A1, R>, Func>::select_destroy_thunk(),
        is_empty: false,
    };
    const INDIRECT: &'static Vtable<RefDyn<A1, R>> = &Vtable {
        call: indirect_call_ref1::<A1, R, Func>,
        move_: NoncopyableFunctionBase::indirect_move,
        destroy: VtableFor::<RefDyn<A1, R>, Func>::indirect_destroy,
        is_empty: false,
    };
}

unsafe fn empty_call_ref1<A1: ?Sized, R>(_: *mut NoncopyableFunctionBase, _: &A1) -> R {
    // Calling an empty function is a precondition violation.
    panic!("call to an empty function")
}

/// The inline and boxed vtables of a closure type for the signature `S`.
pub trait VtCalls<S: Signature + ?Sized> {
    /// The vtable used when the closure is stored inline.
    const DIRECT: &'static Vtable<S>;
    /// The vtable used when the closure is boxed.
    const INDIRECT: &'static Vtable<S>;
}

mod arity0 {
    use super::*;
    impl_signature!(fn() -> R;);
}
mod arity1 {
    use super::*;
    impl_signature!(fn(A1) -> R; A1);
}
mod arity2 {
    use super::*;
    impl_signature!(fn(A1, A2) -> R; A1, A2);
}
mod arity3 {
    use super::*;
    impl_signature!(fn(A1, A2, A3) -> R; A1, A2, A3);
}
mod arity4 {
    use super::*;
    impl_signature!(fn(A1, A2, A3, A4) -> R; A1, A2, A3, A4);
}
mod arity5 {
    use super::*;
    impl_signature!(fn(A1, A2, A3, A4, A5) -> R; A1, A2, A3, A4, A5);
}
mod arity6 {
    use super::*;
    impl_signature!(fn(A1, A2, A3, A4, A5, A6) -> R; A1, A2, A3, A4, A5, A6);
}
mod arity0_dyn {
    use super::*;
    impl_signature!(dyn (FnMut() -> R) + 'static;);
}
// No `dyn FnMut(A1) -> R` with an owned argument: it would overlap with the
// `dyn FnMut(&A1) -> R` form (`RefDyn`); write `fn(A1) -> R` for it.
mod arity2_dyn {
    use super::*;
    impl_signature!(dyn (FnMut(A1, A2) -> R) + 'static; A1, A2);
}
mod arity3_dyn {
    use super::*;
    impl_signature!(dyn (FnMut(A1, A2, A3) -> R) + 'static; A1, A2, A3);
}
mod arity4_dyn {
    use super::*;
    impl_signature!(dyn (FnMut(A1, A2, A3, A4) -> R) + 'static; A1, A2, A3, A4);
}
mod arity5_dyn {
    use super::*;
    impl_signature!(dyn (FnMut(A1, A2, A3, A4, A5) -> R) + 'static; A1, A2, A3, A4, A5);
}
mod arity6_dyn {
    use super::*;
    impl_signature!(dyn (FnMut(A1, A2, A3, A4, A5, A6) -> R) + 'static; A1, A2, A3, A4, A5, A6);
}

impl<S: EmptyVtable + ?Sized> Default for NoncopyableFunction<S> {
    fn default() -> Self {
        NoncopyableFunction {
            base: NoncopyableFunctionBase::new(),
            _vtable: S::EMPTY,
            _s: PhantomData,
        }
    }
}
