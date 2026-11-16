//! Tests of `IoQueue` with a fake sink: requests are dispatched and completed, large
//! requests are split into parts (also with flawed parts), intent references move and
//! cancel, cancelled requests resolve at once, and the read/write split respects the
//! configured limits.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/minstd.rs"]
mod minstd;

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Read;
use std::pin::{Pin, pin};
use std::time::{Duration, Instant};

use minstd::{DefaultRandomEngine, UniformIntDistribution};
use teseladb::engine::core::file::CancelledError;
use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::internal::io_intent::IntentReference;
use teseladb::engine::core::internal::io_request::{IoDirectionAndLength, IoRequest, Part};
use teseladb::engine::core::internal::io_sink::IoSink;
use teseladb::engine::core::io_intent::IoIntent;
use teseladb::engine::core::io_queue::internal::PriorityClass;
use teseladb::engine::core::io_queue::{IoGroup, IoGroupPtr, IoQueue, IoQueueConfig, IovecKeeper};
use teseladb::engine::core::reactor::{IoCompletion, create_scheduling_group, engine_logger};
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::scheduling::current_scheduling_group;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::Timer;
use teseladb::engine::core::when_all::when_all_succeed_iter_void;
use teseladb::engine::testing::random::testing::local_random;
use teseladb::engine::util::internal::iovec_utils::iovec_len;
use teseladb::engine::util::log::LogLevel;
use teseladb::engine::util::shared_token_bucket::internal::{SteadyClock, TokenBucketClock};
use teseladb::engine_test_case;
use teseladb::{log_debug, log_info, log_trace};

/// The storage behind the fake sink: an int per address.
#[derive(Default)]
struct FakeFile {
    data: HashMap<u64, i32>,
}

impl FakeFile {
    fn make_write_req(idx: usize, buf: *mut i32) -> IoRequest {
        IoRequest::make_write(0, idx as u64, buf as *const c_void, 1, false)
    }

    fn make_writev_req(
        idx: usize,
        buf: *mut i32,
        nr: usize,
        buf_len: usize,
        vecs: &mut Vec<Iovec>,
    ) -> IoRequest {
        vecs.reserve(nr);
        for i in 0..nr {
            // SAFETY: `buf` points to an array of at least `nr` ints.
            vecs.push(Iovec { iov_base: unsafe { buf.add(i) } as *mut c_void, iov_len: buf_len });
        }
        IoRequest::make_writev(0, idx as u64, vecs, false)
    }

    fn execute_write_req(&mut self, rq: &IoRequest, desc: *mut IoCompletion) {
        let op = rq.as_write();
        // SAFETY: the address of a write request made by `make_write_req` is a live int.
        self.data.insert(op.pos, unsafe { *(op.addr as *const i32) });
        // SAFETY: `desc` is the live completion of the request the sink handed out.
        unsafe { IoCompletion::complete_with(desc, op.size as isize) };
    }

    fn execute_writev_req(&mut self, rq: &IoRequest, desc: *mut IoCompletion) {
        let mut len: usize = 0;
        let op = rq.as_writev();

        for i in 0..op.iov_len {
            // SAFETY: the iovecs of the request are live and point to live ints.
            let iov = unsafe { &*op.iovec.add(i) };
            // SAFETY: each iovec base of the request points at a live, aligned i32.
            self.data.insert(op.pos + i as u64, unsafe { *(iov.iov_base as *const i32) });
            len += iov.iov_len;
        }

        // SAFETY: as in execute_write_req().
        unsafe { IoCompletion::complete_with(desc, len as isize) };
    }
}

/// An I/O queue over a test sink, kicked by a periodic timer. The queue points to the sink and the timer to the whole
/// structure, so the sink lives in a box and the structure is pinned before the timer is
/// armed by [`IoQueueForTests::start`]. The fields are declared so that they drop in
/// the right order (the timer first, the queue before the sink it points to).
/// queue before the sink it points to).
///
/// The queue is built in place and reached through its root pointer, as the reactor keeps
/// in a shared block; `Drop` destroys it before the sink.
struct IoQueueForTests {
    kicker: Timer,
    queue: *mut IoQueue,
    sink: Box<IoSink>,
    group: IoGroupPtr,
}

impl Drop for IoQueueForTests {
    fn drop(&mut self) {
        // SAFETY: the kicker is a field of the structure, which is pinned once started and
        // is being dropped in place.
        unsafe { Pin::new_unchecked(&mut self.kicker) }.cancel();
        // SAFETY: `queue` is the root pointer of the queue built in `new`, in a block of its
        // own that nothing else frees; no reference to it is live.
        unsafe {
            std::ptr::drop_in_place(self.queue);
            drop(Box::from_raw(self.queue as *mut std::mem::MaybeUninit<IoQueue>));
        }
    }
}

impl IoQueueForTests {
    fn new() -> Self {
        let group: IoGroupPtr = IoGroupPtr::new(
            IoGroup::new(IoQueueConfig { id: 0, ..IoQueueConfig::default() }, 1).expect("io_group"),
        );
        let mut sink = Box::new(IoSink::new());
        let queue =
            Box::into_raw(Box::new(std::mem::MaybeUninit::<IoQueue>::uninit())) as *mut IoQueue;
        // SAFETY: `queue` is a fresh, aligned, uninitialized block that stays put until `Drop`
        // destroys the queue in it, and is used only through this root pointer; the sink
        // lives in its box, which the structure drops after the queue.
        unsafe { IoQueue::construct_in_place(queue, group.clone(), &mut *sink) }.expect("io_queue");
        IoQueueForTests { kicker: Timer::new(), queue, sink, group }
    }

    /// Arms the kicker: every 500 microseconds it polls the queue. Called once the
    /// structure is pinned, since the timer callback points to it.
    fn start(self: Pin<&mut Self>) {
        // SAFETY: the structure is pinned; the timer is cancelled by its drop, before the
        // rest of the structure goes away.
        let me = unsafe { self.get_unchecked_mut() };
        let this: *const IoQueueForTests = me;
        // SAFETY: the kicker is a field of the pinned structure.
        let mut kicker = unsafe { Pin::new_unchecked(&mut me.kicker) };
        // SAFETY: `this` outlives the timer that calls it.
        kicker.as_mut().set_callback(move || unsafe { (*this).kick() });
        kicker.arm_periodic(Duration::from_micros(500));
    }

    fn kick(&self) {
        for fg in self.group.fair_groups() {
            fg.replenish_capacity(<SteadyClock as TokenBucketClock>::now());
        }
    }

    fn queue_request(
        &mut self,
        pc: PriorityClass,
        dnl: IoDirectionAndLength,
        req: IoRequest,
        intent: *mut IoIntent,
        iovs: IovecKeeper,
    ) -> Future<usize> {
        // SAFETY: `queue` is the root pointer of the queue built in the shared block that
        // this structure owns; no reference to the queue is live across the call.
        unsafe { IoQueue::queue_request(self.queue, pc, dnl, req, intent, iovs) }
    }
}

fn get_default_pc() -> PriorityClass {
    PriorityClass::new(&current_scheduling_group())
}

fn tio_mut<'a>(tio: &'a mut Pin<&mut IoQueueForTests>) -> &'a mut IoQueueForTests {
    // SAFETY: only the fields are used; the structure itself is not moved.
    unsafe { tio.as_mut().get_unchecked_mut() }
}

engine_test_case!(test_basic_flow, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut tio = pin!(IoQueueForTests::new());
        tio.as_mut().start();
        let mut file = FakeFile::default();
        let filep: *mut FakeFile = &mut file;

        let mut val = Box::new(42i32);
        let f = tio_mut(&mut tio)
            .queue_request(
                get_default_pc(),
                IoDirectionAndLength::new(IoDirectionAndLength::WRITE_IDX, 0),
                FakeFile::make_write_req(0, &mut *val),
                std::ptr::null_mut(),
                IovecKeeper::new(),
            )
            .then(move |_len: usize| {
                // SAFETY: `file` outlives `f`, waited for below.
                assert!(unsafe { (*filep).data.get(&0).copied().unwrap_or_default() } == 42);
            });

        sleep(Duration::from_millis(500)).get()?;
        let t = tio_mut(&mut tio);
        // SAFETY: the queue's root pointer, with no reference to the queue live.
        unsafe { IoQueue::poll_io_queue(t.queue) };
        t.sink.drain(|rq: &IoRequest, desc: *mut IoCompletion| -> bool {
            // SAFETY: `file` is alive; nothing else borrows it while the sink drains.
            unsafe { (*filep).execute_write_req(rq, desc) };
            true
        });

        f.get()?;
        drop(val);
        Ok(())
    })
});

/// How the parts of a large request complete: all fully (`None`), one partially (`Partial`) or one with an error (`Error`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PartFlaw {
    None,
    Partial,
    Error,
}

fn do_test_large_request_flow(
    flaw: PartFlaw,
) -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
    let mut tio = pin!(IoQueueForTests::new());
    tio.as_mut().start();
    let mut file = FakeFile::default();
    let filep: *mut FakeFile = &mut file;
    let mut values: [i32; 3] = [13, 42, 73];
    let valuesp: *const [i32; 3] = &values;

    // SAFETY: the queue is live and nothing else reaches it during this call.
    let limits = unsafe { &*tio_mut(&mut tio).queue }.get_request_limits();
    let max_write = limits.max_write;

    let mut vecs: Vec<Iovec> = Vec::new();
    let req = FakeFile::make_writev_req(0, values.as_mut_ptr(), 3, max_write, &mut vecs);
    let f = tio_mut(&mut tio)
        .queue_request(
            get_default_pc(),
            IoDirectionAndLength::new(IoDirectionAndLength::WRITE_IDX, max_write * 3),
            req,
            std::ptr::null_mut(),
            vecs,
        )
        .then(move |len: usize| {
            // SAFETY: `file` and `values` outlive `f`, waited for below.
            let (file, values) = unsafe { (&mut *filep, &*valuesp) };
            let data = |pos: usize| file.data.get(&(pos as u64)).copied().unwrap_or_default();
            let mut expected = max_write;
            assert_eq!(data(0), values[0]); // data(0 * max_write)
            if flaw == PartFlaw::None {
                assert_eq!(data(max_write), values[1]);
                assert_eq!(data(2 * max_write), values[2]);
                expected += 2 * max_write;
            }
            if flaw == PartFlaw::Partial {
                assert_eq!(data(max_write), values[1]);
                expected += max_write / 2;
            }
            assert_eq!(len, expected);
        });

    for i in 0..3 {
        sleep(Duration::from_millis(500)).get()?;
        let t = tio_mut(&mut tio);
        // SAFETY: the queue's root pointer, with no reference to the queue live.
        unsafe { IoQueue::poll_io_queue(t.queue) };
        t.sink.drain(|rq: &IoRequest, desc: *mut IoCompletion| -> bool {
            if i == 1 {
                if flaw == PartFlaw::Partial {
                    let op = rq.as_writev();
                    // SAFETY: the request's iovecs are live; the sink writes through
                    // them as a read-only view.
                    unsafe { (*op.iovec).iov_len /= 2 };
                }
                if flaw == PartFlaw::Error {
                    // SAFETY: `desc` is the live completion of the request.
                    unsafe { IoCompletion::complete_with(desc, -(libc_eio() as isize)) };
                    return true;
                }
            }
            // SAFETY: `file` is alive; nothing else borrows it while the sink drains.
            unsafe { (*filep).execute_writev_req(rq, desc) };
            true
        });
    }

    f.get()?;
    Ok(())
}

/// The `EIO` error number.
const fn libc_eio() -> i32 {
    5
}

engine_test_case!(test_large_request_flow, {
    thread_async(move || do_test_large_request_flow(PartFlaw::None))
});

engine_test_case!(test_large_request_flow_partial, {
    thread_async(move || do_test_large_request_flow(PartFlaw::Partial))
});

engine_test_case!(test_large_request_flow_error, {
    thread_async(move || do_test_large_request_flow(PartFlaw::Error))
});

/// Makes `x` a new unlinked reference, then moves `o` into it.
fn move_construct(x: Pin<&mut IntentReference>, o: Pin<&mut IntentReference>) {
    x.take_from(o);
}

engine_test_case!(test_intent_safe_ref, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let get_cancelled = |iref: &IntentReference| -> bool {
            match iref.retrieve() {
                Ok(_) => false,
                Err(err) if err.is::<CancelledError>() => true,
                Err(err) => panic!("unexpected exception {}", err),
            }
        };
        let retrieve =
            |iref: &IntentReference| -> *mut IoIntent { iref.retrieve().expect("not cancelled") };

        let mut intent = pin!(IoIntent::new());
        let mut intent_x = pin!(IoIntent::new());
        let intentp = intent.as_mut().as_ptr();
        let intent_xp = intent_x.as_mut().as_ptr();

        let mut ref_orig = pin!(IntentReference::new(intentp));
        ref_orig.as_mut().bind();
        assert!(retrieve(&ref_orig) == intentp);

        // Test move armed
        let mut ref_armed = pin!(IntentReference::new(std::ptr::null_mut()));
        move_construct(ref_armed.as_mut(), ref_orig.as_mut());
        assert!(retrieve(&ref_orig).is_null());
        assert!(retrieve(&ref_armed) == intentp);

        let mut ref_armed_2 = pin!(IntentReference::new(intent_xp));
        ref_armed_2.as_mut().bind();
        ref_armed_2.as_mut().take_from(ref_armed.as_mut());
        assert!(retrieve(&ref_armed).is_null());
        assert!(retrieve(&ref_armed_2) == intentp);

        intent.as_mut().cancel();
        assert!(get_cancelled(&ref_armed_2));

        // Test move cancelled
        let mut ref_cancelled = pin!(IntentReference::new(std::ptr::null_mut()));
        move_construct(ref_cancelled.as_mut(), ref_armed_2.as_mut());
        assert!(retrieve(&ref_armed_2).is_null());
        assert!(get_cancelled(&ref_cancelled));

        let mut ref_cancelled_2 = pin!(IntentReference::new(intent_xp));
        ref_cancelled_2.as_mut().bind();
        ref_cancelled_2.as_mut().take_from(ref_cancelled.as_mut());
        assert!(retrieve(&ref_cancelled).is_null());
        assert!(get_cancelled(&ref_cancelled_2));

        // Test move empty
        let mut ref_empty = pin!(IntentReference::new(std::ptr::null_mut()));
        move_construct(ref_empty.as_mut(), ref_orig.as_mut());
        assert!(retrieve(&ref_empty).is_null());

        let mut ref_empty_2 = pin!(IntentReference::new(intent_xp));
        ref_empty_2.as_mut().bind();
        ref_empty_2.as_mut().take_from(ref_empty.as_mut());
        assert!(retrieve(&ref_empty_2).is_null());
        Ok(())
    })
});

const NR_REQUESTS: i32 = 24;

/// A nondeterministic random seed.
fn random_device() -> u32 {
    let mut b = [0u8; 4];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)) {
        Ok(()) => u32::from_ne_bytes(b),
        Err(e) => panic!("random_device: {}", e),
    }
}

engine_test_case!(test_io_cancellation, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut file = FakeFile::default();
        let filep: *mut FakeFile = &mut file;

        let mut tio = pin!(IoQueueForTests::new());
        tio.as_mut().start();
        let pc0 = PriorityClass::new(
            &create_scheduling_group(Sstring::from("a"), Sstring::new(), 100.0).get()?,
        );
        let pc1 = PriorityClass::new(
            &create_scheduling_group(Sstring::from("b"), Sstring::new(), 100.0).get()?,
        );

        let mut idx: usize = 0;
        let mut val: i32 = 100;

        let mut live = pin!(IoIntent::new());
        let mut dead = pin!(IoIntent::new());
        let livep = live.as_mut().as_ptr();
        let deadp = dead.as_mut().as_ptr();

        let mut finished: Vec<Future<()>> = Vec::new();
        let mut cancelled: Vec<Future<()>> = Vec::new();

        let queue_legacy_request =
            |q: &mut IoQueueForTests,
             pc: PriorityClass,
             idx: &mut usize,
             val: &mut i32,
             finished: &mut Vec<Future<()>>| {
                let mut buf = Box::new(*val);
                let (i, v) = (*idx, *val);
                let req = FakeFile::make_write_req(i, &mut *buf);
                let f = q
                    .queue_request(
                        pc,
                        IoDirectionAndLength::new(IoDirectionAndLength::WRITE_IDX, 0),
                        req,
                        std::ptr::null_mut(),
                        IovecKeeper::new(),
                    )
                    .then(move |_len: usize| {
                        let _buf = buf;
                        assert!(
                            // SAFETY: `file` outlives the futures, waited for at the end of the test.
                            unsafe { (*filep).data.get(&(i as u64)).copied().unwrap_or_default() }
                                == v
                        );
                        make_ready_future(())
                    });
                finished.push(f);
                *idx += 1;
                *val += 1;
            };

        let queue_live_request = |q: &mut IoQueueForTests,
                                  pc: PriorityClass,
                                  idx: &mut usize,
                                  val: &mut i32,
                                  finished: &mut Vec<Future<()>>| {
            let mut buf = Box::new(*val);
            let (i, v) = (*idx, *val);
            let req = FakeFile::make_write_req(i, &mut *buf);
            let f = q
                .queue_request(
                    pc,
                    IoDirectionAndLength::new(IoDirectionAndLength::WRITE_IDX, 0),
                    req,
                    livep,
                    IovecKeeper::new(),
                )
                .then(move |_len: usize| {
                    let _buf = buf;
                    assert!(
                        // SAFETY: as in queue_legacy_request.
                        unsafe { (*filep).data.get(&(i as u64)).copied().unwrap_or_default() } == v
                    );
                    make_ready_future(())
                });
            finished.push(f);
            *idx += 1;
            *val += 1;
        };

        let queue_dead_request =
            |q: &mut IoQueueForTests,
             pc: PriorityClass,
             idx: &mut usize,
             val: &mut i32,
             cancelled: &mut Vec<Future<()>>| {
                let mut buf = Box::new(*val);
                let i = *idx;
                let req = FakeFile::make_write_req(i, &mut *buf);
                let f = q
                    .queue_request(
                        pc,
                        IoDirectionAndLength::new(IoDirectionAndLength::WRITE_IDX, 0),
                        req,
                        deadp,
                        IovecKeeper::new(),
                    )
                    .then_wrapped(move |f: Future<usize>| {
                        let _buf = buf;
                        if f.get().is_ok() {
                            panic!("must not be reached")
                        }
                        make_ready_future(())
                    })
                    .then(move |()| {
                        assert!(
                            // SAFETY: as in queue_legacy_request.
                            unsafe { (*filep).data.get(&(i as u64)).copied().unwrap_or_default() }
                                == 0
                        );
                    });
                cancelled.push(f);
                *idx += 1;
                *val += 1;
            };

        let seed = random_device();
        let mut reng = DefaultRandomEngine::new(u64::from(seed));
        let dice = UniformIntDistribution::new(0, 5);

        for _ in 0..NR_REQUESTS {
            let pc = dice.sample(&mut reng) % 2;
            let t = tio_mut(&mut tio);
            if dice.sample(&mut reng) < 3 {
                println!("queue live req to pc {}", pc);
                queue_live_request(
                    t,
                    if pc == 0 { pc0 } else { pc1 },
                    &mut idx,
                    &mut val,
                    &mut finished,
                );
            } else if dice.sample(&mut reng) < 5 {
                println!("queue dead req to pc {}", pc);
                queue_dead_request(
                    t,
                    if pc == 0 { pc0 } else { pc1 },
                    &mut idx,
                    &mut val,
                    &mut cancelled,
                );
            } else {
                println!("queue legacy req to pc {}", pc);
                queue_legacy_request(
                    t,
                    if pc == 0 { pc0 } else { pc1 },
                    &mut idx,
                    &mut val,
                    &mut finished,
                );
            }
        }

        dead.as_mut().cancel();

        // cancelled requests must resolve right at once

        when_all_succeed_iter_void(cancelled).get()?;

        sleep(Duration::from_millis(500)).get()?;
        let t = tio_mut(&mut tio);
        // SAFETY: the queue's root pointer, with no reference to the queue live.
        unsafe { IoQueue::poll_io_queue(t.queue) };
        t.sink.drain(|rq: &IoRequest, desc: *mut IoCompletion| -> bool {
            // SAFETY: `file` is alive; nothing else borrows it while the sink drains.
            unsafe { (*filep).execute_write_req(rq, desc) };
            true
        });

        when_all_succeed_iter_void(finished).get()?;
        let _ = &mut live;
        Ok(())
    })
});

engine_test_case!(test_request_buffer_split, {
    let ensure =
        |parts: &[Part], req: &IoRequest, idx: usize, pos: u64, size: usize, mem: usize| {
            assert!(parts[idx].req.opcode() == req.opcode());
            let op = req.as_read();
            let sub_op = parts[idx].req.as_read();
            assert_eq!(sub_op.fd, op.fd);
            assert_eq!(sub_op.pos, pos);
            assert_eq!(sub_op.size, size);
            assert_eq!(sub_op.addr, mem as *mut u8);
            assert_eq!(sub_op.nowait_works, op.nowait_works);
            assert_eq!(parts[idx].iovecs.len(), 0);
            assert_eq!(parts[idx].size, sub_op.size);
        };

    // No split
    {
        let req = IoRequest::make_read(5, 13, 0x420 as *mut c_void, 17, true);
        let parts = req.split(21);
        assert_eq!(parts.len(), 1);
        ensure(&parts, &req, 0, 13, 17, 0x420);
    }

    // Without tail
    {
        let req = IoRequest::make_read(7, 24, 0x4321 as *mut c_void, 24, true);
        let parts = req.split(12);
        assert_eq!(parts.len(), 2);
        ensure(&parts, &req, 0, 24, 12, 0x4321);
        ensure(&parts, &req, 1, 24 + 12, 12, 0x4321 + 12);
    }

    // With tail
    {
        let req = IoRequest::make_read(9, 42, 0x1234 as *mut c_void, 33, true);
        let parts = req.split(13);
        assert_eq!(parts.len(), 3);
        ensure(&parts, &req, 0, 42, 13, 0x1234);
        ensure(&parts, &req, 1, 42 + 13, 13, 0x1234 + 13);
        ensure(&parts, &req, 2, 42 + 26, 7, 0x1234 + 26);
    }

    make_ready_future(())
});

fn show_request(req: &IoRequest, buf_off: *const u8, pfx: &str) {
    if !engine_logger().is_enabled(LogLevel::Trace) {
        return;
    }

    let op = req.as_readv();
    log_trace!(engine_logger(), "{}{} iovecs on req:", pfx, op.iov_len);
    for i in 0..op.iov_len {
        // SAFETY: the iovecs of the request are live.
        let iov = unsafe { &*op.iovec.add(i) };
        log_trace!(
            engine_logger(),
            "{}  base={} len={}",
            pfx,
            iov.iov_base as usize - buf_off as usize,
            iov.iov_len
        );
    }
}

fn show_request_parts(parts: &[Part], buf_off: *const u8) {
    if !engine_logger().is_enabled(LogLevel::Trace) {
        return;
    }

    log_trace!(engine_logger(), "{} parts", parts.len());
    for p in parts {
        log_trace!(engine_logger(), "  size={} iovecs={}", p.size, p.iovecs.len());
        log_trace!(engine_logger(), "  {} iovecs on part:", p.iovecs.len());
        for iov in &p.iovecs {
            log_trace!(
                engine_logger(),
                "    base={} len={}",
                iov.iov_base as usize - buf_off as usize,
                iov.iov_len
            );
        }
        show_request(&p.req, buf_off, "  ");
    }
}

engine_test_case!(test_request_iovec_split, {
    let mut large_buffer = [0u8; 1025];
    let large: *mut u8 = large_buffer.as_mut_ptr();
    const LARGE_LEN: usize = 1025;

    let clear_buffer = || {
        // SAFETY: `large` is the live 1025-byte buffer.
        unsafe { std::ptr::write_bytes(large, 0, LARGE_LEN) };
    };

    let bump_buffer = |vecs: &[Iovec]| {
        for v in vecs {
            for i in 0..v.iov_len {
                // SAFETY: every iovec points into the live buffer.
                unsafe {
                    let p = (v.iov_base as *mut u8).add(i);
                    *p = (*p).wrapping_add(1);
                }
            }
        }
    };

    let check_buffer = |len: usize, value: u8| {
        assert!(len < LARGE_LEN);
        let mut fill_match = true;
        let mut train_match = true;
        for i in 0..LARGE_LEN {
            // SAFETY: `i` is within the live buffer.
            let b = unsafe { *large.add(i) };
            if i < len {
                if b != value {
                    fill_match = false;
                }
            } else if b != b'\0' {
                train_match = false;
            }
        }
        assert!(fill_match);
        assert!(train_match);
    };

    let ensure = |parts: &[Part], req: &IoRequest, idx: usize, pos: u64| {
        assert!(parts[idx].req.opcode() == req.opcode());
        // The request split here is a readv.
        let op = req.as_readv();
        let sub_op = parts[idx].req.as_readv();
        assert_eq!(sub_op.fd, op.fd);
        assert_eq!(sub_op.pos, pos);
        assert_eq!(sub_op.iov_len, parts[idx].iovecs.len());
        assert_eq!(sub_op.nowait_works, op.nowait_works);
        assert_eq!(parts[idx].size, iovec_len(&parts[idx].iovecs));

        for iov in 0..parts[idx].iovecs.len() {
            // SAFETY: the sub-request points to `iov_len` live iovecs.
            let sub = unsafe { &*sub_op.iovec.add(iov) };
            assert_eq!(sub.iov_base, parts[idx].iovecs[iov].iov_base);
            assert_eq!(sub.iov_len, parts[idx].iovecs[iov].iov_len);
        }
    };

    // Draw from the local random engine.
    let dice = UniformIntDistribution::new(1, 31);
    let roll = || dice.sample_fn(&mut || u64::from(local_random())) as usize;
    let stop = Instant::now() + Duration::from_secs(4);
    let mut iter: u64 = 0;
    let mut no_splits: u32 = 0;
    let mut no_tails: u32 = 0;

    loop {
        log_debug!(engine_logger(), "===== iter {} =====", iter);
        iter += 1;
        let mut vecs: Vec<Iovec> = Vec::new();
        let nr_vecs = roll() % 13 + 1;
        log_debug!(engine_logger(), "Generate {} iovecs", nr_vecs);
        let mut total: usize = 0;
        for _ in 0..nr_vecs {
            let iov =
                // SAFETY: `total` stays below 13 * 31 bytes, inside the buffer.
                Iovec { iov_base: unsafe { large.add(total) } as *mut c_void, iov_len: roll() };
            assert!(iov.iov_len != 0);
            total += iov.iov_len;
            vecs.push(iov);
        }

        assert!(total > 0);
        clear_buffer();
        bump_buffer(&vecs);
        check_buffer(total, 1);

        let file_off = roll();
        let req = IoRequest::make_readv(5, file_off as u64, &mut vecs, true);

        show_request(&req, large, "");

        let max_len = roll() * 3;
        let nr_parts = total.div_ceil(max_len);
        log_debug!(engine_logger(), "Split {} into {}-bytes ({} parts)", total, max_len, nr_parts);
        let parts = req.split(max_len);
        show_request_parts(&parts, large);
        assert_eq!(parts.len(), nr_parts);

        let mut parts_total: usize = 0;
        for p in 0..nr_parts {
            ensure(&parts, &req, p, (file_off + parts_total) as u64);
            if p < nr_parts - 1 {
                assert_eq!(parts[p].size, max_len);
            }
            parts_total += parts[p].size;
            bump_buffer(&parts[p].iovecs);
        }
        assert_eq!(parts_total, total);
        check_buffer(total, 2);

        if parts.len() == 1 {
            no_splits += 1;
        }
        if parts.last().map(|p| p.size) == Some(max_len) {
            no_tails += 1;
        }

        if !(Instant::now() < stop || iter < 32 || no_splits < 16 || no_tails < 16) {
            break;
        }
    }

    log_info!(engine_logger(), "{} iters ({} no-splits, {} no-tails)", iter, no_splits, no_tails);
    let _ = &mut large_buffer;

    make_ready_future(())
});
