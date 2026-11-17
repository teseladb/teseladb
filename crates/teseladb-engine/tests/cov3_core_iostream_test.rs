//! Stream entry points not reached elsewhere (the original's iostream.hh): the default `close`
//! of a data source and the default `put(vector)` and `can_batch_flushes` of a data sink, the
//! deprecated boolean constructor of an output stream (batched flushes only when the sink can),
//! raw and NUL-terminated writes, detaching a fresh and a used stream, the generic `Closeable`
//! implementations, the batch flush list, and the consumption result accessors.
#![cfg(target_os = "linux")]
#![allow(unsafe_code, deprecated)]

use std::cell::RefCell;
use std::rc::Rc;

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::iostream::{
    BatchFlushList, ConsumptionResult, DataSink, DataSinkImpl, DataSource, DataSourceImpl,
    InputStream, OutputStream, StopConsuming,
};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::util::closeable::Closeable;
use teseladb::engine_thread_test_case;

/// A sink that keeps what it is given; only `put`, `close` and `buffer_size` are its own.
struct Recorder(Rc<RefCell<Vec<u8>>>);

impl DataSinkImpl for Recorder {
    fn put(&mut self, mut data: Packet) -> Future<()> {
        data.linearize_all();
        for i in 0..data.nr_frags() {
            let f = data.frag(i);
            // SAFETY: the fragment's bytes live in the packet during this call.
            let bytes = unsafe { std::slice::from_raw_parts(f.base, f.size) };
            self.0.borrow_mut().extend_from_slice(bytes);
        }
        make_ready_future(())
    }
    fn close(&mut self) -> Future<()> {
        make_ready_future(())
    }
    fn buffer_size(&self) -> usize {
        8
    }
}

/// A source of one buffer, with the default `close`.
struct OneBuffer(Option<String>);

impl DataSourceImpl for OneBuffer {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        match self.0.take() {
            Some(v) => make_ready_future(TemporaryBuffer::copy_of(&v)),
            None => make_ready_future(TemporaryBuffer::default()),
        }
    }
}

fn recorder() -> (DataSink, Rc<RefCell<Vec<u8>>>) {
    let out = Rc::new(RefCell::new(Vec::new()));
    (DataSink::new(Box::new(Recorder(out.clone()))), out)
}

engine_thread_test_case!(sinks_take_vectors_of_buffers_by_default, {
    let (mut sink, out) = recorder();
    let bufs = vec![
        TemporaryBuffer::copy_of("ab"),
        TemporaryBuffer::copy_of("cd"),
    ];
    sink.put_vec(bufs).get().unwrap();
    assert_eq!(&*out.borrow(), b"abcd");
    Closeable::close(&mut sink).get().unwrap();
});

engine_thread_test_case!(the_boolean_constructor_batches_only_when_the_sink_can, {
    for batch in [false, true] {
        let (sink, out) = recorder();
        let mut os = OutputStream::<u8>::with_flags(sink, 4, true, batch);
        os.write(b"hello").get().unwrap();
        let s = b"raw!";
        // SAFETY: `s` is valid for its length during the call.
        unsafe { os.write_raw(s.as_ptr(), s.len()) }.get().unwrap();
        let c = b"cstr\0";
        // SAFETY: a NUL-terminated string.
        unsafe { os.write_cstr(c.as_ptr()) }.get().unwrap();
        os.flush().get().unwrap();
        Closeable::close(&mut os).get().unwrap();
        assert_eq!(&*out.borrow(), b"helloraw!cstr", "batch {batch}");
    }
});

engine_thread_test_case!(detaching_streams, {
    // A fresh output stream gives its sink back; a used one refuses.
    let (sink, out) = recorder();
    let mut os = OutputStream::<u8>::new(sink);
    let mut sink = os.detach().unwrap();
    drop(os);
    sink.put_vec(vec![TemporaryBuffer::copy_of("x")])
        .get()
        .unwrap();
    assert_eq!(&*out.borrow(), b"x");
    let mut os = OutputStream::<u8>::new(sink);
    os.write(b"abc").get().unwrap();
    let e = os.detach().err().expect("a used stream");
    assert!(
        e.get()
            .unwrap()
            .what()
            .contains("detach() called on a used output_stream")
    );
    // The refused stream is still the caller's: its buffered bytes are flushed on close (as
    // after the original's throwing `std::move(os).detach()`), and dropping it is fine.
    os.close().get().unwrap();
    assert_eq!(&*out.borrow(), b"xabc");
    drop(os);

    // The same for input streams.
    let is = InputStream::<u8>::new(DataSource::new(Box::new(OneBuffer(Some("in".to_string())))));
    let mut src = is.detach().unwrap();
    let first = src.get().get().unwrap();
    assert_eq!(first.as_slice(), b"in");
    Closeable::close(&mut src).get().unwrap();
    let mut is = InputStream::<u8>::new(DataSource::new(Box::new(OneBuffer(Some(
        "more".to_string(),
    )))));
    // A partial read leaves bytes in the stream's buffer.
    let two = is.read_exactly(2).get().unwrap();
    assert_eq!(two.as_slice(), b"mo");
    let e = is.detach().err().expect("a used stream");
    assert!(
        e.get()
            .unwrap()
            .what()
            .contains("detach() called on a used input_stream")
    );
});

engine_thread_test_case!(input_streams_close_through_closeable, {
    let mut is = InputStream::<u8>::new(DataSource::new(Box::new(OneBuffer(None))));
    assert!(is.read().get().unwrap().empty());
    Closeable::close(&mut is).get().unwrap();
});

#[test]
fn the_batch_flush_list_starts_empty() {
    let mut l: BatchFlushList = BatchFlushList::default();
    assert!(l.empty());
    assert!(l.front().is_null());
    l.pop_front();
    assert!(l.empty(), "popping an empty list does nothing");
}

#[test]
fn consumption_results_expose_their_buffers() {
    let stop = StopConsuming::<u8>::new(TemporaryBuffer::copy_of("left"));
    assert_eq!(stop.get_buffer_ref().as_slice(), b"left");
    let r = ConsumptionResult::<u8>::from(stop);
    let _ = r.get_ref();
}
