//! Engine threads that run blocking work on a shard (023 FR-018, FR-019): the commands of a
//! client connection are written to wait (for a core's answer, a majority, `maxTimeMS`), so each
//! runs in an engine thread, a stack of its own on the connection's shard that parks while it
//! waits ([`super::wait`]). The shard keeps a pool of them: a stack is taken for a command and
//! given back when it ends, so an idle connection holds none and a command allocates none once
//! the pool has as many as the shard runs at once.
//!
//! What runs here must not block the shard any other way: it waits through [`super::answer`] and
//! [`super::wait::sleep`].

use std::cell::{Cell, RefCell};
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::thread::{Thread, ThreadAttributes};

use super::post::Wake;

/// The stack of a command's engine thread unless the process sets another
/// ([`set_stack_size`]).
pub const DEFAULT_STACK: usize = 8 << 20;
/// The most idle stacks a shard keeps; past them a stack that ends its work is freed.
pub const MAX_IDLE: usize = 64;

static STACK: AtomicUsize = AtomicUsize::new(DEFAULT_STACK);

/// Sets the stack of the engine threads made from now on.
pub fn set_stack_size(bytes: usize) {
    STACK.store(bytes.max(64 << 10), Ordering::Relaxed);
}

/// The stack of the engine threads made from now on.
pub fn stack_size() -> usize {
    STACK.load(Ordering::Relaxed)
}

type Job = Box<dyn FnOnce()>;

/// The end of one job, which its caller awaits.
#[derive(Default)]
struct Done {
    wake: Wake,
    finished: Cell<bool>,
    panicked: Cell<bool>,
}

/// One engine thread of the pool.
#[derive(Default)]
struct Worker {
    job: RefCell<Option<(Job, Rc<Done>)>>,
    wake: Wake,
    quit: Cell<bool>,
}

#[derive(Default)]
struct Pool {
    idle: Vec<Rc<Worker>>,
    /// Engine threads of the shard, idle or running.
    threads: usize,
}

thread_local! {
    static POOL: RefCell<Pool> = RefCell::new(Pool::default());
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

/// The loop of a pool thread: runs each job it is given, then goes back to the pool or ends.
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
        let ok = catch_unwind(AssertUnwindSafe(job)).is_ok();
        done.panicked.set(!ok);
        done.finished.set(true);
        done.wake.wake();
        let kept = POOL.with(|p| {
            let mut p = p.borrow_mut();
            if w.quit.get() || p.idle.len() >= MAX_IDLE {
                p.threads -= 1;
                return false;
            }
            p.idle.push(Rc::clone(w));
            true
        });
        if !kept {
            return;
        }
    }
}

/// Starts a pool thread with its first job already in `w`.
fn spawn(w: Rc<Worker>) -> io::Result<()> {
    let attr = ThreadAttributes { sched_group: None, stack_size: stack_size() };
    let runs = Rc::clone(&w);
    let mut th = Thread::with_attributes(attr, move || {
        serve(&runs);
        Ok(())
    })
    .map_err(|e| io::Error::other(format!("an engine thread: {e}")))?;
    let joined = th.join();
    // The thread object lives until its thread ends.
    drop(make_coroutine(async move {
        let _ = joined.await;
        drop(th);
        Ok::<(), ExceptionPtr>(())
    }));
    Ok(())
}

/// Runs `job` in an engine thread of this shard; the future resolves when it ends, and fails
/// when it panicked or no engine thread could be made.
pub fn run(job: Box<dyn FnOnce()>) -> Future<()> {
    let done = Rc::new(Done::default());
    let idle = POOL.with(|p| p.borrow_mut().idle.pop());
    let started = match idle {
        Some(w) => {
            *w.job.borrow_mut() = Some((job, Rc::clone(&done)));
            w.wake.wake();
            Ok(())
        }
        None => {
            let w = Rc::new(Worker::default());
            *w.job.borrow_mut() = Some((job, Rc::clone(&done)));
            POOL.with(|p| p.borrow_mut().threads += 1);
            let r = spawn(w);
            if r.is_err() {
                POOL.with(|p| p.borrow_mut().threads -= 1);
            }
            r
        }
    };
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

/// Ends the idle engine threads of this shard (the shard stops serving clients); those running
/// end when their job does.
pub fn close_here() {
    let idle = POOL.with(|p| std::mem::take(&mut p.borrow_mut().idle));
    for w in idle {
        w.quit.set(true);
        POOL.with(|p| p.borrow_mut().threads -= 1);
        w.wake.wake();
    }
}
