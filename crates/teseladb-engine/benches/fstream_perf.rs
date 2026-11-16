//! Sequential write throughput of a file output stream.
//!
//! Writes `--total-ops` buffers of `--buffer-size` bytes to `testfile.tmp` with up to
//! `--concurrency` writes in flight (write-behind) and prints the achieved IOPS.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::time::Instant;

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::file::{FileOpenOptions, open_file_dma_with_options};
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::fstream::{FileOutputStreamOptions, make_file_output_stream};
use teseladb::engine::core::program_options::bpo;

pub(crate) fn main() {
    let mut at = AppTemplate::new(Default::default());
    at.add_options()
        .opt(
            "concurrency",
            bpo::value::<u32>().default_value(1),
            "Write operations to issue in parallel",
        )
        .opt("buffer-size", bpo::value::<usize>().default_value(4096), "Write buffer size")
        .opt(
            "total-ops",
            bpo::value::<u32>().default_value(100000),
            "Total write operations to issue",
        )
        .opt(
            "sloppy-size",
            bpo::value::<bool>().default_value(false),
            "Enable the sloppy-size optimization",
        );
    let atp: *mut AppTemplate = &mut *at;
    let args: Vec<String> = std::env::args().collect();
    let status = at.run(args, move || {
        // SAFETY: the app outlives its run().
        let cfg = unsafe { (*atp).configuration() };
        let concurrency = *cfg.get::<u32>("concurrency").expect("default");
        let buffer_size = *cfg.get::<usize>("buffer-size").expect("default");
        let total_ops = *cfg.get::<u32>("total-ops").expect("default");
        let sloppy_size = *cfg.get::<bool>("sloppy-size").expect("default");
        make_coroutine(async move {
            // The open options of the test file.
            #[allow(clippy::disallowed_names)]
            let foo = FileOpenOptions { sloppy_size, ..FileOpenOptions::default() };
            let f = open_file_dma_with_options(
                "testfile.tmp",
                OpenFlags::WO | OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                foo,
            )
            .await?;
            let foso = FileOutputStreamOptions {
                buffer_size: buffer_size as u32,
                preallocation_size: 32 << 20,
                write_behind: concurrency,
            };
            let mut os = make_file_output_stream(f.clone(), foso).await?;
            let _f = f;
            let mut completed: u32 = 0;
            let start = Instant::now();
            // Write buffers until `total_ops` writes have completed.
            loop {
                if completed == total_ops {
                    break;
                }
                let buf = vec![0u8; buffer_size];
                os.write(&buf).await?;
                completed += 1;
            }
            let end = Instant::now();
            let iops = total_ops as f32 / (end - start).as_secs_f32();
            println!("{:<10} {:<10} {:<10} {:<12}", "bufsize", "ops", "iodepth", "IOPS");
            println!("{:>10} {:>10} {:>10} {:>12.0}", buffer_size, total_ops, concurrency, iops);
            os.flush().await?;
            os.close().await?;
            Ok(())
        })
    });
    std::process::exit(status);
}
