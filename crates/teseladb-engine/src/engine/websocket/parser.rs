//! The WebSocket frame parser (RFC 6455, section 5).
//!
//! [`websocket::WebsocketParser`] consumes the socket's input stream buffer by buffer and
//! yields the unmasked payload of each frame with its opcode; it is resumable at any byte,
//! so a header or payload may span buffers. [`websocket::FrameHeader`] decodes the first
//! two bytes of a frame. A payload whose declared length cannot be allocated fails the
//! connection's consume with an allocation error.

/// The frame parser and its types (interface still subject to change).
pub mod websocket {
    use crate::engine::core::byteorder::consume_be;
    use crate::engine::core::future::{
        BadAlloc, ExceptionPtr, Future, make_exception_future, make_ready_future,
    };
    use crate::engine::core::iostream::{ConsumptionResult, continue_consuming, stop_consuming};
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine_assert;

    /// The opcode of a WebSocket frame, which gives its type.
    ///
    /// A transparent newtype, so any 4-bit opcode of a frame is a value of it, named or not.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    #[repr(transparent)]
    pub struct Opcodes(pub u8);

    impl Opcodes {
        /// CONTINUATION
        pub const CONTINUATION: Opcodes = Opcodes(0x0);
        /// TEXT
        pub const TEXT: Opcodes = Opcodes(0x1);
        /// BINARY
        pub const BINARY: Opcodes = Opcodes(0x2);
        /// CLOSE
        pub const CLOSE: Opcodes = Opcodes(0x8);
        /// PING
        pub const PING: Opcodes = Opcodes(0x9);
        /// PONG
        pub const PONG: Opcodes = Opcodes(0xA);
        /// INVALID
        pub const INVALID: Opcodes = Opcodes(0xFF);
    }

    /// The first two bytes of a frame, with the bit fields
    /// (`fin:1 rsv1:1 rsv2:1 rsv3:1 opcode:4 masked:1 length:7`) packed in two bytes.
    #[derive(Clone, Copy, Debug)]
    #[repr(C)]
    pub struct FrameHeader {
        _b0: u8,
        _b1: u8,
    }

    impl FrameHeader {
        /// The bit of fin in the first byte.
        pub const FIN: u8 = 7;
        /// The bit of rsv1 in the first byte.
        pub const RSV1: u8 = 6;
        /// The bit of rsv2 in the first byte.
        pub const RSV2: u8 = 5;
        /// The bit of rsv3 in the first byte.
        pub const RSV3: u8 = 4;
        /// The bit of masked in the second byte.
        pub const MASKED: u8 = 7;

        /// The header of the frame starting at `input`.
        #[inline]
        pub fn new(input: &[u8]) -> Self {
            FrameHeader { _b0: input[0], _b1: input[1] }
        }

        /// The fin bit.
        #[inline]
        pub fn fin(&self) -> u8 {
            (self._b0 >> Self::FIN) & 1
        }

        /// The rsv1 bit.
        #[inline]
        pub fn rsv1(&self) -> u8 {
            (self._b0 >> Self::RSV1) & 1
        }

        /// The rsv2 bit.
        #[inline]
        pub fn rsv2(&self) -> u8 {
            (self._b0 >> Self::RSV2) & 1
        }

        /// The rsv3 bit.
        #[inline]
        pub fn rsv3(&self) -> u8 {
            (self._b0 >> Self::RSV3) & 1
        }

        /// The opcode.
        #[inline]
        pub fn opcode(&self) -> u8 {
            self._b0 & 0b1111
        }

        /// The masked bit.
        #[inline]
        pub fn masked(&self) -> u8 {
            (self._b1 >> Self::MASKED) & 1
        }

        /// The 7-bit length.
        #[inline]
        pub fn length(&self) -> u8 {
            self._b1 & 0b1111111
        }

        // Returns length of the rest of the header.
        /// The length of the rest of the header: the masking key and the extended length.
        #[inline]
        pub fn get_rest_of_header_length(&self) -> u64 {
            let mut next_read_length = size_of::<u32>() as u64; // Masking key
            if self.length() == 126 {
                next_read_length += size_of::<u16>() as u64;
            } else if self.length() == 127 {
                next_read_length += size_of::<u64>() as u64;
            }
            next_read_length
        }

        /// `get_fin()`
        #[inline]
        pub fn get_fin(&self) -> u8 {
            self.fin()
        }

        /// `get_rsv1()`
        #[inline]
        pub fn get_rsv1(&self) -> u8 {
            self.rsv1()
        }

        /// `get_rsv2()`
        #[inline]
        pub fn get_rsv2(&self) -> u8 {
            self.rsv2()
        }

        /// `get_rsv3()`
        #[inline]
        pub fn get_rsv3(&self) -> u8 {
            self.rsv3()
        }

        /// `get_opcode()`
        #[inline]
        pub fn get_opcode(&self) -> u8 {
            self.opcode()
        }

        /// `get_masked()`
        #[inline]
        pub fn get_masked(&self) -> u8 {
            self.masked()
        }

        /// `get_length()`
        #[inline]
        pub fn get_length(&self) -> u8 {
            self.length()
        }

        /// Whether the opcode is one RFC 6455 defines.
        #[inline]
        pub fn is_opcode_known(&self) -> bool {
            //https://datatracker.ietf.org/doc/html/rfc6455#section-5.1
            let opcode = self.opcode();
            opcode < 0xA && !(opcode < 0x8 && opcode > 0x2)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    enum ParsingState {
        FlagsAndPayloadData,
        PayloadLengthAndMask,
        Payload,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    enum ConnectionState {
        Valid,
        Closed,
        Error,
    }

    type ConsumptionResultT = ConsumptionResult<u8>;
    type BuffT = TemporaryBuffer<u8>;

    /// The parser of the frames of a websocket connection, a consumer of its input stream.
    pub struct WebsocketParser {
        // What parser is currently doing.
        _state: ParsingState,
        // State of connection - can be valid, closed or should be closed
        // due to error.
        _cstate: ConnectionState,
        _buffer: Sstring,
        _header: Option<FrameHeader>,
        _payload_length: u64,
        _consumed_payload_length: u64,
        _masking_key: u32,
        _result: BuffT,
    }

    impl Default for WebsocketParser {
        fn default() -> Self {
            WebsocketParser {
                _state: ParsingState::FlagsAndPayloadData,
                _cstate: ConnectionState::Valid,
                _buffer: Sstring::new(),
                _header: None,
                _payload_length: 0,
                _consumed_payload_length: 0,
                _masking_key: 0,
                _result: BuffT::empty_buffer(),
            }
        }
    }

    impl WebsocketParser {
        #[inline]
        fn dont_stop() -> Future<ConsumptionResultT> {
            make_ready_future(continue_consuming())
        }

        #[inline]
        fn stop(data: BuffT) -> Future<ConsumptionResultT> {
            make_ready_future(stop_consuming(data))
        }

        #[inline]
        fn remaining_payload_length(&self) -> u64 {
            self._payload_length - self._consumed_payload_length
        }

        // Removes mask from payload given in p.
        #[inline]
        fn remove_mask(masking_key: u32, p: &mut BuffT, n: usize) {
            let payload = p.get_write();
            let mut j: u32 = 0;
            for i in 0..n {
                // SAFETY: the buffer holds the n bytes of the payload.
                unsafe { *payload.add(i) ^= ((masking_key << (j * 8)) >> 24) as u8 };
                j = (j + 1) % 4;
            }
        }

        /// Whether the connection is still valid.
        #[inline]
        pub fn is_valid(&self) -> bool {
            self._cstate == ConnectionState::Valid
        }

        /// Whether the stream ended.
        #[inline]
        pub fn eof(&self) -> bool {
            self._cstate == ConnectionState::Closed
        }

        /// The opcode of the last frame.
        pub fn opcode(&self) -> Opcodes {
            match &self._header {
                Some(h) => Opcodes(h.opcode()),
                None => Opcodes::INVALID,
            }
        }

        /// The payload of the last frame.
        pub fn result(&mut self) -> BuffT {
            std::mem::take(&mut self._result)
        }

        /// Consumes a buffer of the stream; resolves to whether to continue or stop
        /// consuming (a frame is complete, or the connection failed).
        pub fn call(&mut self, mut data: BuffT) -> Future<ConsumptionResultT> {
            if data.size() == 0 {
                // EOF
                self._cstate = ConnectionState::Closed;
                return Self::stop(data);
            }
            if self._state == ParsingState::FlagsAndPayloadData {
                if self._buffer.size() + data.size() >= 2 {
                    // _buffer.length() is less than 2 when entering this if body due to how
                    // the rest of code is structured. The else branch will never increase
                    // _buffer.length() to >=2 and other paths to this condition will always
                    // have buffer cleared.
                    engine_assert!(self._buffer.size() < 2);

                    let hlen = self._buffer.size();
                    self._buffer.append_bytes(&data.as_slice()[..2 - hlen]);
                    data.trim_front(2 - hlen);
                    let header = FrameHeader::new(self._buffer.as_bytes());
                    self._header = Some(header);
                    self._buffer = Sstring::new();

                    // https://datatracker.ietf.org/doc/html/rfc6455#section-5.1
                    // We must close the connection if data isn't masked.
                    if header.masked() == 0
                        // RSVX must be 0
                        || (header.rsv1() | header.rsv2() | header.rsv3()) != 0
                        // Opcode must be known.
                        || !header.is_opcode_known()
                    {
                        self._cstate = ConnectionState::Error;
                        return Self::stop(data);
                    }
                    self._state = ParsingState::PayloadLengthAndMask;
                } else {
                    self._buffer.append_bytes(data.as_slice());
                    return Self::dont_stop();
                }
            }
            if self._state == ParsingState::PayloadLengthAndMask {
                let Some(header) = self._header else {
                    self._cstate = ConnectionState::Error;
                    return Self::stop(data);
                };
                let required_bytes = header.get_rest_of_header_length() as usize;
                if self._buffer.size() + data.size() >= required_bytes {
                    if self._buffer.size() < required_bytes {
                        let hlen = self._buffer.size();
                        self._buffer.append_bytes(&data.as_slice()[..required_bytes - hlen]);
                        data.trim_front(required_bytes - hlen);
                    }
                    self._payload_length = u64::from(header.length());
                    let mut input: *const u8 = self._buffer.as_bytes().as_ptr();
                    // SAFETY: `_buffer` holds at least `required_bytes`, which is
                    // `get_rest_of_header_length()`: the length field read here (two
                    // or eight bytes, by `header.length()`) plus the four-byte mask.
                    unsafe {
                        if header.length() == 126 {
                            self._payload_length = u64::from(consume_be::<u16>(&mut input));
                        } else if header.length() == 127 {
                            self._payload_length = consume_be::<u64>(&mut input);
                        }

                        self._masking_key = consume_be::<u32>(&mut input);
                    }
                    self._buffer = Sstring::new();
                    self._state = ParsingState::Payload;
                } else {
                    self._buffer.append_bytes(data.as_slice());
                    return Self::dont_stop();
                }
            }
            if self._state == ParsingState::Payload {
                if (data.size() as u64) < self.remaining_payload_length() {
                    // data has insufficient data to complete the frame - consume data.size() bytes
                    if self._result.empty() {
                        // The length comes from the client (up to 2^64-1): a length that does
                        // not fit `usize` fails this connection's consume only, with BadAlloc.
                        let Ok(len) = usize::try_from(self.remaining_payload_length()) else {
                            return make_exception_future(ExceptionPtr::new(BadAlloc));
                        };
                        self._result = match BuffT::try_new(len) {
                            Ok(b) => b,
                            Err(e) => return make_exception_future(e),
                        };
                        self._consumed_payload_length = 0;
                    }
                    // SAFETY: _result holds the whole remaining payload, more than data.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            data.get(),
                            self._result.get_write().add(self._consumed_payload_length as usize),
                            data.size(),
                        );
                    }
                    self._consumed_payload_length += data.size() as u64;
                    return Self::dont_stop();
                } else {
                    // data has sufficient data to complete the frame - consume remaining_payload_length()
                    let consumed_bytes = self.remaining_payload_length() as usize;
                    if self._result.empty() {
                        // Try to avoid memory copies in case when network packets contain one or more full
                        // websocket frames.
                        if consumed_bytes == data.size() {
                            self._result = std::mem::replace(&mut data, BuffT::new(0));
                        } else {
                            self._result = data.share();
                            self._result.trim(consumed_bytes);
                            data.trim_front(consumed_bytes);
                        }
                    } else {
                        // SAFETY: _result has room for the remaining payload.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                data.get(),
                                self._result
                                    .get_write()
                                    .add(self._consumed_payload_length as usize),
                                consumed_bytes,
                            );
                        }
                        data.trim_front(consumed_bytes);
                    }
                    Self::remove_mask(
                        self._masking_key,
                        &mut self._result,
                        self._payload_length as usize,
                    );
                    self._consumed_payload_length = 0;
                    self._state = ParsingState::FlagsAndPayloadData;
                    return Self::stop(data);
                }
            }
            self._cstate = ConnectionState::Error;
            Self::stop(data)
        }
    }
}
