//! Engine threads that run blocking work on a shard (023 FR-018, FR-019): the commands of a
//! client connection are written to wait (for a core's answer, a majority, `maxTimeMS`), so each
//! runs in an engine thread of the connection's shard that parks while it waits
//! ([`super::wait`]). The shard keeps a pool of them: a thread is taken for a command and given
//! back when it ends, so an idle connection holds none and a command allocates none once the pool
//! has as many as the shard runs at once.
//!
//! An engine thread's own stack is the engine's default, small and taken from the shard's memory
//! (as the reference engine's `thread_attributes::stack_size`); the command runs on a stack the
//! thread keeps besides it, of [`stack_size`] bytes, outside the shard's memory: address space the
//! kernel commits a page at a time as the command reaches it, with a guard below
//! ([`teseladb_sys::deep_stack::OwnStack`]). So what a running command takes from the shard does
//! not grow with the depth its stack may reach, and the threads of a shard take at most
//! 1/[`STACKS_SHARE`] of its memory: past that, a job waits for a thread that ends its work
//! instead of failing (and so does one whose thread cannot be made while others run).
//!
//! What runs here must not block the shard any other way: it waits through [`super::answer`] and
//! [`super::wait::sleep`].

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::memory;
use teseladb::engine::core::thread::{Thread, ThreadAttributes};
use teseladb_sys::deep_stack::OwnStack;

use super::post::Wake;

/// The stack a job runs on unless the process sets another ([`set_stack_size`]).
pub const DEFAULT_STACK: usize = 8 << 20;
/// The engine thread's own stack, from the shard's memory: the reference engine's default.
pub const ENGINE_STACK: usize = 128 << 10;
/// What an engine thread takes from the shard's memory besides its stack (its context, the
/// pool's records), rounded up.
const THREAD_OVERHEAD: usize = 4 << 10;
/// The engine threads of a shard take at most this part of its memory (1/8).
pub const STACKS_SHARE: usize = 8;
/// The most idle engine threads a shard keeps; past them one that ends its work ends.
pub const MAX_IDLE: usize = 64;

static STACK: AtomicUsize = AtomicUsize::new(DEFAULT_STACK);

/// Sets the stack the jobs of the engine threads made from now on run on.
pub fn set_stack_size(bytes: usize) {
    STACK.store(bytes.max(64 << 10), Ordering::Relaxed);
}

/// The stack the jobs of the engine threads made from now on run on.
pub fn stack_size() -> usize {
    STACK.load(Ordering::Relaxed)
}

/// Runs `f` on a stack of its own of [`stack_size`] bytes with a guard below
/// ([`teseladb_sys::deep_stack::run`]), and gives back what it returns; from such a stack it runs
/// where it is. Work an engine thread does beyond a few frames goes through it: the engine
/// thread's own stack is [`ENGINE_STACK`] of the shard's memory with no guard page in a release
/// build, so a call that went past it would write over the heap blocks beside it instead of
/// faulting. A call may park its engine thread meanwhile.
pub fn deep<R>(f: impl FnOnce() -> R) -> R {
    let mut f = Some(f);
    let mut out = None;
    teseladb_sys::deep_stack::run(stack_size(), &mut |_| out = f.take().map(|f| f()));
    out.expect("the deep stack runs its call once")
}

type Job = Box<dyn FnOnce()>;

/// The end of one job, which its caller awaits.
#[derive(Default)]
struct Done {
    wake: Wake,
    finished: Cell<bool>,
    panicked: Cell<bool>,
}

/// One engine thread of the pool, with the stack its jobs run on.
struct Worker {
    job: RefCell<Option<(Job, Rc<Done>)>>,
    wake: Wake,
    quit: Cell<bool>,
    stack: OwnStack,
}

#[derive(Default)]
struct Pool {
    idle: Vec<Rc<Worker>>,
    /// Engine threads of the shard, idle or running.
    threads: usize,
    /// Jobs waiting for an engine thread, in arrival order.
    waiting: VecDeque<(Job, Rc<Done>)>,
}

thread_local! {
    static POOL: RefCell<Pool> = RefCell::new(Pool::default());
}

/// The most engine threads this shard runs at once: those whose stacks fit in
/// 1/[`STACKS_SHARE`] of its memory (no bound where the engine's memory is not counted).
pub fn most_threads_here() -> usize {
    let total = memory::stats().total_memory();
    if total == 0 {
        return usize::MAX;
    }
    (total / STACKS_SHARE / (ENGINE_STACK + THREAD_OVERHEAD)).max(1)
}

/// Parks the running engine thread until `w` is woken.
fn park_on(w: &Rc<Worker>) {
    let w = Rc::clone(w);
    let _ = make_coroutine(async move {
        w.wake.wait().await;
        Ok::<(), ExceptionPtr>(())
    })
    .get();
}

/// Runs `job` on the worker's stack and tells its caller it ended.
fn run_job(w: &Worker, job: Job, done: &Done) {
    let mut job = Some(job);
    let ok = catch_unwind(AssertUnwindSafe(|| {
        w.stack.run(&mut || {
            if let Some(job) = job.take() {
                job();
            }
        });
    }))
    .is_ok();
    done.panicked.set(!ok);
    done.finished.set(true);
    done.wake.wake();
}

/// What a pool thread does once a job ended.
enum Next {
    /// Runs a job that waited for a thread.
    Run((Job, Rc<Done>)),
    /// Waits in the pool for the next job.
    Idle,
    /// Ends.
    End,
}

/// The loop of a pool thread: runs each job it is given, then a job that waits, goes back to the
/// pool or ends.
fn serve(w: &Rc<Worker>) {
    loop {
        let job = w.job.borrow_mut().take();
        let Some((job, done)) = job else {
            if w.quit.get() {
                return;
            }
            park_on(w);
            continue;
        };
        run_job(w, job, &done);
        drop(done);
        let next = POOL.with(|p| {
            let mut p = p.borrow_mut();
            if let Some(waiting) = p.waiting.pop_front() {
                return Next::Run(waiting);
            }
            if w.quit.get() || p.idle.len() >= MAX_IDLE {
                p.threads -= 1;
                return Next::End;
            }
            p.idle.push(Rc::clone(w));
            Next::Idle
        });
        match next {
            Next::Run(waiting) => *w.job.borrow_mut() = Some(waiting),
            Next::Idle => {}
            Next::End => return,
        }
    }
}

/// Starts a pool thread with its first job; on failure the job comes back with the error.
fn spawn(job: Job, done: Rc<Done>) -> Result<(), (io::Error, Job, Rc<Done>)> {
    let Some(stack) = OwnStack::new(stack_size()) else {
        let e = io::Error::new(io::ErrorKind::OutOfMemory, "no address space for a job's stack");
        return Err((e, job, done));
    };
    let w = Rc::new(Worker {
        job: RefCell::new(Some((job, done))),
        wake: Wake::default(),
        quit: Cell::new(false),
        stack,
    });
    let attr = ThreadAttributes { sched_group: None, stack_size: ENGINE_STACK };
    let runs = Rc::clone(&w);
    let made = Thread::with_attributes(attr, move || {
        serve(&runs);
        Ok(())
    });
    let mut th = match made {
        Ok(th) => th,
        Err(e) => {
            let e = io::Error::new(io::ErrorKind::OutOfMemory, format!("an engine thread: {e}"));
            return match w.job.borrow_mut().take() {
                Some((job, done)) => Err((e, job, done)),
                // The thread took its job: it runs (not reached, the thread exists then).
                None => Ok(()),
            };
        }
    };
    let joined = th.join();
    // The thread object lives until its thread ends.
    drop(make_coroutine(async move {
        let _ = joined.await;
        drop(th);
        Ok::<(), ExceptionPtr>(())
    }));
    Ok(())
}

/// Hands `job` to an idle engine thread, a new one, or the queue of those that wait for one.
/// Fails only when no engine thread of the shard runs and none can be made.
fn start(job: Job, done: Rc<Done>) -> io::Result<()> {
    if let Some(w) = POOL.with(|p| p.borrow_mut().idle.pop()) {
        *w.job.borrow_mut() = Some((job, done));
        w.wake.wake();
        return Ok(());
    }
    let full = POOL.with(|p| {
        let mut p = p.borrow_mut();
        if p.threads >= most_threads_here() {
            return true;
        }
        p.threads += 1;
        false
    });
    if full {
        POOL.with(|p| p.borrow_mut().waiting.push_back((job, done)));
        return Ok(());
    }
    let Err((e, job, done)) = spawn(job, done) else { return Ok(()) };
    POOL.with(|p| {
        let mut p = p.borrow_mut();
        p.threads -= 1;
        if p.threads == 0 {
            return Err(e);
        }
        // A thread that runs takes it when its job ends.
        p.waiting.push_back((job, done));
        Ok(())
    })
}

/// Runs `job` in an engine thread of this shard; the future resolves when it ends, and fails
/// when it panicked or no engine thread could run it (none runs on the shard and none could be
/// made). A job that finds the shard's threads at their bound waits for one.
pub fn run(job: Box<dyn FnOnce()>) -> Future<()> {
    let done = Rc::new(Done::default());
    let started = start(job, Rc::clone(&done));
    make_coroutine(async move {
        started.map_err(ExceptionPtr::from)?;
        while !done.finished.get() {
            done.wake.wait().await;
        }
        if done.panicked.get() {
            return Err(ExceptionPtr::from(io::Error::other("a command panicked")));
        }
        Ok(())
    })
}

/// The engine threads of this shard, idle or running.
pub fn threads_here() -> usize {
    POOL.with(|p| p.borrow().threads)
}

/// The jobs of this shard waiting for an engine thread.
pub fn waiting_here() -> usize {
    POOL.with(|p| p.borrow().waiting.len())
}

/// Ends the idle engine threads of this shard (the shard stops serving clients); those running
/// end when their job and those waiting do.
pub fn close_here() {
    let idle = POOL.with(|p| std::mem::take(&mut p.borrow_mut().idle));
    for w in idle {
        w.quit.set(true);
        POOL.with(|p| p.borrow_mut().threads -= 1);
        w.wake.wake();
    }
}
