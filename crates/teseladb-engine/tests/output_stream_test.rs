//! Tests of `OutputStream`: how writes are split into packets of the buffer size, with and
//! without trimming, flushing an empty buffer, and a simple write.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicI32, Ordering};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::Future;
use teseladb::engine::core::iostream::{DataSink, OutputStream, OutputStreamOptions};
use teseladb::engine::core::shared_ptr::{SharedPtr, make_shared};
use teseladb::engine::core::sstring::{Sstring, uninitialized_string};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::vector_data_sink::VectorDataSink;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::util::later::now;
use teseladb::engine_test_case;

fn to_sstring(p: &Packet) -> Sstring {
    let mut res = uninitialized_string(p.len() as usize);
    let mut i = 0usize;
    for frag in p.fragments().iter() {
        // SAFETY: `res` has `p.len()` bytes and the fragments add up to that length.
        unsafe { std::ptr::copy_nonoverlapping(frag.base, res.data().add(i), frag.size) };
        i += frag.size;
    }
    res
}

#[derive(Clone, Default)]
struct StreamMaker {
    opts: OutputStreamOptions,
    size: usize,
}

impl StreamMaker {
    fn size(mut self, size: usize) -> Self {
        self.size = size;
        self
    }

    fn trim(mut self, do_trim: bool) -> Self {
        self.opts.trim_to_size = do_trim;
        self
    }

    fn call(&self, sink: DataSink) -> OutputStream<u8> {
        OutputStream::with_options(sink, self.size, self.opts)
    }
}

static SPLIT_INDEX: AtomicI32 = AtomicI32::new(0);

fn assert_split(
    stream_maker: StreamMaker,
    write_calls: &[&'static str],
    expected_split: &[&'static str],
) -> Future<()> {
    println!("checking split: {}", SPLIT_INDEX.fetch_add(1, Ordering::Relaxed));
    let sh_write_calls: Vec<&'static str> = write_calls.to_vec();
    let sh_expected_splits: Vec<&'static str> = expected_split.to_vec();
    let v: SharedPtr<Vec<Packet>> = make_shared(Vec::new());
    let vp = v.get().expect("live").as_ptr();
    make_coroutine(async move {
        // SAFETY: `v` lives in this coroutine until its end, at its address in the shared
        // pointer; the sink writes it only during the writes and the close of `out`, and the
        // coroutine reads it only after the close.
        let mut out =
            stream_maker.call(DataSink::new(Box::new(unsafe { VectorDataSink::new(vp) })));
        for chunk in &sh_write_calls {
            out.write_str(chunk).await?;
        }
        out.close().await?;
        assert_eq!(v.len(), sh_expected_splits.len());
        for (i, chunk) in sh_expected_splits.iter().enumerate() {
            assert!(to_sstring(&v[i]) == *chunk);
        }
        Ok(())
    })
}

engine_test_case!(test_splitting, {
    let ctor = StreamMaker::default().trim(false).size(4);
    let c = ctor.clone();
    now()
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["1"], &["1"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12", "3"], &["123"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12", "34"], &["1234"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12", "345"], &["1234", "5"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["1234"], &["1234"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12345"], &["12345"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["1234567890"], &["1234567890"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["1", "23456"], &["1234", "56"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["123", "4567"], &["1234", "567"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["123", "45678"], &["1234", "5678"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["123", "4567890"], &["1234", "567890"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["1234", "567"], &["1234", "567"])
        })
        .then(|()| {
            assert_split(
                StreamMaker::default().trim(false).size(3),
                &["1", "234567", "89"],
                &["123", "4567", "89"],
            )
        })
        .then(|()| {
            assert_split(
                StreamMaker::default().trim(false).size(3),
                &["1", "2345", "67"],
                &["123", "456", "7"],
            )
        })
});

engine_test_case!(test_splitting_with_trimming, {
    let c = StreamMaker::default().trim(true).size(4);
    now()
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["1"], &["1"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12", "3"], &["123"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12", "3456789"], &["1234", "5678", "9"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12", "3456789", "12"], &["1234", "5678", "912"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["123456789"], &["1234", "5678", "9"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12345678"], &["1234", "5678"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["12345678", "9"], &["1234", "5678", "9"])
        })
        .then({
            let c = c.clone();
            move |()| assert_split(c, &["1234", "567890"], &["1234", "5678", "90"])
        })
});

engine_test_case!(test_flush_on_empty_buffer_does_not_push_empty_packet_down_stream, {
    let v: SharedPtr<Vec<Packet>> = make_shared(Vec::new());
    // SAFETY: `v` is kept alive, at its address in the shared pointer, by the continuation
    // that reads it after the flush, which runs before the close.
    let sink = unsafe { VectorDataSink::new(v.get().expect("live").as_ptr()) };
    let out = make_shared(OutputStream::<u8>::with_options(
        DataSink::new(Box::new(sink)),
        8,
        OutputStreamOptions::default(),
    ));
    let outp = out.get().expect("live").as_ptr();
    // SAFETY: `out` is kept alive by the finally() continuation.
    unsafe { (*outp).flush() }
        .then(move |()| {
            assert!(v.is_empty());
            // SAFETY: as above.
            unsafe { (*outp).close() }
        })
        .finally(move || drop(out))
});

engine_test_case!(test_simple_write, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut vec: Vec<Packet> = Vec::new();
        // SAFETY: `vec` outlives `out` (dropped below before `vec` is read) and stays in place;
        // nothing borrows it while the stream writes.
        let sink = unsafe { VectorDataSink::new(&mut vec) };
        let mut out = OutputStream::<u8>::with_options(
            DataSink::new(Box::new(sink)),
            8,
            OutputStreamOptions::default(),
        );

        let value1 = Sstring::from("te");
        out.write_str(value1.as_str()).get()?;

        let value2 = Sstring::from("st");
        out.write_str(value2.as_str()).get()?;

        let value3 = Sstring::from("abcdefgh1234");
        out.write_str(value3.as_str()).get()?;

        out.close().get()?;
        drop(out);

        let value = &(&value1 + &value2) + &value3;
        let mut packets = Packet::default();
        for p in vec.drain(..) {
            packets.append(p);
        }
        packets.linearize_all();
        let buf = packets.release();
        assert_eq!(buf.len(), 1);
        assert_eq!(Sstring::from_bytes(buf[0].as_slice()), value);
        Ok(())
    })
});
