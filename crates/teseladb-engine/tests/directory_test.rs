//! Directory listing: `list_directory` through the subscription and through the generator,
//! on a real directory and through a file implementation that does not override the
//! generator (so the default fallback is used). A program with its own `main`
//! (`harness = false`).

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::RefCell;

use teseladb::engine::core::api::FollowSymlink;
use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::file::{
    DirectoryEntry, File, FileImpl, FileImplBase, StatData, open_directory,
};
use teseladb::engine::core::file_types::DirectoryEntryType;
use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::io_intent::IoIntent;
use teseladb::engine::core::posix::Stat;
use teseladb::engine::core::reactor_backend::Iovec;
use teseladb::engine::core::shared_ptr::{make_shared, static_pointer_cast};
use teseladb::engine::core::stream::{NextFn, Subscription};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::util::file::file_stat;

fn de_type_desc(t: DirectoryEntryType) -> &'static str {
    match t {
        DirectoryEntryType::Unknown => "unknown",
        DirectoryEntryType::BlockDevice => "block_device",
        DirectoryEntryType::CharDevice => "char_device",
        DirectoryEntryType::Directory => "directory",
        DirectoryEntryType::Fifo => "fifo",
        DirectoryEntryType::Link => "link",
        DirectoryEntryType::Regular => "regular",
        DirectoryEntryType::Socket => "socket",
    }
}

fn report(de: DirectoryEntry) -> Future<()> {
    file_stat(de.name.as_str(), FollowSymlink::NO).then(move |sd: StatData| {
        if let Some(t) = de.type_ {
            assert!(t == sd.type_);
        } else {
            assert!(sd.type_ == DirectoryEntryType::Unknown);
        }
        println!("{} (type={})", de.name, de_type_desc(sd.type_));
        make_ready_future(())
    })
}

struct Lister {
    _f: File,
    listing: Subscription<DirectoryEntry>,
}

impl Lister {
    fn new(mut f: File) -> Self {
        let listing = f.list_directory(report);
        Lister { _f: f, listing }
    }

    fn done(&mut self) -> Future<()> {
        self.listing.done()
    }
}

fn lister_test() -> Future<()> {
    println!("--- Regular lister test ---");
    open_directory(".").then(|f: File| do_with(Lister::new(f), |l: &mut Lister| l.done()))
}

fn lister_generator_test_file(mut f: File) -> Future<()> {
    make_coroutine(async move {
        let mut lister = f.experimental_list_directory();
        while let Some(de) = lister.next().await? {
            let sd = file_stat(de.name.as_str(), FollowSymlink::NO).await?;
            if let Some(t) = de.type_ {
                assert!(t == sd.type_);
            } else {
                assert!(sd.type_ == DirectoryEntryType::Unknown);
            }
            println!("{} (type={})", de.name, de_type_desc(sd.type_));
        }
        drop(lister);
        f.close().await?;
        Ok(())
    })
}

/// A file implementation that forwards every operation to a lower file. The operations take
/// `&self`, and `close()` changes the lower file, so it sits in a cell.
struct TestFileImpl {
    base: FileImplBase,
    lower: RefCell<File>,
}

impl TestFileImpl {
    fn new(f: File) -> Self {
        TestFileImpl { base: FileImplBase::default(), lower: RefCell::new(f) }
    }

    /// The implementation of the lower file.
    fn lower(&self) -> &dyn FileImpl {
        let p = match <dyn FileImpl>::get_file_impl(&mut self.lower.borrow_mut()) {
            Some(p) => p,
            None => panic!("get_file_impl of an uninitialized file"),
        };
        // SAFETY: the lower file owns its impl through a shared pointer, and keeps it alive for as
        // long as this object, which the returned reference cannot outlive.
        unsafe { &*p }
    }
}

impl FileImpl for TestFileImpl {
    fn file_impl_base(&self) -> &FileImplBase {
        &self.base
    }
    fn flush(&self) -> Future<()> {
        self.lower().flush()
    }
    fn stat(&self) -> Future<Stat> {
        self.lower().stat()
    }
    fn truncate(&self, length: u64) -> Future<()> {
        self.lower().truncate(length)
    }
    fn discard(&self, offset: u64, length: u64) -> Future<()> {
        self.lower().discard(offset, length)
    }
    fn allocate(&self, position: u64, length: u64) -> Future<()> {
        self.lower().allocate(position, length)
    }
    fn size(&self) -> Future<u64> {
        self.lower().size()
    }
    fn close(&self) -> Future<()> {
        self.lower.borrow_mut().close()
    }
    fn list_directory(&self, next: NextFn<DirectoryEntry>) -> Subscription<DirectoryEntry> {
        self.lower().list_directory(next)
    }
    // ! no override for generator list_directory, so that fallback is used
    unsafe fn write_dma(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        i: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: the caller's contract for this operation, passed on unchanged.
        unsafe { self.lower().write_dma(pos, buffer, len, i) }
    }
    unsafe fn write_dma_iov(&self, pos: u64, iov: Vec<Iovec>, i: *mut IoIntent) -> Future<usize> {
        // SAFETY: the caller's contract for this operation, passed on unchanged.
        unsafe { self.lower().write_dma_iov(pos, iov, i) }
    }
    unsafe fn read_dma(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        i: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: the caller's contract for this operation, passed on unchanged.
        unsafe { self.lower().read_dma(pos, buffer, len, i) }
    }
    unsafe fn read_dma_iov(&self, pos: u64, iov: Vec<Iovec>, i: *mut IoIntent) -> Future<usize> {
        // SAFETY: the caller's contract for this operation, passed on unchanged.
        unsafe { self.lower().read_dma_iov(pos, iov, i) }
    }
    unsafe fn dma_read_bulk(
        &self,
        offset: u64,
        range_size: usize,
        i: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the caller's contract for this operation, passed on unchanged.
        unsafe { self.lower().dma_read_bulk(offset, range_size, i) }
    }
}

fn lister_generator_test() -> Future<()> {
    make_coroutine(async move {
        println!("--- Generator lister test ---");
        let f = open_directory(".").await?;
        lister_generator_test_file(f).await?;

        println!("--- Generator fallback test ---");
        let lf = open_directory(".").await?;
        let tf = make_shared(TestFileImpl::new(lf));
        // SAFETY: the cast keeps the pointer into the same object (an upcast from the concrete
        // implementation to the `FileImpl` trait object).
        let tf = unsafe { static_pointer_cast(&tf, |x: *mut TestFileImpl| x as *mut dyn FileImpl) };
        let f2 = File::from_impl(tf);
        lister_generator_test_file(f2).await?;
        Ok(())
    })
}

pub(crate) fn main() {
    let args: Vec<String> = std::env::args().collect();
    let status = AppTemplate::new(Default::default())
        .run(args, || lister_test().then(|()| lister_generator_test()));
    std::process::exit(status);
}
