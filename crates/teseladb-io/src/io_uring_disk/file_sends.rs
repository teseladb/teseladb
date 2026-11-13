//! Frames for other nodes whose last bytes come from a file of the core's disk (012 FR-026): the
//! ring reads the bytes into registered buffers and sends the frame's head and those buffers with
//! one `IORING_OP_SENDMSG_ZC`. The bytes go from the disk to the buffers and from the buffers to
//! the network card without the process copying them; the buffers come back to the reserve with
//! the send's notification.
//!
//! A send waits in its queue until [`MAX_FILE_SENDS`] are not running and the reserve has its
//! buffers beyond the log's (012 FR-023). One whose bytes cannot be read whole sends nothing: its
//! owner learns it from [`IoUringDisk::take_unread`] and answers otherwise.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

use teseladb_sys::uring::{Completion, Op, Part};
use teseladb_raftlog::FileId;

use super::{IoUringDisk, LOG_BUFFERS};
use crate::buffers::{BUFFER_BYTES, BUFFERS_PER_CORE};

/// File sends running at once on a core: each holds up to four buffers of the reserve.
pub const MAX_FILE_SENDS: usize = 2;

/// The bit that sets the identifiers of file sends apart from those of the ring's operations,
/// which [`IoUringDisk::send_zc`] returns and the ring numbers from 1.
const FILE_SEND_ID: u64 = 1 << 63;

/// A ring operation of a file send: the read into its part `i`, or the send.
#[derive(Clone, Copy, Debug)]
enum Step {
    Read(usize),
    Send,
}

#[derive(Debug)]
enum Stage {
    /// Reads in flight.
    Reading,
    /// Read whole; the send waits for room in the ring.
    Ready,
    /// The send is in flight, with the result of its first completion.
    Sending(Option<io::Result<u32>>),
}

/// One part of a send's bytes: a buffer of the reserve, the bytes it takes and those read.
#[derive(Debug)]
struct Piece {
    buf: u16,
    want: u32,
    got: u32,
}

#[derive(Debug)]
struct Job {
    id: u64,
    /// The socket, a descriptor of its own: a connection that closes meanwhile cannot hand its
    /// number to another socket under the send.
    sock: OwnedFd,
    head: Vec<u8>,
    /// Bytes of the head, which goes to the ring with the send.
    head_len: u64,
    file: FileId,
    offset: u64,
    len: u32,
    pieces: Vec<Piece>,
    reads: usize,
    failed: bool,
    /// Its owner no longer waits for it: nothing is sent and nothing reported.
    cancelled: bool,
    stage: Stage,
}

/// The file sends of a core's ring.
#[derive(Debug, Default)]
pub(super) struct FileSends {
    queued: VecDeque<Job>,
    running: Vec<Job>,
    by_ring: HashMap<u64, (u64, Step)>,
    unread: Vec<u64>,
    next: u64,
}

impl FileSends {
    /// Whether sends that read nothing wait to be taken.
    pub(super) fn has_unread(&self) -> bool {
        !self.unread.is_empty()
    }

    /// Drops the sends not started.
    pub(super) fn drop_queued(&mut self) {
        self.queued.clear();
    }

    /// Whether any send is running (the queued ones can be dropped).
    pub(super) fn running(&self) -> bool {
        !self.running.is_empty()
    }
}

/// Buffers a send of `len` bytes takes.
fn buffers_for(len: u32) -> usize {
    (len as usize).div_ceil(BUFFER_BYTES).max(1)
}

impl IoUringDisk {
    /// Queues a frame for socket `sock` made of `head` and `len` bytes of open file `file` at
    /// `offset` (012 FR-026), and returns its identifier. Its result comes from
    /// [`IoUringDisk::take_sent`] once the kernel no longer reads its buffers, with the bytes of
    /// head and file sent, or from [`IoUringDisk::take_unread`] if the bytes could not be read
    /// whole and nothing went. The socket's owner keeps the order: one send at a time.
    pub fn send_file(
        &mut self,
        sock: OwnedFd,
        head: Vec<u8>,
        file: FileId,
        offset: u64,
        len: u32,
    ) -> u64 {
        let id = FILE_SEND_ID | self.file_sends.next;
        self.file_sends.next += 1;
        let job = Job {
            id,
            sock,
            head_len: head.len() as u64,
            head,
            file,
            offset,
            len,
            pieces: Vec::new(),
            reads: 0,
            failed: false,
            cancelled: false,
            stage: Stage::Reading,
        };
        self.file_sends.queued.push_back(job);
        self.start_file_sends();
        let _ = self.ring.submit(0);
        id
    }

    /// Forgets send `id`: one not started is dropped, and one running sends nothing more and
    /// reports nothing. A send already handed to the kernel goes out.
    pub fn cancel_file_send(&mut self, id: u64) {
        self.file_sends.queued.retain(|j| j.id != id);
        if let Some(j) = self.file_sends.running.iter_mut().find(|j| j.id == id) {
            j.cancelled = true;
        }
    }

    /// The file sends that read nothing whole since the last call, and so sent nothing.
    pub fn take_unread(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.file_sends.unread)
    }

    /// Starts the queued sends that fit and hands the ready ones to the kernel.
    pub(super) fn start_file_sends(&mut self) {
        let ready: Vec<usize> = (0..self.file_sends.running.len())
            .filter(|&i| matches!(self.file_sends.running[i].stage, Stage::Ready))
            .collect();
        for i in ready.into_iter().rev() {
            self.push_send(i);
        }
        while self.file_sends.running.len() < MAX_FILE_SENDS {
            let Some(front) = self.file_sends.queued.front() else { return };
            let need = buffers_for(front.len);
            // More than the reserve leaves the data is never there: it reads nothing.
            if need > usize::from(BUFFERS_PER_CORE) - LOG_BUFFERS
                || !self.files.contains_key(&front.file)
            {
                let job = self.file_sends.queued.pop_front().expect("front");
                self.file_sends.unread.push(job.id);
                continue;
            }
            if self.ensure_buffers().is_err() {
                let job = self.file_sends.queued.pop_front().expect("front");
                self.file_sends.unread.push(job.id);
                continue;
            }
            if self.pool.available() < need + LOG_BUFFERS || (self.ring.space() as usize) < need {
                return;
            }
            let mut job = self.file_sends.queued.pop_front().expect("front");
            let mut left = job.len;
            for _ in 0..need {
                let want = left.min(BUFFER_BYTES as u32);
                left -= want;
                let buf = self.pool.take().expect("counted above");
                job.pieces.push(Piece { buf, want, got: 0 });
            }
            self.file_sends.running.push(job);
            let i = self.file_sends.running.len() - 1;
            for p in 0..need {
                self.push_read(i, p);
            }
            self.settle_file_send(i);
        }
    }

    /// Reads what is missing of piece `p` of running send `i`.
    fn push_read(&mut self, i: usize, p: usize) {
        let job = &mut self.file_sends.running[i];
        let piece = &job.pieces[p];
        if piece.want == 0 {
            return;
        }
        let at = job.offset + (p * BUFFER_BYTES) as u64 + u64::from(piece.got);
        let n = piece.want - piece.got;
        let Some(f) = self.files.get(&job.file) else {
            job.failed = true;
            return;
        };
        let op = Op::ReadFixed {
            target: f.target(at, u64::from(n)),
            buf: piece.buf,
            at: piece.got,
            len: n,
            offset: at,
        };
        match self.ring.push(op) {
            Ok(ring_id) => {
                job.reads += 1;
                self.file_sends.by_ring.insert(ring_id, (job.id, Step::Read(p)));
            }
            Err(_) => job.failed = true,
        }
    }

    /// Hands the send of running send `i`, read whole, to the kernel; it stays ready if the ring
    /// is full.
    fn push_send(&mut self, i: usize) {
        let job = &mut self.file_sends.running[i];
        if job.cancelled {
            return self.end_file_send(i);
        }
        let bufs = job.pieces.iter().filter(|p| p.want > 0).map(|p| (p.buf, p.want)).collect();
        let head = Part::new(std::mem::take(&mut job.head));
        let op = Op::SendMsgZc { fd: job.sock.as_raw_fd(), head, bufs };
        match self.ring.push(op) {
            Ok(ring_id) => {
                job.stage = Stage::Sending(None);
                self.file_sends.by_ring.insert(ring_id, (job.id, Step::Send));
            }
            Err((Op::SendMsgZc { head, .. }, _)) => job.head = head.bytes().to_vec(),
            Err((op, why)) => unreachable!("the ring gave back another operation: {op:?} ({why})"),
        }
    }

    /// After a read of running send `i` was pushed or completed: with none in flight, the send
    /// goes or, with a piece missing, it ends unread.
    fn settle_file_send(&mut self, i: usize) {
        let job = &mut self.file_sends.running[i];
        if job.reads > 0 {
            return;
        }
        if job.failed || job.cancelled {
            if !job.cancelled {
                self.file_sends.unread.push(job.id);
            }
            return self.end_file_send(i);
        }
        job.stage = Stage::Ready;
        self.push_send(i);
    }

    /// Ends running send `i`: its buffers go back to the reserve.
    fn end_file_send(&mut self, i: usize) {
        let job = self.file_sends.running.swap_remove(i);
        for p in job.pieces {
            self.pool.give(p.buf);
        }
    }

    /// Takes completion `c` if it is of a file send; gives it back otherwise.
    pub(super) fn file_send_completed(&mut self, c: Completion) -> Option<Completion> {
        let Some(&(id, step)) = self.file_sends.by_ring.get(&c.id) else { return Some(c) };
        if !c.more {
            self.file_sends.by_ring.remove(&c.id);
        }
        let i = self.file_sends.running.iter().position(|j| j.id == id).expect("a running send");
        match step {
            Step::Read(p) => {
                let job = &mut self.file_sends.running[i];
                job.reads -= 1;
                match c.result {
                    // The end of the file before the bytes the logic counted: they are not there.
                    Ok(0) | Err(_) => job.failed = true,
                    Ok(n) => {
                        let piece = &mut job.pieces[p];
                        piece.got = piece.got.saturating_add(n);
                        if piece.got < piece.want && !job.failed {
                            self.push_read(i, p);
                        }
                    }
                }
                self.settle_file_send(i);
            }
            Step::Send => {
                let Stage::Sending(first) = &mut self.file_sends.running[i].stage else {
                    unreachable!("a send completes in its stage")
                };
                if !c.notification {
                    *first = Some(c.result);
                }
                if !c.more {
                    let job = &mut self.file_sends.running[i];
                    let Stage::Sending(first) = &mut job.stage else { unreachable!() };
                    let result = first.take().unwrap_or(Ok(0));
                    let whole = job.head_len + u64::from(job.len);
                    let went = matches!(result, Ok(n) if u64::from(n) == whole);
                    self.zero_copy_sends += u64::from(went);
                    if !job.cancelled {
                        self.peer_sent.push((id, result));
                    }
                    self.end_file_send(i);
                }
            }
        }
        None
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};

    use teseladb_raftlog::{Disk, IoDone, IoOp};
    use teseladb_types::Bytes;

    use super::*;

    fn disk(name: &str) -> (IoUringDisk, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("teseladb-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (IoUringDisk::open(&dir, true).unwrap(), dir)
    }

    fn ok(d: &mut IoUringDisk, op: IoOp) {
        let r = d.execute(op);
        assert!(matches!(r, IoDone::Ok(_)), "{r:?}");
    }

    /// A file of `len` bytes open as `FileId(1)`.
    fn file(d: &mut IoUringDisk, len: usize) -> Vec<u8> {
        let data: Vec<u8> = (0..len).map(|i| (i % 249) as u8).collect();
        ok(d, IoOp::Create { file: FileId(1), path: "f".into(), prealloc: 0 });
        let write = IoOp::WriteAt {
            file: FileId(1),
            offset: 0,
            data: Bytes::from_vec(data.clone()),
            batch: 0,
        };
        ok(d, write);
        data
    }

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    fn sock(s: &TcpStream) -> OwnedFd {
        OwnedFd::from(s.try_clone().unwrap())
    }

    /// Turns the ring until no file send runs.
    fn settle(d: &mut IoUringDisk) {
        while d.file_sends.running() {
            d.ring.submit(1).unwrap();
            d.reap(&mut Vec::new()).unwrap();
        }
    }

    #[test]
    fn a_file_s_bytes_go_behind_the_head_and_the_buffers_come_back() {
        let (mut d, dir) = disk("whole");
        let len = 3 * BUFFER_BYTES + 5_000;
        let data = file(&mut d, len);
        let (mut c, s) = pair();
        let reader = std::thread::spawn(move || {
            let mut got = vec![0u8; 4 + len];
            c.read_exact(&mut got).unwrap();
            got
        });
        let id = d.send_file(sock(&s), b"head".to_vec(), FileId(1), 0, len as u32);
        assert_eq!(id & FILE_SEND_ID, FILE_SEND_ID, "apart from the ring's identifiers");
        settle(&mut d);
        let sent = d.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].0, *sent[0].1.as_ref().unwrap() as usize), (id, 4 + len));
        assert_eq!(d.zero_copy_sends(), 1);
        assert_eq!(d.pool.available(), usize::from(BUFFERS_PER_CORE));
        let got = reader.join().unwrap();
        assert_eq!(&got[..4], b"head");
        assert!(got[4..] == data[..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bytes_that_cannot_be_read_send_nothing_and_say_so() {
        let (mut d, dir) = disk("unread");
        file(&mut d, 10_000);
        let (_c, s) = pair();
        // Not open, past the end of the file, and more than the reserve leaves the data.
        let absent = d.send_file(sock(&s), Vec::new(), FileId(9), 0, 10);
        assert!(d.ready(), "a result no completion signals");
        let past = d.send_file(sock(&s), Vec::new(), FileId(1), 9_990, 100);
        let huge = (usize::from(BUFFERS_PER_CORE) - LOG_BUFFERS + 1) * BUFFER_BYTES;
        let big = d.send_file(sock(&s), Vec::new(), FileId(1), 0, huge as u32);
        settle(&mut d);
        let mut unread = d.take_unread();
        unread.sort_unstable();
        assert_eq!(unread, [absent, past, big]);
        assert!(d.take_sent().is_empty() && !d.ready());
        assert_eq!(d.pool.available(), usize::from(BUFFERS_PER_CORE));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_send_reports_nothing_and_gives_its_buffers_back() {
        let (mut d, dir) = disk("cancel");
        file(&mut d, 2 * BUFFER_BYTES);
        let (mut c, s) = pair();
        // The peer reads all it gets, so that the send that goes is not held by a full socket.
        let reader = std::thread::spawn(move || {
            let mut all = Vec::new();
            c.read_to_end(&mut all).unwrap();
            all.len()
        });
        // Two run and a third waits; the running one and the waiting one are cancelled.
        let a = d.send_file(sock(&s), Vec::new(), FileId(1), 0, 2 * BUFFER_BYTES as u32);
        let b = d.send_file(sock(&s), Vec::new(), FileId(1), 0, BUFFER_BYTES as u32);
        let w = d.send_file(sock(&s), Vec::new(), FileId(1), 0, 100);
        assert_eq!(d.file_sends.queued.len(), 1);
        d.cancel_file_send(w);
        assert!(d.file_sends.queued.is_empty());
        d.cancel_file_send(a);
        settle(&mut d);
        let sent: Vec<u64> = d.take_sent().into_iter().map(|(id, _)| id).collect();
        assert_eq!(sent, [b], "only the send that was not cancelled");
        assert!(d.take_unread().is_empty());
        assert_eq!(d.pool.available(), usize::from(BUFFERS_PER_CORE));
        d.drain();
        drop(s);
        assert_eq!(reader.join().unwrap(), BUFFER_BYTES, "only the send of `b` reached the peer");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
