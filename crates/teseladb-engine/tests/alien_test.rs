//! Calls into the engine from a foreign (non-engine) thread: `alien::run_on` and
//! `alien::submit_to`, with an eventfd that tells the foreign thread when the engine is up
//! and the engine when the foreign thread is done. A program with its own `main`
//! (`harness = false`).

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;

use teseladb::engine::core::alien;
use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, LogicError, RuntimeError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::internal::pollable_fd::PollableFd;
use teseladb::engine::core::posix::FileDesc;
use teseladb::engine::core::reactor::Reactor;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::util::later::now;

const ENGINE_READY: u64 = 24;
const ALIEN_DONE: u64 = 42;

unsafe extern "C" {
    fn eventfd(initval: u32, flags: i32) -> i32;
    fn eventfd_read(fd: i32, value: *mut u64) -> i32;
    fn eventfd_write(fd: i32, value: u64) -> i32;
}

pub(crate) fn main() {
    // we need a protocol that both engine and alien understand.
    // and on which, a engine future can wait.
    // SAFETY: plain syscall.
    let engine_ready_fd = unsafe { eventfd(0, 0) };
    let alien_done = FileDesc::eventfd(0, 0).expect("file_desc::eventfd");
    let mut app = AppTemplate::new(Default::default());
    let alien_instance: *const alien::Instance = app.alien();
    let alien_instance = alien_instance as usize;

    // use the raw fd, because the engine wants to take over the fds, if it
    // polls on them.
    let alien_done_fd = alien_done.get();
    let zim = std::thread::spawn(move || -> Result<(char, i32), String> {
        // SAFETY: the app (and its alien instance) outlives this thread, joined below.
        let instance = unsafe { &*(alien_instance as *const alien::Instance) };
        let mut result: u64 = 0;
        // wait until the engine is ready
        // SAFETY: `result` is a valid eventfd_t.
        let r = unsafe { eventfd_read(engine_ready_fd, &mut result) };
        if r < 0 {
            return Err("failed to wait for engine engine".into());
        }
        if result != ENGINE_READY {
            return Err("engine failed to sent us the ready message".into());
        }
        // test for alien::run_on()
        let (question, answer) = mpsc::channel::<char>();
        alien::run_on(instance, 0, move || {
            let _ = question.send('*');
        });
        // test for alien::submit_to(), which returns a value
        let mut counts = Vec::new();
        for i in 0..Smp::count() {
            // send messages from alien.
            counts.push(alien::submit_to(instance, i, move || make_ready_future::<i32>(i as i32)));
        }
        // test for alien::submit_to(), which returns nothing
        // `.wait()`: waits for the result without looking at it.
        let _ = alien::submit_to(instance, 0, || make_ready_future(())).recv();
        let mut total = 0;
        for count in counts {
            total += count.recv().map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        }
        // i am done. dismiss the engine
        // SAFETY: plain syscall.
        unsafe { eventfd_write(alien_done_fd, ALIEN_DONE) };
        Ok((answer.recv().map_err(|e| e.to_string())?, total))
    });

    let result = Rc::new(Cell::new(0u64));
    let args: Vec<String> = std::env::args().collect();
    let mut alien_done = Some(alien_done);
    app.run(args, move || {
        let result = result.clone();
        let result2 = result.clone();
        let alien_done = alien_done.take().expect("run once");
        now()
            .then(move |()| {
                // engine ready!
                // SAFETY: plain syscall.
                unsafe { eventfd_write(engine_ready_fd, ENGINE_READY) };
                now()
            })
            .then(move |()| {
                do_with(
                    PollableFd::from_file_desc(alien_done),
                    move |alien_done_fds: &mut PollableFd| {
                        let fds: *mut PollableFd = alien_done_fds;
                        // check if alien has dismissed me.
                        alien_done_fds.readable().then(move |()| {
                            let mut v: u64 = 0;
                            // SAFETY: do_with keeps the fd alive until this continuation resolves.
                            let ret = unsafe {
                                (*fds).get_file_desc().read(
                                    &mut v as *mut u64 as *mut std::ffi::c_void,
                                    std::mem::size_of::<u64>(),
                                )
                            };
                            result.set(v);
                            make_ready_future::<usize>(ret.expect("read").expect("ready"))
                        })
                    },
                )
            })
            .then(move |n: usize| {
                if n != std::mem::size_of::<u64>() {
                    return make_exception_future(ExceptionPtr::new(RuntimeError(
                        "read from eventfd failed".into(),
                    )));
                }
                if result2.get() != ALIEN_DONE {
                    return make_exception_future(ExceptionPtr::new(LogicError(
                        "alien failed to dismiss me".into(),
                    )));
                }
                now()
            })
            .handle_exception(|ep: ExceptionPtr| {
                eprintln!("Error: {}", ep);
            })
            .finally(|| {
                Reactor::exit(0);
            })
    });

    let (everything, total) = match zim.join() {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => panic!("{}", e),
        Err(_) => panic!("alien thread panicked"),
    };
    let expected = '*';
    if everything != '*' {
        eprintln!("Bad everything: {} != {}", everything, expected);
        std::process::exit(1);
    }
    let expected: i32 = (0..Smp::count() as i32).sum();
    if total != expected {
        eprintln!("Bad total: {} != {}", total, expected);
        std::process::exit(1);
    }
}
