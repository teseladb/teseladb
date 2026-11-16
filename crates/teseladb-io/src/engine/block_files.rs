//! Blocks of the files of the LSM read by code that waits (005 FR-028, 023 FR-010, FR-029): the
//! client workers that serve a read outside the cores read a block the shared cache lacks from
//! its SST file themselves.
//!
//! In an engine thread a read goes through the engine's file API, as the cores' own reads of
//! their files do (and as the original engine's users read their sstables): the file is opened
//! with `open_file_dma` (on the engine's syscall thread) and the block read with a DMA read on the
//! shard's I/O queue (io_uring), and the engine thread parks until it ends, so the shard runs its
//! other work meanwhile. Whether the reads bypass the kernel's page cache is the engine's mode of
//! its files (`--direct-io`: `off` keeps the page cache, `on` and `auto` read directly where the
//! file system takes it).
//!
//! The open files are the shard's: a table per shard of the files the engine threads of that
//! shard opened, each with the number of readers that hold it. A reader ([`BlockFiles`]) holds
//! each file it reads until [`BlockFiles::release`] (the end of an operation), and the last one
//! to let go closes it, so a file a compaction replaced and deleted does not keep its disk space
//! once no operation reads it (H-052). A reader that moved to another shard gives its files back
//! to the shard that holds them with a message.
//!
//! A thread that is not a shard (a command of the command line, the tests, the benchmarks' reader
//! threads) has nothing to block: it reads with `pread` on files of its own. A shard's task that
//! is not an engine thread cannot wait, and its read fails with
//! [`io::ErrorKind::WouldBlock`] (the read then goes to the core).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::File as StdFile;
use std::io;
use std::path::Path;

use teseladb::engine::core::file::{File, OpenFlags, open_file_dma};
use teseladb::engine::core::future::ExceptionPtr;

use super::wait;

/// Files a reader holds at most; past it, it lets go of them all before it opens another.
pub const MAX_HELD: usize = 64;

/// A file of a shard's table and the readers that hold it.
struct Shared {
    file: File,
    holders: u32,
}

thread_local! {
    /// The files this shard holds open for its readers, by path.
    static OPEN: RefCell<HashMap<String, Shared>> = RefCell::new(HashMap::new());
}

/// The files one reader (a client worker) reads blocks from, between two releases.
#[derive(Debug, Default)]
pub struct BlockFiles {
    /// The files it holds in a shard's table: shard and path.
    held: Vec<(u32, String)>,
    /// The files it opened itself, on a thread that is not a shard.
    plain: BTreeMap<String, StdFile>,
}

impl BlockFiles {
    /// The bytes of file `path` under `dir` from `offset`, `len` of them or fewer at the end of
    /// the file. In an engine thread the read parks the thread, never its shard.
    ///
    /// # Errors
    ///
    /// The error of opening or reading the file; [`io::ErrorKind::WouldBlock`] on a shard outside
    /// an engine thread, which cannot wait.
    pub fn read(&mut self, dir: &Path, path: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let full = dir.join(path).to_string_lossy().into_owned();
        if let Some(shard) = wait::engine_thread_shard() {
            return self.read_on_shard(shard, full, offset, len);
        }
        if wait::this_shard().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a shard's task cannot wait for a file read",
            ));
        }
        self.read_plain(full, offset, len)
    }

    fn read_on_shard(
        &mut self,
        shard: u32,
        full: String,
        offset: u64,
        len: usize,
    ) -> io::Result<Vec<u8>> {
        if !self.held.iter().any(|(s, p)| *s == shard && *p == full) {
            if self.held.len() >= MAX_HELD {
                self.release();
            }
            acquire(&full)?;
            self.held.push((shard, full.clone()));
        }
        read_shared(&full, offset, len)
    }

    fn read_plain(&mut self, full: String, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if !self.plain.contains_key(&full) {
            if self.plain.len() >= MAX_HELD {
                self.plain.clear();
            }
            let file = StdFile::open(&full)?;
            self.plain.insert(full.clone(), file);
        }
        let Some(file) = self.plain.get(&full) else {
            return Err(io::Error::other("the file was just opened"));
        };
        let mut buf = vec![0; len];
        let got = crate::files::read_up_to(file, &mut buf, offset)?;
        buf.truncate(got);
        Ok(buf)
    }

    /// The files this reader holds open.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held.len() + self.plain.len()
    }

    /// Lets go of every file this reader holds: a shard's file is closed once no reader holds
    /// it. The files of another shard go back to it with a message.
    pub fn release(&mut self) {
        self.plain.clear();
        let here = wait::this_shard();
        let mut elsewhere: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for (shard, path) in std::mem::take(&mut self.held) {
            if Some(shard) == here {
                let_go(&path);
            } else {
                elsewhere.entry(shard).or_default().push(path);
            }
        }
        for (shard, paths) in elsewhere {
            // A shard that is gone (the engine stopped) took its files with it.
            let _ = wait::post(shard, Box::new(move || paths.iter().for_each(|p| let_go(p))));
        }
    }
}

impl Drop for BlockFiles {
    fn drop(&mut self) {
        self.release();
    }
}

/// The files this shard holds open for its readers (0 on a thread that is not a shard).
#[must_use]
pub fn shard_open_files() -> usize {
    OPEN.with(|o| o.borrow().len())
}

/// Holds file `full` in this shard's table, opening it with the engine when it is not there; the
/// engine thread parks while it opens.
fn acquire(full: &str) -> io::Result<()> {
    if OPEN.with(|o| o.borrow_mut().get_mut(full).map(|s| s.holders += 1).is_some()) {
        return Ok(());
    }
    let file = open_file_dma(full, OpenFlags::RO).get().map_err(|e| io_error(full, &e))?;
    // Another engine thread of the shard may have opened it while this one waited: the first
    // open is kept and this one is closed as it drops.
    OPEN.with(|o| {
        o.borrow_mut()
            .entry(full.to_owned())
            .and_modify(|s| s.holders += 1)
            .or_insert(Shared { file, holders: 1 });
    });
    Ok(())
}

/// Lets go of one hold of file `full` of this shard's table, closing it with the last.
fn let_go(full: &str) {
    let last = OPEN.with(|o| {
        let mut o = o.borrow_mut();
        let s = o.get_mut(full)?;
        s.holders = s.holders.saturating_sub(1);
        if s.holders > 0 {
            return None;
        }
        o.remove(full)
    });
    // Closed out of the table's borrow: a reader still reading has its own handle.
    drop(last);
}

/// `len` bytes at `offset` of file `full` of this shard's table, or fewer at its end, with a DMA
/// read on the shard's I/O queue; the engine thread parks until it ends.
fn read_shared(full: &str, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    // A handle of its own, so the table is not borrowed while the thread parks.
    let Some(mut file) = OPEN.with(|o| o.borrow().get(full).map(|s| s.file.clone())) else {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("{full}: not open")));
    };
    let buf = teseladb_sys::dma::read(&mut file, offset, len)
        .get()
        .map_err(|e| io_error(full, &e))?;
    let got = buf.size().min(len);
    Ok(buf.as_slice()[..got].to_vec())
}

/// The I/O error of `e`, naming file `full`.
fn io_error(full: &str, e: &ExceptionPtr) -> io::Error {
    match e.downcast_ref::<io::Error>() {
        Some(io) => io::Error::new(io.kind(), format!("{full}: {io}")),
        None => io::Error::other(format!("{full}: {e:?}")),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_thread_that_is_not_a_shard_reads_with_files_of_its_own() {
        let file = std::env::current_exe().expect("this test's file");
        let (dir, name) = (file.parent().unwrap(), file.file_name().unwrap().to_str().unwrap());
        let whole = std::fs::read(&file).unwrap();
        let mut files = BlockFiles::default();
        assert_eq!(files.read(dir, name, 3, 100).unwrap(), whole[3..103]);
        assert_eq!(files.read(dir, name, 1, 7).unwrap(), whole[1..8]);
        assert_eq!(files.held(), 1, "one file, opened once");
        let end = whole.len() as u64;
        assert_eq!(files.read(dir, name, end - 4, 100).unwrap(), whole[whole.len() - 4..]);
        assert!(files.read(dir, "none", 0, 1).is_err());
        files.release();
        assert_eq!(files.held(), 0);
        assert_eq!(shard_open_files(), 0, "no shard's table here");
    }
}
