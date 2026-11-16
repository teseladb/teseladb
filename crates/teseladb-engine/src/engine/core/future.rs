//! Futures and promises: the core of asynchronous programming on a shard.
//!
//! A [`Future<T>`] is the reader side and a [`Promise<T>`] the writer side of a single-item,
//! single-use channel. Work is chained with continuations (`then`, `then_wrapped`,
//! `finally`, ...) that run as tasks of the current shard's reactor once the value is set,
//! or awaited inside coroutines. Futures and promises never leave their shard.
//!
//! * Errors are [`ExceptionPtr`], a nullable reference-counted pointer to a
//!   `dyn Exception`. Failing is returning `Err(ExceptionPtr)`; handling a failure is
//!   matching the `Err`. Functions that handle "the current error" take it explicitly.
//! * Rust moves are plain bitwise copies without hooks, so the identity that a linked
//!   promise and future both point to cannot live inside either handle. It is placed in a
//!   small node ([`internal::PromiseNode`]) that the promise handles point to; the node
//!   also holds the result while no continuation has taken the state. When a promise is
//!   forwarded into another one (in `forward_to`), the node of the target forwards to the
//!   node of the source, and the forward chains are compressed so that they never grow.
//!   Ready futures never allocate.
//! * Where the node lives decides whether linking allocates. The continuation of
//!   `then()` and the coroutine frame embed it (one allocation). A promise that lives in a
//!   heap object of its own is a [`HostedPromise`], embedded in that object ([`HostBox`]),
//!   which keeps its memory until the future lets go (one allocation). A stand-alone
//!   [`Promise`] that already holds its result gives it to a ready future without a node.
//!   Only a stand-alone promise that is still pending when `get_future()` is called
//!   allocates a node: both the promise and the future are movable values, and either may
//!   be dropped first while the other has moved without notice, so the shared state cannot
//!   live in either of them, and pinning one of the two would make every `Future<T>`
//!   (returned by value everywhere) or every container of promises pinned. That node is
//!   taken from a per-thread free list of recycled blocks (`node_pool`), so a waiter
//!   costs a list pop and push. The node is only needed while the future is held
//!   unconsumed: `then()`, `.await` in a coroutine (whose pinned awaiter takes the state)
//!   and `get()` in a thread (whose state on the thread's stack does) point the promise at
//!   a state of their own.
//! * `then()` on an unavailable future allocates one [`Continuation`] task; the promise
//!   points its state pointer at the state embedded in that continuation and schedules it
//!   on the reactor's task queue, under the current scheduling group, when the value
//!   arrives.
//!
//! A failed future that is dropped without its error being consumed is reported (logged)
//! as an abandoned failed future.
//!   when the value arrives.

use std::any::Any as StdAny;
use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomPinned;
use std::mem::{self, ManuallyDrop, MaybeUninit};
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering, fence};

use crate::engine::core::scheduling::current_scheduling_group;
#[cfg(engine_debug_promise)]
use crate::engine::core::shard_id::{ShardId, this_shard_id};
use crate::engine::core::task::{Task, TaskBase, TaskPtr, schedule, schedule_urgent};

// ---------------------------------------------------------------------------------------
// Errors carried by failed futures
// ---------------------------------------------------------------------------------------

/// Anything that can be carried by a failed future. Every
/// `std::error::Error + Send + Sync + 'static` type is an exception; its `what()` is its
/// `Display` output.
pub trait Exception: StdAny + Send + Sync + fmt::Debug + fmt::Display {
    /// Upcast used to downcast to the concrete error type.
    fn as_any(&self) -> &dyn StdAny;
    /// The name of the concrete type.
    fn type_name(&self) -> &'static str;
    /// The error message (the `Display` output).
    fn what(&self) -> String {
        self.to_string()
    }
}

impl<E: std::error::Error + Send + Sync + 'static> Exception for E {
    #[inline]
    fn as_any(&self) -> &dyn StdAny {
        self
    }
    #[inline]
    fn type_name(&self) -> &'static str {
        std::any::type_name::<E>()
    }
}

/// Header of a heap exception object; `ExceptionPtr` points at it (a thin pointer, so that
/// it fits in the state word of a future state).
#[repr(C, align(8))]
pub(crate) struct ExceptionHeader {
    refs: AtomicUsize,
    vtable: &'static ExceptionVTable,
    /// The exception this one wraps as its cause, null for a plain exception.
    nested: ExceptionPtr,
}

struct ExceptionVTable {
    get: unsafe fn(*const ExceptionHeader) -> *const dyn Exception,
    destroy: unsafe fn(*mut ExceptionHeader),
    /// The base classes a [`DerivedException`] derives from (`class backtraced : public
    /// Exc`), which `catch (Exc&)` also finds: the subobject of the given type, if it is
    /// one of them; `None` for the other exceptions.
    base: Option<ExceptionBaseFn>,
}

/// The lookup of [`ExceptionVTable::base`]: the base subobject of the given type, if any.
type ExceptionBaseFn =
    unsafe fn(*const ExceptionHeader, std::any::TypeId) -> Option<*const dyn StdAny>;

/// An exception type that derives from another one (`D` derives from `D::Base`): a handler
/// looking for the base finds it too, as [`ExceptionPtr::downcast_ref`] finds the base as well.
/// Build its pointer with [`ExceptionPtr::new_derived`].
pub trait DerivedException: Exception {
    /// The base exception type.
    type Base: 'static;
    /// The base subobject.
    fn base(&self) -> &Self::Base;
    /// The base subobject of type `ty`, for a hierarchy deeper than one level
    /// (`named_semaphore_aborted : semaphore_aborted : abort_requested_exception`): the
    /// default finds only [`base`](Self::base).
    #[inline]
    fn find_base(&self, ty: std::any::TypeId) -> Option<&dyn StdAny> {
        let base: &dyn StdAny = self.base();
        (base.type_id() == ty).then_some(base)
    }
}

#[repr(C)]
struct ExceptionObject<E> {
    header: ExceptionHeader,
    value: E,
}

impl<E: Exception> ExceptionObject<E> {
    const VTABLE: ExceptionVTable =
        ExceptionVTable { get: Self::get, destroy: Self::destroy, base: None };

    /// # Safety
    /// `h` must be the header of a live `ExceptionObject<E>`.
    unsafe fn get(h: *const ExceptionHeader) -> *const dyn Exception {
        // SAFETY: the header is the first field of a repr(C) ExceptionObject<E>.
        unsafe { &(*(h as *const Self)).value as *const E as *const dyn Exception }
    }

    /// # Safety
    /// `h` must be the header of a live `ExceptionObject<E>` with no references left.
    unsafe fn destroy(h: *mut ExceptionHeader) {
        // SAFETY: the object was allocated with Box::new in ExceptionPtr::new.
        unsafe { drop(Box::from_raw(h as *mut Self)) };
    }
}

impl<E: Exception> ExceptionObject<E> {
    /// The vtable of a standard exception that derives from another standard one
    /// ([`FilesystemError`] from [`SystemError`]), so that a lookup of the base finds it without the
    /// thrower naming the hierarchy; `VTABLE` for every other type. The comparison is of two
    /// constants once monomorphized.
    #[inline]
    fn vtable() -> &'static ExceptionVTable {
        if std::any::TypeId::of::<E>() == std::any::TypeId::of::<FilesystemError>() {
            &Self::FILESYSTEM_ERROR_VTABLE
        } else {
            &Self::VTABLE
        }
    }

    const FILESYSTEM_ERROR_VTABLE: ExceptionVTable = ExceptionVTable {
        get: Self::get,
        destroy: Self::destroy,
        base: Some(Self::get_system_error_base),
    };

    /// # Safety
    /// `h` must be the header of a live `ExceptionObject<FilesystemError>`.
    unsafe fn get_system_error_base(
        h: *const ExceptionHeader,
        ty: std::any::TypeId,
    ) -> Option<*const dyn StdAny> {
        // SAFETY: as in get(); `vtable()` gives this entry only when `E` is `FilesystemError`.
        let value: &dyn StdAny = unsafe { &(*(h as *const Self)).value };
        let e = value.downcast_ref::<FilesystemError>()?;
        (ty == std::any::TypeId::of::<SystemError>())
            .then_some(&e.system_error as *const SystemError as *const dyn StdAny)
    }
}

impl<E: DerivedException> ExceptionObject<E> {
    const DERIVED_VTABLE: ExceptionVTable =
        ExceptionVTable { get: Self::get, destroy: Self::destroy, base: Some(Self::get_base) };

    /// # Safety
    /// `h` must be the header of a live `ExceptionObject<E>`.
    unsafe fn get_base(
        h: *const ExceptionHeader,
        ty: std::any::TypeId,
    ) -> Option<*const dyn StdAny> {
        // SAFETY: as in get().
        unsafe { (*(h as *const Self)).value.find_base(ty).map(|b| b as *const dyn StdAny) }
    }
}

/// A nullable, cloneable, reference-counted pointer to an
/// exception object. It is one machine word, and a non-null one is at least 8.
#[repr(transparent)]
pub struct ExceptionPtr(Option<NonNull<ExceptionHeader>>);

// SAFETY: the pointee is an `Exception` (Send + Sync) with an atomic reference count.
unsafe impl Send for ExceptionPtr {}
// SAFETY: as above.
unsafe impl Sync for ExceptionPtr {}

impl Default for ExceptionPtr {
    #[inline]
    fn default() -> Self {
        Self::null()
    }
}

impl Clone for ExceptionPtr {
    #[inline]
    fn clone(&self) -> Self {
        if let Some(h) = self.0 {
            // SAFETY: a non-null ExceptionPtr keeps its object alive.
            unsafe { h.as_ref().refs.fetch_add(1, Ordering::Relaxed) };
        }
        ExceptionPtr(self.0)
    }
}

impl Drop for ExceptionPtr {
    #[inline]
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: a non-null ExceptionPtr holds one reference on its object.
            unsafe {
                if h.as_ref().refs.fetch_sub(1, Ordering::Release) == 1 {
                    fence(Ordering::Acquire);
                    (h.as_ref().vtable.destroy)(h.as_ptr());
                }
            }
        }
    }
}

impl ExceptionPtr {
    /// A null pointer that carries no exception.
    #[inline]
    pub const fn null() -> Self {
        ExceptionPtr(None)
    }

    /// Wraps the error `e` in a new exception object.
    pub fn new<E: Exception>(e: E) -> Self {
        Self::with_nested(e, ExceptionPtr::null())
    }

    /// Wraps `e`, an exception that derives from `E::Base`: the
    /// pointer is caught as `E` and as `E::Base` (`downcast_ref` finds both).
    pub fn new_derived<E: DerivedException>(e: E) -> Self {
        let obj = Box::new(ExceptionObject {
            header: ExceptionHeader {
                refs: AtomicUsize::new(1),
                vtable: &ExceptionObject::<E>::DERIVED_VTABLE,
                nested: ExceptionPtr::null(),
            },
            value: e,
        });
        // SAFETY: Box::into_raw never returns null.
        ExceptionPtr(Some(unsafe {
            NonNull::new_unchecked(Box::into_raw(obj) as *mut ExceptionHeader)
        }))
    }

    /// Wraps `e` in an exception object that also carries `nested`, the error that was
    /// being handled when `e` was raised (`downcast_ref::<E>()` still finds the `E`).
    pub fn with_nested<E: Exception>(e: E, nested: ExceptionPtr) -> Self {
        let obj = Box::new(ExceptionObject {
            header: ExceptionHeader {
                refs: AtomicUsize::new(1),
                vtable: ExceptionObject::<E>::vtable(),
                nested,
            },
            value: e,
        });
        // SAFETY: Box::into_raw never returns null.
        ExceptionPtr(Some(unsafe {
            NonNull::new_unchecked(Box::into_raw(obj) as *mut ExceptionHeader)
        }))
    }

    /// Returns the error that was being handled when this one was raised, if it was built
    /// with [`ExceptionPtr::with_nested`].
    #[inline]
    pub fn nested_ptr(&self) -> Option<&ExceptionPtr> {
        // SAFETY: a non-null ExceptionPtr keeps its object, header included, alive for
        // the borrow; the header is never written after construction except its count.
        self.0.map(|h| unsafe { &(*h.as_ptr()).nested }).filter(|n| !n.is_null())
    }

    /// The exception of a caught panic (`std::current_exception()` inside `catch (...)`
    /// when the "throw" was a Rust panic): a panic that carried an `ExceptionPtr` gives
    /// it back; any other becomes a [`RuntimeError`] with the panic message (the text of
    /// a `&str` or `String` payload, `"unknown panic"` otherwise).
    pub fn from_panic(payload: Box<dyn StdAny + Send>) -> ExceptionPtr {
        let payload = match payload.downcast::<ExceptionPtr>() {
            Ok(ep) => return *ep,
            Err(p) => p,
        };
        let msg = match payload.downcast::<String>() {
            Ok(s) => *s,
            Err(p) => match p.downcast::<&'static str>() {
                Ok(s) => (*s).to_owned(),
                Err(_) => "unknown panic".to_owned(),
            },
        };
        ExceptionPtr::new(RuntimeError(msg))
    }

    /// Returns whether the pointer is null (holds no error).
    #[inline]
    pub fn is_null(&self) -> bool {
        self.0.is_none()
    }

    /// The exception object, if any.
    #[inline]
    pub fn get(&self) -> Option<&dyn Exception> {
        // SAFETY: a non-null ExceptionPtr keeps its object alive for the borrow.
        self.0.map(|h| unsafe { &*(h.as_ref().vtable.get)(h.as_ptr()) })
    }

    /// Returns the error as `E` (or as a base of it, see [`DerivedException`]), if it is one.
    #[inline]
    pub fn downcast_ref<E: 'static>(&self) -> Option<&E> {
        let h = self.0?;
        // SAFETY: a non-null ExceptionPtr keeps its object alive for the borrow.
        unsafe {
            let vt = h.as_ref().vtable;
            if let Some(e) = (*(vt.get)(h.as_ptr())).as_any().downcast_ref::<E>() {
                return Some(e);
            }
            // `catch (Base&)` of a derived exception.
            match vt.base {
                Some(base) => base(h.as_ptr(), std::any::TypeId::of::<E>())
                    .and_then(|b| (*b).downcast_ref::<E>()),
                None => None,
            }
        }
    }

    /// Whether the exception is of type `E`.
    #[inline]
    pub fn is<E: 'static>(&self) -> bool {
        self.downcast_ref::<E>().is_some()
    }

    /// Returns `Err(self)`: propagates this error.
    #[inline]
    pub fn rethrow<T>(self) -> Result<T, ExceptionPtr> {
        Err(self)
    }

    /// Releases the pointer, with its provenance (null for a null `exception_ptr`).
    #[inline]
    pub(crate) fn into_raw(self) -> *mut ExceptionHeader {
        let me = ManuallyDrop::new(self);
        me.0.map_or(ptr::null_mut(), |h| h.as_ptr())
    }

    /// Rebuilds a pointer released by [`ExceptionPtr::into_raw`].
    ///
    /// # Safety
    /// `raw` must come from `into_raw` and be used once.
    #[inline]
    pub(crate) unsafe fn from_raw(raw: *mut ExceptionHeader) -> Self {
        ExceptionPtr(NonNull::new(raw))
    }
}

impl PartialEq for ExceptionPtr {
    /// Identity comparison: two pointers are equal when they share the same exception object.
    #[inline]
    fn eq(&self, o: &Self) -> bool {
        self.0 == o.0
    }
}

impl Eq for ExceptionPtr {}

impl fmt::Debug for ExceptionPtr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.get() {
            None => f.write_str("null"),
            Some(e) => write!(f, "{} ({:?})", e.type_name(), e),
        }
    }
}

// `Display for ExceptionPtr` is implemented in `util::log`.

impl<E: Exception> From<E> for ExceptionPtr {
    #[inline]
    fn from(e: E) -> Self {
        ExceptionPtr::new(e)
    }
}

/// Wraps the error `e` in a new [`ExceptionPtr`].
#[inline]
pub fn make_exception_ptr<E: Exception>(e: E) -> ExceptionPtr {
    ExceptionPtr::new(e)
}

/// The standard errors that carry the message they were built with: `what()` is that
/// message.
macro_rules! std_message_exception {
    ($(#[$doc:meta] $name:ident),* $(,)?) => {$(
        #[$doc]
        #[derive(Debug, Clone)]
        pub struct $name(pub String);

        impl $name {
            #[doc = concat!("`", stringify!($name), "(what_arg)`: `what()` is `what_arg`.")]
            #[inline]
            pub fn new(what: impl Into<String>) -> Self {
                Self(what.into())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::error::Error for $name {}
    )*};
}

std_message_exception!(
    /// A runtime error.
    RuntimeError,
    /// A logic error: a violated precondition or invariant.
    LogicError,
    /// An argument or index out of range.
    OutOfRange,
    /// An invalid argument.
    InvalidArgument,
    /// A result out of range.
    RangeError,
    /// An arithmetic overflow.
    OverflowError,
);

/// Calling an empty function object.
#[derive(Debug, Clone, Copy, Default)]
pub struct BadFunctionCall;

impl fmt::Display for BadFunctionCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("call to an empty function")
    }
}
impl std::error::Error for BadFunctionCall {}

/// The text the system gives for error number `code`, as `std::system_category().message(code)`: `strerror`'s text
/// without the ` (os error N)` that Rust's `io::Error` adds.
fn system_message(code: i32) -> String {
    let text = std::io::Error::from_raw_os_error(code).to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// The `std::error_category` of a [`SystemError`]'s code: the two of `<system_error>` that
/// carry `errno` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorCategory {
    /// The operating system's error numbers (`errno`).
    #[default]
    System,
    /// The portable error numbers.
    Generic,
}

impl ErrorCategory {
    /// Returns the category's name: `"system"` or `"generic"`.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            ErrorCategory::System => "system",
            ErrorCategory::Generic => "generic",
        }
    }
}

/// An error number of a category and what failed.
#[derive(Debug, Clone)]
pub struct SystemError {
    code: i32,
    category: ErrorCategory,
    what: String,
}

impl SystemError {
    /// A system-category error for `code`; its `what()` is `"<what>: <message>"`.
    pub fn new(code: i32, what: &str) -> Self {
        Self::with_category(code, ErrorCategory::System, what)
    }

    /// An error of `category` for `code`; its `what()` is `"<what>: <message>"`; the
    /// message of an `errno` value is `strerror`'s in both categories.
    pub fn with_category(code: i32, category: ErrorCategory, what: &str) -> Self {
        Self { code, category, what: format!("{what}: {}", system_message(code)) }
    }

    /// Returns the category of the error number.
    #[inline]
    pub fn category(&self) -> ErrorCategory {
        self.category
    }

    /// A system-category error for `code` whose `what()` is the error's message alone.
    pub fn from_errno(code: i32) -> Self {
        Self { code, category: ErrorCategory::System, what: system_message(code) }
    }

    /// Returns the error number.
    #[inline]
    pub fn code(&self) -> i32 {
        self.code
    }
}

impl fmt::Display for SystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}
impl std::error::Error for SystemError {}

/// Memory allocation failure.
#[derive(Debug, Clone, Copy, Default)]
pub struct BadAlloc;

impl fmt::Display for BadAlloc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("memory allocation failed")
    }
}
impl std::error::Error for BadAlloc {}

/// A [`SystemError`] of a file-system operation that also carries the paths it was about.
/// A lookup of a [`SystemError`] finds it too (it derives from it), and that base's
/// `what()` is this one's.
#[derive(Debug, Clone)]
pub struct FilesystemError {
    system_error: SystemError,
    path1: std::path::PathBuf,
    path2: Option<std::path::PathBuf>,
}

impl FilesystemError {
    /// A file-system error for `reason`, the path(s) and the error number `code`; its
    /// `what()` is `"filesystem error: <reason>: <message> [<path1>] [<path2>]"`.
    pub fn new(
        reason: &str,
        path1: &std::path::Path,
        path2: Option<&std::path::Path>,
        code: i32,
    ) -> Self {
        let mut what =
            format!("filesystem error: {reason}: {} [{}]", system_message(code), path1.display());
        if let Some(p) = path2 {
            what.push_str(&format!(" [{}]", p.display()));
        }
        Self {
            system_error: SystemError { code, category: ErrorCategory::System, what },
            path1: path1.to_owned(),
            path2: path2.map(std::path::Path::to_owned),
        }
    }

    /// Returns the error number.
    #[inline]
    pub fn code(&self) -> i32 {
        self.system_error.code
    }

    /// Returns the [`SystemError`] base.
    #[inline]
    pub fn as_system_error(&self) -> &SystemError {
        &self.system_error
    }

    /// Returns the first path.
    pub fn path1(&self) -> &std::path::Path {
        &self.path1
    }

    /// Returns the second path, `None` when it has one path.
    pub fn path2(&self) -> Option<&std::path::Path> {
        self.path2.as_deref()
    }
}

impl fmt::Display for FilesystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.system_error.what)
    }
}
impl std::error::Error for FilesystemError {}

pub use crate::engine::core::loop_::do_until;

// ---------------------------------------------------------------------------------------
// Futures

/// An error raised while handling another one: `outer` was raised while `inner` was being
/// handled.
#[derive(Debug, Clone)]
pub struct NestedException {
    /// The error that was being handled.
    pub inner: ExceptionPtr,
    /// The error raised while handling it.
    pub outer: ExceptionPtr,
}

impl NestedException {
    /// Builds the pair.
    pub fn new(inner: ExceptionPtr, outer: ExceptionPtr) -> Self {
        Self { inner, outer }
    }
    /// Returns the outer error.
    pub fn rethrow_nested(&self) -> Result<Infallible, ExceptionPtr> {
        Err(self.outer.clone())
    }
    /// Returns the error message.
    pub fn what(&self) -> &'static str {
        "teseladb::nested_exception"
    }
}

impl fmt::Display for NestedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}
impl std::error::Error for NestedException {}

// Implementation notes on promise states.
//
// A future holds a state: either no value yet, a value, or an error. A future without an
// initial value is created by making a promise and calling `get_future`. While the future
// is held unconsumed, its result lives in the promise node; when the future is consumed by
// `then`, the promise is pointed at the state embedded in the new continuation; when it is
// awaited in a coroutine, at the awaiter's state; when `get()` blocks in a thread, at the
// state of the blocked future, and the thread is the task woken up. Setting the value
// writes into whichever state the promise points at and schedules the waiting task, if any.

/// Returns a ready `future` that is already resolved.
#[inline]
pub fn as_ready_future<T: 'static>(v: T) -> Future<T> {
    make_ready_future(v)
}

/// Builds a failed future from a borrowed error, which is cloned.
#[inline]
pub fn make_exception_future_ref<T: 'static>(ex: &ExceptionPtr) -> Future<T> {
    // as ex is const, we cannot move it, but can copy it.
    make_exception_future::<T>(ex.clone())
}

// engine_exit(): see below.

/// Error for broken promises.
///
/// When a promise is broken, i.e. a promise object with an attached
/// continuation is destroyed before setting any value or exception, an
/// exception of [`BrokenPromise`] type is propagated to that abandoned
/// continuation.
#[derive(Debug, Clone)]
pub struct BrokenPromise {
    what: &'static str,
}

impl BrokenPromise {
    /// The broken-promise error.
    pub fn new() -> Self {
        BrokenPromise { what: "broken promise" }
    }
}

impl Default for BrokenPromise {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BrokenPromise {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what)
    }
}
impl std::error::Error for BrokenPromise {}

/// Per-thread recycling of the blocks of stand-alone promise nodes.
///
/// A [`Promise`] that is still pending when `get_future()` is called needs a node that
/// outlives both handles (see the notes at the top of this file): the promise and the
/// future may each move or be dropped first. The node blocks are kept
/// on per-size free lists of the thread that freed them (futures and promises never
/// leave their shard), so the waits of semaphores, queues, `shared_mutex`,
/// `shared_future`, execution stages and gates reuse a block instead of calling the
/// allocator.
mod node_pool {
    use std::alloc::Layout;
    use std::cell::Cell;

    /// The size step of the free lists.
    const GRANULE: usize = 16;
    /// Free lists for blocks up to `GRANULE * CLASSES` bytes; larger nodes use the
    /// allocator directly.
    const CLASSES: usize = 32;
    /// At most this many blocks are kept per class (the rest go back to the allocator).
    const MAX_CACHED: u32 = 1024;

    /// A free block: the first word links to the next one.
    struct FreeBlock {
        next: *mut FreeBlock,
    }

    struct Pool {
        heads: [Cell<*mut FreeBlock>; CLASSES],
        counts: [Cell<u32>; CLASSES],
    }

    // No destructor: the cached blocks stay with the thread when it exits, as a
    // thread_local without one (a shard's memory goes with the shard), and without a
    // registered destructor every access is a plain thread-local load, with no check of
    // its registration state. Every `get_future()` of a stand-alone promise reaches it, so
    // it is an initial-exec thread-local (see `util::initial_exec_tls`); all zero bits are
    // empty lists (null heads, zero counts).
    crate::engine::util::initial_exec_tls::initial_exec_thread_local! {
        fn pool() -> Pool, symbol = "__engine_rs_future_node_pool";
    }

    /// With the system allocator (`engine_default_allocator`, the debug and sanitizer
    /// builds) memory outlives the thread, so the blocks cached when a thread exits are
    /// handed back to it, as the blocks of a build without this cache would be: a
    /// thread-local with a destructor, registered by the first block cached, empties the
    /// lists when the thread exits.
    #[cfg(engine_default_allocator)]
    struct ReleaseAtExit;

    #[cfg(engine_default_allocator)]
    impl Drop for ReleaseAtExit {
        fn drop(&mut self) {
            with_pool(|pool| {
                for (c, head) in pool.heads.iter().enumerate() {
                    let mut b = head.replace(std::ptr::null_mut());
                    while !b.is_null() {
                        // SAFETY: a block on the list is a live FreeBlock owned by it,
                        // allocated with its class layout; it is unlinked before release.
                        unsafe {
                            let next = (*b).next;
                            std::alloc::dealloc(b.cast::<u8>(), class_layout(c));
                            b = next;
                        }
                    }
                    pool.counts[c].set(0);
                }
            });
        }
    }

    #[cfg(engine_default_allocator)]
    std::thread_local! {
        static RELEASE_AT_EXIT: ReleaseAtExit = const { ReleaseAtExit };
    }

    /// Whether a block may be cached: always with the engine's allocator; with the system
    /// allocator, while the exit-time release of this thread's lists is registered (it is
    /// registered here on first use; once it has run, blocks go straight back).
    #[inline(always)]
    fn release_at_exit_registered() -> bool {
        #[cfg(engine_default_allocator)]
        {
            RELEASE_AT_EXIT.try_with(|_| ()).is_ok()
        }
        #[cfg(not(engine_default_allocator))]
        {
            true
        }
    }

    /// Runs `f` on this thread's pool.
    #[inline(always)]
    fn with_pool<R>(f: impl FnOnce(&Pool) -> R) -> R {
        // SAFETY: the calling thread's live copy; its fields are Cells and the reference
        // lasts for the call only (Pool is !Sync, so it cannot reach another thread).
        f(unsafe { &*pool() })
    }

    /// The class of `layout`, if it is pooled.
    #[inline(always)]
    fn class_of(layout: Layout) -> Option<usize> {
        if layout.align() > GRANULE || layout.size() == 0 || layout.size() > GRANULE * CLASSES {
            return None;
        }
        Some((layout.size() - 1) / GRANULE)
    }

    #[inline(always)]
    fn class_layout(class: usize) -> Layout {
        // SAFETY: the size is a non-zero multiple of the alignment, far below isize::MAX.
        unsafe { Layout::from_size_align_unchecked((class + 1) * GRANULE, GRANULE) }
    }

    /// A block for `layout` (never null: allocation failure aborts, as `new` would
    /// terminate inside the `noexcept` code that allocates a node).
    #[inline]
    pub(super) fn alloc(layout: Layout) -> *mut u8 {
        let (class, real) = match class_of(layout) {
            Some(c) => (Some(c), class_layout(c)),
            None => (None, layout),
        };
        if let Some(c) = class {
            let b = with_pool(|p| {
                let b = p.heads[c].get();
                if !b.is_null() {
                    // SAFETY: a block on the list is a live FreeBlock owned by it.
                    p.heads[c].set(unsafe { (*b).next });
                    p.counts[c].set(p.counts[c].get() - 1);
                }
                b
            });
            if !b.is_null() {
                return b.cast::<u8>();
            }
        }
        // SAFETY: `real` has a non-zero size.
        let p = unsafe { std::alloc::alloc(real) };
        if p.is_null() {
            std::alloc::handle_alloc_error(real);
        }
        p
    }

    /// Gives back a block from [`alloc`] with the same `layout`.
    ///
    /// # Safety
    /// `p` must come from `alloc(layout)` and not be used afterwards.
    #[inline]
    pub(super) unsafe fn dealloc(p: *mut u8, layout: Layout) {
        let Some(c) = class_of(layout) else {
            // SAFETY: guaranteed by the caller; unpooled blocks use the plain layout.
            unsafe { std::alloc::dealloc(p, layout) };
            return;
        };
        let kept = with_pool(|pool| {
            if pool.counts[c].get() >= MAX_CACHED || !release_at_exit_registered() {
                return false;
            }
            let b = p.cast::<FreeBlock>();
            // SAFETY: the block is at least GRANULE bytes, aligned to GRANULE, and owned
            // by the caller, who hands it to the list.
            unsafe { b.write(FreeBlock { next: pool.heads[c].get() }) };
            pool.heads[c].set(b);
            pool.counts[c].set(pool.counts[c].get() + 1);
            true
        });
        if !kept {
            // SAFETY: guaranteed by the caller; pooled blocks use their class layout.
            unsafe { std::alloc::dealloc(p, class_layout(c)) };
        }
    }
}

/// Building blocks of futures and promises: the promise node and the writer handles.
pub mod internal {
    use super::*;

    pub use super::internal_report::*;
    pub use super::promise_detail::*;

    /// The value of a future without a value.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Monostate;

    /// The stored type of a `Future<T>`: `T` itself (a future without a value is
    /// `Future<()>`).
    pub type FutureStoredType<T> = T;

    /// The value of a future as a one-element tuple.
    pub type FutureTupleType<T> = (T,);

    /// Returns the value unchanged.
    #[inline]
    pub fn get0<T>(v: T) -> T {
        v
    }

    /// Wrapper for keeping uninitialized values of non default constructible types.
    ///
    /// This is similar to an `Option<T>`, but it doesn't know if it is holding a value or not, so the user is
    /// responsible for calling constructors and destructors.
    pub struct UninitializedWrapper<T> {
        v: MaybeUninit<T>,
    }

    impl<T> Default for UninitializedWrapper<T> {
        fn default() -> Self {
            Self { v: MaybeUninit::uninit() }
        }
    }

    impl<T> UninitializedWrapper<T> {
        #[inline]
        /// Stores `v` in a state known to hold no value yet.
        pub fn uninitialized_set(&mut self, v: T) {
            self.v.write(v);
        }
        /// # Safety
        /// The value must have been set and not taken.
        #[inline]
        pub unsafe fn uninitialized_get(&self) -> &T {
            // SAFETY: guaranteed by the caller.
            unsafe { self.v.assume_init_ref() }
        }
        /// # Safety
        /// The value must have been set and not taken.
        #[inline]
        pub unsafe fn uninitialized_get_mut(&mut self) -> &mut T {
            // SAFETY: guaranteed by the caller.
            unsafe { self.v.assume_init_mut() }
        }
        /// # Safety
        /// The value must have been set; it is moved out and must not be read again.
        #[inline]
        pub unsafe fn uninitialized_take(&mut self) -> T {
            // SAFETY: guaranteed by the caller.
            unsafe { self.v.assume_init_read() }
        }
        /// # Safety
        /// The value must have been set; it is destroyed in place.
        #[inline]
        pub unsafe fn uninitialized_destroy(&mut self) {
            // SAFETY: guaranteed by the caller.
            unsafe { self.v.assume_init_drop() }
        }
    }

    /// Whether `T` moves and drops trivially: every Rust move is a
    /// bitwise copy, so only destruction matters.
    pub const fn is_trivially_move_constructible_and_destructible<T>() -> bool {
        !mem::needs_drop::<T>()
    }

    /// Returns whether every element of `v` is true.
    pub const fn all_true(v: &[bool]) -> bool {
        let mut i = 0;
        while i < v.len() {
            if !v[i] {
                return false;
            }
            i += 1;
        }
        true
    }
}

//
// A future/promise pair maintain one logical value (a future_state).
// There are up to three places that can store it, but only one is
// active at any time.
//
// - in the promise _local_state member variable
//
//   This is necessary because a promise is created first and there
//   would be nowhere else to put the value.
//
// - in the future _state variable
//
//   This is used anytime a future exists and then has not been called
//   yet. This guarantees a simple access to the value for any code
//   that already has a future.
//
// - in the task associated with the .then() clause (after .then() is called,
//   if a value was not set)
//
//
// The promise maintains a pointer to the state, which is modified as
// the state moves to a new location due to events (such as .then() or
// get_future being called) or due to the promise or future being
// moved around.
//

/// The non-error states of a future state, one machine word.
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Moved from or never set; must not be used.
    Invalid = 0,
    /// No result yet; a promise will set it.
    Future = 1,
    // the substate is intended to decouple the run-time prevention
    // for duplicative result extraction (calling e.g. then() twice
    // ends up in abandoned()) from the wrapped object's destruction
    // handling which is orchestrated by future_state. Instead of
    // creating a temporary future_state just for the sake of setting
    // the "invalid" in the source instance, result_unavailable can
    // be set to ensure future_state_base::available() returns false.
    /// The result was taken: the state is available no more.
    ResultUnavailable = 2,
    /// Holds a value.
    Result = 3,
    /// The first value of an error pointer: every word from here on is an error.
    ExceptionMin = 4, // or anything greater
}

/// The state word of a future state: one machine word that is either a [`State`]
/// value below `exception_min`, or the [`ExceptionPtr`] of the error (which is always
/// `>= exception_min`).
///
/// The word is a pointer: the small states are pointers without provenance
/// (`ptr::without_provenance_mut`) and an exception is the pointer to its object, with its
/// provenance, so [`AnyState::ex`] reinterprets a pointer as a pointer.
#[repr(transparent)]
pub(crate) struct AnyState {
    st: *mut ExceptionHeader,
}

// SAFETY: the word is either a plain state or an owned ExceptionPtr, which is Send + Sync.
unsafe impl Send for AnyState {}
// SAFETY: as above.
unsafe impl Sync for AnyState {}

impl Default for AnyState {
    #[inline]
    fn default() -> Self {
        Self::from_state(State::Future)
    }
}

impl AnyState {
    /// A word holding the state `s`.
    #[inline]
    pub(crate) const fn from_state(s: State) -> Self {
        Self { st: ptr::without_provenance_mut(s as usize) }
    }
    /// A word holding the error `e`.
    #[inline]
    pub(crate) fn from_exception(e: ExceptionPtr) -> Self {
        let mut a = Self::from_state(State::Invalid);
        a.set_exception(e);
        a
    }
    /// The address of the state word.
    #[inline(always)]
    fn word(&self) -> usize {
        self.st.addr()
    }
    /// The state word as a [`State`] (`exception_min` for any exception).
    #[inline]
    pub(crate) fn st(&self) -> State {
        match self.word() {
            0 => State::Invalid,
            1 => State::Future,
            2 => State::ResultUnavailable,
            3 => State::Result,
            _ => State::ExceptionMin,
        }
    }
    /// Sets a non-error state.
    #[inline]
    pub(crate) fn set_st(&mut self, s: State) {
        debug_assert!(self.word() < State::ExceptionMin as usize && s < State::ExceptionMin);
        self.st = ptr::without_provenance_mut(s as usize);
    }
    /// Sets the error `e`.
    #[inline]
    pub(crate) fn set_exception(&mut self, e: ExceptionPtr) {
        self.st = e.into_raw();
        assert!(self.word() >= State::ExceptionMin as usize);
    }
    // From a users' perspective, a result_unavailable is not valid
    /// Whether the state is valid (not moved from).
    #[inline]
    pub(crate) fn valid(&self) -> bool {
        self.word() != State::Invalid as usize && self.word() != State::ResultUnavailable as usize
    }
    /// Whether the state holds a value or an error.
    #[inline]
    pub(crate) fn available(&self) -> bool {
        self.word() == State::Result as usize || self.word() >= State::ExceptionMin as usize
    }
    /// Whether the state holds an error.
    #[inline]
    pub(crate) fn failed(&self) -> bool {
        self.word() >= State::ExceptionMin as usize
    }
    /// Reports an error that is being dropped unconsumed.
    ///
    /// The test is inlined everywhere (every drop of a future state runs it); the report,
    /// out of line, is a cold call that takes the
    /// word by value, so the address of the state does not escape.
    #[inline(always)]
    pub(crate) fn check_failure(&mut self) {
        if self.failed() {
            internal_report::report_failed_future_any(mem::replace(
                self,
                Self::from_state(State::Invalid),
            ));
        }
    }
    /// Takes the error out, leaving the state invalid.
    #[inline]
    pub(crate) fn take_exception(&mut self) -> ExceptionPtr {
        // Moving the pointer out leaves nothing for the old word to release, so no drop
        // call is made on it; that keeps both branches of the callers identical, so the
        // compiler can merge them.
        debug_assert!(self.failed());
        // SAFETY: a state word >= exception_min is the pointer of an ExceptionPtr owned by
        // this state, with its provenance.
        let ret = unsafe { ExceptionPtr::from_raw(self.st) };
        self.st = ptr::without_provenance_mut(State::Invalid as usize);
        ret
    }
    /// Moves the word of `x` into this one, leaving `x` invalid.
    #[inline]
    pub(crate) fn move_it(&mut self, x: &mut AnyState) {
        // A plain copy of the word and a reset of `x` (see take_exception); correct also
        // when `x` and `self` are the same word.
        let v = x.st;
        x.st = ptr::without_provenance_mut(State::Invalid as usize);
        self.st = v;
    }
    /// Replaces this word with `x`, releasing an error held before.
    #[inline]
    pub(crate) fn assign(&mut self, mut x: AnyState) {
        self.check_failure();
        // If this is a self move assignment, check_failure
        // guarantees that we don't have an exception and calling
        // move_it is safe.
        self.move_it(&mut x);
    }
    /// The `ex` member of the union; the state must be failed.
    #[inline]
    pub(crate) fn ex(&self) -> &ExceptionPtr {
        debug_assert!(self.failed());
        // SAFETY: ExceptionPtr is a repr(transparent) `Option<NonNull<ExceptionHeader>>`,
        // with the layout of `*mut ExceptionHeader`, and a failed state word is the non-null
        // pointer of the ExceptionPtr owned by this state, provenance included.
        unsafe { &*(self as *const AnyState as *const ExceptionPtr) }
    }
}

/// Marker type: build a failed state from the error being handled.
pub struct CurrentExceptionFutureMarker;
/// Marker type: build a failed state from an error nested under another.
pub struct NestedExceptionMarker;

// non templated base class to reduce code duplication
/// The type-independent part of a future state: the state word, holding a [`State`] or an
/// error.
pub struct FutureStateBase {
    pub(crate) u: AnyState,
}

impl Default for FutureStateBase {
    fn default() -> Self {
        Self { u: AnyState::from_state(State::Future) }
    }
}

impl Drop for FutureStateBase {
    // We never need to destruct this polymorphicly, so we can make it
    // protected instead of virtual.
    //
    // Owning a state by value means dropping it is the event that checks for an
    // unconsumed error.
    #[inline(always)]
    fn drop(&mut self) {
        self.u.check_failure();
    }
}

impl FutureStateBase {
    #[inline]
    /// A state holding `st`.
    pub fn with_state(st: State) -> Self {
        Self { u: AnyState::from_state(st) }
    }
    #[inline]
    /// A failed state holding `ex`.
    pub fn from_exception(ex: ExceptionPtr) -> Self {
        Self { u: AnyState::from_exception(ex) }
    }
    /// A failed state holding `current`, the error being handled.
    #[inline]
    pub fn current_exception(_m: CurrentExceptionFutureMarker, current: ExceptionPtr) -> Self {
        Self::from_exception(current)
    }
    /// A failed state whose error is `n`'s, nested under the error of `old`.
    pub fn nested(
        _m: NestedExceptionMarker,
        mut n: FutureStateBase,
        mut old: FutureStateBase,
    ) -> Self {
        let inner = n.get_exception();
        if !old.failed() {
            Self::from_exception(inner)
        } else {
            Self::from_exception(make_nested(inner, &mut old))
        }
    }
    /// A failed state whose error is `current`, nested under the error of `old`.
    pub fn nested_current(
        _m: NestedExceptionMarker,
        current: ExceptionPtr,
        mut old: FutureStateBase,
    ) -> Self {
        if !old.failed() {
            Self::current_exception(CurrentExceptionFutureMarker, current)
        } else {
            Self::from_exception(make_nested(current, &mut old))
        }
    }

    /// Takes the error and returns it as `Err`.
    pub fn rethrow_exception<T>(&mut self) -> Result<T, ExceptionPtr> {
        // Move ex out so future::~future() knows we've handled it
        Err(self.get_exception())
    }
    /// Returns a clone of the error as `Err`.
    pub fn rethrow_exception_ref<T>(&self) -> Result<T, ExceptionPtr> {
        Err(self.u.ex().clone())
    }

    #[inline]
    /// Whether the state is valid (not moved from).
    pub fn valid(&self) -> bool {
        self.u.valid()
    }
    #[inline]
    /// Whether the state holds a result: a value or an error.
    pub fn available(&self) -> bool {
        self.u.available()
    }
    #[inline]
    /// Whether the state holds an error.
    pub fn failed(&self) -> bool {
        self.u.failed()
    }

    /// Discards the result (which must be there), without reporting an unconsumed error.
    pub fn ignore(&mut self) {
        match self.u.st() {
            State::Invalid | State::Future | State::ResultUnavailable => {
                crate::engine::core::reactor_backend::engine_assert!(
                    false,
                    "invalid state for ignore"
                );
                self.u.set_st(State::ResultUnavailable);
            }
            State::Result => {
                self.u.set_st(State::ResultUnavailable);
            }
            _ => {
                // Ignore the exception
                self.u.take_exception();
            }
        }
    }

    #[inline]
    /// Sets the error `ex`.
    pub fn set_exception(&mut self, ex: ExceptionPtr) {
        assert!(self.u.st() == State::Future);
        self.u.set_exception(ex);
    }
    /// Replaces this state with `x`.
    #[inline]
    pub fn assign(&mut self, mut x: FutureStateBase) {
        let u = mem::replace(&mut x.u, AnyState::from_state(State::Invalid));
        self.u.assign(u);
    }
    #[inline]
    /// Sets the error held by `state`.
    pub fn set_exception_state(&mut self, state: FutureStateBase) {
        assert!(self.u.st() == State::Future);
        self.assign(state);
    }
    /// Takes the error out.
    #[inline]
    pub fn get_exception(&mut self) -> ExceptionPtr {
        assert!(self.u.st() >= State::ExceptionMin);
        // Move ex out so future::~future() knows we've handled it
        self.u.take_exception()
    }
    /// Returns the error.
    #[inline]
    pub fn get_exception_ref(&self) -> &ExceptionPtr {
        assert!(self.u.st() >= State::ExceptionMin);
        self.u.ex()
    }
    /// Moves the state word and exception out, leaving `invalid` behind.
    #[inline]
    pub fn take_base(&mut self) -> FutureStateBase {
        FutureStateBase { u: mem::replace(&mut self.u, AnyState::from_state(State::Invalid)) }
    }
}

/// Nests errors: the error of `old` becomes the outer one, nested under `inner`.
fn make_nested(inner: ExceptionPtr, old: &mut FutureStateBase) -> ExceptionPtr {
    let outer = old.get_exception();
    let nested = NestedException::new(inner, outer);
    make_exception_ptr::<NestedException>(nested)
}

/// The report of failed futures dropped with their error unconsumed.
pub mod internal_report {
    use super::*;

    /// Logs that a failed future holding `eptr` was abandoned.
    pub fn report_failed_future(eptr: &ExceptionPtr) {
        // SAFETY: a future is only abandoned on a reactor thread, and no reference to the
        // reactor is live across this single increment.
        unsafe { (*crate::engine::core::reactor::engine())._abandoned_failed_futures += 1 };
        crate::log_warn!(
            crate::engine::core::reactor::engine_logger(),
            "Exceptional future ignored: {}, backtrace: {}",
            eptr,
            crate::engine::util::backtrace::current_backtrace()
        );
    }

    /// Logs that a failed future state was abandoned.
    pub fn report_failed_future_state(state: &FutureStateBase) {
        report_failed_future(state.u.ex());
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn report_failed_future_any(mut state: AnyState) {
        report_failed_future(&state.take_exception());
    }
} // internal namespace

pub use internal_report::{report_failed_future, report_failed_future_state};

/// Marker type: build a state holding a value.
pub struct ReadyFutureMarker;
/// Marker type: build a failed state.
pub struct ExceptionFutureMarker;
/// Marker type: build a future for a promise.
pub struct FutureForGetPromiseMarker;

/// The state of a future or promise: no result yet, a value of type `T`, or an error.
pub struct FutureState<T> {
    pub(crate) base: FutureStateBase,
    v: internal::UninitializedWrapper<T>,
}

impl<T> Default for FutureState<T> {
    fn default() -> Self {
        Self { base: FutureStateBase::default(), v: Default::default() }
    }
}

impl<T> Drop for FutureState<T> {
    /// Drops the state. Only a held value needs work here: an
    /// exception is reported by the drop of `base` right after (its `check_failure`), so
    /// this is `clear()` without a second test of the state, and for a `T` with nothing to
    /// destroy it is no code at all.
    #[inline(always)]
    fn drop(&mut self) {
        if mem::needs_drop::<T>() && self.base.u.st() == State::Result {
            // SAFETY: state::result means the value is initialized and owned here.
            unsafe { self.v.uninitialized_destroy() };
            self.base.u.set_st(State::Invalid);
        }
    }
}

impl<T> FutureState<T> {
    /// Whether cloning a value cannot fail; Rust clones are not checked for
    /// panics; kept for the API.
    pub const COPY_NOEXCEPT: bool = true;
    /// Whether moving and dropping the state is trivial; always true in Rust.
    pub const HAS_TRIVIAL_MOVE_AND_DESTROY: bool =
        internal::is_trivially_move_constructible_and_destructible::<T>();

    #[inline]
    /// A state with no result yet.
    pub fn new() -> Self {
        Self::default()
    }
    #[inline]
    /// An invalid (moved-from) state.
    pub fn invalid() -> Self {
        Self { base: FutureStateBase::with_state(State::Invalid), v: Default::default() }
    }

    /// Drops the held value, if any, and resets the state. A Rust value whose state is `result_unavailable` has
    /// already been moved out (or destroyed by `ignore()`), so only `result` owns it.
    #[inline]
    pub fn clear(&mut self) {
        if self.base.u.st() == State::Result {
            // SAFETY: state::result means the value is initialized and owned here.
            unsafe { self.v.uninitialized_destroy() };
            self.base.u.set_st(State::Invalid);
        } else {
            self.base.u.check_failure();
        }
    }

    /// Replaces this state with `x`.
    #[inline]
    pub fn assign(&mut self, x: FutureState<T>) {
        *self = x;
    }

    /// A state holding the value `a`.
    #[inline]
    pub fn ready(_m: ReadyFutureMarker, a: T) -> Self {
        let mut s =
            Self { base: FutureStateBase::with_state(State::Result), v: Default::default() };
        s.v.uninitialized_set(a);
        s
    }

    #[inline]
    /// Stores the value `a`; the state must hold no result yet.
    pub fn set(&mut self, a: T) {
        assert!(self.base.u.st() == State::Future);
        // The old state is `future`: overwriting it destroys nothing.
        let old = mem::replace(self, Self::ready(ReadyFutureMarker, a));
        mem::forget(old);
    }
    #[inline]
    /// A failed state holding `ex`.
    pub fn exception(_m: ExceptionFutureMarker, ex: ExceptionPtr) -> Self {
        Self { base: FutureStateBase::from_exception(ex), v: Default::default() }
    }
    #[inline]
    /// A failed state holding the error of `state`.
    pub fn from_base(_m: ExceptionFutureMarker, state: FutureStateBase) -> Self {
        Self { base: state, v: Default::default() }
    }
    #[inline]
    /// A failed state holding `current`, the error being handled.
    pub fn current_exception(m: CurrentExceptionFutureMarker, current: ExceptionPtr) -> Self {
        Self { base: FutureStateBase::current_exception(m, current), v: Default::default() }
    }
    #[inline]
    /// A failed state whose error is `n`'s, nested under the error of `old`.
    pub fn nested(m: NestedExceptionMarker, n: FutureStateBase, old: FutureStateBase) -> Self {
        Self { base: FutureStateBase::nested(m, n, old), v: Default::default() }
    }
    #[inline]
    /// A failed state whose error is `current`, nested under the error of `old`.
    pub fn nested_current(
        m: NestedExceptionMarker,
        current: ExceptionPtr,
        old: FutureStateBase,
    ) -> Self {
        Self { base: FutureStateBase::nested_current(m, current, old), v: Default::default() }
    }

    #[inline]
    /// Whether the state is valid (not moved from).
    pub fn valid(&self) -> bool {
        self.base.valid()
    }
    #[inline]
    /// Whether the state holds a result: a value or an error.
    pub fn available(&self) -> bool {
        self.base.available()
    }
    #[inline]
    /// Whether the state holds an error.
    pub fn failed(&self) -> bool {
        self.base.failed()
    }
    #[inline]
    /// Discards the result (which must be there), without reporting an unconsumed error.
    pub fn ignore(&mut self) {
        if self.base.u.st() == State::Result {
            // SAFETY: state::result means the value is initialized.
            unsafe { self.v.uninitialized_destroy() };
        }
        self.base.ignore();
    }
    #[inline]
    /// Sets the error `ex`.
    pub fn set_exception(&mut self, ex: ExceptionPtr) {
        self.base.set_exception(ex);
    }
    #[inline]
    /// Sets the error held by `state`.
    pub fn set_exception_state(&mut self, state: FutureStateBase) {
        self.base.set_exception_state(state);
    }
    #[inline]
    /// Takes the error out.
    pub fn get_exception(&mut self) -> ExceptionPtr {
        self.base.get_exception()
    }
    #[inline]
    /// Returns the error.
    pub fn get_exception_ref(&self) -> &ExceptionPtr {
        self.base.get_exception_ref()
    }
    /// Takes the state word out, as a state without a value type.
    #[inline]
    pub fn take_base(&mut self) -> FutureStateBase {
        assert!(self.base.u.st() != State::Result);
        self.base.take_base()
    }

    /// Takes the value, which must be there.
    #[inline]
    pub fn get_value(&mut self) -> T {
        assert!(self.base.u.st() == State::Result);
        self.base.u.set_st(State::ResultUnavailable);
        // SAFETY: state was result, the value is initialized; it is now marked taken.
        unsafe { self.v.uninitialized_take() }
    }
    /// Takes the value, which must be there, leaving the state without a result.
    #[inline]
    pub fn take_value(&mut self) -> T {
        assert!(self.base.u.st() == State::Result);
        self.base.u.set_st(State::ResultUnavailable);
        // SAFETY: state was result, the value is initialized; it is now marked taken.
        unsafe { self.v.uninitialized_take() }
    }
    /// Returns the value, which must be there.
    #[inline]
    pub fn get_value_ref(&self) -> &T {
        assert!(self.base.u.st() == State::Result);
        // SAFETY: state is result, the value is initialized.
        unsafe { self.v.uninitialized_get() }
    }
    /// Takes the result: the value, or the error.
    #[inline]
    pub fn take(&mut self) -> Result<T, ExceptionPtr> {
        assert!(self.available());
        if self.base.u.st() >= State::ExceptionMin {
            return self.base.rethrow_exception();
        }
        self.base.u.set_st(State::ResultUnavailable);
        // SAFETY: state was result, the value is initialized; it is now marked taken.
        Ok(unsafe { self.v.uninitialized_take() })
    }
    /// Takes the result: the value, or the error.
    #[inline]
    pub fn get(&mut self) -> Result<T, ExceptionPtr> {
        self.take()
    }
    /// Returns the result by reference: the value, or a clone of the error.
    #[inline]
    pub fn get_ref(&self) -> Result<&T, ExceptionPtr> {
        assert!(self.available());
        if self.base.u.st() >= State::ExceptionMin {
            return self.base.rethrow_exception_ref();
        }
        // SAFETY: state is result, the value is initialized.
        Ok(unsafe { self.v.uninitialized_get() })
    }
    #[inline]
    /// Takes the result: the value, or the error.
    pub fn get0(&mut self) -> Result<T, ExceptionPtr> {
        self.get()
    }
}

/// A task with a [`FutureState<T>`] slot that a promise can
/// fill. Implementors embed a [`TaskBase`] and a [`FutureState<T>`].
///
/// # Safety
/// [`ContinuationBase::state_ptr`] must return the address of a `FutureState<T>` field of
/// the task, derived from `this` (`ptr::addr_of_mut!((*this).field)`).
pub unsafe trait ContinuationBase<T>: Task {
    /// The `_state` member, reached from the pointer to the whole task.
    ///
    /// # Safety
    /// `this` must point to a live task.
    unsafe fn state_ptr(this: *mut Self) -> *mut FutureState<T>;

    /// Writes `state` into the task's slot.
    ///
    /// # Safety
    /// `this` must point to a live task, with no reference to its state live.
    #[inline]
    unsafe fn set_state(this: *mut Self, state: FutureState<T>) {
        // SAFETY: guaranteed by the caller.
        unsafe { *Self::state_ptr(this) = state };
    }
}

// Given a future type, find the corresponding continuation_base.
// (A continuation for a future of `T` is any `ContinuationBase<T>`.)

/// A continuation:
/// the task created by `then()`/`then_wrapped()` on an unavailable future.
///
/// The promise of the next future is the
/// embedded [`internal::PromiseNode`]; the next future points at it. Embedding it keeps
/// the allocation count at one per continuation: the allocation outlives
/// the run of the task until the next future has let go of the node.
#[repr(C)]
pub struct Continuation<T, U, Func, Wrapper>
where
    Wrapper: FnOnce(internal::PromiseBaseWithType<U>, Func, FutureState<T>),
{
    // First field: the task header (see `Task`).
    task: TaskBase,
    state: FutureState<T>,
    node: internal::PromiseNode<U>,
    // Func is the user's function passed to then/then_wrapped. The
    // Wrapper is a helper function that implements the specific logic
    // needed by then/then_wrapped. We call the wrapper passing it the
    // function, promise and state.
    // Note that if Func's move constructor throws, this will call
    // std::unexpected. We could try to require Func to be nothrow
    // move constructible, but that will cause a lot of churn. Since
    // we can't support a failure to create a continuation, calling
    // std::unexpected as close to the failure as possible is the best
    // we can do.
    func: ManuallyDrop<Func>,
    wrapper: ManuallyDrop<Wrapper>,
    // `node_live` (the embedded node is still referenced, by the next future or a writer)
    // and `done` (`run_and_dispose()` has run) are `node.holder_node_live` and
    // `node.holder_done`, in the padding of the node.
}

impl<T, U, Func, Wrapper> Continuation<T, U, Func, Wrapper>
where
    T: 'static,
    U: 'static,
    Func: 'static,
    Wrapper: FnOnce(internal::PromiseBaseWithType<U>, Func, FutureState<T>) + 'static,
{
    /// Allocates the continuation; its node starts with the writer reference (owned by
    /// the continuation until it runs) and the future reference (owned by the caller's
    /// next future).
    #[inline]
    fn alloc(func: Func, wrapper: Wrapper) -> *mut Self {
        // Out of drop's reach before the allocation, whose failure handler could unwind:
        // a drop on that path takes the functions' address and keeps them in a stack
        // temporary that is then copied whole (see `CoroutinePromise::alloc`). The
        // handler aborts, so nothing is lost.
        let func = ManuallyDrop::new(func);
        let wrapper = ManuallyDrop::new(wrapper);
        let sg = current_scheduling_group();
        // Built in place on the heap, field by field (as anotherproduct and async-task do), so the
        // function object is written once and not built on the stack and copied.
        let mut b = Box::<Self>::new_uninit();
        let c = b.as_mut_ptr();
        // SAFETY: c is the uninitialized allocation of a Self, uniquely owned here; every
        // field is written once before the box is given up as initialized, and the field
        // pointers are derived from the pointer to the whole object.
        unsafe {
            ptr::addr_of_mut!((*c).task).write(TaskBase::new::<Self>(sg));
            ptr::addr_of_mut!((*c).state).write(FutureState::new());
            ptr::addr_of_mut!((*c).node)
                .write(internal::PromiseNode::new_detached(true, Self::release_node));
            ptr::addr_of_mut!((*c).func).write(func);
            ptr::addr_of_mut!((*c).wrapper).write(wrapper);
            let c = Box::into_raw(b).cast::<Self>();
            let n = ptr::addr_of_mut!((*c).node);
            (*n).state = ptr::addr_of_mut!((*n).local_state);
            (*c).task.make_backtrace();
            c
        }
    }

    /// The embedded node, with the provenance of the whole continuation (so that
    /// [`Continuation::release_node`] can step back to the continuation).
    ///
    /// # Safety
    /// `c` must point to a live continuation.
    #[inline(always)]
    unsafe fn node_ptr(c: *mut Self) -> *mut internal::PromiseNode<U> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*c).node) }
    }

    /// Release hook of the embedded node.
    ///
    /// # Safety
    /// `n` must be the `node` field of a live `Continuation` of this type, obtained with
    /// [`Continuation::node_ptr`].
    unsafe fn release_node(n: *mut internal::PromiseNode<U>) {
        // SAFETY: the node is embedded at a fixed offset of its continuation, and `n` has
        // the provenance of the whole continuation (node_ptr).
        unsafe {
            let c = n.byte_sub(mem::offset_of!(Self, node)).cast::<Self>();
            (*n).holder_node_live = false;
            if (*n).holder_done {
                drop(Box::from_raw(c));
            }
        }
    }
}

// SAFETY: Continuation is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl<T, U, Func, Wrapper> Task for Continuation<T, U, Func, Wrapper>
where
    T: 'static,
    U: 'static,
    Func: 'static,
    Wrapper: FnOnce(internal::PromiseBaseWithType<U>, Func, FutureState<T>) + 'static,
{
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        let c = this.as_ptr();
        // SAFETY: the task runs exactly once, so func and wrapper are taken once; every
        // access goes through the raw pointer, so no reference to the continuation is live
        // when the wrapper (which may release the node) runs or when it is freed.
        unsafe {
            let func = ManuallyDrop::take(&mut (*c).func);
            let wrapper = ManuallyDrop::take(&mut (*c).wrapper);
            let state = mem::replace(&mut (*c).state, FutureState::invalid());
            // The writer reference of the node passes to the promise handed to the wrapper.
            let pr = internal::PromiseBaseWithType::from_node(Self::node_ptr(c));
            // When the next future no longer refers to the node (it was consumed by the
            // next continuation or awaiter, which hold their own state), the continuation
            // holds the node's future reference while the promise lets go of it: releasing
            // the writer then does not go through the release hook, and the continuation
            // is freed below, with its promise.
            let n = Self::node_ptr(c);
            let held = !(*n).future_ref;
            if held {
                (*n).future_ref = true;
            }
            // try { _wrapper(...) } catch (...) { _pr.set_to_current_exception(); }:
            // the wrappers turn an `Err` returned by the function into an exception on the
            // promise themselves.
            wrapper(pr, func, state);
            (*n).holder_done = true;
            if held {
                (*n).future_ref = false;
                // A forward chain may still hold the writer reference (the function
                // returned an unavailable future); release_node frees the continuation
                // when it lets go.
                if (*n).writer_ref {
                    return;
                }
                (*n).holder_node_live = false;
            }
            if !(*n).holder_node_live {
                // Continuations are allocated with Box::new in alloc(); nothing refers to
                // the node any more and `c` is not used after this point.
                drop(Box::from_raw(c));
            }
        }
    }
    // The waiting task of a continuation is the one waiting on its promise.
    #[inline]
    unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
        let c = this.as_ptr();
        // SAFETY: the continuation is live; it holds the writer reference on its node
        // until it runs.
        unsafe {
            if (*Self::node_ptr(c)).holder_done {
                return None;
            }
            (*internal::PromiseNode::resolve(Self::node_ptr(c))).task
        }
    }
}

// SAFETY: state_ptr returns the `state` field, derived from `this`.
unsafe impl<T, U, Func, Wrapper> ContinuationBase<T> for Continuation<T, U, Func, Wrapper>
where
    T: 'static,
    U: 'static,
    Func: 'static,
    Wrapper: FnOnce(internal::PromiseBaseWithType<U>, Func, FutureState<T>) + 'static,
{
    #[inline]
    unsafe fn state_ptr(this: *mut Self) -> *mut FutureState<T> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this).state) }
    }
}

// --------------------------------------------------------------------------------------
// The untyped and typed promise bases (`internal`).
// --------------------------------------------------------------------------------------

mod promise_detail {
    use super::*;

    /// Whether a ready task is scheduled ahead of the others.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Urgent {
        /// Schedule normally.
        No,
        /// Schedule at the front of the queue.
        Yes,
    }

    /// The address-stable identity of a promise (see the notes at the top of
    /// this file). It is shared by the writer and, while one is linked, by the future;
    /// when neither refers to it any more its `release` hook frees the memory that holds
    /// it (its own box, or the continuation it is embedded in).
    pub struct PromiseNode<T> {
        /// A future is linked to this promise.
        pub(crate) future: bool,
        /// The linked (or detached but not yet consumed) future reads its state from
        /// `local_state`.
        pub(crate) future_ref: bool,
        /// A writer (a promise handle, the continuation that embeds the node, or another
        /// node's `forward`) refers to this node.
        pub(crate) writer_ref: bool,
        // This points to the future_state that is currently being
        // used. See comment above the future_state struct definition for
        // details.
        pub(crate) state: *mut FutureState<T>,
        pub(crate) task: Option<TaskPtr>,
        #[cfg(engine_debug_promise)]
        pub(crate) task_shard: i32,
        /// Set when the writer of the promise living here was replaced by a moved
        /// promise (in `forward_to`): every writer operation goes to
        /// the end of the chain.
        pub(crate) forward: *mut PromiseNode<T>,
        /// Frees the memory holding the node once it is unreferenced.
        release: unsafe fn(*mut PromiseNode<T>),
        /// The result while no continuation or awaiter has its own state: the promise's
        /// local state, and the state of the linked future.
        pub(crate) local_state: FutureState<T>,
        /// For a node embedded in a task ([`crate::engine::core::make_task::LambdaTask`],
        /// [`Continuation`]): the node is still referenced (by the future or a writer).
        /// Kept here, in the padding after the node's flags, and not in the task: a
        /// yield task is then 64 bytes instead of 72, so it falls in the
        /// 64-byte size class, whose free list keeps 128 objects (the 80-byte one keeps 102,
        /// and a hundred tasks queued at once trimmed it and refilled it every time, as in
        /// the `cor_pfe_suspend_100` benchmark). Unused by the other nodes.
        pub(crate) holder_node_live: bool,
        /// For a node embedded in a task: the task's `run_and_dispose()` has run.
        pub(crate) holder_done: bool,
    }

    impl<T> PromiseNode<T> {
        /// A node whose `state` pointer is still null (the caller points it at
        /// `local_state` once the node has its final address).
        #[inline]
        pub(crate) fn new_detached(
            with_future: bool,
            release: unsafe fn(*mut PromiseNode<T>),
        ) -> Self {
            PromiseNode {
                future: with_future,
                future_ref: with_future,
                writer_ref: true,
                state: ptr::null_mut(),
                task: None,
                #[cfg(engine_debug_promise)]
                task_shard: -1,
                forward: ptr::null_mut(),
                release,
                local_state: FutureState::new(),
                holder_node_live: true,
                holder_done: false,
            }
        }

        /// Allocates a stand-alone node linked to a future, holding `local_state`. The
        /// memory comes from this shard's recycled node blocks ([`super::node_pool`]),
        /// so in the steady state it costs a free-list pop, not an allocator call.
        pub(crate) fn alloc_boxed(local_state: FutureState<T>) -> *mut Self {
            let mut node = Self::new_detached(true, Self::release_boxed);
            node.local_state = local_state;
            let n = super::node_pool::alloc(std::alloc::Layout::new::<Self>()).cast::<Self>();
            // SAFETY: n is a fresh block with the layout of Self, uniquely owned here.
            unsafe {
                ptr::write(n, node);
                (*n).state = ptr::addr_of_mut!((*n).local_state);
            }
            n
        }

        /// # Safety
        /// `n` must be a node allocated by `alloc_boxed` with no references left.
        unsafe fn release_boxed(n: *mut Self) {
            // SAFETY: guaranteed by the caller; the block came from node_pool::alloc with
            // this layout.
            unsafe {
                ptr::drop_in_place(n);
                super::node_pool::dealloc(n.cast::<u8>(), std::alloc::Layout::new::<Self>());
            }
        }

        /// # Safety
        /// `n` must be a live node.
        #[inline(always)]
        pub(crate) unsafe fn maybe_free(n: *mut Self) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                if !(*n).future_ref && !(*n).writer_ref {
                    ((*n).release)(n);
                }
            }
        }

        /// Follows the forward chain to the node that currently acts as the promise.
        ///
        /// # Safety
        /// `n` must be a live node.
        #[inline]
        pub(crate) unsafe fn resolve(mut n: *mut Self) -> *mut Self {
            // SAFETY: every node in a forward chain is kept alive by the writer reference
            // its predecessor holds.
            unsafe {
                while !(*n).forward.is_null() {
                    n = (*n).forward;
                }
            }
            n
        }

        /// Releases the writer reference held on `n` and, through the forward chain, on
        /// every node it forwards to.
        ///
        /// # Safety
        /// `n` must be a live node on which the caller holds the writer reference.
        pub(crate) unsafe fn release_writer_chain(mut n: *mut Self) {
            // SAFETY: see resolve().
            unsafe {
                loop {
                    let next = (*n).forward;
                    (*n).forward = ptr::null_mut();
                    (*n).writer_ref = false;
                    Self::maybe_free(n);
                    if next.is_null() {
                        break;
                    }
                    n = next;
                }
            }
        }

        /// Moves the promise of node `p` into `w`, the promise of a future being forwarded;
        /// the writer reference on `p` is transferred.
        ///
        /// # Safety
        /// `w` and `p` must be live nodes; the caller gives up its writer reference on `p`.
        pub(crate) unsafe fn adopt_writer(w: *mut Self, mut p: *mut Self) {
            // SAFETY: see resolve().
            unsafe {
                // clear(): nothing is linked to w any more (its future was detached and it
                // has no task), so there is nothing to break.
                debug_assert!((*w).task.is_none() && !(*w).future && (*w).forward.is_null());
                // Compress the chain so that it never grows: take over p's forward and
                // drop p.
                while !(*p).forward.is_null() {
                    let q = (*p).forward;
                    (*p).forward = ptr::null_mut();
                    (*p).writer_ref = false;
                    Self::maybe_free(p);
                    p = q;
                }
                (*w).forward = p;
            }
        }

        /// Sets the task waiting for the result (debug build: records its shard).
        #[cfg(engine_debug_promise)]
        #[inline]
        pub(crate) fn set_task(&mut self, task: Option<TaskPtr>) {
            self.task = task;
            self.task_shard = this_shard_id() as i32;
        }
        /// Checks that the waiting task belongs to this shard (debug build).
        #[cfg(engine_debug_promise)]
        pub(crate) fn assert_task_shard(&self) {
            if self.task_shard >= 0 && self.task_shard as ShardId != this_shard_id() {
                crate::engine::core::on_internal_error::on_fatal_internal_error(
                    crate::engine::core::reactor::engine_logger(),
                    &format!(
                        "Promise task was set on shard {} but made ready on shard {}",
                        self.task_shard,
                        this_shard_id()
                    ),
                );
            }
        }
        /// Sets the task waiting for the result.
        #[cfg(not(engine_debug_promise))]
        #[inline]
        pub(crate) fn set_task(&mut self, task: Option<TaskPtr>) {
            self.task = task;
        }
        /// Checks that the waiting task belongs to this shard; no check in this build.
        #[cfg(not(engine_debug_promise))]
        #[inline]
        pub(crate) fn assert_task_shard(&self) {}

        /// Makes the result available and schedules the waiting task, if any (urgently
        /// if `urgent`).
        ///
        /// # Safety
        /// `n` must be a live node, with no reference to it live.
        #[inline]
        pub(crate) unsafe fn make_ready(n: *mut Self, urgent: Urgent) {
            // SAFETY: guaranteed by the caller; the fields are accessed through `n` only.
            let task = unsafe { (*n).task.take() };
            if let Some(t) = task {
                // SAFETY: as above.
                unsafe { (*n).assert_task_shard() };
                if urgent == Urgent::Yes {
                    schedule_urgent(t);
                } else {
                    schedule(t);
                }
            }
        }
    }

    /// Sets `state` to the broken-promise error.
    #[inline(never)]
    fn set_to_broken_promise<T>(state: &mut FutureState<T>) {
        // Kept out of line: building the BrokenPromise error allocates.
        state.set_exception(make_exception_ptr(BrokenPromise::new()));
    }

    /// The writer handle of a promise. It owns the writer reference on its
    /// node; dropping it breaks a promise that was not resolved.
    #[repr(transparent)]
    pub struct PromiseBase<T> {
        pub(crate) node: NonNull<PromiseNode<T>>,
    }

    impl<T> Drop for PromiseBase<T> {
        // We never need to destruct this polymorphicly, so we can make it
        // protected instead of virtual
        //
        // The usual end of a promise is inline: it has done its job (no task waits on it,
        // no future is still linked) and does not forward, so clear() only gives up the
        // writer reference. Anything else goes through the out-of-line clear().
        #[inline(always)]
        fn drop(&mut self) {
            let n = self.node.as_ptr();
            // SAFETY: the handle owns the writer reference on its node, which keeps it
            // alive; its fields are accessed through the raw pointer.
            unsafe {
                if (*n).forward.is_null() && (*n).task.is_none() && !(*n).future {
                    (*n).writer_ref = false;
                    PromiseNode::maybe_free(n);
                    return;
                }
            }
            self.clear();
        }
    }

    impl<T> PromiseBase<T> {
        /// A handle taking over the writer reference on `n`.
        #[inline]
        pub(crate) fn from_node(n: *mut PromiseNode<T>) -> Self {
            debug_assert!(!n.is_null());
            // SAFETY: callers pass live nodes, never null.
            Self { node: unsafe { NonNull::new_unchecked(n) } }
        }

        /// The node that currently plays the role of this promise.
        ///
        /// It is a raw pointer: its fields are read and written through it, so no
        /// reference to the whole node overlaps the pointer to its `local_state` that
        /// `state` may hold.
        #[inline(always)]
        pub(crate) fn r(&self) -> *mut PromiseNode<T> {
            // SAFETY: the handle holds a writer reference, which keeps the whole chain
            // alive.
            unsafe { PromiseNode::resolve(self.node.as_ptr()) }
        }

        /// Gives up the handle without running `clear()`, returning its writer reference.
        #[inline]
        pub(crate) fn into_raw(self) -> *mut PromiseNode<T> {
            let n = self.node.as_ptr();
            mem::forget(self);
            n
        }

        /// Releases the writer reference, breaking an unresolved promise (out of line; the
        /// drop inlines its usual case)
        #[inline(never)]
        pub(crate) fn clear(&mut self) {
            let r = self.r();
            // SAFETY: the handle keeps the node alive; its fields are accessed through the
            // raw pointer, and `state` points to a live future_state (the node's or a
            // continuation's) whenever it is not null.
            unsafe {
                if let Some(t) = (*r).task.take() {
                    let st = (*r).state;
                    assert!(!st.is_null());
                    assert!(!(*st).available());
                    set_to_broken_promise(&mut *st);
                    schedule(t);
                }
                if (*r).future {
                    let st = (*r).state;
                    assert!(!st.is_null());
                    if !(*st).available() {
                        set_to_broken_promise(&mut *st);
                    }
                    // _future->detach_promise()
                    (*r).future = false;
                    (*r).state = ptr::null_mut();
                }
                // The handle owns the writer reference on its node.
                PromiseNode::release_writer_chain(self.node.as_ptr());
            }
        }

        /// Resolves the promise with the failed state `val`.
        #[inline]
        pub(crate) fn set_exception_impl(&mut self, val: FutureStateBase) {
            let r = self.r();
            // SAFETY: the handle keeps the node alive.
            let st = unsafe { (*r).state };
            if !st.is_null() {
                // SAFETY: state points to a live future_state; no reference to the node is
                // live.
                unsafe {
                    (*st).set_exception_state(val);
                    PromiseNode::make_ready(r, Urgent::No);
                }
            } else {
                // We get here if promise::get_future is called and the
                // returned future is destroyed without creating a
                // continuation.
                // In older versions of engine we would store a local
                // copy of ex and warn in the promise destructor.
                // Since there isn't any way for the user to clear
                // the exception, we issue the warning from here.
                internal_report::report_failed_future_state(&val);
                let mut val = val;
                drop(val.u.take_exception());
            }
        }

        /// Fails the promise with the error of `state`.
        #[inline]
        pub fn set_exception_state(&mut self, state: FutureStateBase) {
            self.set_exception_impl(state);
        }

        /// Fails the promise with `ex`.
        #[inline]
        pub fn set_exception(&mut self, ex: ExceptionPtr) {
            self.set_exception_impl(FutureStateBase::from_exception(ex));
        }

        /// Fails the promise with the error `e`.
        #[inline]
        pub fn set_exception_from<E: Exception>(&mut self, e: E) {
            self.set_exception(make_exception_ptr(e));
        }

        /// Set this promise to the current exception.
        ///
        /// This is equivalent to set_exception(std::current_exception()),
        /// but expands to less code.
        #[inline]
        pub fn set_to_current_exception(&mut self, current: ExceptionPtr) {
            self.set_exception(current);
        }

        /// Returns the task which is waiting for this promise to resolve, or nullptr.
        #[inline]
        pub fn waiting_task(&self) -> Option<TaskPtr> {
            // SAFETY: the handle keeps the node alive.
            unsafe { (*self.r()).task }
        }
    }

    /// A promise with type but no local data.
    ///
    /// This is a promise without any local data. We use this for when the
    /// future is created first, so we know the promise always has an
    /// external place to point to. We cannot just use promise_base
    /// because we need to know the type that is being stored.
    #[repr(transparent)]
    pub struct PromiseBaseWithType<T> {
        pub(crate) base: PromiseBase<T>,
    }

    impl<T> PromiseBaseWithType<T> {
        /// Whether cloning a value cannot fail (always true in Rust).
        pub const COPY_NOEXCEPT: bool = FutureState::<T>::COPY_NOEXCEPT;

        /// A handle taking over the writer reference on `n`.
        #[inline]
        pub(crate) fn from_node(n: *mut PromiseNode<T>) -> Self {
            Self { base: PromiseBase::from_node(n) }
        }

        /// Gives up the handle, returning its writer reference.
        #[inline]
        pub(crate) fn into_raw(self) -> *mut PromiseNode<T> {
            self.base.into_raw()
        }

        /// Resolves the promise with `state` and schedules the waiting task urgently.
        #[inline]
        pub fn set_urgent_state(&mut self, state: FutureState<T>) {
            // The forward chain is followed once: get_state() and make_ready() act on the
            // same node, which nothing in between can change.
            let r = self.base.r();
            // SAFETY: the handle keeps the node alive.
            let ptr = unsafe { (*r).state };
            // The state can be null if the corresponding future has been
            // destroyed without producing a continuation.
            if !ptr.is_null() {
                // SAFETY: ptr points to a live future_state; no reference to the node is
                // live.
                unsafe {
                    // FIXME: This is a fairly expensive assert. It would be a
                    // good candidate for being disabled in release builds if
                    // we had such an assert.
                    assert!((*ptr).base.u.st() == State::Future);
                    // Overwrite the state in place: the old state is `future`
                    // and owns nothing.
                    ptr::write(ptr, state);
                    PromiseNode::make_ready(r, Urgent::Yes);
                }
            }
        }

        /// Resolves the promise with the value `a`.
        #[inline]
        pub fn set_value(&mut self, a: T) {
            // The forward chain is followed once (see set_urgent_state).
            let r = self.base.r();
            // SAFETY: the handle keeps the node alive.
            let s = unsafe { (*r).state };
            if !s.is_null() {
                // SAFETY: s points to a live future_state; no reference to the node is live.
                unsafe {
                    (*s).set(a);
                    PromiseNode::make_ready(r, Urgent::No);
                }
            }
        }

        /// Fails the promise with `ex`.
        #[inline]
        pub fn set_exception(&mut self, ex: ExceptionPtr) {
            self.base.set_exception(ex);
        }

        /// Fails the promise with the error of `state`.
        #[inline]
        pub fn set_exception_state(&mut self, state: FutureStateBase) {
            self.base.set_exception_state(state);
        }

        /// Fails the promise with the error `e`.
        #[inline]
        pub fn set_exception_from<E: Exception>(&mut self, e: E) {
            self.base.set_exception_from(e);
        }

        /// Set this promise to the current exception.
        ///
        /// This is equivalent to set_exception(std::current_exception()),
        /// but expands to less code.
        #[inline]
        pub fn set_to_current_exception(&mut self, current: ExceptionPtr) {
            self.base.set_to_current_exception(current);
        }

        /// Returns the task which is waiting for this promise to resolve, or nullptr.
        #[inline]
        pub fn waiting_task(&self) -> Option<TaskPtr> {
            self.base.waiting_task()
        }
    }

    /// Builds a failed future from the failed state `state`.
    #[inline]
    pub fn make_exception_future_from_state<T: 'static>(state: FutureStateBase) -> Future<T> {
        Future::from_state(FutureState::from_base(ExceptionFutureMarker, state))
    }

    /// Attaches the continuation `callback` to `fut`: it runs once the future resolves.
    ///
    /// # Safety
    /// `callback` must point to a live task that stays alive until it is run.
    #[inline]
    pub unsafe fn set_callback<T: 'static, C: ContinuationBase<T> + 'static>(
        fut: Future<T>,
        callback: NonNull<C>,
    ) {
        // SAFETY: forwarded from the caller.
        unsafe { fut.set_callback(callback) }
    }

    /// Calls `func` with the value (`()` for a future without a value).
    #[inline]
    pub fn future_invoke<T, R, F: FnOnce(T) -> R>(func: F, v: T) -> R {
        func(v)
    }
}

/// Allows a future value to be made available at a later time.
///
/// `T` is the type carried as the result of the associated future; `()` (default) for no
/// result.
///
/// Until `get_future()` is called the state is held inline. If a value
/// or an exception is already there, `get_future()` hands it to a ready future and
/// allocates nothing; otherwise it creates the node that links the promise with its
/// future (see the notes at the top of this file). A promise that lives in a heap
/// object of its own (a task, a waiter) should be a [`HostedPromise`] instead, whose
/// node is embedded in that object.
pub struct Promise<T: 'static = ()> {
    base: Option<internal::PromiseBaseWithType<T>>,
    local_state: FutureState<T>,
}

impl<T: 'static> Default for Promise<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: 'static> Promise<T> {
    /// Constructs an empty `promise`.
    ///
    /// Creates promise with no associated future yet (see get_future()).
    #[inline]
    pub fn new() -> Self {
        Self { base: None, local_state: FutureState::new() }
    }

    /// Set this promise to the current exception.
    ///
    /// This is equivalent to set_exception(std::current_exception()),
    /// but expands to less code.
    #[inline]
    pub fn set_to_current_exception(&mut self, current: ExceptionPtr) {
        self.set_exception(current);
    }

    /// Returns the task which is waiting for this promise to resolve, or nullptr.
    #[inline]
    pub fn waiting_task(&self) -> Option<TaskPtr> {
        self.base.as_ref().and_then(|b| b.waiting_task())
    }

    /// Gets the promise's associated future.
    ///
    /// The future and promise will be remember each other, even if either or
    /// both are moved.  When `set_value()` or `set_exception()` are called
    /// on the promise, the future will be become ready, and if a continuation
    /// was attached to the future, it will run.
    pub fn get_future(&mut self) -> Future<T> {
        // assert(!this->_future && this->_state && !this->_task): a promise whose ready
        // state was already handed out has no state any more.
        assert!(self.base.is_none() && self.local_state.valid());
        let st = mem::replace(&mut self.local_state, FutureState::invalid());
        if st.available() {
            // The value is already there: the future owns it from the start and the
            // promise is left detached (`_state == nullptr`), so nothing links them.
            return Future::from_state(st);
        }
        let n = internal::PromiseNode::alloc_boxed(st);
        self.base = Some(internal::PromiseBaseWithType::from_node(n));
        Future { promise: n, state: FutureState::invalid() }
    }

    /// Sets the promises value
    ///
    /// Forwards the arguments and makes them available to the associated
    /// future.  May be called either before or after `get_future()`.
    ///
    /// The argument is the promise's value type `T`; a promise of several values takes them
    /// as one tuple.
    #[inline]
    pub fn set_value(&mut self, a: T) {
        match &mut self.base {
            Some(b) => b.set_value(a),
            None if self.local_state.valid() => self.local_state.set(a),
            // Detached by get_future(): `_state == nullptr`, the value is dropped.
            None => {}
        }
    }

    /// Marks the promise as failed
    ///
    /// Forwards the exception argument to the future and makes it
    /// available.  May be called either before or after `get_future()`.
    #[inline]
    pub fn set_exception(&mut self, ex: ExceptionPtr) {
        match &mut self.base {
            Some(b) => b.set_exception(ex),
            None if self.local_state.valid() => self.local_state.set_exception(ex),
            // Detached by get_future(): `set_exception_impl` with a null `_state`.
            None => internal_report::report_failed_future(&ex),
        }
    }

    /// Marks the promise as failed
    ///
    /// Forwards the exception argument to the future and makes it
    /// available.  May be called either before or after `get_future()`.
    #[inline]
    pub fn set_exception_from<E: Exception>(&mut self, e: E) {
        self.set_exception(make_exception_ptr(e));
    }

    /// Resolves the promise with `state` and schedules the waiting task urgently.
    #[inline]
    pub fn set_urgent_state(&mut self, state: FutureState<T>) {
        match &mut self.base {
            Some(b) => b.set_urgent_state(state),
            None if self.local_state.valid() => {
                assert!(self.local_state.base.u.st() == State::Future);
                // SAFETY: the old state is `future` and owns nothing.
                unsafe { ptr::write(&mut self.local_state, state) };
            }
            // Detached by get_future(): the state is dropped.
            None => drop(state),
        }
    }
}

/// A promise embedded in a heap object of its own (a task or a waiter, owned by a
/// [`HostBox`]).
///
/// Its linking node is part of the object, so `get_future()` allocates nothing: this
/// promise never
/// moves (it is `!Unpin`, and its host is only reachable mutably as `Pin<&mut H>` or
/// through raw pointers). When the host object is destroyed while its future still
/// refers to the node, the host's fields are dropped right away and only its memory is
/// kept until the future lets go of the node, like the continuations of `then()`.
#[repr(C)]
pub struct HostedPromise<T: 'static> {
    // First field: the node's address is the promise's address.
    node: ManuallyDrop<internal::PromiseNode<T>>,
    /// The node, with the provenance of the whole host (set by `bind`); the writer and
    /// the future hold copies of it, so `release_node` can reach the host.
    node_ptr: *mut internal::PromiseNode<T>,
    /// The writer handle on `node` (`None` before the host is bound, or once it was
    /// moved away by `forward_to`).
    writer: Option<internal::PromiseBaseWithType<T>>,
    /// The host allocation and its deallocation.
    host: *mut u8,
    dealloc: unsafe fn(*mut u8),
    /// The host has been destroyed; its memory goes when the node is unreferenced.
    host_dropped: bool,
    /// The node points to itself and the future points to the node.
    _pin: PhantomPinned,
}

impl<T: 'static> Default for HostedPromise<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Frees the memory of a host allocated by [`HostBox::new`].
///
/// # Safety
/// `p` must come from `Box::<H>::into_raw`, with the value already dropped.
unsafe fn dealloc_host<H>(p: *mut u8) {
    // SAFETY: guaranteed by the caller; Box allocates with Layout::new::<H>().
    unsafe { std::alloc::dealloc(p, std::alloc::Layout::new::<H>()) };
}

/// Placeholder deallocation of an unbound hosted promise (never called: an unbound
/// promise has no writer, so its node is never released).
unsafe fn dealloc_unbound(_: *mut u8) {}

impl<T: 'static> HostedPromise<T> {
    /// An unbound promise, to be placed in its host; [`HostBox::new`] binds it.
    #[inline]
    pub fn new() -> Self {
        Self {
            node: ManuallyDrop::new(internal::PromiseNode::new_detached(false, Self::release_node)),
            node_ptr: ptr::null_mut(),
            writer: None,
            host: ptr::null_mut(),
            dealloc: dealloc_unbound,
            host_dropped: false,
            _pin: PhantomPinned,
        }
    }

    /// Binds the promise to its host at its final address.
    ///
    /// # Safety
    /// `this` must be the promise of the live boxed host `host`, derived from `host`.
    #[inline]
    unsafe fn bind<H>(this: *mut Self, host: *mut H) {
        // SAFETY: guaranteed by the caller; the node is at its final address and every
        // pointer stored here is derived from the pointer to the whole host.
        unsafe {
            let n: *mut internal::PromiseNode<T> = ptr::addr_of_mut!((*this).node).cast();
            (*n).state = ptr::addr_of_mut!((*n).local_state);
            (*this).node_ptr = n;
            (*this).host = host.cast::<u8>();
            (*this).dealloc = dealloc_host::<H>;
            (*this).writer = Some(internal::PromiseBaseWithType::from_node(n));
        }
    }

    /// Release hook of the node: frees the host once it is destroyed too.
    ///
    /// # Safety
    /// `n` must be the `node_ptr` of a live, bound `HostedPromise<T>`.
    unsafe fn release_node(n: *mut internal::PromiseNode<T>) {
        // SAFETY: the node is the first field of the repr(C) hosted promise, and `n` has
        // the provenance of the whole host (bind).
        unsafe {
            let hp = n.cast::<Self>();
            if (*hp).host_dropped {
                let (host, dealloc) = ((*hp).host, (*hp).dealloc);
                ManuallyDrop::drop(&mut *ptr::addr_of_mut!((*hp).node));
                dealloc(host);
            }
        }
    }

    /// Ends the host's use of the promise after its value was dropped in place, and
    /// frees the host now if nothing refers to the node any more.
    ///
    /// # Safety
    /// `this` must be the promise of the host `host`, derived from it, which was bound by
    /// [`HostBox::new`] and has just been dropped in place.
    unsafe fn finish_host<H>(this: *mut Self, host: *mut H) {
        // SAFETY: guaranteed by the caller; the node and these plain fields are not
        // dropped with the host (the writer was).
        unsafe {
            (*this).host_dropped = true;
            let n = (*this).node_ptr;
            if !(*n).future_ref && !(*n).writer_ref {
                ManuallyDrop::drop(&mut *ptr::addr_of_mut!((*this).node));
                dealloc_host::<H>(host.cast::<u8>());
            }
        }
    }

    // The operations below take the promise as a raw pointer: the writer and the future
    // reach the node through `node_ptr`, so no reference to the whole promise (which
    // covers the node) may be live while they run.

    /// Gets the promise's associated future (see [`Promise::get_future`]).
    ///
    /// # Safety
    /// `this` must point to a live, bound promise, derived from the pointer to its host,
    /// with no reference to the promise live.
    pub unsafe fn get_future(this: *mut Self) -> Future<T> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            assert!((*this).writer.is_some());
            let n = (*this).node_ptr;
            // assert(!this->_future && this->_state && !this->_task)
            assert!(!(*n).future && !(*n).future_ref && !(*n).state.is_null());
            (*n).future = true;
            (*n).future_ref = true;
            // The node lives as long as the future refers to it (see release_node).
            Future::from_node(n)
        }
    }

    /// Sets the promises value (see [`Promise::set_value`]).
    ///
    /// # Safety
    /// As [`HostedPromise::get_future`].
    #[inline]
    pub unsafe fn set_value(this: *mut Self, a: T) {
        // SAFETY: guaranteed by the caller; the reference covers the writer field only.
        if let Some(w) = unsafe { (*ptr::addr_of_mut!((*this).writer)).as_mut() } {
            w.set_value(a);
        }
    }

    /// Marks the promise as failed (see [`Promise::set_exception`]).
    ///
    /// # Safety
    /// As [`HostedPromise::get_future`].
    #[inline]
    pub unsafe fn set_exception(this: *mut Self, ex: ExceptionPtr) {
        // SAFETY: as in set_value.
        if let Some(w) = unsafe { (*ptr::addr_of_mut!((*this).writer)).as_mut() } {
            w.set_exception(ex);
        }
    }

    /// Marks the promise as failed with an exception object.
    ///
    /// # Safety
    /// As [`HostedPromise::get_future`].
    #[inline]
    pub unsafe fn set_exception_from<E: Exception>(this: *mut Self, e: E) {
        // SAFETY: forwarded from the caller.
        unsafe { Self::set_exception(this, make_exception_ptr(e)) };
    }

    /// Set this promise to the current exception.
    ///
    /// # Safety
    /// As [`HostedPromise::get_future`].
    #[inline]
    pub unsafe fn set_to_current_exception(this: *mut Self, current: ExceptionPtr) {
        // SAFETY: forwarded from the caller.
        unsafe { Self::set_exception(this, current) };
    }

    /// Resolves the promise with `state` and schedules the waiting task urgently.
    ///
    /// # Safety
    /// As [`HostedPromise::get_future`].
    #[inline]
    pub unsafe fn set_urgent_state(this: *mut Self, state: FutureState<T>) {
        // SAFETY: as in set_value.
        if let Some(w) = unsafe { (*ptr::addr_of_mut!((*this).writer)).as_mut() } {
            w.set_urgent_state(state);
        }
    }

    /// Returns the task which is waiting for this promise to resolve, or nullptr.
    ///
    /// # Safety
    /// `this` must point to a live promise.
    #[inline]
    pub unsafe fn waiting_task(this: *const Self) -> Option<TaskPtr> {
        // SAFETY: guaranteed by the caller.
        unsafe { (*ptr::addr_of!((*this).writer)).as_ref().and_then(|w| w.waiting_task()) }
    }

    /// The writer handle, for forwarding a future into this promise; the promise is empty
    /// afterwards.
    ///
    /// # Safety
    /// `this` must point to a live promise.
    #[inline]
    pub unsafe fn take_writer(this: *mut Self) -> Option<internal::PromiseBaseWithType<T>> {
        // SAFETY: guaranteed by the caller; the reference covers the writer field only.
        unsafe { (*ptr::addr_of_mut!((*this).writer)).take() }
    }

    /// Swaps two promises: afterwards each one resolves the future the other
    /// one had. The writer handles are exchanged, as a move of a promise repoints its
    /// future; each node stays in its host, which keeps its memory until the writer now held
    /// by the other host lets go of it (see `release_node`).
    ///
    /// # Safety
    /// `a` and `b` must point to live, distinct promises, with no reference to either live.
    #[inline]
    pub unsafe fn swap_with(a: *mut Self, b: *mut Self) {
        // SAFETY: guaranteed by the caller; the two writer fields are distinct and only they
        // are touched.
        unsafe { ptr::swap(ptr::addr_of_mut!((*a).writer), ptr::addr_of_mut!((*b).writer)) }
    }
}

/// A heap object that holds a [`HostedPromise`].
///
/// # Safety
/// [`PromiseHost::hosted_promise`] must return the address of a `HostedPromise` field of
/// the host, derived from `this` (`ptr::addr_of_mut!((*this).field)`), and always the
/// same field.
pub unsafe trait PromiseHost {
    /// The value type of the promise.
    type Value: 'static;
    /// The promise field, reached from the pointer to the whole host.
    ///
    /// # Safety
    /// `this` must point to a live host.
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<Self::Value>;
}

/// The owning pointer of a [`PromiseHost`] (a heap task or waiter):
/// like a `Pin<Box<H>>`, but dropping it keeps the memory until the future of the
/// hosted promise lets go of it.
///
/// The host never moves: it is reachable as `&H`, as `Pin<&mut H>` (and `H` is `!Unpin`
/// because its promise is), or through [`HostBox::as_ptr`].
pub struct HostBox<H: PromiseHost> {
    ptr: NonNull<H>,
}

impl<H: PromiseHost> HostBox<H> {
    /// Allocates the host (the only allocation) and binds its promise.
    #[inline]
    pub fn new(h: H) -> Self {
        // The value is moved straight into the allocation (no stack copy once inlined). It
        // is out of drop's reach before the allocation, whose failure handler could unwind:
        // a drop on that path takes its address, and the compiler then kept it in a stack
        // temporary copied whole into the allocation with 16-byte loads over narrower
        // stores (see `CoroutinePromise::alloc`). The handler aborts, so nothing is lost.
        let h = ManuallyDrop::new(h);
        let p = Box::into_raw(Box::<H>::new_uninit()).cast::<H>();
        // SAFETY: p is a fresh, uninitialized allocation of an H, uniquely owned here.
        unsafe { p.write(ManuallyDrop::into_inner(h)) };
        // SAFETY: p was just allocated and is uniquely owned here; the promise pointer is
        // derived from it.
        unsafe {
            let hp = H::hosted_promise(p);
            HostedPromise::bind(hp, p);
            HostBox { ptr: NonNull::new_unchecked(p) }
        }
    }

    /// Gives up ownership (`release()`); [`HostBox::from_raw`] takes it back.
    #[inline]
    pub fn into_raw(this: Self) -> *mut H {
        let me = ManuallyDrop::new(this);
        me.ptr.as_ptr()
    }

    /// Takes back ownership of a host released by [`HostBox::into_raw`].
    ///
    /// # Safety
    /// `p` must come from `HostBox::into_raw` and be taken back once.
    #[inline]
    pub unsafe fn from_raw(p: *mut H) -> Self {
        // SAFETY: guaranteed by the caller (into_raw never returns null).
        HostBox { ptr: unsafe { NonNull::new_unchecked(p) } }
    }

    /// The raw pointer to the host, with the provenance of the whole allocation.
    #[inline]
    pub fn as_ptr(&self) -> *mut H {
        self.ptr.as_ptr()
    }

    /// The host, pinned: it can be changed in place but not moved.
    #[inline]
    pub fn as_mut(&mut self) -> Pin<&mut H> {
        // SAFETY: the box owns a live host that never moves until it is freed.
        unsafe { Pin::new_unchecked(self.ptr.as_mut()) }
    }
}

impl<H: PromiseHost> std::ops::Deref for HostBox<H> {
    type Target = H;
    #[inline]
    fn deref(&self) -> &H {
        // SAFETY: the box owns a live host.
        unsafe { self.ptr.as_ref() }
    }
}

impl<H: PromiseHost> Drop for HostBox<H> {
    /// Drops the host: the fields are dropped now (the promise breaks if unset), the
    /// memory when the future no longer refers to the promise node.
    fn drop(&mut self) {
        let host = self.ptr.as_ptr();
        // SAFETY: the box owns the live, bound host; it is dropped once; the promise
        // pointer is derived from the host pointer.
        unsafe {
            let hp = H::hosted_promise(host);
            ptr::drop_in_place(host);
            HostedPromise::finish_host(hp, host);
        }
    }
}

/// Check whether a type is a future
///
/// This is a type trait evaluating to `true` if the given type is a
/// future.
pub trait IsFuture {
    /// Whether the type is a future.
    const VALUE: bool;
}

impl<T: 'static> IsFuture for Future<T> {
    const VALUE: bool = true;
}

/// Calls a function with the elements of a tuple as its arguments.
pub trait ApplyTuple<F> {
    /// What the function returns.
    type Output;
    /// Calls `f` with the elements of the tuple.
    fn apply(self, f: F) -> Self::Output;
}

macro_rules! impl_apply_tuple {
    ($($a:ident),*) => {
        impl<F, R, $($a),*> ApplyTuple<F> for ($($a,)*) where F: FnOnce($($a),*) -> R {
            type Output = R;
            #[allow(non_snake_case)]
            #[inline]
            fn apply(self, f: F) -> R {
                let ($($a,)*) = self;
                f($($a),*)
            }
        }
    };
}
impl_apply_tuple!();
impl_apply_tuple!(A0);
impl_apply_tuple!(A0, A1);
impl_apply_tuple!(A0, A1, A2);
impl_apply_tuple!(A0, A1, A2, A3);
impl_apply_tuple!(A0, A1, A2, A3, A4);
impl_apply_tuple!(A0, A1, A2, A3, A4, A5);
impl_apply_tuple!(A0, A1, A2, A3, A4, A5, A6);
impl_apply_tuple!(A0, A1, A2, A3, A4, A5, A6, A7);

/// Converts a type to a future type, if it isn't already.
///
/// The converted type is `Future<Self::Output>`.
///
/// The Rust results that a continuation can return: a [`Future`], a
/// `Result<T, ExceptionPtr>` (a value, or an error) or `()` (no value).
pub trait Futurize: Sized + 'static {
    /// The value type of the converted future.
    type Output: 'static;
    /// Whether `Self` is a future already.
    const IS_FUTURE: bool = false;

    /// Convert a value or a future to a future
    fn convert(self) -> Future<Self::Output>;

    /// Forwards the result to the promise. This avoids creating a future if func()
    /// doesn't return one.
    fn satisfy(self, pr: internal::PromiseBaseWithType<Self::Output>);

    /// Makes an exceptional future of type `type`.
    #[inline]
    fn make_exception_future(ex: ExceptionPtr) -> Future<Self::Output> {
        make_exception_future::<Self::Output>(ex)
    }

    #[inline]
    /// A failed future of the converted type, holding the error of `state`.
    fn make_exception_future_from_state(state: FutureStateBase) -> Future<Self::Output> {
        internal::make_exception_future_from_state::<Self::Output>(state)
    }

    /// Apply a function to an argument list (expressed as a tuple)
    /// and return the result, as a future (if it wasn't already).
    #[inline]
    fn apply<F, A: ApplyTuple<F, Output = Self>>(func: F, args: A) -> Future<Self::Output> {
        args.apply(func).convert()
    }

    /// Invoke a function to an argument list
    /// and return the result, as a future (if it wasn't already).
    #[inline]
    fn invoke<F: FnOnce() -> Self>(func: F) -> Future<Self::Output> {
        func().convert()
    }

    #[inline]
    /// A failed future of the converted type, holding `current`, the error being handled.
    fn current_exception_as_future(current: ExceptionPtr) -> Future<Self::Output> {
        Future::from_state(FutureState::current_exception(CurrentExceptionFutureMarker, current))
    }

    /// Convert the tuple representation into a future
    #[inline]
    fn from_tuple(value: Self::Output) -> Future<Self::Output> {
        Future::ready(ReadyFutureMarker, value)
    }

    /// Forwards the result of, or exception thrown by, func() to the
    /// promise. This avoids creating a future if func() doesn't
    /// return one.
    #[inline]
    fn satisfy_with_result_of<F: FnOnce() -> Self>(
        pr: internal::PromiseBaseWithType<Self::Output>,
        func: F,
    ) {
        func().satisfy(pr);
    }
}

impl<T: 'static> Futurize for Future<T> {
    type Output = T;
    const IS_FUTURE: bool = true;
    #[inline]
    fn convert(self) -> Future<T> {
        self
    }
    #[inline]
    fn satisfy(self, pr: internal::PromiseBaseWithType<T>) {
        self.forward_to_base(pr);
    }
}

impl<T: 'static> Futurize for Result<T, ExceptionPtr> {
    type Output = T;
    #[inline]
    fn convert(self) -> Future<T> {
        match self {
            Ok(v) => make_ready_future(v),
            Err(e) => make_exception_future(e),
        }
    }
    #[inline]
    fn satisfy(self, mut pr: internal::PromiseBaseWithType<T>) {
        match self {
            Ok(v) => pr.set_value(v),
            Err(e) => pr.set_exception(e),
        }
    }
}

/// Converts the result of a function that fails or returns a future: the error
/// becomes a failed future, the future is kept. `Futurize` itself cannot cover
/// `Result<Future<T>, _>`: it would overlap with the impl for `Result<T, _>` (stable Rust has no
/// specialization), which makes it a future of a future.
impl<T: 'static> From<Result<Future<T>, ExceptionPtr>> for Future<T> {
    #[inline]
    fn from(r: Result<Future<T>, ExceptionPtr>) -> Future<T> {
        match r {
            Ok(f) => f,
            Err(e) => make_exception_future(e),
        }
    }
}

impl Futurize for () {
    type Output = ();
    #[inline]
    fn convert(self) -> Future<()> {
        make_ready_future(())
    }
    #[inline]
    fn satisfy(self, mut pr: internal::PromiseBaseWithType<()>) {
        pr.set_value(());
    }
}

// Converts a type to a future type, if it isn't already.
/// The future type that `T` converts to.
pub type FuturizeT<T> = Future<<T as Futurize>::Output>;

/// A representation of a possibly not-yet-computed value.
///
/// A `future` represents a value that has not yet been computed
/// (an asynchronous computation).  It can be in one of several
/// states:
///    - unavailable: the computation has not been completed yet
///    - value: the computation has been completed successfully and a
///      value is available.
///    - failed: the computation completed with an exception.
///
/// methods in `future` allow querying the state and, most importantly,
/// scheduling a `continuation` to be executed when the future becomes
/// available.  Only one such continuation may be scheduled.
///
/// A `future` should not be discarded before it is waited upon and
/// its result is extracted. Discarding a `future` means that the
/// computed value becomes inaccessible, but more importantly, any
/// exceptions raised from the computation will disappear unchecked as
/// well. Another very important consequence is potentially unbounded
/// resource consumption due to the launcher of the deserted
/// continuation not being able track the amount of in-progress
/// continuations, nor their individual resource consumption.
/// To prevent accidental discarding of futures, `future` is
/// declared `[[nodiscard]]` if the compiler supports it. Also, when a
/// discarded `future` resolves with an error a warning is logged
/// (at runtime).
/// That said there can be legitimate cases where a `future` is
/// discarded. The most prominent example is launching a new
/// [fiber](`fiber`-module), or in other words, moving a continuation
/// chain to the background (off the current [fiber](`fiber`-module)).
/// Even if a `future` is discarded purposefully, it is still strongly
/// advisable to wait on it indirectly (via a `gate` or
/// `semaphore`), control their concurrency, their resource consumption
/// and handle any errors raised from them.
///
/// `T` is the type carried as the result of the future, `()` for none. An available
/// `Future<()>` only contains a success or failure indication (and, for a failure, the
/// error).
///
/// The layout is fixed: the promise pointer first, then the state.
/// A future is built with the pointer and the state word written together (one store
/// pair) and is moved in 16-byte pieces, so the first piece of a move is that pair and
/// forwards from the store buffer. In the order the compiler chose for a `Future<T>` with a
/// value (the state first), the first piece was the state word and the value bytes, which
/// an unavailable future never writes: a 16-byte load over an 8-byte store, which cannot
/// forward. `FutureState` keeps the order
/// the compiler gives it: with `repr(C)` there, a `future<>` stopped being returned in two
/// registers.
#[must_use]
#[repr(C)]
pub struct Future<T: 'static = ()> {
    /// The linked promise node, or null.
    promise: *mut internal::PromiseNode<T>,
    /// The state, used while no promise is linked (while one is, the state lives in the
    /// promise node).
    state: FutureState<T>,
}

impl<T: 'static> Drop for Future<T> {
    #[inline]
    fn drop(&mut self) {
        // ~future_base() { clear(); }
        if !self.promise.is_null() {
            self.detach_promise();
        }
    }
}

/// The value type of a `Future<T>`.
pub type FutureValueType<T> = T;

impl<T: 'static> Future<T> {
    /// Whether cloning a value cannot fail (always true in Rust).
    pub const COPY_NOEXCEPT: bool = FutureState::<T>::COPY_NOEXCEPT;

    // This constructor creates a future that is not ready but has no
    // associated promise yet. The use case is to have a less flexible
    // but more efficient future/promise pair where we know that
    // promise::set_value cannot possibly be called without a matching
    // future and so that promise doesn't need to store a
    // future_state.
    // (In Rust this future is the one `schedule()` returns: its promise is embedded in
    // the continuation.)

    /// A future linked to the promise `n` (its future reference is taken over), as
    /// Builds the future of a promise node; used by promises embedded in other objects.
    ///
    /// # Safety
    /// `n` must be a live node whose future reference the caller owns.
    #[inline]
    pub(crate) unsafe fn from_node(n: *mut internal::PromiseNode<T>) -> Self {
        Future { promise: n, state: FutureState::invalid() }
    }

    /// A ready future holding the value `a`.
    #[inline]
    pub(crate) fn ready(m: ReadyFutureMarker, a: T) -> Self {
        Future { promise: ptr::null_mut(), state: FutureState::ready(m, a) }
    }
    #[inline]
    pub(crate) fn current_exception(
        m: CurrentExceptionFutureMarker,
        current: ExceptionPtr,
    ) -> Self {
        Future { promise: ptr::null_mut(), state: FutureState::current_exception(m, current) }
    }
    #[inline]
    pub(crate) fn nested(
        m: NestedExceptionMarker,
        n: FutureStateBase,
        old: FutureStateBase,
    ) -> Self {
        Future { promise: ptr::null_mut(), state: FutureState::nested(m, n, old) }
    }
    #[inline]
    pub(crate) fn nested_current(
        m: NestedExceptionMarker,
        current: ExceptionPtr,
        old: FutureStateBase,
    ) -> Self {
        Future { promise: ptr::null_mut(), state: FutureState::nested_current(m, current, old) }
    }
    #[inline]
    pub(crate) fn exception(m: ExceptionFutureMarker, ex: ExceptionPtr) -> Self {
        Future { promise: ptr::null_mut(), state: FutureState::exception(m, ex) }
    }
    #[inline(always)]
    pub(crate) fn from_state(state: FutureState<T>) -> Self {
        Future { promise: ptr::null_mut(), state }
    }

    /// The future's own state, with the provenance of `f`: the coroutine awaiter links its
    /// promise to it, so the promise writes into the awaited future.
    ///
    /// # Safety
    /// `f` must point to a live future.
    #[inline(always)]
    pub(crate) unsafe fn state_ptr(f: *mut Self) -> *mut FutureState<T> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*f).state) }
    }

    /// The state the future currently reads (its own, or the one in the linked node).
    #[inline(always)]
    pub(crate) fn state(&self) -> &FutureState<T> {
        if self.promise.is_null() {
            &self.state
        } else {
            // SAFETY: the future holds a reference on the node.
            unsafe { &(*self.promise).local_state }
        }
    }
    #[inline(always)]
    pub(crate) fn state_mut(&mut self) -> &mut FutureState<T> {
        if self.promise.is_null() {
            &mut self.state
        } else {
            // SAFETY: the future holds a reference on the node.
            unsafe { &mut (*self.promise).local_state }
        }
    }

    /// Unlinks the promise; the state the node held for
    /// this future comes back into the future. Returns the node if a writer still
    /// refers to it (null otherwise).
    #[inline]
    pub(crate) fn detach_promise(&mut self) -> *mut internal::PromiseNode<T> {
        let n = mem::replace(&mut self.promise, ptr::null_mut());
        // SAFETY: the future held a reference on the node.
        unsafe {
            if (*n).future {
                (*n).state = ptr::null_mut();
                (*n).future = false;
            }
            let st = mem::replace(&mut (*n).local_state, FutureState::invalid());
            let old = mem::replace(&mut self.state, st);
            mem::forget(old); // invalid or future: nothing to destroy
            (*n).future_ref = false;
            let alive = (*n).writer_ref;
            internal::PromiseNode::maybe_free(n);
            if alive { n } else { ptr::null_mut() }
        }
    }

    /// Schedules `tws`, which waits on this future, with `state` as the state it receives.
    ///
    /// # Safety
    /// `state` must point into `tws`, which must stay alive until it is run.
    #[inline]
    pub(crate) unsafe fn schedule_task(&mut self, tws: TaskPtr, state: *mut FutureState<T>) {
        let p = self.detach_promise();
        assert!(!p.is_null());
        // SAFETY: p is alive (a writer refers to it).
        unsafe {
            (*p).state = state;
            (*p).set_task(Some(tws));
        }
    }

    /// Schedules the continuation `func`, run through `wrapper`, fused with the creation of
    /// the next future, whose promise is embedded in the continuation.
    #[inline]
    pub(crate) fn schedule<U, Func, Wrapper>(&mut self, func: Func, wrapper: Wrapper) -> Future<U>
    where
        U: 'static,
        Func: 'static,
        Wrapper: FnOnce(internal::PromiseBaseWithType<U>, Func, FutureState<T>) + 'static,
    {
        // If this allocation fails there is nothing that can be done about it: the
        // corresponding future is not ready and the chain cannot be broken, so the
        // allocation runs in a critical section and a failure aborts the process.
        let _g =
            crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
        let tws = Continuation::<T, U, Func, Wrapper>::alloc(func, wrapper);
        // SAFETY: tws was just allocated; it is owned by the reactor queue / the promise
        // from now on and frees itself once it has run and its node is unreferenced.
        unsafe {
            let fut =
                Future::<U> { promise: Continuation::node_ptr(tws), state: FutureState::invalid() };
            let tp = TaskPtr::from_task(NonNull::new_unchecked(tws));
            // In a debug build we schedule ready futures, but not in
            // other build modes.
            #[cfg(engine_debug)]
            if self.state().available() {
                Continuation::set_state(tws, self.get_available_state_ref());
                schedule(tp);
                return fut;
            }
            let st = Continuation::state_ptr(tws);
            self.schedule_task(tp, st);
            self.state.base.u.set_st(State::Invalid);
            fut
        }
    }

    #[inline(always)]
    pub(crate) fn get_available_state_ref(&mut self) -> FutureState<T> {
        if !self.promise.is_null() {
            self.detach_promise();
        }
        mem::replace(&mut self.state, FutureState::invalid())
    }

    fn take_old_base(&mut self) -> FutureStateBase {
        let mut s = self.get_available_state_ref();
        if s.failed() { s.take_base() } else { FutureStateBase::with_state(State::Invalid) }
    }

    pub(crate) fn rethrow_with_nested(mut self, n: FutureStateBase) -> Future<T> {
        let old = self.take_old_base();
        Future::nested(NestedExceptionMarker, n, old)
    }

    pub(crate) fn rethrow_with_nested_current(mut self, current: ExceptionPtr) -> Future<T> {
        let old = self.take_old_base();
        Future::nested_current(NestedExceptionMarker, current, old)
    }

    /// gets the value returned by the computation
    ///
    /// Requires that the future be available.  If the value
    /// was computed successfully, it is returned inside `Ok`. Otherwise, the error is
    /// returned.
    ///
    /// If get() is called in a `teseladb::thread` context,
    /// then it need not be available; instead, the thread will
    /// be paused until the future becomes available.
    #[inline(always)]
    pub fn get(mut self) -> Result<T, ExceptionPtr> {
        self.wait();
        self.get_available_state_ref().take()
    }

    /// Takes the result in place: the result of an
    /// available future, which is left unlinked and without a value, owning nothing.
    /// For the coroutine awaiter, whose future lives in the pinned frame.
    #[inline(always)]
    pub(crate) fn take_available(&mut self) -> Result<T, ExceptionPtr> {
        if self.promise.is_null() {
            // The usual case (a ready future, or one whose promise is gone): the state is
            // the one available() just tested.
            return self.state.take();
        }
        self.take_available_linked()
    }

    /// [`Future::take_available`] of a future still linked to its promise.
    #[inline(never)]
    fn take_available_linked(&mut self) -> Result<T, ExceptionPtr> {
        self.detach_promise();
        self.state.take()
    }

    #[inline(always)]
    /// Returns the error of the future, which must be available and failed.
    pub fn get_exception(mut self) -> ExceptionPtr {
        self.get_available_state_ref().get_exception()
    }

    /// Wait for the future to be available (in a teseladb::thread)
    ///
    /// When called from a teseladb::thread, this function blocks the
    /// thread until the future is available. Other threads and
    /// continuations continue to execute; only the thread is blocked.
    #[inline]
    pub fn wait(&mut self) {
        if self.state().available() {
            return;
        }
        self.do_wait();
    }

    /// Checks whether the future is available.
    ///
    /// Returns `true` if the future has a value, or has failed.
    #[inline(always)]
    pub fn available(&self) -> bool {
        self.state().available()
    }

    /// Checks whether the future has failed.
    ///
    /// Returns `true` if the future is available and has failed.
    #[inline(always)]
    pub fn failed(&self) -> bool {
        self.state().failed()
    }

    /// Schedule a block of code to run when the future is ready.
    ///
    /// Schedules a function (often a lambda) to run when the future becomes
    /// available.  The function is called with the result of this future's
    /// computation as parameters.  The return value of the function becomes
    /// the return value of then(), itself as a future; this allows then()
    /// calls to be chained.
    ///
    /// If the future failed, the function is not called, and the exception
    /// is propagated into the return value of then().
    ///
    /// `func` is the function to be called when the future becomes available,
    ///               unless it has failed.
    /// Returns a `future` representing the return value of `func`, applied
    ///         to the eventual value of this future.
    #[inline]
    pub fn then<F, R>(self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(T) -> R + 'static,
        R: Futurize,
    {
        #[cfg(not(engine_type_erase_more))]
        {
            self.then_impl(func)
        }
        #[cfg(engine_type_erase_more)]
        {
            let ncf: Box<dyn FnOnce(T) -> FuturizeT<R>> = {
                let _g = crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
                Box::new(move |v: T| futurize_invoke(move || func(v)))
            };
            self.then_impl(ncf)
        }
    }

    /// Schedule a block of code to run when the future is ready, unpacking tuples.
    ///
    /// Schedules a function (often a lambda) to run when the future becomes
    /// available.  The function is called with the result of this future's
    /// computation as parameters.  The return value of the function becomes
    /// the return value of then(), itself as a future; this allows then()
    /// calls to be chained.
    ///
    /// This member function is only available when the payload is a tuple;
    /// the tuple elements are passed as individual arguments to `func`, which
    /// must have the same arity as the tuple.
    ///
    /// If the future failed, the function is not called, and the exception
    /// is propagated into the return value of then().
    ///
    /// `func` is the function to be called when the future becomes available,
    ///               unless it has failed.
    /// Returns a `future` representing the return value of `func`, applied
    ///         to the eventual value of this future.
    pub fn then_unpack<F>(self, func: F) -> FuturizeT<<T as ApplyTuple<F>>::Output>
    where
        F: 'static,
        T: ApplyTuple<F>,
        <T as ApplyTuple<F>>::Output: Futurize,
    {
        self.then(move |tuple: T| {
            // sizeof...(tuple) is required to be 1
            tuple.apply(func)
        })
    }

    // Keep this simple so that Named Return Value Optimization is used.
    #[inline]
    fn then_impl_nrvo<F, R>(mut self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(T) -> R + 'static,
        R: Futurize,
    {
        self.schedule(
            func,
            |pr: internal::PromiseBaseWithType<R::Output>, func: F, mut state: FutureState<T>| {
                if state.failed() {
                    let mut pr = pr;
                    pr.set_exception_state(state.take_base());
                } else {
                    R::satisfy_with_result_of(pr, move || {
                        // clang thinks that "state" is not used, below, for future<>.
                        // Make it think it is used to avoid an unused-lambda-capture warning.
                        internal::future_invoke(func, state.get_value())
                    });
                }
            },
        )
    }

    #[inline]
    // The fast path that needs `mut` is compiled out with `engine_debug`.
    #[cfg_attr(engine_debug, allow(unused_mut))]
    fn then_impl<F, R>(mut self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(T) -> R + 'static,
        R: Futurize,
    {
        #[cfg(not(engine_debug))]
        {
            if self.failed() {
                let mut s = self.get_available_state_ref();
                return R::make_exception_future_from_state(s.take_base());
            } else if self.available() {
                let v = self.get_available_state_ref().take_value();
                return R::invoke(move || func(v));
            }
        }
        self.then_impl_nrvo(func)
    }

    /// Schedule a block of code to run when the future is ready, allowing
    ///        for exception handling.
    ///
    /// Schedules a function (often a lambda) to run when the future becomes
    /// available.  The function is called with the this future as a parameter;
    /// it will be in an available state.  The return value of the function becomes
    /// the return value of then_wrapped(), itself as a future; this allows
    /// then_wrapped() calls to be chained.
    ///
    /// Unlike then(), the function will be called for both value and exceptional
    /// futures.
    ///
    /// `func` is the function to be called when the future becomes available,
    /// Returns a `future` representing the return value of `func`, applied
    ///         to the eventual value of this future.
    #[inline]
    pub fn then_wrapped<F, R>(self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(Future<T>) -> R + 'static,
        R: Futurize,
    {
        self.then_wrapped_maybe_erase::<true, F, R>(func)
    }

    /// The `then_wrapped(Func&&) &` overload: the future is left consumed (invalid).
    #[inline]
    pub fn then_wrapped_ref<F, R>(&mut self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(Future<T>) -> R + 'static,
        R: Futurize,
    {
        let me = mem::replace(self, Future::from_state(FutureState::invalid()));
        me.then_wrapped_maybe_erase::<false, F, R>(func)
    }

    #[inline]
    fn then_wrapped_maybe_erase<const AS_SELF: bool, F, R>(self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(Future<T>) -> R + 'static,
        R: Futurize,
    {
        #[cfg(not(engine_type_erase_more))]
        {
            self.then_wrapped_common::<AS_SELF, F, R>(func)
        }
        #[cfg(engine_type_erase_more)]
        {
            let ncf: Box<dyn FnOnce(Future<T>) -> FuturizeT<R>> = {
                let _g = crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection::new();
                Box::new(move |f: Future<T>| R::invoke(move || func(f)))
            };
            self.then_wrapped_common::<AS_SELF, _, FuturizeT<R>>(ncf)
        }
    }

    // Keep this simple so that Named Return Value Optimization is used.
    #[inline]
    fn then_wrapped_nrvo<F, R>(mut self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(Future<T>) -> R + 'static,
        R: Futurize,
    {
        self.schedule(
            func,
            |pr: internal::PromiseBaseWithType<R::Output>, func: F, state: FutureState<T>| {
                R::satisfy_with_result_of(pr, move || func(Future::from_state(state)));
            },
        )
    }

    #[inline]
    // The fast path that needs `mut` is compiled out with `engine_debug`.
    #[cfg_attr(engine_debug, allow(unused_mut))]
    fn then_wrapped_common<const AS_SELF: bool, F, R>(mut self, func: F) -> FuturizeT<R>
    where
        F: FnOnce(Future<T>) -> R + 'static,
        R: Futurize,
    {
        #[cfg(not(engine_debug))]
        {
            if self.available() {
                if AS_SELF {
                    if !self.promise.is_null() {
                        self.detach_promise();
                    }
                    return R::invoke(move || func(self));
                } else {
                    let s = self.get_available_state_ref();
                    return R::invoke(move || func(Future::from_state(s)));
                }
            }
        }
        self.then_wrapped_nrvo(func)
    }

    #[inline(always)]
    pub(crate) fn forward_to_base(mut self, mut pr: internal::PromiseBaseWithType<T>) {
        if self.state().available() {
            let s = self.get_available_state_ref();
            pr.set_urgent_state(s);
        } else {
            self.forward_unavailable_to_base(pr);
        }
    }

    /// The unavailable branch of [`Future::forward_to_base`]: moves `pr` into the promise
    /// detached from this future.
    #[inline(never)]
    fn forward_unavailable_to_base(&mut self, pr: internal::PromiseBaseWithType<T>) {
        let w = self.detach_promise();
        assert!(!w.is_null());
        // SAFETY: w is alive (a writer refers to it); pr gives up its writer reference.
        unsafe { internal::PromiseNode::adopt_writer(w, pr.into_raw()) };
    }

    /// Satisfy some `promise` object with this future as a result.
    ///
    /// Arranges so that when this future is resolve, it will be used to
    /// satisfy an unrelated promise.  This is similar to scheduling a
    /// continuation that moves the result of this future into the promise
    /// (using promise::set_value() or promise::set_exception(), except
    /// that it is more efficient.
    ///
    /// `pr` is a promise that will be fulfilled with the results of this
    /// future.
    pub fn forward_to(mut self, mut pr: Promise<T>) {
        if self.state().available() {
            let s = self.get_available_state_ref();
            pr.set_urgent_state(s);
        } else {
            if let Some(base) = pr.base.take() {
                // The only case when _state points to _local_state is
                // when get_future was never called. Given that pr will
                // soon be destroyed, we know get_future will never be
                // called and we can just ignore this request.
                let w = self.detach_promise();
                assert!(!w.is_null());
                // SAFETY: as in forward_to_base.
                unsafe { internal::PromiseNode::adopt_writer(w, base.into_raw()) };
            }
        }
    }

    /// Runs `func` once this future resolves, successfully or not, and resolves with this
    /// future's result.
    ///
    /// `func` may return a future, which is waited for; its value is ignored, so this
    /// future's result is preserved, but its error is propagated.
    ///
    /// # Errors
    ///
    /// The returned future fails if this future or the future of `func` fails. When both
    /// fail, it fails with a [`NestedException`] holding the error of `func` on top and
    /// this future's error nested in it.
    pub fn finally<F, R>(self, func: F) -> Future<T>
    where
        F: FnOnce() -> R + 'static,
        R: Futurize,
    {
        if R::IS_FUTURE {
            let body = FinallyBody::<F, true>::new(func);
            self.then_wrapped(move |result: Future<T>| body.call_future(result))
        } else {
            let body = FinallyBody::<F, false>::new(func);
            self.then_wrapped(move |result: Future<T>| body.call_value(result))
        }
    }

    /// Terminate the program if this future fails.
    ///
    /// Terminates the entire program is this future resolves
    /// to an exception.  Use with caution.
    pub fn or_terminate(self) -> Future<()> {
        self.then_wrapped(|f: Future<T>| {
            if let Err(e) = f.get() {
                engine_exit(e);
            }
        })
    }

    /// Discards the value carried by this future.
    ///
    /// Converts the future into a no-value `future<>`, by
    /// ignoring any result.  Exceptions are propagated unchanged.
    pub fn discard_result(self) -> Future<()> {
        // We need the generic variadic lambda, below, because then() behaves differently
        // when value_type is when_all_succeed_tuple
        self.then(|_: T| {})
    }

    /// Handle the exception carried by this future.
    ///
    /// When the future resolves, if it resolves with an exception,
    /// handle_exception(func) replaces the exception with the value
    /// returned by func. The exception is passed (as an [`ExceptionPtr`])
    /// as a parameter to func; func may return the replacement value
    /// immediately (a `T`) or in the future (a `Future<T>`)
    /// and is even allowed to return (or throw) its own exception.
    ///
    /// The idiom fut.discard_result().handle_exception(...) can be used
    /// to handle an exception (if there is one) without caring about the
    /// successful value; Because handle_exception() is used here on a
    /// future<>, the handler function does not need to return anything.
    pub fn handle_exception<F, R>(self, func: F) -> Future<T>
    where
        F: FnOnce(ExceptionPtr) -> R + 'static,
        R: Futurize<Output = T>,
    {
        self.then_wrapped(move |fut: Future<T>| -> Future<T> {
            if !fut.failed() {
                match fut.get() {
                    Ok(v) => make_ready_future::<T>(v),
                    Err(e) => make_exception_future::<T>(e),
                }
            } else {
                let ex = fut.get_exception();
                futurize_invoke(move || func(ex))
            }
        })
    }

    /// Handle the exception of a certain type carried by this future.
    ///
    /// When the future resolves, if it resolves with an exception of a type that
    /// provided callback receives as a parameter, `handle_exception_type`(func) replaces
    /// the exception with the value returned by func. The exception is passed (by
    /// reference) as a parameter to func; func may return the replacement value
    /// immediately (a `T`) or in the future (a `Future<T>`)
    /// and is even allowed to return (or throw) its own exception.
    /// If exception, that future holds, does not match func parameter type
    /// it is propagated as is.
    pub fn handle_exception_type<E, F, R>(self, func: F) -> Future<T>
    where
        E: 'static,
        F: FnOnce(&E) -> R + 'static,
        R: Futurize<Output = T>,
    {
        // using trait = function_traits<Func>; static_assert(trait::arity == 1, ...):
        // the single parameter is `&E`.
        self.then_wrapped(move |fut: Future<T>| -> Future<T> {
            match fut.get() {
                Ok(v) => make_ready_future::<T>(v),
                Err(ex) => match ex.downcast_ref::<E>() {
                    Some(e) => func(e).convert(),
                    None => make_exception_future::<T>(ex),
                },
            }
        })
    }

    /// Ignore any result hold by this future
    ///
    /// Ignore any result (value or exception) hold by this future.
    /// Use with caution since usually ignoring exception is not what
    /// you want
    pub fn ignore_ready_future(&mut self) {
        self.state_mut().ignore();
    }

    /// Makes `coroutine` the task woken up when this future resolves.
    pub fn set_coroutine(&mut self, coroutine: TaskPtr) {
        assert!(!self.promise.is_null());
        // SAFETY: the future holds a reference on its linked node.
        unsafe { (*self.promise).set_task(Some(coroutine)) };
    }

    /// Undoes [`Future::set_coroutine`] for an awaiter dropped while it waits: takes the
    /// task back from the promise. Returns whether it was still registered (`false` once
    /// the promise has scheduled it, or when no promise is linked).
    #[inline]
    pub(crate) fn take_coroutine(&mut self) -> bool {
        if self.promise.is_null() {
            return false;
        }
        // SAFETY: the future holds a reference on its linked node, the one
        // `set_coroutine` registered the task on.
        unsafe { (*self.promise).task.take().is_some() }
    }

    /// Suspends a coroutine awaiter on this future: the promise is pointed
    /// at `state` (in the pinned awaiter) with `coroutine` as its task, and this future is
    /// detached. Returns the node, for [`Future::unlink_awaiter`].
    ///
    /// # Safety
    /// The future must be unavailable and linked; `state` must stay in place until the
    /// coroutine runs or `unlink_awaiter` is called. The awaiter passes this future's own
    /// state, which it has already set to `future` (no result yet, owns nothing).
    #[inline(always)]
    pub(crate) unsafe fn link_awaiter(
        &mut self,
        coroutine: TaskPtr,
        state: *mut FutureState<T>,
    ) -> *mut internal::PromiseNode<T> {
        // detach_promise() of an unavailable future, without its general cases: the node's
        // local state is `future` and owns nothing, so there is nothing to move into this
        // future (whose own state the awaiter has already set to `future`, the value the
        // general path would move here), and the writer is alive (a writer that goes breaks
        // the promise into the state, which would make it available), so the node cannot be
        // freed here.
        let p = mem::replace(&mut self.promise, ptr::null_mut());
        assert!(!p.is_null());
        // SAFETY: p is alive (its writer refers to it); guaranteed by the caller.
        unsafe {
            debug_assert!(
                !(*p).local_state.available() && (*p).writer_ref && !self.state.available()
            );
            (*p).future = false;
            (*p).future_ref = false;
            (*p).state = state;
            (*p).set_task(Some(coroutine));
        }
        p
    }

    /// Undoes [`Future::link_awaiter`] for an awaiter dropped before its promise was
    /// fulfilled: the promise forgets the state and the task. Returns whether the task was
    /// still registered (it was not scheduled).
    ///
    /// # Safety
    /// `p` must be the node `link_awaiter` returned, and the awaited state must still be
    /// unavailable (which proves the writer, and so the node, alive).
    pub(crate) unsafe fn unlink_awaiter(p: *mut internal::PromiseNode<T>) -> bool {
        // SAFETY: guaranteed by the caller.
        unsafe {
            (*p).state = ptr::null_mut();
            (*p).task.take().is_some()
        }
    }

    pub(crate) fn set_task(&mut self, t: TaskPtr) {
        assert!(!self.promise.is_null());
        // SAFETY: the future holds a reference on its linked node.
        unsafe { (*self.promise).set_task(Some(t)) };
    }

    /// # Safety
    /// `callback` must stay alive until it is run.
    pub(crate) unsafe fn set_callback<C: ContinuationBase<T> + 'static>(
        mut self,
        callback: NonNull<C>,
    ) {
        let cb = callback.as_ptr();
        // SAFETY: guaranteed by the caller.
        unsafe {
            let tp = TaskPtr::from_task(callback);
            if self.state().available() {
                C::set_state(cb, self.get_available_state_ref());
                schedule(tp);
            } else {
                assert!(!self.promise.is_null());
                let st = C::state_ptr(cb);
                self.schedule_task(tp, st);
            }
        }
    }

    /// Blocks the running engine thread until the future is available.
    #[cold]
    #[inline(never)]
    fn do_wait(&mut self) {
        let thread = crate::engine::core::thread_impl::get();
        assert!(!thread.is_null());
        let mut wake_task = ThreadWakeTask::new(thread);
        wake_task.task.make_backtrace();
        assert!(!self.promise.is_null());
        // The task is only reached through this raw pointer from now on: the reactor runs
        // it while this frame is suspended in switch_out.
        let wp: *mut ThreadWakeTask = &raw mut wake_task;
        // The promise writes into this future's own state, which cannot move
        // while the thread is suspended here (it is borrowed by this call): the future is
        // detached from the node, so the node is not needed once the promise is done.
        let st: *mut FutureState<T> = &raw mut self.state;
        // SAFETY: wake_task lives on this thread's stack until the thread is switched back
        // in by it; the state stays in place for the same time; the node is alive (the
        // future is unavailable, so its writer has not gone).
        unsafe {
            let tp = TaskPtr::from_task(NonNull::new_unchecked(wp));
            self.schedule_task(tp, st);
        }
        // SAFETY: `thread` is the running engine thread.
        unsafe { crate::engine::core::thread_impl::switch_out(thread) };
    }
}

/// The continuation of `finally`: runs the function and keeps the initial result.
pub struct FinallyBody<F, const FUNC_RETURNS_FUTURE: bool> {
    func: F,
}

impl<F, const B: bool> FinallyBody<F, B> {
    /// Wraps the function of a `finally` call.
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

impl<F, R> FinallyBody<F, true>
where
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    /// Runs the function, whose result is a future, and then resolves to `result`; if the
    /// function failed, the result fails with both errors nested.
    pub fn call_future<T: 'static>(self, result: Future<T>) -> Future<T> {
        futurize_invoke(self.func).then_wrapped(move |mut f_res: Future<R::Output>| {
            if !f_res.failed() {
                result
            } else {
                let n = f_res.get_available_state_ref().take_base();
                result.rethrow_with_nested(n)
            }
        })
    }
}

impl<F, R> FinallyBody<F, false>
where
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    /// Runs the function, whose result is a value, and resolves to `result`; if the
    /// function failed, the result fails with both errors nested.
    pub fn call_value<T: 'static>(self, result: Future<T>) -> Future<T> {
        let r = futurize_invoke(self.func);
        if !r.failed() {
            let mut r = r;
            r.ignore_ready_future();
            result
        } else {
            result.rethrow_with_nested_current(r.get_exception())
        }
    }
}

/// Creates a `future` in an available, value state.
///
/// Creates a `future` object that is already resolved.  This
/// is useful when it is determined that no I/O needs to be performed
/// to perform a computation (for example, because the data is cached
/// in some buffer).
#[inline]
pub fn make_ready_future<T: 'static>(value: T) -> Future<T> {
    Future::ready(ReadyFutureMarker, value)
}

/// Creates a `future` in an available, failed state.
///
/// Creates a `future` object that is already resolved in a failed
/// state.  This is useful when no I/O needs to be performed to perform
/// a computation (for example, because the connection is closed and
/// we cannot read from it).
#[inline]
pub fn make_exception_future<T: 'static>(ex: ExceptionPtr) -> Future<T> {
    Future::exception(ExceptionFutureMarker, ex)
}

/// Returns std::current_exception() wrapped in a future
///
/// This is equivalent to
/// make_exception_future(std::current_exception()), but expands to
/// less code.
#[inline]
pub fn current_exception_as_future<T: 'static>(current: ExceptionPtr) -> Future<T> {
    Future::current_exception(CurrentExceptionFutureMarker, current)
}

pub use crate::engine::core::exception_hacks::log_exception_trace;

/// Creates a `future` in an available, failed state.
///
/// Creates a `future` object that is already resolved in a failed
/// state.  This no I/O needs to be performed to perform a computation
/// (for example, because the connection is closed and we cannot read
/// from it).
#[inline]
pub fn make_exception_future_from<T: 'static, E: Exception>(ex: E) -> Future<T> {
    log_exception_trace();
    make_exception_future::<T>(make_exception_ptr(ex))
}

/// A failed future whose error carries
/// the backtrace of this call in its message.
pub fn make_exception_future_with_backtrace<
    T: 'static,
    E: std::error::Error + Send + Sync + 'static,
>(
    ex: E,
) -> Future<T> {
    make_exception_future::<T>(crate::engine::util::backtrace::make_backtraced_exception_ptr::<E>(
        ex,
    ))
}

/// Calls `func` and converts its result to a future; the arguments are captured by the
/// closure.
#[inline]
pub fn futurize_invoke<F, R>(func: F) -> FuturizeT<R>
where
    F: FnOnce() -> R,
    R: Futurize,
{
    R::invoke(func)
}

/// Calls `func` with the elements of the tuple `args` and converts its result to a future.
#[inline]
pub fn futurize_apply<F, A>(func: F, args: A) -> FuturizeT<<A as ApplyTuple<F>>::Output>
where
    A: ApplyTuple<F>,
    <A as ApplyTuple<F>>::Output: Futurize,
{
    <<A as ApplyTuple<F>>::Output as Futurize>::apply(func, args)
}

// ---------------------------------------------------------------------------------------
// Abandoned failed futures and blocking waits
// ---------------------------------------------------------------------------------------

// We can't test future_state_base directly because its private
// destructor is protected.
//
const _: () = assert!(mem::size_of::<FutureState<()>>() <= 8, "FutureState<()> is too large");
const _: () = assert!(mem::size_of::<FutureState<i64>>() <= 16, "future_state<long> is too large");
const _: () = assert!(
    FutureState::<()>::HAS_TRIVIAL_MOVE_AND_DESTROY,
    "FutureState<()> must have a trivial move and destroy"
);
const _: () = assert!(
    FutureState::<i64>::HAS_TRIVIAL_MOVE_AND_DESTROY,
    "FutureState<i64> must have a trivial move and destroy"
);

// We need to be able to move and copy an ExceptionPtr in and out
// of future/promise/continuations without that producing a new
// exception. (ExceptionPtr clones are reference count increments.)

/// Exits the reactor.
///
/// `eptr` is the error that prompted the exit, or a null pointer when no error caused it;
/// the reactor exits with status 0 for a null pointer, and otherwise reports the error
/// and exits with status 1.
pub fn engine_exit(eptr: ExceptionPtr) {
    if eptr.is_null() {
        crate::engine::core::reactor::Reactor::exit(0);
        return;
    }
    crate::engine::core::report_exception::report_exception(
        "Exiting on unhandled exception",
        &eptr,
    );
    crate::engine::core::reactor::Reactor::exit(1);
}

/// Test support: tolerate abandoned failed futures.
impl crate::engine::core::reactor::ReactorTest {
    /// Runs `func` with the engine logger at error level, and asserts that exactly `count`
    /// failed futures were abandoned meanwhile.
    pub fn with_allow_abandoned_failed_futures(count: u32, func: Box<dyn FnOnce()>) {
        let r = crate::engine::core::reactor::engine();
        // SAFETY: called on a reactor thread; each access is a single read or write of the
        // counter with no reference to the reactor live.
        let before = unsafe { (*r)._abandoned_failed_futures };
        let old_level = crate::engine::core::reactor::engine_logger().level();
        crate::engine::core::reactor::engine_logger()
            .set_level(crate::engine::util::log::LogLevel::Error);
        func();
        // SAFETY: as above.
        let after = unsafe { (*r)._abandoned_failed_futures };
        assert!(after - before == u64::from(count));
        // SAFETY: as above.
        unsafe { (*r)._abandoned_failed_futures = before };
        crate::engine::core::reactor::engine_logger().set_level(old_level);
    }
}

/// The task that wakes a thread blocked in `get()` on an unavailable future.
#[repr(C)]
struct ThreadWakeTask {
    // First field: the task header.
    task: TaskBase,
    thread: *mut crate::engine::core::thread::ThreadContext,
}

impl ThreadWakeTask {
    fn new(thread: *mut crate::engine::core::thread::ThreadContext) -> Self {
        Self { task: TaskBase::new::<Self>(current_scheduling_group()), thread }
    }
}

// SAFETY: ThreadWakeTask is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl Task for ThreadWakeTask {
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        // SAFETY: the task lives on the suspended thread's stack until the switch below
        // resumes that thread; the field is copied out first.
        let thread = unsafe { (*this.as_ptr()).thread };
        // SAFETY: the thread is live and suspended in do_wait().
        unsafe { crate::engine::core::thread_impl::switch_in(thread) };
        // no need to delete, since this is always allocated on
        // _thread's stack.
    }
    /// Returns the task which is waiting for this thread to be done, or nullptr.
    unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
        // SAFETY: the task is live and the thread context outlives its wake task.
        unsafe {
            crate::engine::core::thread::ThreadContext::waiting_task_of((*this.as_ptr()).thread)
        }
    }
}
