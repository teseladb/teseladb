//! What would block a shard, done off it (023 FR-029): names resolved by the system resolver,
//! configuration and certificate files read, and any other blocking call.
//!
//! The engine's syscall pool is not used, for two reasons. It is private to the engine:
//! `Reactor::thread_pool` is `pub(crate)`, and the engine does not change (023, owner decision
//! of 16-11-2026). It is also one thread per shard that the engine's own file operations share:
//! `open`, `stat`, `rename` and the `fdatasync` fallback. A lookup that waits 2 s on an
//! unresponsive DNS server there would hold every file open of the shard for those 2 s.
//!
//! Instead:
//! - [`resolve`] hands the lookup to glibc's asynchronous resolver (`getaddrinfo_a`), which runs it
//!   on glibc's own threads. Its completion is posted back to the shard that asked through the
//!   engine's `alien` queue, which also wakes a sleeping shard. The deadline is an engine timer,
//!   and a lookup it overtakes is cancelled with `gai_cancel` when it has not started yet.
//! - [`read_file`] reads with the engine's file API (DMA reads on its I/O queues), which never
//!   blocks the shard.
//! - [`offload`] runs any other call that blocks (a `statvfs`, a file written and synced) on a
//!   few threads of the process that are not shards, and posts its result back to the shard that
//!   asked, as the resolver's completions are.
//!
//! Numeric addresses are answered at once, without the resolver.
// glibc's resolver is a C interface, and the engine's file reads take a raw I/O intent; every
// `unsafe` block states why it is sound.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::alien;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, Promise, make_exception_future, make_ready_future,
};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;

use super::within;

// glibc's interface (`<netdb.h>`, `<signal.h>`), for the x86-64 and aarch64 layouts.

/// `struct addrinfo`.
#[repr(C)]
struct AddrInfo {
    ai_flags: c_int,
    ai_family: c_int,
    ai_socktype: c_int,
    ai_protocol: c_int,
    ai_addrlen: u32,
    ai_addr: *mut c_void,
    ai_canonname: *mut c_char,
    ai_next: *mut AddrInfo,
}

/// `struct gaicb`.
#[repr(C)]
struct Gaicb {
    ar_name: *const c_char,
    ar_service: *const c_char,
    ar_request: *const AddrInfo,
    ar_result: *mut AddrInfo,
    return_: c_int,
    reserved: [c_int; 5],
}

/// `struct sigevent` with `SIGEV_THREAD`: 64 bytes.
#[repr(C)]
struct SigEvent {
    value: *mut c_void,
    signo: c_int,
    notify: c_int,
    function: Option<unsafe extern "C" fn(*mut c_void)>,
    attribute: *mut c_void,
    pad: [u8; 32],
}

const SIGEV_THREAD: c_int = 2;
const GAI_NOWAIT: c_int = 1;
const SOCK_STREAM: c_int = 1;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;
const EAI_CANCELED: c_int = -101;
const EAI_SYSTEM: c_int = -11;
const EAI_NONAME: c_int = -2;

// The asynchronous lookups are in libc since glibc 2.34 and in libanl before; from 2.34 libanl is
// an empty library kept for this link, so naming it builds against the release's glibc 2.28 too.
#[link(name = "anl")]
unsafe extern "C" {
    fn getaddrinfo_a(
        mode: c_int,
        list: *const *mut Gaicb,
        nitems: c_int,
        sevp: *mut SigEvent,
    ) -> c_int;
    fn gai_error(req: *mut Gaicb) -> c_int;
    fn gai_cancel(req: *mut Gaicb) -> c_int;
    fn freeaddrinfo(res: *mut AddrInfo);
    fn gai_strerror(code: c_int) -> *const c_char;
}

/// The shard's side of a lookup: whoever waits for it, and the request while glibc has it.
struct Waiter {
    promise: Option<Promise<Vec<SocketAddr>>>,
    /// The request, until its completion frees it or a cancellation does.
    request: Option<*mut Request>,
}

/// A lookup handed to glibc. Its address is the one glibc keeps: it stays boxed and still until
/// it is freed, on the shard that made it.
struct Request {
    cb: Gaicb,
    hints: AddrInfo,
    name: CString,
    port: u16,
    /// The shard that waits, and the instance whose queue reaches it.
    shard: u32,
    instance: *const alien::Instance,
    /// The shard's side, as `Rc::into_raw`: only the shard turns it back.
    waiter: *const RefCell<Waiter>,
}

/// A pointer handed from glibc's thread to the shard.
struct Posted(*mut Request);

// SAFETY: the request is only touched by the shard that made it; glibc's thread only reads its
// immutable `shard` and `instance` and passes the pointer on.
unsafe impl Send for Posted {}

impl Posted {
    fn get(&self) -> *mut Request {
        self.0
    }
}

/// glibc's notification, on one of its threads: posts the completion to the shard.
unsafe extern "C" fn on_resolved(value: *mut c_void) {
    let req = value.cast::<Request>();
    // SAFETY: `value` is the request given to getaddrinfo_a, alive until the shard frees it,
    // which happens only after this posting; `shard` and `instance` do not change after the
    // submission, and glibc's own lock orders the submission before this notification.
    let (shard, instance) = unsafe { ((*req).shard, &*(*req).instance) };
    let posted = Posted(req);
    alien::run_on(instance, shard, move || {
        // SAFETY: on the shard that made the request, which nothing freed yet (a request is
        // freed by its completion or by a cancellation that glibc confirmed, never both).
        unsafe { completed(posted.get()) }
    });
}

/// Frees `req` and its shard-side reference.
///
/// # Safety
/// `req` comes from `Box::into_raw` in [`submit`], on this shard, and is not used afterwards.
unsafe fn free(req: *mut Request) {
    // SAFETY: the caller's contract.
    let req = unsafe { Box::from_raw(req) };
    // SAFETY: made by `Rc::into_raw` in `submit`, turned back once, here.
    drop(unsafe { Rc::from_raw(req.waiter) });
}

/// The completion of `req` on its shard: its addresses go to whoever waits, if anyone still does.
///
/// # Safety
/// As [`free`].
unsafe fn completed(req: *mut Request) {
    // SAFETY: the caller's contract; glibc is done with the request.
    let mut req = unsafe { Box::from_raw(req) };
    // SAFETY: made by `Rc::into_raw` in `submit`, turned back once, here.
    let waiter = unsafe { Rc::from_raw(req.waiter) };
    // SAFETY: the request is complete: its error code and its result list are final.
    let result = unsafe {
        let code = gai_error(&raw mut req.cb);
        let result = if code == 0 { Ok(addresses(req.cb.ar_result, req.port)) } else { Err(code) };
        if !req.cb.ar_result.is_null() {
            freeaddrinfo(req.cb.ar_result);
        }
        result
    };
    let promise = {
        let mut w = waiter.borrow_mut();
        w.request = None;
        w.promise.take()
    };
    let Some(mut promise) = promise else { return };
    let name = req.name.to_string_lossy();
    match result {
        Ok(addrs) if !addrs.is_empty() => promise.set_value(addrs),
        Ok(_) => promise.set_exception(lookup_error(&name, EAI_NONAME)),
        Err(code) => promise.set_exception(lookup_error(&name, code)),
    }
}

/// The addresses of the list `ai`, with `port`.
///
/// # Safety
/// `ai` is null or a list from getaddrinfo.
unsafe fn addresses(mut ai: *const AddrInfo, port: u16) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    while !ai.is_null() {
        // SAFETY: the caller's contract: each node and its address are valid.
        let (addr, next) = unsafe { ((*ai).ai_addr.cast::<u8>().cast_const(), (*ai).ai_next) };
        // SAFETY: as above; the family says which `sockaddr` it is.
        if let Some(a) = unsafe { sockaddr(addr, port) }
            && !out.contains(&a)
        {
            out.push(a);
        }
        ai = next;
    }
    out
}

/// The address of the `sockaddr_in` or `sockaddr_in6` at `sa`, with `port`.
///
/// # Safety
/// `sa` is null or points to a `sockaddr` of its family's size.
unsafe fn sockaddr(sa: *const u8, port: u16) -> Option<SocketAddr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: the caller's contract; `sa_family` is the first field of every `sockaddr`, and
    // the addresses are at their fixed offsets (4 in `sockaddr_in`, 8 in `sockaddr_in6`).
    unsafe {
        match std::ptr::read_unaligned(sa.cast::<u16>()) {
            AF_INET => {
                let ip: [u8; 4] = std::ptr::read_unaligned(sa.add(4).cast());
                Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port)))
            }
            AF_INET6 => {
                let ip: [u8; 16] = std::ptr::read_unaligned(sa.add(8).cast());
                let scope = std::ptr::read_unaligned(sa.add(24).cast::<u32>());
                Some(SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(ip), port, 0, scope)))
            }
            _ => None,
        }
    }
}

fn lookup_error(name: &str, code: c_int) -> ExceptionPtr {
    let text = if code == EAI_SYSTEM {
        io::Error::last_os_error().to_string()
    } else {
        // SAFETY: gai_strerror returns a static NUL-terminated string for any code.
        unsafe { CStr::from_ptr(gai_strerror(code)) }.to_string_lossy().into_owned()
    };
    let kind = if code == EAI_NONAME { io::ErrorKind::NotFound } else { io::ErrorKind::Other };
    ExceptionPtr::from(io::Error::new(kind, format!("{name}: {text}")))
}

/// Hands the lookup of `name` to glibc; its addresses come to `waiter`'s promise.
fn submit(name: CString, port: u16, waiter: &Rc<RefCell<Waiter>>) -> Result<(), ExceptionPtr> {
    let instance = alien::internal::default_instance();
    if instance.is_null() {
        return Err(ExceptionPtr::from(io::Error::other("no shards run")));
    }
    let req = Box::into_raw(Box::new(Request {
        cb: Gaicb {
            ar_name: std::ptr::null(),
            ar_service: std::ptr::null(),
            ar_request: std::ptr::null(),
            ar_result: std::ptr::null_mut(),
            return_: 0,
            reserved: [0; 5],
        },
        hints: AddrInfo {
            ai_flags: 0,
            ai_family: 0,
            ai_socktype: SOCK_STREAM,
            ai_protocol: 0,
            ai_addrlen: 0,
            ai_addr: std::ptr::null_mut(),
            ai_canonname: std::ptr::null_mut(),
            ai_next: std::ptr::null_mut(),
        },
        name,
        port,
        shard: this_shard_id(),
        instance: instance.cast_const(),
        waiter: Rc::into_raw(Rc::clone(waiter)),
    }));
    // SAFETY: `req` was just boxed and is only reached through this pointer; the name and the
    // hints are fields of the box, which does not move until it is freed.
    let rc = unsafe {
        (*req).cb.ar_name = (*req).name.as_ptr();
        (*req).cb.ar_request = &raw const (*req).hints;
        let mut sev = SigEvent {
            value: req.cast(),
            signo: 0,
            notify: SIGEV_THREAD,
            function: Some(on_resolved),
            attribute: std::ptr::null_mut(),
            pad: [0; 32],
        };
        let list = [&raw mut (*req).cb];
        getaddrinfo_a(GAI_NOWAIT, list.as_ptr(), 1, &raw mut sev)
    };
    if rc != 0 {
        // SAFETY: glibc did not take the request.
        let name = unsafe { (*req).name.to_string_lossy().into_owned() };
        // SAFETY: as above; nothing else holds it.
        unsafe { free(req) };
        return Err(lookup_error(&name, rc));
    }
    waiter.borrow_mut().request = Some(req);
    Ok(())
}

/// After a deadline: nobody waits for the lookup of `waiter` any more; it is cancelled if glibc
/// has not started it, and otherwise freed by its completion.
fn abandon(waiter: &RefCell<Waiter>) {
    let mut w = waiter.borrow_mut();
    w.promise = None;
    let Some(req) = w.request else { return };
    // SAFETY: the request is alive (its completion clears `request` before freeing it).
    if unsafe { gai_cancel(&raw mut (*req).cb) } == EAI_CANCELED {
        w.request = None;
        drop(w);
        // SAFETY: glibc dropped it and will not notify; this is the only owner left.
        unsafe { free(req) };
    }
}

/// The addresses of `host` with `port`, from the system resolver (`/etc/hosts`, DNS), in its
/// order and without repeats; a numeric address is answered at once. Fails with
/// [`io::ErrorKind::NotFound`] when the name has no address, with [`io::ErrorKind::TimedOut`]
/// when the lookup takes more than `timeout`, and with the resolver's error otherwise.
pub fn resolve(host: &str, port: u16, timeout: Duration) -> Future<Vec<SocketAddr>> {
    let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return make_ready_future(vec![SocketAddr::new(ip, port)]);
    }
    let Ok(name) = CString::new(host) else {
        let e = io::Error::new(io::ErrorKind::InvalidInput, format!("{host:?}: not a host name"));
        return make_exception_future(ExceptionPtr::from(e));
    };
    let waiter = Rc::new(RefCell::new(Waiter { promise: Some(Promise::new()), request: None }));
    let fut = waiter.borrow_mut().promise.as_mut().map(Promise::get_future);
    let Some(fut) = fut else {
        return make_exception_future(ExceptionPtr::from(io::Error::other("lookup")));
    };
    if let Err(e) = submit(name, port, &waiter) {
        return make_exception_future(e);
    }
    within(timeout, fut, format!("{host}: name lookup timed out"), move || abandon(&waiter))
}

/// The bytes of the file `path`, read with the engine's file API (023 FR-029): configuration
/// files, certificates and keys. Errors name the file.
pub fn read_file(path: &str) -> Future<TemporaryBuffer<u8>> {
    let path = path.to_string();
    make_coroutine(async move {
        let named = |e: ExceptionPtr| {
            ExceptionPtr::from(io::Error::other(format!("{path}: {}", describe(&e))))
        };
        let mut file = open_file_dma(&path, OpenFlags::RO).await.map_err(named)?;
        let read = async {
            let size = usize::try_from(file.size().await?).unwrap_or(usize::MAX);
            // SAFETY: no I/O intent (a null pointer).
            unsafe { file.dma_read_bulk::<u8>(0, size, std::ptr::null_mut()) }.await
        }
        .await;
        let closed = file.close().await;
        let bytes = read.map_err(named)?;
        closed.map_err(named)?;
        Ok(bytes)
    })
}

/// Threads of the process that run [`offload`]'s work.
const OFFLOAD_THREADS: usize = 2;

type OffloadJob = Box<dyn FnOnce() + Send>;

/// The queue of [`offload`]'s threads, started by the first call.
static OFFLOAD: std::sync::OnceLock<io::Result<std::sync::mpsc::Sender<OffloadJob>>> =
    std::sync::OnceLock::new();

thread_local! {
    /// The promises of this shard's work on [`offload`]'s threads, by number.
    static OFFLOADED: RefCell<(u64, std::collections::HashMap<u64, Box<dyn std::any::Any>>)> =
        RefCell::new((0, std::collections::HashMap::new()));
}

/// Starts [`offload`]'s threads; they take jobs until the process ends.
fn offload_threads() -> io::Result<std::sync::mpsc::Sender<OffloadJob>> {
    let (tx, rx) = std::sync::mpsc::channel::<OffloadJob>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    for n in 0..OFFLOAD_THREADS {
        let rx = std::sync::Arc::clone(&rx);
        std::thread::Builder::new().name(format!("offload-{n}")).spawn(move || {
            loop {
                // Not a shard: these threads exist to wait.
                let job = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner).recv();
                match job {
                    Ok(job) => job(),
                    Err(_) => return,
                }
            }
        })?;
    }
    Ok(tx)
}

/// The end of offloaded work `id` on its shard: its result goes to whoever waits.
fn offload_done<T: 'static>(id: u64, result: std::thread::Result<T>) {
    let Some(slot) = OFFLOADED.with(|o| o.borrow_mut().1.remove(&id)) else { return };
    let Ok(mut promise) = slot.downcast::<Promise<T>>() else { return };
    match result {
        Ok(v) => promise.set_value(v),
        Err(panic) => promise.set_exception(ExceptionPtr::from_panic(panic)),
    }
}

/// Runs `f`, which blocks (a file system call, a file written and synced, a child process), on
/// one of the process's threads that are not shards, and resolves on the calling shard with its
/// result: the shard runs its other work meanwhile, as the engine's syscall thread does for its
/// own file operations. A panic of `f` fails the future. Runs on a shard.
pub fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Future<T> {
    let queue = match OFFLOAD.get_or_init(offload_threads) {
        Ok(q) => q.clone(),
        Err(e) => {
            let e = io::Error::new(e.kind(), format!("the offload threads did not start: {e}"));
            return make_exception_future(ExceptionPtr::from(e));
        }
    };
    let mut promise = Promise::<T>::new();
    let fut = promise.get_future();
    let id = OFFLOADED.with(|o| {
        let mut o = o.borrow_mut();
        o.0 += 1;
        let id = o.0;
        o.1.insert(id, Box::new(promise));
        id
    });
    let shard = this_shard_id();
    let job: OffloadJob = Box::new(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // A shard that is gone (the engine stopped) has nobody waiting.
        let _ = super::wait::post(shard, Box::new(move || offload_done(id, result)));
    });
    if queue.send(job).is_err() {
        OFFLOADED.with(|o| o.borrow_mut().1.remove(&id));
        return make_exception_future(ExceptionPtr::from(io::Error::other(
            "the offload threads are gone",
        )));
    }
    fut
}

/// The text of `e`: the I/O error's when it is one.
fn describe(e: &ExceptionPtr) -> String {
    match e.downcast_ref::<io::Error>() {
        Some(io) => io.to_string(),
        None => format!("{e:?}"),
    }
}
