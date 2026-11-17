//! Workarounds that let stack unwinding scale across many threads, and accounting of unwinds.
//!
//! Every unwind (a panic, or an exception raised by linked native code) asks the libgcc unwinder to
//! find the frame description entries of each frame. Two global locks sit on that path:
//!
//! 1. The lock in `_Unwind_Find_FDE` (libgcc) protects the list of entries registered with
//!    `__register_frame*`. Dynamically linked binaries never register any, and current libgcc
//!    checks that the list is empty before taking the lock, so this one costs nothing.
//! 2. The lock in `dl_iterate_phdr` (glibc) protects the list of loaded objects against concurrent
//!    loading and unloading.
//!
//! The second lock is removed by relying on the fact that the process does not load or unload
//! shared objects once the reactors run: [`init_phdr_cache`] copies the program headers of every
//! loaded object once, and this module exports its own `dl_iterate_phdr` that walks the copy without
//! any lock.
//!
//! One catch: because of its lock, glibc's `dl_iterate_phdr` never runs two callbacks at the same
//! time, and libgcc relies on that for a small unsynchronized cache of lookups. libgcc only uses
//! that cache when the `dl_phdr_info` it receives includes the `dlpi_adds`/`dlpi_subs` fields (which
//! tell it when the object list changed). The lock-free `dl_iterate_phdr` passes a size that excludes
//! those fields, so libgcc disables its cache and the callback becomes safe to run concurrently.
//!
//! Unless the `no_exception_intercept` cfg is set, `_Unwind_RaiseException` is also intercepted on
//! Linux, to count each unwind started on a reactor thread and to log its origin at trace level on
//! the `exception` logger.

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::atomic::{AtomicPtr, Ordering};

    #[cfg(not(no_exception_intercept))]
    use super::{increase_thrown_exceptions_counter, log_exception_trace};

    /// Program-header information of one loaded object, with the layout of glibc's
    /// `struct dl_phdr_info` on 64-bit targets.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct DlPhdrInfo {
        /// Base address of the object.
        pub dlpi_addr: u64,
        /// NUL-terminated path of the object.
        pub dlpi_name: *const c_char,
        /// Pointer to the object's array of program headers.
        pub dlpi_phdr: *const c_void,
        /// Number of program headers.
        pub dlpi_phnum: u16,
        /// Count of objects loaded so far in the process.
        pub dlpi_adds: u64,
        /// Count of objects unloaded so far in the process.
        pub dlpi_subs: u64,
        /// Thread-local storage module id of the object, or 0.
        pub dlpi_tls_modid: usize,
        /// The calling thread's TLS block of the object, or null.
        pub dlpi_tls_data: *mut c_void,
    }

    type Callback =
        unsafe extern "C" fn(info: *mut DlPhdrInfo, size: usize, data: *mut c_void) -> c_int;
    type DlIterateFn = unsafe extern "C" fn(callback: Callback, data: *mut c_void) -> c_int;

    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

    static ORG_DL_ITERATE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    fn dl_iterate_phdr_org() -> DlIterateFn {
        // The next `dl_iterate_phdr` (glibc's) is resolved lazily, on first use, with no static
        // initializer. This matters under AddressSanitizer: during its own initialization ASan calls
        // `dl_iterate_phdr(3)` to check that it is the first object in the list, and the dynamic
        // linker resolves that call to the override below, possibly before this object's
        // initializers have run. Anything that needed initialization would not be ready then, so
        // the lookup is done here, by hand, and cached in an atomic.
        let mut org = ORG_DL_ITERATE.load(Ordering::Relaxed);
        if org.is_null() {
            // SAFETY: RTLD_NEXT is a valid pseudo-handle and the name is a NUL-terminated literal.
            org = unsafe { dlsym(RTLD_NEXT, c"dl_iterate_phdr".as_ptr()) };
            ORG_DL_ITERATE.store(org, Ordering::Relaxed);
        }
        // SAFETY: the next definition of `dl_iterate_phdr` is glibc's, which always exists in a
        // dynamically linked process and has the signature of DlIterateFn.
        unsafe { std::mem::transmute::<*mut c_void, DlIterateFn>(org) }
    }

    // The cache must stay valid until very late in the life of the process (unwinding can happen
    // during exit), so it is allocated once and never freed.
    static PHDRS_CACHE: AtomicPtr<Vec<DlPhdrInfo>> = AtomicPtr::new(std::ptr::null_mut());

    unsafe extern "C" fn fill_cache(
        info: *mut DlPhdrInfo,
        _size: usize,
        _data: *mut c_void,
    ) -> c_int {
        // SAFETY: init_phdr_cache() creates the cache before it iterates with this callback, from a
        // single thread; `info` points to a valid entry for the duration of the callback.
        unsafe { (*PHDRS_CACHE.load(Ordering::Relaxed)).push(*info) };
        0
    }

    /// Fills the cache of loaded objects' program headers, once per process; later calls do
    /// nothing.
    ///
    /// Called when the reactors start. After it, the lock-free `dl_iterate_phdr` serves unwinding
    /// from the cache, which assumes no shared object is loaded or unloaded from then on.
    pub fn init_phdr_cache() {
        // A reactor already started in this process and filled the cache.
        if !PHDRS_CACHE.load(Ordering::Relaxed).is_null() {
            return;
        }
        PHDRS_CACHE.store(Box::into_raw(Box::new(Vec::new())), Ordering::Relaxed);
        // SAFETY: glibc's dl_iterate_phdr with a callback of the right signature that only pushes
        // the entry into the cache allocated just above.
        unsafe { dl_iterate_phdr_org()(fill_cache, std::ptr::null_mut()) };
    }

    /// Override of `dl_iterate_phdr(3)` that walks the cached program headers without a lock.
    ///
    /// Until the cache is filled and the engine is ready, it forwards to glibc's function. From then
    /// on it calls `callback` for each cached object with a copy of its entry and a size that
    /// excludes `dlpi_adds` and `dlpi_subs` (see the module documentation), stopping at the first
    /// non-zero return, which it returns.
    ///
    /// # Safety
    /// `callback` must be safe to call with a valid `dl_phdr_info` pointer, its size and `data`, as
    /// for `dl_iterate_phdr(3)`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn dl_iterate_phdr(callback: Callback, data: *mut c_void) -> c_int {
        let cache = PHDRS_CACHE.load(Ordering::Relaxed);
        if cache.is_null() || !crate::engine::core::reactor::engine_is_ready() {
            // The cache is not filled yet: forward to glibc's function.
            // SAFETY: the caller's contract is that of glibc's function, which is forwarded as is.
            return unsafe { dl_iterate_phdr_org()(callback, data) };
        }
        let mut r = 0;
        // SAFETY: once filled, the cache is never modified or freed.
        for h in unsafe { &*cache } {
            let mut h = *h;
            // Pass dl_phdr_info size that does not include dlpi_adds and dlpi_subs.
            // This forces libgcc to disable caching which is not thread safe and
            // requires dl_iterate_phdr to serialize calls to callback. Since we do
            // not serialize here we have to disable caching.
            // SAFETY: `h` is a valid local copy of the entry; the callback's contract is the
            // caller's.
            r = unsafe { callback(&mut h, std::mem::offset_of!(DlPhdrInfo, dlpi_adds), data) };
            if r != 0 {
                break;
            }
        }
        r
    }

    #[cfg(not(no_exception_intercept))]
    static ORG_RAISE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

    /// Override of libgcc's `_Unwind_RaiseException` that counts and logs every unwind started on a
    /// reactor thread, then forwards to libgcc's function.
    ///
    /// # Safety
    /// `h` must be a valid unwind exception object, as libgcc's function requires.
    #[cfg(not(no_exception_intercept))]
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn _Unwind_RaiseException(h: *mut c_void) -> c_int {
        type ThrowFn = unsafe extern "C" fn(*mut c_void) -> c_int;
        let mut org = ORG_RAISE.load(Ordering::Relaxed);
        if org.is_null() {
            // SAFETY: RTLD_NEXT is a valid pseudo-handle and the name is a NUL-terminated literal.
            org = unsafe { dlsym(RTLD_NEXT, c"_Unwind_RaiseException".as_ptr()) };
            ORG_RAISE.store(org, Ordering::Relaxed);
        }
        if crate::engine::core::reactor::engine_is_ready() {
            increase_thrown_exceptions_counter();
            log_exception_trace();
        }
        // SAFETY: the next definition is libgcc's `_Unwind_RaiseException`, linked into every
        // process that unwinds, with the signature of ThrowFn; `h` is the caller's.
        unsafe { std::mem::transmute::<*mut c_void, ThrowFn>(org)(h) }
    }
}

#[cfg(target_os = "linux")]
pub use linux::init_phdr_cache;

/// Hooks used by the reactor.
pub mod internal {
    pub use super::increase_thrown_exceptions_counter;
}

/// Adds one to the current reactor's count of unwinds (exported as a metric).
///
/// Must be called on a reactor thread.
pub fn increase_thrown_exceptions_counter() {
    // SAFETY: it is called only on threads whose reactor is set (callers check
    // `engine_is_ready()`), and no reference to the reactor is live across this single increment.
    unsafe { (*crate::engine::core::reactor::engine())._exceptions_thrown += 1 };
}

#[cfg(not(no_exception_intercept))]
pub(crate) static EXCEPTION_LOGGER: std::sync::LazyLock<std::pin::Pin<Box<crate::engine::util::log::Logger>>> =
    std::sync::LazyLock::new(|| {
        crate::engine::util::log::Logger::new("exception").expect("logger registered once")
    });

#[cfg(not(no_exception_intercept))]
thread_local! {
    static NESTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Logs the backtrace of the current unwind at trace level on the `exception` logger.
///
/// Guarded against recursion: an unwind raised while logging is not logged again.
#[cfg(not(no_exception_intercept))]
pub fn log_exception_trace() {
    use crate::engine::util::log::LogLevel;
    if !NESTED.with(|n| n.get()) && EXCEPTION_LOGGER.is_enabled(LogLevel::Trace) {
        NESTED.with(|n| n.set(true));
        crate::log_trace!(
            EXCEPTION_LOGGER,
            "Throw exception at:\n{}",
            crate::engine::util::backtrace::current_backtrace()
        );
        NESTED.with(|n| n.set(false));
    }
}

/// Does nothing: unwinds are not intercepted in this build.
#[cfg(no_exception_intercept)]
pub fn log_exception_trace() {}

/// Fills the cache of loaded ELF headers (no-op where `dl_iterate_phdr` is not used for
/// unwinding).
#[cfg(not(target_os = "linux"))]
pub fn init_phdr_cache() {}
