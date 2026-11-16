//! An input stream over the bytes of a [`Packet`], without copying.
//!
//! [`PacketDataSource`] hands out the packet's fragments one by one as temporary buffers
//! that keep the packet alive; [`as_input_stream`] wraps it in an [`InputStream`].

use crate::engine::core::deleter::{Deleter, make_deleter_with};
use crate::engine::core::future::{Future, make_ready_future};
use crate::engine::core::iostream::{DataSource, DataSourceImpl, InputStream};
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::packet::Packet;

/// A data source that yields the fragments of a packet, one per `get`, without copying;
/// after the last fragment it yields an empty buffer (end of stream).
pub struct PacketDataSource {
    _cur_frag: usize,
    _p: Packet,
}

impl PacketDataSource {
    /// A source over the fragments of `p`.
    pub fn new(p: Packet) -> Self {
        PacketDataSource { _cur_frag: 0, _p: p }
    }
}

impl DataSourceImpl for PacketDataSource {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        if self._cur_frag != self._p.nr_frags() as usize {
            // `share()` first: it may move the first fragment out of the packet's internal
            // area, and the buffer must point to where the fragment lives afterwards, which
            // the shared packet keeps alive.
            let p = self._p.share();
            let f = self._p.frag(self._cur_frag as u32);
            self._cur_frag += 1;
            // SAFETY: the fragment's bytes live as long as the shared packet `p`, which the
            // buffer's deleter owns.
            return make_ready_future::<TemporaryBuffer<u8>>(unsafe {
                TemporaryBuffer::<u8>::with_deleter(
                    f.base,
                    f.size,
                    make_deleter_with(Deleter::default(), move || drop(p)),
                )
            });
        }
        make_ready_future::<TemporaryBuffer<u8>>(TemporaryBuffer::<u8>::empty_buffer())
    }
}

/// Returns an input stream over the bytes of `p`.
#[inline]
pub fn as_input_stream(p: Packet) -> InputStream<u8> {
    InputStream::<u8>::new(DataSource::new(Box::new(PacketDataSource::new(p))))
}
