//! Files on a block device and on XFS (the original's file.cc `blockdev_file_impl` and the
//! XFS branch of `make_file_impl`), over a loop device made for the test: a block device
//! reports its size with BLKGETSIZE64, ignores truncation, discards ranges with BLKDISCARD and
//! reads back what was written; a file on XFS takes its DMA alignments from XFS_IOC_DIOINFO
//! (memory: `d_mem`, reads: `d_miniosz`, writes: at least the block size, overwrites:
//! `d_miniosz` on kernels from 5.12) and is append-challenged: it grows and keeps its logical
//! size across close. Needs root, `losetup` and `mkfs.xfs`, as in the privileged containers
//! the battery runs in; without them each case says why it did nothing.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use teseladb::engine::core::file::{File, open_file_dma};
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine_test_case;

/// Runs `cmd args...`; its standard output, or `None` when it fails.
fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        eprintln!("{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A loop device over a sparse image of `mib` MiB, detached (and the image removed) on drop.
struct Loop {
    dev: String,
    image: PathBuf,
    mount: Option<PathBuf>,
}

impl Loop {
    fn new(tag: &str, mib: u64) -> Option<Loop> {
        let image = std::env::temp_dir().join(format!("cov2-{tag}-{}.img", std::process::id()));
        let f = std::fs::File::create(&image).ok()?;
        f.set_len(mib << 20).ok()?;
        drop(f);
        match run("losetup", &["-f", "--show", image.to_str()?]) {
            Some(dev) => Some(Loop { dev, image, mount: None }),
            None => {
                let _ = std::fs::remove_file(&image);
                None
            }
        }
    }

    /// Makes an XFS file system on the device and mounts it; returns the mount point.
    fn mount_xfs(&mut self) -> Option<PathBuf> {
        run("mkfs.xfs", &["-q", "-f", &self.dev])?;
        let dir = self.image.with_extension("mnt");
        std::fs::create_dir_all(&dir).ok()?;
        run("mount", &[&self.dev, dir.to_str()?])?;
        self.mount = Some(dir.clone());
        Some(dir)
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        if let Some(m) = self.mount.take() {
            let _ = run("umount", &[m.to_str().unwrap_or_default()]);
            let _ = std::fs::remove_dir(&m);
        }
        let _ = run("losetup", &["-d", &self.dev]);
        let _ = std::fs::remove_file(&self.image);
    }
}

/// An aligned buffer of `len` bytes of `fill`.
fn filled(len: usize, fill: u8) -> TemporaryBuffer<u8> {
    let mut b = TemporaryBuffer::<u8>::aligned(4096, len);
    // SAFETY: the buffer has `len` writable bytes.
    unsafe { std::ptr::write_bytes(b.get_write(), fill, len) };
    b
}

fn write_at(f: &mut File, pos: u64, buf: &TemporaryBuffer<u8>) -> Result<usize, ExceptionPtr> {
    // SAFETY: the buffer outlives the write, resolved here.
    unsafe { f.dma_write(pos, buf.get(), buf.size(), std::ptr::null_mut()) }.get()
}

fn read_at(f: &mut File, pos: u64, len: usize) -> Result<TemporaryBuffer<u8>, ExceptionPtr> {
    // SAFETY: the file outlives the read, resolved here.
    unsafe { f.dma_read_exactly::<u8>(pos, len, std::ptr::null_mut()) }.get()
}

/// XFS_IOC_DIOINFO's answer for the file at `path`: (d_mem, d_miniosz).
fn dioinfo(path: &Path) -> Option<(u32, u32)> {
    #[repr(C)]
    #[derive(Default)]
    struct Dioattr {
        d_mem: u32,
        d_miniosz: u32,
        d_maxiosz: u32,
    }
    unsafe extern "C" {
        fn ioctl(fd: i32, req: u64, ...) -> i32;
    }
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(path).ok()?;
    let mut da = Dioattr::default();
    // XFS_IOC_DIOINFO = _IOR('X', 30, struct dioattr)
    let req = (2u64 << 30)
        | ((std::mem::size_of::<Dioattr>() as u64) << 16)
        | (u64::from(b'X') << 8)
        | 30;
    // SAFETY: the ioctl writes one dioattr.
    let r = unsafe { ioctl(f.as_raw_fd(), req, &mut da as *mut Dioattr) };
    (r == 0).then_some((da.d_mem, da.d_miniosz))
}

/// Whether the running kernel is at least `major.minor`.
fn kernel_at_least(major: u32, minor: u32) -> bool {
    let rel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let mut it = rel.trim().split(['.', '-']).map(|p| p.parse::<u32>().unwrap_or(0));
    let (a, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (a, b) >= (major, minor)
}

engine_test_case!(a_block_device_file_sizes_discards_and_round_trips, {
    async_(|| -> Result<(), ExceptionPtr> {
        let Some(lo) = Loop::new("blk", 16) else {
            eprintln!("no loop device (needs root and losetup): nothing tested");
            return Ok(());
        };
        let mut f = open_file_dma(&lo.dev, OpenFlags::RW).get()?;
        assert_eq!(f.size().get()?, 16 << 20, "BLKGETSIZE64");
        let data = filled(8192, 0x5a);
        assert_eq!(write_at(&mut f, 1 << 20, &data)?, 8192);
        f.flush().get()?;
        assert_eq!(read_at(&mut f, 1 << 20, 8192)?.as_slice(), data.as_slice());
        // Truncation does nothing on a device.
        f.truncate(4096).get()?;
        assert_eq!(f.size().get()?, 16 << 20);
        // A discarded range of a loop device reads back as zeros.
        f.discard(1 << 20, 4096).get()?;
        let after = read_at(&mut f, 1 << 20, 8192)?;
        assert!(after.as_slice()[..4096].iter().all(|&b| b == 0), "discarded");
        assert!(after.as_slice()[4096..].iter().all(|&b| b == 0x5a), "kept");
        let st = f.stat().get()?;
        assert_eq!(st.st_mode & 0o170000, 0o060000, "a block device");
        f.close().get()?;
        Ok(())
    })
});

engine_test_case!(an_xfs_file_takes_its_alignments_from_dioinfo_and_grows, {
    async_(|| -> Result<(), ExceptionPtr> {
        let Some(mut lo) = Loop::new("xfs", 320) else {
            eprintln!("no loop device (needs root and losetup): nothing tested");
            return Ok(());
        };
        let Some(dir) = lo.mount_xfs() else {
            eprintln!("cannot make or mount XFS: nothing tested");
            return Ok(());
        };
        let path = dir.join("data");
        let mut f =
            open_file_dma(path.to_str().unwrap(), OpenFlags::RW | OpenFlags::CREATE).get()?;
        let (d_mem, d_miniosz) = dioinfo(&path).expect("XFS answers XFS_IOC_DIOINFO");
        assert_eq!(f.memory_dma_alignment(), u64::from(d_mem));
        assert_eq!(f.disk_read_dma_alignment(), u64::from(d_miniosz));
        assert!(f.disk_write_dma_alignment() >= u64::from(d_miniosz));
        assert!(f.disk_write_dma_alignment() >= 4096, "at least the block size");
        let overwrite = if kernel_at_least(5, 12) {
            u64::from(d_miniosz)
        } else {
            f.disk_write_dma_alignment()
        };
        assert_eq!(f.disk_overwrite_dma_alignment(), overwrite);
        // Appending writes grow the file; its logical size is what was written.
        let a = filled(4096, 1);
        for i in 0..4 {
            assert_eq!(write_at(&mut f, i * 4096, &a)?, 4096);
        }
        assert_eq!(f.size().get()?, 4 * 4096);
        f.flush().get()?;
        f.close().get()?;
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4 * 4096, "trimmed on close");
        let mut r = open_file_dma(path.to_str().unwrap(), OpenFlags::RO).get()?;
        assert!(read_at(&mut r, 8192, 4096)?.as_slice().iter().all(|&b| b == 1));
        r.close().get()?;
        Ok(())
    })
});
