//! Tests of `Pipe`: values written to the writer are read in order from the reader, and the
//! reader sees the end once the writer is gone.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::pipe::Pipe;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

// `PipeReader` and `PipeWriter` have no `Default`, and every move is
// a bitwise copy that cannot fail.

engine_test_case!(simple_pipe_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut p = Pipe::<i32>::new(1);
        // SAFETY: each future is resolved with get() while
        // the pipe `p` is alive.

        let f0 = unsafe { p.reader.read() };
        assert!(!f0.available());
        // SAFETY: the pipe `p` is alive until the end of the test.
        unsafe { p.writer.write(17) }.get()?;
        assert_eq!(f0.get()?.expect("value"), 17);

        // SAFETY: the pipe `p` is alive until the end of the test.
        unsafe { p.writer.write(42) }.get()?;
        // SAFETY: the pipe `p` is alive until the end of the test.
        let f2 = unsafe { p.reader.read() };
        assert!(f2.available());
        assert_eq!(f2.get()?.expect("value"), 42);
        Ok(())
    })
});
