//! A forwarded answer whose body ends with bytes of a file of the core's disk (012 FR-026): the
//! pieces of a range's files that a replica fetches (007). The logic says which bytes of which
//! open file end the body ([`FileSend`]) instead of reading them; on a core with an io_uring disk
//! and plaintext links, the runtime reads them into the ring's registered buffers and sends the
//! frame's head and those buffers with one `IORING_OP_SENDMSG_ZC`, so the bytes never pass
//! through memory the process copies. A core told otherwise ([`crate::runtime::CoreLogic`]'s
//! `file_sends`) reads the piece and answers it whole, as before.

use teseladb_keys::Hlc;
use teseladb_raftlog::FileId;
use teseladb_types::{Bytes, Encoder};

use crate::peer::{CLOCKED_FORWARD_VERSION, LEN_COVERED, MAX_FRAME_LEN, PeerMsg};

/// Most file bytes one [`FileSend`] carries: four of a core's registered buffers, the size of a
/// range file's piece.
pub const MAX_FILE_SEND: u32 = 4 * crate::buffers::BUFFER_BYTES as u32;

/// A `ForwardResp` for request `req` whose body is `head` followed by `len` bytes of `file` at
/// `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSend {
    /// The forwarded request it answers.
    pub req: u64,
    /// The sender's HLC, as [`PeerMsg::ForwardResp`] carries it.
    pub hlc: Option<Hlc>,
    /// The body's bytes before the file's.
    pub head: Vec<u8>,
    /// An open file of the core's disk.
    pub file: FileId,
    /// Where the bytes start in the file.
    pub offset: u64,
    /// How many bytes, at most [`MAX_FILE_SEND`]: the file holds all of them.
    pub len: u32,
    /// The body sent instead when the bytes cannot be read.
    pub fallback: Bytes,
}

impl FileSend {
    /// The frame's bytes before the file's, for a node that speaks protocol `version`: what
    /// [`crate::peer::encode_frame_for`] writes for [`FileSend::with_data`] up to the data.
    /// `None` when the frame would pass [`MAX_FRAME_LEN`].
    pub fn frame_head(&self, version: u16) -> Option<Vec<u8>> {
        let hlc = self.hlc.filter(|_| version >= CLOCKED_FORWARD_VERSION);
        let kind = PeerMsg::ForwardResp { req: self.req, hlc, body: Bytes::default() }.kind();
        let body = self.head.len() as u64 + u64::from(self.len);
        let mut e = Encoder::new();
        e.u64(self.req);
        if let Some(h) = hlc {
            e.u64(h.physical).u32(h.logical);
        }
        e.var(body).raw(&self.head);
        let payload = e.len() as u64 + u64::from(self.len);
        if payload + LEN_COVERED as u64 > MAX_FRAME_LEN as u64 {
            return None;
        }
        let mut out = Vec::with_capacity(8 + e.len());
        out.extend_from_slice(&((payload + LEN_COVERED as u64) as u32).to_le_bytes());
        out.extend_from_slice(&[kind, 0, 0, 0]);
        out.extend_from_slice(e.as_slice());
        Some(out)
    }

    /// The answer with `data` as the file's bytes.
    pub fn with_data(&self, data: &[u8]) -> PeerMsg {
        let body = Bytes::from_vec([&self.head[..], data].concat());
        PeerMsg::ForwardResp { req: self.req, hlc: self.hlc, body }
    }

    /// The answer sent when the bytes cannot be read.
    pub fn fallback(&self) -> PeerMsg {
        PeerMsg::ForwardResp { req: self.req, hlc: self.hlc, body: self.fallback.clone() }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::peer::{decode_frame, encode_frame_for};

    fn send(hlc: Option<Hlc>, head: &[u8], len: u32) -> FileSend {
        FileSend {
            req: 41,
            hlc,
            head: head.to_vec(),
            file: FileId(3),
            offset: 1 << 20,
            len,
            fallback: Bytes::from_vec(vec![0]),
        }
    }

    #[test]
    fn the_head_and_the_file_bytes_make_the_frame_of_the_whole_answer() {
        let clock = Some(Hlc { physical: 9, logical: 2 });
        for (hlc, version) in [(clock, 7), (clock, 6), (None, 7)] {
            for len in [0u32, 1, 200, 300_000] {
                let s = send(hlc, b"chunk head", len);
                let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                let mut whole = Vec::new();
                encode_frame_for(&s.with_data(&data), false, version, &mut whole).unwrap();
                let head = s.frame_head(version).unwrap();
                assert_eq!([head, data].concat(), whole, "{hlc:?} v{version} {len}");
                let (msg, used) = decode_frame(&whole).unwrap().unwrap();
                assert_eq!(used, whole.len());
                assert!(matches!(msg, PeerMsg::ForwardResp { req: 41, .. }));
            }
        }
    }

    #[test]
    fn a_frame_past_the_limit_has_no_head_and_the_fallback_carries_its_body() {
        let s = send(None, b"", MAX_FRAME_LEN as u32);
        assert_eq!(s.frame_head(7), None);
        let PeerMsg::ForwardResp { req, body, .. } = s.fallback() else { panic!() };
        assert_eq!((req, body.as_ref()), (41, &[0u8][..]));
    }
}
