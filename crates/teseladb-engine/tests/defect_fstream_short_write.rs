#![cfg(target_os = "linux")]
//! A file output stream whose `dma_write` completes only part of a request resumes the write
//! from an aligned position. For `O_DIRECT` files the completed amount is always a multiple of
//! the disk write alignment, but a buffered write (what the reactor does with the kernel page
//! cache enabled) can complete any amount; resuming at `pos + completed` broke the alignment
//! that `dma_write` requires and wrote zero padding past the end of the request.

#[path = "common/mock_file.rs"]
mod mock_file;

use mock_file::MockWriteOnlyFile;
use teseladb::engine::core::file::{File, FileImpl};
use teseladb::engine::core::fstream::{FileOutputStreamOptions, make_file_output_stream};
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::shared_ptr::{make_shared, static_pointer_cast};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

engine_test_case!(test_fstream_short_write, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        const BUFFER_SIZE: usize = 8192;
        // Enough to fill the stream buffer a few times and leave an unaligned tail for the
        // final flush.
        let data: Vec<u8> = (0..3 * BUFFER_SIZE + 1000).map(|i| (i % 251) as u8).collect();

        for write_behind in [0u32, 4] {
            for completed in [1usize, 100, 4095, 4096, 4097, BUFFER_SIZE - 1] {
                let mf = make_shared(MockWriteOnlyFile::default());
                mf.complete_partially(&[completed, completed]);
                // SAFETY: the cast keeps the pointer into the same object.
                let f = File::from_impl(unsafe {
                    static_pointer_cast(&mf, |x| x as *mut dyn FileImpl)
                });
                let options = FileOutputStreamOptions {
                    buffer_size: BUFFER_SIZE as u32,
                    write_behind,
                    ..FileOutputStreamOptions::default()
                };
                let mut out = make_file_output_stream(f, options).get()?;
                out.write(&data).get()?;
                out.close().get()?;
                assert!(
                    mf.contents() == data,
                    "wrong contents with write_behind {write_behind}, {completed} bytes completed"
                );
            }
        }
        Ok(())
    })
});
