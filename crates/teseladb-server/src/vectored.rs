//! Writing a reply in pieces with vectored writes (`writev`, 006 FR-040): the stored documents of
//! a reply go from their own buffers, and only what the socket did not take is copied, to go out
//! when it can take more.

use std::io::{self, ErrorKind, IoSlice, Write};

/// Writes `pieces` in order to the non-blocking `w` until all of them went or it would block:
/// the bytes it took, and the ones it did not in one buffer.
///
/// # Errors
///
/// The error of a write other than `WouldBlock`, or `WriteZero` when it takes nothing.
pub(crate) fn write_pieces(w: &mut impl Write, pieces: &[&[u8]]) -> io::Result<(usize, Vec<u8>)> {
    let mut first = 0;
    let mut offset = 0;
    let mut written = 0;
    while first < pieces.len() {
        let slices: Vec<IoSlice<'_>> = std::iter::once(&pieces[first][offset..])
            .chain(pieces[first + 1..].iter().copied())
            .map(IoSlice::new)
            .collect();
        let n = match w.write_vectored(&slices) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        };
        written += n;
        let mut left = n;
        while first < pieces.len() && left >= pieces[first].len() - offset {
            left -= pieces[first].len() - offset;
            first += 1;
            offset = 0;
        }
        offset += left;
    }
    let mut rest = Vec::new();
    if first < pieces.len() {
        rest.extend_from_slice(&pieces[first][offset..]);
        for p in &pieces[first + 1..] {
            rest.extend_from_slice(p);
        }
    }
    Ok((written, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A socket that takes at most `per_call` bytes a call, `budget` in all, then would block;
    /// it counts its calls.
    struct Socket {
        taken: Vec<u8>,
        per_call: usize,
        budget: usize,
        calls: usize,
        fail: Option<ErrorKind>,
    }

    impl Write for Socket {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.write_vectored(&[IoSlice::new(buf)])
        }

        fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
            self.calls += 1;
            if let Some(kind) = self.fail {
                return Err(kind.into());
            }
            if self.budget == 0 {
                return Err(ErrorKind::WouldBlock.into());
            }
            let mut n = 0;
            for b in bufs {
                let take = b.len().min(self.per_call - n).min(self.budget);
                self.taken.extend_from_slice(&b[..take]);
                n += take;
                self.budget -= take;
                if n == self.per_call || self.budget == 0 {
                    break;
                }
            }
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn socket(per_call: usize, budget: usize) -> Socket {
        Socket { taken: Vec::new(), per_call, budget, calls: 0, fail: None }
    }

    #[test]
    fn every_piece_goes_in_order_in_one_call_when_the_socket_takes_them() {
        let pieces: [&[u8]; 3] = [b"head", b"", b"stored document"];
        let mut s = socket(usize::MAX, usize::MAX);
        let (n, rest) = write_pieces(&mut s, &pieces).unwrap();
        assert_eq!((n, rest.as_slice()), (19, &b""[..]));
        assert_eq!(s.taken, b"headstored document");
        assert_eq!(s.calls, 1);
    }

    #[test]
    fn partial_writes_resume_inside_a_piece_and_what_was_not_taken_is_kept() {
        let pieces: [&[u8]; 3] = [b"abc", b"defgh", b"ij"];
        for per_call in 1..=10 {
            for budget in 0..=10 {
                let mut s = socket(per_call, budget);
                let (n, rest) = write_pieces(&mut s, &pieces).unwrap();
                assert_eq!(n, budget);
                assert_eq!(s.taken, b"abcdefghij"[..budget]);
                assert_eq!(rest, b"abcdefghij"[budget..]);
            }
        }
    }

    #[test]
    fn a_failed_socket_or_one_that_takes_nothing_is_an_error() {
        let pieces: [&[u8]; 1] = [b"x"];
        let mut s = socket(1, 1);
        s.fail = Some(ErrorKind::BrokenPipe);
        assert_eq!(write_pieces(&mut s, &pieces).unwrap_err().kind(), ErrorKind::BrokenPipe);
        let mut s = socket(0, 1);
        assert_eq!(write_pieces(&mut s, &pieces).unwrap_err().kind(), ErrorKind::WriteZero);
    }
}
