//! Event descriptors (the original's `writeable_eventfd` and `readable_eventfd`): a write side
//! made from a read side and a read side made from a write side reach the same counter; a wait
//! resolves with the sum of the signals given since the last read, and waits while there are
//! none.
#![cfg(target_os = "linux")]

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::internal::pollable_fd::{ReadableEventfd, WriteableEventfd};
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::thread::async_;
use teseladb::engine_test_case;

engine_test_case!(signals_add_up_until_a_wait_reads_them, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut r = ReadableEventfd::new(2)?;
        let w = r.write_side()?;
        w.signal(3);
        w.signal(4);
        assert_eq!(r.wait().get()?, 9, "the initial count plus both signals");
        let pending = r.wait();
        yield_().get()?;
        assert!(!pending.available(), "nothing signalled since the read");
        w.signal(1);
        assert_eq!(pending.get()?, 1);
        Ok(())
    })
});

engine_test_case!(a_read_side_made_from_a_write_side_sees_its_signals, {
    async_(|| -> Result<(), ExceptionPtr> {
        let w = WriteableEventfd::new(0)?;
        let mut r = w.read_side()?;
        w.signal(5);
        assert_eq!(r.wait().get()?, 5);
        let mut r2 = r.write_side()?.read_side()?;
        w.signal(2);
        assert_eq!(r2.wait().get()?, 2, "every copy reaches the same counter");
        Ok(())
    })
});
