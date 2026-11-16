//! Backtraces and task traces.
//!
//! [`current_backtrace`] captures the call stack of the running code with `backtrace(3)`
//! and, on a reactor thread, the chain of tasks waiting for it ([`Tasktrace`]), so that
//! a report shows how an asynchronous operation was reached. Frames are recorded as
//! (shared object, offset) pairs ([`Frame`]) so they can be symbolized offline; the
//! shared objects of the process are enumerated with `dl_iterate_phdr` before `main`, so
//! capturing a backtrace does not allocate for that and is usable from a crash handler.
//! Capacities are fixed (64 frames, 16 tasks) and stored inline.
//!
//! [`make_backtraced_exception_ptr`] attaches the current backtrace to an error message.

#[cfg(target_os = "linux")]
use std::ffi::{CStr, c_char};
use std::ffi::{c_int, c_void};
use std::fmt;
use std::sync::LazyLock;

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::core::shared_ptr::LwSharedPtr;
use crate::engine::core::sstring::Sstring;

/// A loaded ELF object (the executable or a shared library) and its address range.
pub struct SharedObject {
    /// The path of the object (empty for the executable and the unknown object).
    pub name: Sstring,
    /// The first address of the object's loaded segments.
    pub begin: usize,
    /// One past the last address of the object's loaded segments.
    pub end: usize,
}

/// One stack frame: the shared object it belongs to and the offset inside it.
///
/// `so` always points to one of the shared objects of the process (they live for its
/// whole life), hence the static reference.
#[derive(Clone, Copy)]
pub struct Frame {
    /// The object containing the address.
    pub so: &'static SharedObject,
    /// The address relative to `so.begin`.
    pub addr: usize,
}

impl Default for Frame {
    /// Address 0 in the unknown shared object, so that a frame always has one.
    fn default() -> Self {
        Frame { so: &UKNOWN_SHARED_OBJECT, addr: 0 }
    }
}

impl PartialEq for Frame {
    fn eq(&self, b: &Frame) -> bool {
        std::ptr::eq(self.so, b.so) && self.addr == b.addr
    }
}
impl Eq for Frame {}

unsafe extern "C" {
    fn backtrace(buffer: *mut *mut c_void, size: c_int) -> c_int;
}

/// Invokes `func` for each frame of the current call stack (at most 100), innermost first.
///
/// With `incremental == false` (the default mode) it calls `backtrace(3)` once and then
/// `func` for each frame; if `backtrace` crashes, `func` is never called. With
/// `incremental == true` it calls `backtrace` in a loop, capturing one more frame each
/// time and calling `func` with the new one. This is useful where `backtrace` itself may
/// crash (e.g. in a crash handler): `func` still sees every frame captured before the
/// crash. It is much slower.
pub fn backtrace_with<F: FnMut(Frame)>(mut func: F, incremental: bool) {
    const MAX_BACKTRACE: usize = 100;
    let mut buffer: [*mut c_void; MAX_BACKTRACE] = [std::ptr::null_mut(); MAX_BACKTRACE];
    if incremental {
        for last_frame in 1..=MAX_BACKTRACE {
            // SAFETY: buffer has MAX_BACKTRACE >= last_frame slots.
            let n = unsafe { backtrace(buffer.as_mut_ptr(), last_frame as c_int) };
            if n < last_frame as c_int {
                return;
            }
            let ip = buffer[last_frame - 1] as usize;
            func(decorate(ip - 1));
        }
    } else {
        // SAFETY: buffer has MAX_BACKTRACE slots.
        let n = unsafe { backtrace(buffer.as_mut_ptr(), MAX_BACKTRACE as c_int) };
        for &ip in &buffer[..n as usize] {
            func(decorate(ip as usize - 1));
        }
    }
}

/// A vector with fixed, inline capacity `N` (no heap allocation).
pub struct StaticVector<T, const N: usize> {
    data: [std::mem::MaybeUninit<T>; N],
    len: usize,
}

impl<T, const N: usize> StaticVector<T, N> {
    /// An empty vector.
    #[inline]
    pub const fn new() -> Self {
        StaticVector { data: [const { std::mem::MaybeUninit::uninit() }; N], len: 0 }
    }
    /// Number of stored elements.
    #[inline]
    pub fn size(&self) -> usize {
        self.len
    }
    /// The fixed capacity `N`.
    #[inline]
    pub fn capacity(&self) -> usize {
        N
    }
    /// The fixed capacity `N`.
    #[inline]
    pub fn max_size(&self) -> usize {
        N
    }
    /// Appends `x`.
    ///
    /// # Panics
    ///
    /// Aborts the process (via [`engine_assert!`](crate::engine_assert)) if the vector
    /// is full.
    #[inline]
    pub fn push_back(&mut self, x: T) {
        crate::engine_assert!(self.len < N);
        self.data[self.len].write(x);
        self.len += 1;
    }
    /// The stored elements.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first `len` slots are initialized.
        unsafe { std::slice::from_raw_parts(self.data.as_ptr() as *const T, self.len) }
    }
    /// Iterates over the stored elements.
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.as_slice().iter()
    }
}

impl<T, const N: usize> Default for StaticVector<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone, const N: usize> Clone for StaticVector<T, N> {
    fn clone(&self) -> Self {
        let mut r = Self::new();
        for x in self.iter() {
            r.push_back(x.clone());
        }
        r
    }
}

impl<T: PartialEq, const N: usize> PartialEq for StaticVector<T, N> {
    fn eq(&self, o: &Self) -> bool {
        self.as_slice() == o.as_slice()
    }
}

impl<T: Eq, const N: usize> Eq for StaticVector<T, N> {}

impl<T, const N: usize> Drop for StaticVector<T, N> {
    fn drop(&mut self) {
        // SAFETY: the first `len` slots are initialized and dropped once.
        unsafe {
            std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
                self.data.as_mut_ptr() as *mut T,
                self.len,
            ));
        }
    }
}

/// The call stack of a single thread (at most 64 frames), with a precomputed hash.
#[derive(Clone)]
pub struct SimpleBacktrace {
    _frames: StaticVector<Frame, 64>,
    _hash: usize,
    _delimeter: u8,
}

/// The frame storage of a [`SimpleBacktrace`].
pub type SimpleBacktraceVectorType = StaticVector<Frame, 64>;

impl SimpleBacktrace {
    fn calculate_hash(&self) -> usize {
        let mut h: usize = 0;
        for f in self._frames.iter() {
            let begin = f.so.begin;
            h = (h << 5).wrapping_sub(h) ^ (begin.wrapping_add(f.addr));
        }
        h
    }
    /// A backtrace of the frames `f`, printed separated by spaces.
    pub fn new(f: SimpleBacktraceVectorType) -> Self {
        let mut b = SimpleBacktrace { _frames: f, _hash: 0, _delimeter: b' ' };
        b._hash = b.calculate_hash();
        b
    }
    /// A backtrace of the frames `f`, printed separated by `delimeter`. Deprecated.
    #[deprecated]
    pub fn with_delimeter(f: SimpleBacktraceVectorType, delimeter: u8) -> Self {
        let mut b = SimpleBacktrace { _frames: f, _hash: 0, _delimeter: delimeter };
        b._hash = b.calculate_hash();
        b
    }
    /// An empty backtrace printed with `delimeter`. Deprecated.
    #[deprecated]
    pub fn empty_with_delimeter(delimeter: u8) -> Self {
        SimpleBacktrace { _frames: StaticVector::new(), _hash: 0, _delimeter: delimeter }
    }
    /// The hash of the frames.
    pub fn hash(&self) -> usize {
        self._hash
    }
    /// The separator printed between frames.
    pub fn delimeter(&self) -> u8 {
        self._delimeter
    }
}

impl Default for SimpleBacktrace {
    fn default() -> Self {
        SimpleBacktrace { _frames: StaticVector::new(), _hash: 0, _delimeter: b' ' }
    }
}

impl PartialEq for SimpleBacktrace {
    fn eq(&self, o: &SimpleBacktrace) -> bool {
        self._hash == o._hash && self._frames == o._frames
    }
}
impl Eq for SimpleBacktrace {}

impl std::hash::Hash for SimpleBacktrace {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_usize(self._hash);
    }
}

/// A reference-counted backtrace, as kept by tasks that record where they were created.
pub type SharedBacktrace = LwSharedPtr<SimpleBacktrace>;

/// A task in a [`Tasktrace`] that has no recorded backtrace: its type name and hash.
#[derive(Clone, Copy)]
pub struct TaskEntry {
    _task_type: &'static str,
    _task_hash: usize,
}

impl TaskEntry {
    /// An entry for a task of type `type_name` with hash `hash_code`.
    pub fn new(type_name: &'static str, hash_code: usize) -> Self {
        TaskEntry { _task_type: type_name, _task_hash: hash_code }
    }
    /// The hash of the task type.
    pub fn hash(&self) -> usize {
        self._task_hash
    }
}

impl PartialEq for TaskEntry {
    fn eq(&self, o: &TaskEntry) -> bool {
        self._task_type == o._task_type
    }
}

/// One waiting task in a [`Tasktrace`].
#[derive(Clone)]
pub enum TasktraceEntry {
    /// A task that recorded the backtrace of its creation.
    SharedBacktrace(SharedBacktrace),
    /// A task known only by its type.
    TaskEntry(TaskEntry),
}

impl PartialEq for TasktraceEntry {
    fn eq(&self, o: &TasktraceEntry) -> bool {
        match (self, o) {
            (TasktraceEntry::SharedBacktrace(a), TasktraceEntry::SharedBacktrace(b)) => a == b,
            (TasktraceEntry::TaskEntry(a), TasktraceEntry::TaskEntry(b)) => a == b,
            _ => false,
        }
    }
}

/// An extended backtrace: the backtrace of the currently running task plus the chain of
/// tasks (at most 16) waiting for the current operation to complete, and the scheduling
/// group it runs in.
#[derive(Clone, Default)]
pub struct Tasktrace {
    _main: SimpleBacktrace,
    _prev: StaticVector<TasktraceEntry, 16>,
    _sg: SchedulingGroup,
    _hash: usize,
}

/// The waiting-task storage of a [`Tasktrace`].
pub type TasktraceVectorType = StaticVector<TasktraceEntry, 16>;

impl Tasktrace {
    /// Builds a task trace; `prev_hash` is the combined hash of `prev`.
    pub fn new(
        main: SimpleBacktrace,
        prev: TasktraceVectorType,
        prev_hash: usize,
        sg: SchedulingGroup,
    ) -> Self {
        let h = main.hash().wrapping_mul(31) ^ prev_hash;
        Tasktrace { _main: main, _prev: prev, _sg: sg, _hash: h }
    }
    /// The hash of the whole trace.
    pub fn hash(&self) -> usize {
        self._hash
    }
    /// The separator printed between frames.
    pub fn delimeter(&self) -> u8 {
        self._main.delimeter()
    }
}

impl PartialEq for Tasktrace {
    fn eq(&self, o: &Tasktrace) -> bool {
        self._hash == o._hash && self._main == o._main && self._prev == o._prev
    }
}

impl std::hash::Hash for Tasktrace {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_usize(self._hash);
    }
}

/// The backtrace type returned by [`current_backtrace`].
pub type SavedBacktrace = Tasktrace;

/// Internal types; not part of the stable API.
pub mod internal {
    use super::*;

    /// An error whose message has a backtrace appended to it.
    #[derive(Debug, Clone)]
    pub struct Backtraced<Exc> {
        /// The wrapped error.
        pub exc: Exc,
        // Atomically counted, as the error may cross shards.
        _backtrace: std::sync::Arc<Sstring>,
    }

    // SAFETY: the backtrace sstring is written once, in new(), before the Arc is shared, and
    // only read afterwards (an error may be handed to another shard);
    // its heap block is freed once, by whichever thread drops the last Arc.
    unsafe impl<Exc: Send> Send for Backtraced<Exc> {}
    // SAFETY: as above; shared access only reads the sstring.
    unsafe impl<Exc: Sync> Sync for Backtraced<Exc> {}

    impl<Exc: std::error::Error> Backtraced<Exc> {
        /// Wraps `exc`, capturing the current backtrace into the message.
        pub fn new(exc: Exc) -> Self {
            let bt = Sstring::from(format!("{} Backtrace: {}", exc, current_backtrace()));
            Backtraced { exc, _backtrace: std::sync::Arc::new(bt) }
        }

        /// Returns the message of the wrapped error followed by the backtrace.
        pub fn what(&self) -> &str {
            self._backtrace.as_str()
        }
    }

    impl<Exc: std::error::Error> fmt::Display for Backtraced<Exc> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.what())
        }
    }

    impl<Exc: std::error::Error> std::error::Error for Backtraced<Exc> {}

    /// `class backtraced : public Exc`: caught as `Exc` too.
    impl<Exc: std::error::Error + Send + Sync + 'static>
        crate::engine::core::future::DerivedException for Backtraced<Exc>
    {
        type Base = Exc;
        #[inline]
        fn base(&self) -> &Exc {
            &self.exc
        }
    }
}

/// Wraps `exc` in an [`ExceptionPtr`] whose message has the current backtrace appended.
///
/// The receiving side can still match the error as `Exc`.
pub fn make_backtraced_exception_ptr<Exc: std::error::Error + Send + Sync + 'static>(
    exc: Exc,
) -> ExceptionPtr {
    ExceptionPtr::new_derived(internal::Backtraced::new(exc))
}

/// Returns `Err` with `exc` and the current backtrace attached to its message (see
/// [`make_backtraced_exception_ptr`]).
///
/// # Errors
///
/// Always returns an error.
pub fn throw_with_backtrace<T, Exc: std::error::Error + Send + Sync + 'static>(
    exc: Exc,
) -> Result<T, ExceptionPtr> {
    Err(make_backtraced_exception_ptr(exc))
}

// The ELF program headers that dl_iterate_phdr walks exist on Linux only.
#[cfg(target_os = "linux")]
const PT_LOAD: u32 = 1;

#[cfg(target_os = "linux")]
#[repr(C)]
struct ElfPhdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct DlPhdrInfo {
    dlpi_addr: usize,
    dlpi_name: *const c_char,
    dlpi_phdr: *const ElfPhdr,
    dlpi_phnum: u16,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn dl_iterate_phdr(
        callback: unsafe extern "C" fn(
            info: *mut DlPhdrInfo,
            size: usize,
            data: *mut c_void,
        ) -> c_int,
        data: *mut c_void,
    ) -> c_int;
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn dl_iterate_phdr_callback(
    info: *mut DlPhdrInfo,
    _size: usize,
    data: *mut c_void,
) -> c_int {
    // SAFETY: called by dl_iterate_phdr with a valid info and our data pointer.
    unsafe {
        let info = &*info;
        let mut total_size: usize = 0;
        for i in 0..info.dlpi_phnum as usize {
            let hdr = &*info.dlpi_phdr.add(i);
            // Only account loadable segments
            if hdr.p_type == PT_LOAD {
                total_size += hdr.p_memsz as usize;
            }
        }
        let name = if info.dlpi_name.is_null() {
            String::new()
        } else {
            CStr::from_ptr(info.dlpi_name).to_string_lossy().into_owned()
        };
        (*(data as *mut Vec<SharedObject>)).push(SharedObject {
            name: Sstring::from(name),
            begin: info.dlpi_addr,
            end: info.dlpi_addr + total_size,
        });
    }
    0
}

fn enumerate_shared_objects() -> Vec<SharedObject> {
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut shared_objects: Vec<SharedObject> = Vec::new();
    #[cfg(target_os = "linux")]
    // SAFETY: the callback only pushes into the vector passed as data, which outlives the
    // call and is reached through nothing else meanwhile.
    unsafe {
        dl_iterate_phdr(dl_iterate_phdr_callback, (&raw mut shared_objects).cast::<c_void>());
    }
    shared_objects
}

/// The shared objects of the process, enumerated once (before `main` on Linux).
static SHARED_OBJECTS: LazyLock<Vec<SharedObject>> = LazyLock::new(enumerate_shared_objects);
/// The object frames are attributed to when the shared objects are unknown: it spans
/// the whole address space.
static UKNOWN_SHARED_OBJECT: SharedObject =
    SharedObject { name: Sstring::new(), begin: 0, end: usize::MAX };

/// Enumerates the shared objects before `main`, from the ELF `.init_array`, so that a
/// backtrace taken in a signal handler (the reactor prints one on `SIGSEGV`/`SIGABRT`)
/// never enumerates them: `dl_iterate_phdr` takes the loader lock and the vector
/// allocates, neither safe there.
#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array")]
static INIT_SHARED_OBJECTS: extern "C" fn() = {
    extern "C" fn init_shared_objects() {
        LazyLock::force(&SHARED_OBJECTS);
    }
    init_shared_objects
};

/// Turns an absolute address into a [`Frame`]: the shared object containing it and the
/// offset inside it. Addresses outside every object are attributed to the executable.
pub fn decorate(addr: usize) -> Frame {
    let shared_objects: &'static Vec<SharedObject> = &SHARED_OBJECTS;
    // If the shared-objects are not enumerated yet, or the enumeration
    // failed return the addr as-is with a dummy shared-object.
    if shared_objects.is_empty() {
        return Frame { so: &UKNOWN_SHARED_OBJECT, addr };
    }
    let it = shared_objects.iter().find(|so| addr >= so.begin && addr < so.end);
    // Unidentified addresses are assumed to originate from the executable.
    let so = match it {
        None => &shared_objects[0],
        Some(so) => so,
    };
    Frame { so, addr: addr - so.begin }
}

/// Captures the backtrace of the currently executing task only (no waiting tasks).
pub fn current_backtrace_tasklocal() -> SimpleBacktrace {
    let mut v = SimpleBacktraceVectorType::new();
    backtrace_with(
        |f| {
            if v.size() < v.capacity() {
                v.push_back(f);
            }
        },
        false,
    );
    SimpleBacktrace::new(v)
}

/// A hash identifying a task type: the hash of its Rust type name, without allocating.
#[inline]
fn type_hash_code(name: &'static str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    name.hash(&mut h);
    h.finish() as usize
}

/// Captures the backtrace of the current task and, on a reactor thread, the chain of
/// tasks waiting for it.
pub fn current_tasktrace() -> Tasktrace {
    let main = current_backtrace_tasklocal();

    let mut prev = TasktraceVectorType::new();
    let mut hash: usize = 0;
    if !crate::engine::core::reactor::local_engine().is_null()
        && !crate::engine::core::thread_impl::g_current_context().is_null()
    {
        let mut tsk: Option<crate::engine::core::task::TaskPtr>;

        let thread = crate::engine::core::thread_impl::get();
        if !thread.is_null() {
            // SAFETY: a non-null thread_impl::get() is the running engine thread, alive
            // while it runs this code.
            tsk = unsafe {
                crate::engine::core::task::Task::waiting_task(std::ptr::NonNull::new_unchecked(
                    thread,
                ))
            };
        } else {
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            tsk = unsafe { (*crate::engine::core::reactor::engine()).current_task() };
        }

        while let Some(t) = tsk {
            if prev.size() >= prev.max_size() {
                break;
            }
            // SAFETY: tasks in a waiting chain are alive while the current task runs,
            // which is the invariant of the waiting-task chain.
            let bt: SharedBacktrace = unsafe { t.get_backtrace() };
            hash = hash.wrapping_mul(31);
            if bt.is_some() {
                hash ^= bt.hash();
                prev.push_back(TasktraceEntry::SharedBacktrace(bt));
            } else {
                // SAFETY: as above.
                let ti = unsafe { t.type_name() };
                let hc = type_hash_code(ti);
                prev.push_back(TasktraceEntry::TaskEntry(TaskEntry::new(ti, hc)));
                hash ^= hc;
            }
            // SAFETY: as above.
            tsk = unsafe { t.waiting_task() };
        }
    }

    Tasktrace::new(main, prev, hash, current_scheduling_group())
}

/// Captures the current backtrace (see [`current_tasktrace`]).
pub fn current_backtrace() -> SavedBacktrace {
    current_tasktrace()
}

impl fmt::Display for Frame {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let so = self.so;
        if !so.name.empty() {
            write!(out, "{}+", so.name)?;
        }
        write!(out, "0x{:x}", self.addr)
    }
}

impl fmt::Display for SimpleBacktrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for fr in self._frames.iter() {
            if !first {
                f.write_str(" ")?;
            }
            first = false;
            write!(f, "{}", fr)?;
        }
        Ok(())
    }
}

impl fmt::Display for Tasktrace {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}", self._main)?;
        for e in self._prev.iter() {
            write!(out, "\n   --------")?;
            match e {
                TasktraceEntry::SharedBacktrace(sb) => write!(out, "\n{}", **sb)?,
                TasktraceEntry::TaskEntry(f) => write!(out, "\n   {}", f)?,
            }
        }
        Ok(())
    }
}

impl fmt::Display for TaskEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self._task_type)
    }
}
