//! Body transformers for the file handler: placeholder substitution on the fly.
//!
//! The transformer wraps the output stream of a file reply and replaces every
//! `{{key}}` placeholder with its value as the file streams through, including
//! placeholders split across buffers. Potential matches are kept in a `Vec` in insertion
//! order, so erasures keep their order without one allocation per entry; an entry keeps
//! offsets into its key instead of pointers.

use crate::engine::core::do_with::do_with;
use crate::engine::core::future::{Future, make_ready_future};
use crate::engine::core::iostream::{DataSink, DataSinkImpl, OutputStream, OutputStreamOptions};
use crate::engine::core::loop_::{StopIteration, repeat};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::http::file_handler::FileTransformer;
use crate::engine::http::request::Request;
use crate::engine::net::packet::Packet;

/// The buffer size of the transformed stream.
const CONTENT_REPLACE_BUFFER_SIZE: usize = 32000;

/// A [`FileTransformer`] that replaces variables in a file with values from the request.
///
/// It replaces `{{Host}}` with the request's `Host` header and `{{Protocol}}` with its
/// protocol name. Requests without a `Host` header are not transformed. The replacement
/// can be restricted to the files of one extension.
#[derive(Clone, Debug, Default)]
pub struct ContentReplace {
    extension: Sstring,
}

impl ContentReplace {
    /// Creates a transformer for the files with extension `extension`, or for all files
    /// when `extension` is empty.
    pub fn new(extension: &Sstring) -> Self {
        ContentReplace { extension: extension.clone() }
    }
}

#[derive(Clone, Copy, Debug)]
struct PotentialMatchEntry {
    /// The offset in the key of the next byte to match.
    begin: usize,
    /// The length of the key.
    end: usize,
    /// The index of the key/value.
    pos: usize,
}

/// The current matching state of a [`BufferReplace`]: the keys that may still match.
///
/// When a key matches, it is the first entry.
#[derive(Default)]
struct BufferReplaceState {
    _potential_match: Vec<PotentialMatchEntry>,
}

impl BufferReplaceState {
    #[inline]
    fn add_potential_match(&mut self, s: usize, e: usize, pos: usize) {
        self._potential_match.push(PotentialMatchEntry { begin: s, end: e, pos });
    }

    #[inline]
    fn empty(&self) -> bool {
        self._potential_match.is_empty()
    }

    #[inline]
    fn last(&self) -> bool {
        self._potential_match.len() == 1
    }

    /// Returns the index of the matching key/value in the [`BufferReplace`].
    #[inline]
    fn get_pos(&self) -> usize {
        self._potential_match[0].pos
    }

    /// Returns the length of the part of the matching key not matched yet.
    #[inline]
    fn get_remaining_length(&self) -> usize {
        let m = &self._potential_match[0];
        m.end - m.begin
    }

    #[inline]
    fn clear(&mut self) {
        self._potential_match.clear();
    }
}

/// Replaces keys with values in a stream of buffers; a key appears in the text inside
/// double braces (`{{key}}`) and may span buffers.
#[derive(Default)]
struct BufferReplace {
    _values: Vec<(Sstring, Sstring)>,
    _current: BufferReplaceState,
}

/// Returns the position of the opening braces of a key in `s`: two consecutive `{`, or a
/// single `{` as the last byte (the second may start the next buffer).
fn find_braces(s: &[u8]) -> Option<usize> {
    let n = s.len();
    (0..n).find(|&i| s[i] == b'{' && (i + 1 == n || s[i + 1] == b'{'))
}

impl BufferReplace {
    #[inline]
    fn get_value(&self, pos: usize) -> &Sstring {
        &self._values[pos].1
    }

    #[inline]
    fn get_key(&self, pos: usize) -> &Sstring {
        &self._values[pos].0
    }

    /// Adds the key `key` (matched as `{{key}}`) to be replaced with `value`, and returns
    /// `self`.
    fn add(&mut self, key: &Sstring, value: &Sstring) -> &mut Self {
        let mut k = Sstring::new();
        k.append_bytes(b"{{");
        k.append_bytes(key.as_bytes());
        k.append_bytes(b"}}");
        self._values.push((k, value.clone()));
        self
    }

    /// Returns the bytes of a partial match held back, for when no more buffers come;
    /// empty when no match is in progress.
    fn get_remaining(&self) -> TemporaryBuffer<u8> {
        if !self.is_consuming() {
            return TemporaryBuffer::empty_buffer();
        }
        let pos = self._current.get_pos();
        let key = self.get_key(pos);
        let size = key.size() - self._current.get_remaining_length();
        TemporaryBuffer::copy_from(&key.as_bytes()[..size])
    }

    /// Continues the match in progress with the start of `buf`, consuming what it matches.
    ///
    /// Returns the replacement value when a key completes, the held-back bytes when no
    /// key can match any more, or an empty buffer while the match goes on.
    fn match_(&mut self, buf: &mut TemporaryBuffer<u8>) -> TemporaryBuffer<u8> {
        if self._current.empty() {
            return TemporaryBuffer::empty_buffer();
        }
        let buf_len = buf.size();
        let mut first = 0;
        while first < self._current._potential_match.len() {
            let PotentialMatchEntry { begin, end, pos } = self._current._potential_match[first];
            let len_compare = buf_len.min(end - begin);
            let key = self._values[pos].0.as_bytes();
            if key[begin..begin + len_compare] != buf.as_slice()[..len_compare] {
                // No match remove the entry unless it's the last one
                // In that case, there is no match
                // we should return what we consumed so far;
                if self._current.last() {
                    let res = self.get_remaining();
                    self._current._potential_match.remove(first);
                    return res;
                }
                self._current._potential_match.remove(first);
            } else {
                // we found a match
                if begin + len_compare == end {
                    // this is a full match, there could be only one so this is the first
                    // consume the buffer and return
                    let value = self.get_value(self._current.get_pos());
                    let res = TemporaryBuffer::copy_from(value.as_bytes());
                    buf.trim_front(len_compare);
                    self._current.clear();
                    return res;
                }
                // only partial match
                self._current._potential_match[first].begin += len_compare;
                first += 1;
            }
        }
        // if we are here we run out of buffer
        buf.trim_front(buf_len);
        TemporaryBuffer::empty_buffer()
    }

    /// Returns the next piece of output from `buf`, after substitution, and consumes from
    /// `buf` what it read; the caller calls it again while `buf` is not empty.
    ///
    /// For example, with `buf` holding `abcd{{key}}`, the call returns `abcd` and leaves
    /// `{{key}}` in `buf`.
    fn replace(&mut self, buf: &mut TemporaryBuffer<u8>) -> TemporaryBuffer<u8> {
        if buf.empty() {
            return std::mem::take(buf);
        }
        if self.is_consuming() {
            return self.match_(buf);
        }
        if let Some(start) = find_braces(buf.as_slice()) {
            // we found an opening brace that is followed by a second brace or buffer end.
            // 1. All values are a possible match
            // 2. We can output the beginning of the buffer
            // 3. Need to continue matching the remaining buffer
            for (pos, (key, _)) in self._values.iter().enumerate() {
                self._current.add_potential_match(1, key.size(), pos);
            }
            let res = buf.share_range(0, start);
            buf.trim_front(start + 1);
            return res;
        }

        std::mem::take(buf)
    }

    /// Returns whether a match is in progress.
    #[inline]
    fn is_consuming(&self) -> bool {
        !self._current.empty()
    }
}

struct ContentReplaceDataSinkImpl {
    _out: OutputStream<u8>,
    _br: BufferReplace,
}

impl ContentReplaceDataSinkImpl {
    fn new(out: OutputStream<u8>, key_value: Vec<(Sstring, Sstring)>) -> Self {
        let mut s = ContentReplaceDataSinkImpl { _out: out, _br: BufferReplace::default() };
        for (k, v) in &key_value {
            s._br.add(k, v);
        }
        s
    }
}

impl DataSinkImpl for ContentReplaceDataSinkImpl {
    fn put(&mut self, _data: Packet) -> Future<()> {
        make_ready_future(())
    }

    fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
        if buf.empty() {
            return make_ready_future(());
        }
        let this: *mut ContentReplaceDataSinkImpl = self;
        do_with(buf, move |buf: &mut TemporaryBuffer<u8>| {
            let bufp: *mut TemporaryBuffer<u8> = buf;
            repeat(move || {
                // SAFETY: the sink outlives its puts, and a put runs alone on the sink.
                let s = unsafe { &mut *this };
                // SAFETY: do_with keeps the buffer alive and unmoved until the loop ends, and
                // the loop body is its only user.
                let bf = s._br.replace(unsafe { &mut *bufp });
                s._out.write(bf.as_slice()).then(move |()| {
                    drop(bf);
                    // SAFETY: do_with keeps the buffer alive until the loop ends.
                    make_ready_future(if unsafe { (*bufp).empty() } {
                        StopIteration::YES
                    } else {
                        StopIteration::NO
                    })
                })
            })
        })
    }

    fn flush(&mut self) -> Future<()> {
        self._out.flush()
    }

    fn close(&mut self) -> Future<()> {
        // if we are in the middle of a consuming a key
        // there will be no match, write the remaining.
        if self._br.is_consuming() {
            let this: *mut ContentReplaceDataSinkImpl = self;
            let rem = self._br.get_remaining();
            return do_with(rem, move |buf: &mut TemporaryBuffer<u8>| {
                // SAFETY: the sink outlives its close and do_with keeps the buffer alive.
                let s = unsafe { &mut *this };
                // SAFETY: the sink outlives its close; the write has finished with the stream.
                s._out.write(buf.as_slice()).then(move |()| unsafe { (*this)._out.flush() })
            });
        }
        self._out.flush()
    }
}

/// A data sink that writes into `out` with every `{{key}}` of `key_value` replaced.
fn content_replace_data_sink(
    out: OutputStream<u8>,
    key_value: Vec<(Sstring, Sstring)>,
) -> DataSink {
    DataSink::new(Box::new(ContentReplaceDataSinkImpl::new(out, key_value)))
}

impl FileTransformer for ContentReplace {
    fn transform(
        &mut self,
        req: Box<Request>,
        extension: &Sstring,
        s: OutputStream<u8>,
    ) -> OutputStream<u8> {
        let host = req.get_header("Host");
        if host.empty() || (!self.extension.empty() && *extension != self.extension) {
            return s;
        }
        let protocol = req.get_protocol_name();
        let opts = OutputStreamOptions { trim_to_size: true, ..Default::default() };
        OutputStream::with_options(
            content_replace_data_sink(
                s,
                vec![(Sstring::from("Protocol"), protocol), (Sstring::from("Host"), host)],
            ),
            CONTENT_REPLACE_BUFFER_SIZE,
            opts,
        )
    }
}
