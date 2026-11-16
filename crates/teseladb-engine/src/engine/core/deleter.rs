//! Buffer lifetime management through deleters.
//!
//! A [`Deleter`] tells the consumer of a buffer how to release it, without referencing the
//! buffer itself: dropping the deleter runs its action (calling a closure, dropping an
//! owned object or `free()`ing a block). Deleters are shared by reference counting with
//! [`Deleter::share`] and chained with [`Deleter::append`], which is how
//! [`TemporaryBuffer`](crate::engine::core::temporary_buffer::TemporaryBuffer) shares and
//! packets combine fragment lifetimes without copying data.
//!
//! The reference count is not atomic: a deleter and its shares belong to one shard and
//! must be dropped there. A single malloc()ed block is stored without any allocation by
//! tagging bit 0 of the pointer.

use std::ffi::c_void;

unsafe extern "C" {
    fn free(ptr: *mut c_void);
}

/// Provides a mechanism for managing the lifetime of a buffer.
///
/// A deleter informs the consumer of some buffer (not referenced by the deleter itself)
/// how to release the buffer. This can be by calling an arbitrary function or dropping an
/// object carried by the deleter. Examples of a deleter's encapsulated actions are:
///
///  - calling `free(p)` on some captured pointer `p`;
///  - dropping a boxed object;
///  - decrementing a reference count somewhere.
///
/// A deleter performs its action when it is dropped (after its last share is dropped).
#[repr(transparent)]
#[derive(Debug)]
pub struct Deleter {
    // Null when empty; with bit 0 set, a malloc()ed block to free directly; otherwise a
    // shared `DeleterImpl`.
    _impl: *mut DeleterImpl,
}

/// Marker argument selecting [`Deleter::from_raw`], the allocation-free form for a single
/// malloc()ed block.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawObjectTag;

/// The shared, reference-counted part of a deleter. Concrete implementations embed it as
/// their first field (`#[repr(C)]`), so a pointer to the implementation is a pointer to
/// this struct, and provide `destroy`, which drops and frees the whole object.
#[repr(C)]
#[derive(Debug)]
pub struct DeleterImpl {
    /// Number of deleters sharing this implementation.
    pub refs: u32,
    /// The deleter run after this one.
    pub next: Deleter,
    /// Drops and frees the whole concrete object that embeds this struct.
    pub destroy: unsafe fn(*mut DeleterImpl),
}

impl DeleterImpl {
    /// A base with one reference, chaining to `next`, destroyed by `destroy`.
    #[inline]
    pub fn new(next: Deleter, destroy: unsafe fn(*mut DeleterImpl)) -> Self {
        Self { refs: 1, next, destroy }
    }
}

impl Deleter {
    /// Constructs an empty deleter that does nothing in its destructor.
    #[inline]
    pub const fn new() -> Self {
        Self { _impl: std::ptr::null_mut() }
    }

    /// Takes ownership of one reference to the implementation `i` (or null for empty).
    #[inline]
    pub const fn from_impl(i: *mut DeleterImpl) -> Self {
        Self { _impl: i }
    }

    /// A deleter that calls `free(object)` when dropped, without allocating; `object`
    /// must be a malloc()ed block aligned to at least 2 bytes.
    #[inline]
    pub fn from_raw(_tag: RawObjectTag, object: *mut c_void) -> Self {
        Self { _impl: Self::from_raw_object(object) }
    }
    /// Returns a deleter with the same encapsulated action as this one. The action runs
    /// only after both this deleter and the returned one are dropped.
    ///
    /// A raw-object deleter is first converted into a shared implementation, which
    /// allocates once.
    #[inline]
    pub fn share(&mut self) -> Deleter {
        if self._impl.is_null() {
            return Deleter::new();
        }
        if self.is_raw_object() {
            self._impl = FreeDeleterImpl::new_impl(self.to_raw_object());
        }
        // SAFETY: a non-raw, non-null impl is a live DeleterImpl.
        unsafe { (*self._impl).refs += 1 };
        Deleter::from_impl(self._impl)
    }

    /// Checks whether the deleter has an associated action.
    #[inline]
    pub fn is_some(&self) -> bool {
        !self._impl.is_null()
    }

    /// Replaces this deleter with one owning a reference to `i`; the previous action runs
    /// now if this was its last reference.
    #[inline]
    pub fn reset(&mut self, i: *mut DeleterImpl) {
        *self = Deleter::from_impl(i);
    }
    /// Appends `d` to this deleter's chain: when this deleter is dropped, both actions
    /// are carried out.
    ///
    /// It avoids allocation when possible. The cost is linear in the length of the
    /// current chain, so the current chain should be the shorter one.
    #[inline]
    pub fn append(&mut self, mut d: Deleter) {
        if d._impl.is_null() {
            return;
        }
        let mut next_impl = self._impl;
        let mut next_d: *mut Deleter = self;
        // SAFETY: the chain consists of live impls owned by this deleter; `next_d` always points
        // at a deleter of the chain.
        unsafe {
            while !next_impl.is_null() {
                if next_impl == d._impl {
                    return; // Already appended
                }
                if Self::is_raw_object_ptr(next_impl) {
                    next_impl = FreeDeleterImpl::new_impl(Self::to_raw_object_ptr(next_impl));
                    (*next_d)._impl = next_impl;
                }
                if (*next_impl).refs != 1 {
                    next_impl = make_object_deleter_impl(Deleter::from_impl(next_impl), d);
                    (*next_d)._impl = next_impl;
                    return;
                }
                next_d = &mut (*next_impl).next;
                next_impl = (*next_d)._impl;
            }
            (*next_d)._impl = d._impl;
        }
        d._impl = std::ptr::null_mut();
    }

    #[inline(always)]
    fn is_raw_object_ptr(i: *mut DeleterImpl) -> bool {
        let x = i as usize;
        x & 1 != 0
    }

    #[inline(always)]
    fn is_raw_object(&self) -> bool {
        Self::is_raw_object_ptr(self._impl)
    }

    #[inline(always)]
    fn to_raw_object_ptr(i: *mut DeleterImpl) -> *mut c_void {
        let x = i as usize;
        (x & !1usize) as *mut c_void
    }

    #[inline(always)]
    fn to_raw_object(&self) -> *mut c_void {
        Self::to_raw_object_ptr(self._impl)
    }

    #[inline(always)]
    fn from_raw_object(object: *mut c_void) -> *mut DeleterImpl {
        let x = object as usize;
        (x | 1) as *mut DeleterImpl
    }
}

impl Default for Deleter {
    fn default() -> Self {
        Self::new()
    }
}

/// Dropping the deleter releases its reference and, on the last one, carries out the
/// encapsulated action.
impl Drop for Deleter {
    #[inline]
    fn drop(&mut self) {
        if self.is_raw_object() {
            // SAFETY: a raw object deleter owns a malloc()ed block.
            unsafe { free(self.to_raw_object()) };
            return;
        }
        if !self._impl.is_null() {
            // SAFETY: a non-raw, non-null impl is a live DeleterImpl shared `refs` times.
            unsafe {
                (*self._impl).refs -= 1;
                if (*self._impl).refs == 0 {
                    ((*self._impl).destroy)(self._impl);
                }
            }
        }
    }
}

// A closure run when the last reference is dropped.
#[repr(C)]
struct LambdaDeleterImpl<D: FnOnce()> {
    base: DeleterImpl,
    del: std::mem::ManuallyDrop<D>,
}

impl<D: FnOnce()> LambdaDeleterImpl<D> {
    unsafe fn destroy(p: *mut DeleterImpl) {
        // SAFETY: `p` was created by Box::into_raw of a LambdaDeleterImpl<D> (base is first).
        let mut b = unsafe { Box::from_raw(p as *mut Self) };
        // SAFETY: `del` is taken exactly once, here.
        let del = unsafe { std::mem::ManuallyDrop::take(&mut b.del) };
        del();
        drop(b);
    }
}

#[repr(C)]
struct ObjectDeleterImpl<O> {
    base: DeleterImpl,
    // `ManuallyDrop` so that `destroy` drops the object before the base: the object may point
    // into the base's `next` chain, which must still be alive while it is dropped.
    obj: std::mem::ManuallyDrop<O>,
}

impl<O> ObjectDeleterImpl<O> {
    unsafe fn destroy(p: *mut DeleterImpl) {
        // SAFETY: `p` was created by Box::into_raw of an ObjectDeleterImpl<O> (base is first).
        let mut b = unsafe { Box::from_raw(p as *mut Self) };
        // SAFETY: `obj` is dropped exactly once, here, and never used again; dropping the box
        // afterwards drops only `base` (the `next` chain).
        unsafe { std::mem::ManuallyDrop::drop(&mut b.obj) };
        drop(b);
    }
}

#[inline]
fn make_object_deleter_impl<O>(next: Deleter, obj: O) -> *mut DeleterImpl {
    Box::into_raw(Box::new(ObjectDeleterImpl {
        base: DeleterImpl::new(next, ObjectDeleterImpl::<O>::destroy),
        obj: std::mem::ManuallyDrop::new(obj),
    })) as *mut DeleterImpl
}
/// Makes a [`Deleter`] that calls the closure `o` and then runs `next`.
///
/// `o` is moved into the deleter (one allocation) and called once, when the last
/// share of the deleter is dropped.
#[inline]
pub fn make_deleter_with<D: FnOnce()>(next: Deleter, o: D) -> Deleter {
    Deleter::from_impl(Box::into_raw(Box::new(LambdaDeleterImpl {
        base: DeleterImpl::new(next, LambdaDeleterImpl::<D>::destroy),
        del: std::mem::ManuallyDrop::new(o),
    })) as *mut DeleterImpl)
}

/// Makes a [`Deleter`] that calls the closure `o` when the last share is dropped.
#[inline]
pub fn make_deleter<D: FnOnce()>(o: D) -> Deleter {
    make_deleter_with(Deleter::new(), o)
}

// A malloc()ed block freed when the last reference is dropped.
#[repr(C)]
struct FreeDeleterImpl {
    base: DeleterImpl,
    obj: *mut c_void,
}

impl FreeDeleterImpl {
    fn new_impl(obj: *mut c_void) -> *mut DeleterImpl {
        Box::into_raw(Box::new(Self { base: DeleterImpl::new(Deleter::new(), Self::destroy), obj }))
            as *mut DeleterImpl
    }

    unsafe fn destroy(p: *mut DeleterImpl) {
        // SAFETY: `p` was created by new_impl (base is first).
        let b = unsafe { Box::from_raw(p as *mut Self) };
        // SAFETY: the impl owns the malloc()ed block.
        unsafe { free(b.obj) };
    }
}
/// Makes a deleter that calls `free(obj)` when it is dropped. A null `obj` gives an empty
/// deleter; no allocation is made.
#[inline]
pub fn make_free_deleter(obj: *mut c_void) -> Deleter {
    if obj.is_null() {
        return Deleter::new();
    }
    Deleter::from_raw(RawObjectTag, obj)
}

/// Makes a deleter that calls `free(obj)` when it is dropped and also runs `next`.
#[inline]
pub fn make_free_deleter_with(next: Deleter, obj: *mut c_void) -> Deleter {
    let obj = obj as usize;
    // SAFETY: the deleter owns the malloc()ed block.
    make_deleter_with(next, move || unsafe { free(obj as *mut c_void) })
}

/// Makes a deleter that owns `obj` and drops it when the last share is dropped.
#[inline]
pub fn make_object_deleter<T>(obj: T) -> Deleter {
    Deleter::from_impl(make_object_deleter_impl(Deleter::new(), obj))
}

/// Makes a deleter that owns `obj`, drops it when the last share is dropped, and then
/// runs `d`.
#[inline]
pub fn make_object_deleter_with<T>(d: Deleter, obj: T) -> Deleter {
    Deleter::from_impl(make_object_deleter_impl(d, obj))
}
