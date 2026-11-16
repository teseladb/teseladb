//! Pipes (the original's pipe_test semantics): an unread item comes back first and only one
//! may wait; a writer blocked on a full pipe fails with "Broken pipe" when the reader goes
//! away, and later writes fail at once; a reader drains what a closed writer left and then
//! sees end of file. Fair-queue tickets (the original's fair_queue_ticket): truth value,
//! non-zero test, normalisation, arithmetic, clamped wrapping difference and printing.
//! Temporary-buffer views: begin/end, prefix, at and indexing.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use teseladb::engine::core::fair_queue::{FairQueueTicket, wrapping_difference};
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::pipe::{BrokenPipeException, Pipe, UnreadOverflowException};
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

engine_test_case!(an_unread_item_comes_back_first_and_only_one_may_wait, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut p = Pipe::<i32>::new(2);
        // SAFETY: every future is resolved with get() while the pipe is alive.
        unsafe { p.writer.write(1) }.get()?;
        // SAFETY: as above.
        let first = unsafe { p.reader.read() }.get()?;
        assert_eq!(first, Some(1));
        p.reader.unread(1)?;
        let e = p.reader.unread(2).err().expect("a second unread overflows");
        assert!(e.downcast_ref::<UnreadOverflowException>().is_some());
        assert!(
            e.to_string().ends_with("(pipe_reader::unread() overflow)"),
            "{e}"
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { p.reader.read() }.get()?,
            Some(1),
            "the unread item comes back"
        );
        // SAFETY: as above.
        unsafe { p.writer.write(3) }.get()?;
        // SAFETY: as above.
        assert_eq!(unsafe { p.reader.read() }.get()?, Some(3));
        Ok(())
    })
});

engine_test_case!(a_blocked_writer_fails_when_the_reader_goes_away, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let Pipe { reader, mut writer } = Pipe::<i32>::new(1);
        // SAFETY: the futures are resolved while the writer (and so the buffer) is alive.
        unsafe { writer.write(1) }.get()?;
        // SAFETY: as above.
        let blocked = unsafe { writer.write(2) };
        assert!(!blocked.available(), "the pipe is full");
        drop(reader);
        let e = blocked.get().err().expect("broken pipe");
        assert!(e.downcast_ref::<BrokenPipeException>().is_some());
        assert!(e.to_string().ends_with("(Broken pipe)"), "{e}");
        // SAFETY: as above.
        let e = unsafe { writer.write(3) }
            .get()
            .err()
            .expect("the reader is gone");
        assert!(e.downcast_ref::<BrokenPipeException>().is_some());
        Ok(())
    })
});

engine_test_case!(a_reader_drains_a_closed_writer_then_sees_the_end, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let Pipe {
            mut reader,
            mut writer,
        } = Pipe::<String>::new(3);
        // SAFETY: the futures are resolved while the reader (and so the buffer) is alive.
        unsafe { writer.write("a".into()) }.get()?;
        // SAFETY: as above.
        unsafe { writer.write("b".into()) }.get()?;
        drop(writer);
        // SAFETY: as above.
        assert_eq!(unsafe { reader.read() }.get()?.as_deref(), Some("a"));
        // SAFETY: as above.
        assert_eq!(unsafe { reader.read() }.get()?.as_deref(), Some("b"));
        for _ in 0..2 {
            // SAFETY: as above.
            assert_eq!(
                unsafe { reader.read() }.get()?,
                None,
                "end of file, every time"
            );
        }
        Ok(())
    })
});

engine_test_case!(a_reader_waiting_on_an_empty_pipe_wakes_at_end_of_file, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let Pipe { mut reader, writer } = Pipe::<i32>::new(1);
        // SAFETY: resolved while the reader is alive.
        let f = unsafe { reader.read() };
        assert!(!f.available());
        drop(writer);
        yield_().get()?;
        assert_eq!(f.get()?, None);
        Ok(())
    })
});

#[test]
fn tickets_truth_value_and_non_zero() {
    assert!(!FairQueueTicket::new(0, 0).as_bool());
    assert!(FairQueueTicket::new(1, 0).as_bool());
    assert!(FairQueueTicket::new(0, 1).as_bool());
    assert!(!FairQueueTicket::new(1, 0).is_non_zero());
    assert!(!FairQueueTicket::new(0, 1).is_non_zero());
    assert!(FairQueueTicket::new(1, 1).is_non_zero());
}

#[test]
fn tickets_normalise_along_an_axis() {
    let axis = FairQueueTicket::new(4, 1000);
    assert_eq!(FairQueueTicket::new(2, 500).normalize(axis), 1.0);
    assert_eq!(FairQueueTicket::new(4, 0).normalize(axis), 1.0);
    assert_eq!(FairQueueTicket::new(0, 250).normalize(axis), 0.25);
}

#[test]
fn tickets_add_subtract_and_clamp_differences() {
    let a = FairQueueTicket::new(3, 30);
    let b = FairQueueTicket::new(1, 50);
    assert_eq!(a + b, FairQueueTicket::new(4, 80));
    let mut c = a;
    c += b;
    assert_eq!(c, FairQueueTicket::new(4, 80));
    c -= b;
    assert_eq!(c, a);
    assert_eq!(a - FairQueueTicket::new(1, 10), FairQueueTicket::new(2, 20));
    // Each axis of the difference is clamped at zero, as a signed 32-bit difference.
    assert_eq!(wrapping_difference(&a, &b), FairQueueTicket::new(2, 0));
    assert_eq!(wrapping_difference(&b, &a), FairQueueTicket::new(0, 20));
    // The counters wrap: a value just past the wrap is still "after" the old one.
    let old = FairQueueTicket::new(u32::MAX, u32::MAX);
    let new = old + FairQueueTicket::new(2, 3);
    assert_eq!(new, FairQueueTicket::new(1, 2));
    assert_eq!(wrapping_difference(&new, &old), FairQueueTicket::new(2, 3));
    assert_eq!(format!("{a}"), "3:30");
}

#[test]
fn temporary_buffer_views() {
    let b = TemporaryBuffer::copy_from(b"abcdef");
    assert_eq!(b.end() as usize - b.begin() as usize, 6);
    assert_eq!(b.begin(), b.get());
    assert_eq!(b.at(2), b'c');
    assert_eq!(b[5], b'f');
    // SAFETY: the position is within the buffer.
    assert_eq!(unsafe { b.at_unchecked(0) }, b'a');
    let p = b.prefix(3);
    assert_eq!(p.as_slice(), b"abc");
    // SAFETY: 1 is within the prefix.
    let q = unsafe { p.prefix_unchecked(1) };
    assert_eq!(q.as_slice(), b"a");
}

#[test]
#[should_panic(expected = "temporary_buffer::prefix")]
fn a_prefix_longer_than_the_buffer_panics() {
    let _ = TemporaryBuffer::copy_from(b"ab").prefix(3);
}
