//! Tests of the file utilities: temporary files and directories (creation, removal, leftovers),
//! failures in the `do_with` helpers, recursive directory removal, and reading a whole file.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/expected_exception.rs"]
mod expected_exception;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use expected_exception::ExpectedException;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::file::{File, FileOpenOptions, open_file_dma};
use teseladb::engine::core::file_types::{FilePermissions, OpenFlags};
use teseladb::engine::core::future::{
    ExceptionPtr, Future, SystemError, make_exception_future, make_ready_future,
};
use teseladb::engine::core::loop_::parallel_for_each;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::testing::engine_test::testing::exception_predicate::message_contains;
use teseladb::engine::testing::random::testing::local_random;
use teseladb::engine::util::file::util::read_entire_file_contiguous;
use teseladb::engine::util::file::{
    file_exists, recursive_remove_directory, rename_file, touch_directory,
};
use teseladb::engine::util::tmp_file::{
    TmpDir, TmpFile, default_tmpdir, make_tmp_dir, make_tmp_file, set_default_tmpdir,
};
use teseladb::engine_test_case;

fn native(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

engine_test_case!(test_make_tmp_file, {
    make_tmp_file(default_tmpdir(), OpenFlags::RW, FileOpenOptions::default()).then(
        |tf: TmpFile| {
            let mut tf = tf;
            thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
                let tmp_path = native(tf.get_path());
                assert!(file_exists(&tmp_path).get()?);
                // SAFETY: the temporary file stays in place and unused until get() returns, right here.
                unsafe { tf.close() }.get()?;
                // SAFETY: the temporary file stays in place and unused until get() returns, right here.
                unsafe { tf.remove() }.get()?;
                assert!(!file_exists(&tmp_path).get()?);
                Ok(())
            })
        },
    )
});

fn get_init_buffer(f: &File) -> TemporaryBuffer<u8> {
    let a = f.memory_dma_alignment() as usize;
    let mut buf = TemporaryBuffer::<u8>::aligned(a, a);
    // SAFETY: the buffer has size() writable bytes.
    unsafe { std::ptr::write_bytes(buf.get_write(), 0, buf.size()) };
    buf
}

/// The body both tmp_file tests share: one aligned dma_write, recording sizes.
fn write_one_buffer(
    f: *mut File,
    expected: Rc<Cell<usize>>,
    actual: Rc<Cell<usize>>,
) -> Future<()> {
    // SAFETY: the tmp_file owning the file outlives the returned future.
    let buf = get_init_buffer(unsafe { &*f });
    do_with(buf, move |buf: &mut TemporaryBuffer<u8>| {
        expected.set(buf.size());
        // SAFETY: as above; do_with keeps `buf` alive until the write resolves.
        unsafe { (*f).dma_write(0, buf.get(), buf.size(), std::ptr::null_mut()) }.then(
            move |written: usize| {
                actual.set(written);
                make_ready_future(())
            },
        )
    })
}

engine_test_case!(test_tmp_file, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let expected = Rc::new(Cell::new(!0usize));
        let actual = Rc::new(Cell::new(0usize));
        let (e, a) = (expected.clone(), actual.clone());
        TmpFile::do_with_default(move |tf: &mut TmpFile| {
            let f: *mut File = tf.get_file();
            write_one_buffer(f, e, a)
        })
        .get()?;
        assert_eq!(expected.get(), actual.get());
        Ok(())
    })
});

engine_test_case!(test_non_existing_TMPDIR, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let r = TmpFile::do_with(
            PathBuf::from("/tmp/non-existing-TMPDIR"),
            |_tf: &mut TmpFile| make_ready_future(()),
            OpenFlags::RW,
            FileOpenOptions::default(),
        )
        .get();
        let e = r.expect_err("a SystemError");
        assert!(e.is::<SystemError>());
        assert!(message_contains("No such file or directory")(&e));
        Ok(())
    })
});

fn touch_file(filename: &str, oflags: OpenFlags) -> Future<()> {
    open_file_dma(filename, oflags).then(|mut f: File| {
        let fc = f.clone();
        f.close().finally(move || drop(fc))
    })
}

/// A directory tree to create and remove. Children keep the address of
/// their parent, so the tree is built in boxes that never move.
struct TestDir {
    parent: *const TestDir,
    name: Sstring,
    sub_files: Vec<Sstring>,
    // Boxed: each sub-directory's `parent` points at its owner, which must not move.
    #[allow(clippy::vec_box)]
    sub_dirs: Vec<Box<TestDir>>,
}

fn dist() -> u32 {
    // A full-range unsigned draw from the local random engine.
    local_random()
}

impl TestDir {
    fn new(parent: *const TestDir, name: Sstring) -> Box<Self> {
        Box::new(TestDir { parent, name, sub_files: Vec::new(), sub_dirs: Vec::new() })
    }

    fn path(&self) -> PathBuf {
        if self.parent.is_null() {
            return PathBuf::from(self.name.as_str());
        }
        // SAFETY: the parent box outlives its children.
        unsafe { (*self.parent).path() }.join(self.name.as_str())
    }

    fn fill_random_file(&mut self) {
        self.sub_files.push(Sstring::from(format!("file-{}", dist()).as_str()));
    }

    fn fill_random_dir(&mut self) -> &mut TestDir {
        let me: *const TestDir = self;
        self.sub_dirs.push(TestDir::new(me, Sstring::from(format!("dir-{}", dist()).as_str())));
        self.sub_dirs.last_mut().expect("just pushed")
    }

    fn random_fill(&mut self, mut level: i32, levels: i32) {
        let num_files = (dist() % 10) as i32;
        let num_dirs = if level < levels - 1 { (1 + dist() % 3) as i32 } else { 0 };
        for _ in 0..num_files {
            self.fill_random_file();
        }
        if num_dirs != 0 {
            level += 1;
            for _ in 0..num_dirs {
                self.fill_random_dir().random_fill(level, levels);
            }
        }
    }

    fn populate(&self) -> Future<()> {
        let this: *const TestDir = self;
        touch_directory(&native(&self.path()), FilePermissions::DEFAULT_DIR_PERMISSIONS).then(
            move |()| {
                // SAFETY: the tree outlives the populate() it is waited for in.
                let me = unsafe { &*this };
                parallel_for_each(me.sub_files.iter(), move |name: &Sstring| {
                    // SAFETY: the tree outlives the populate() it is waited for in; it is only read.
                    let me = unsafe { &*this };
                    touch_file(
                        &native(&me.path().join(name.as_str())),
                        OpenFlags::RW | OpenFlags::CREATE,
                    )
                })
                .then(move |()| {
                    // SAFETY: the tree outlives the populate() it is waited for in; it is only read.
                    let me = unsafe { &*this };
                    parallel_for_each(me.sub_dirs.iter(), |sub_dir| sub_dir.populate())
                })
            },
        )
    }
}

engine_test_case!(test_recursive_remove_directory, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let levels = (1 + dist() % 3) as i32;
        let root =
            TestDir::new(std::ptr::null(), Sstring::from(native(&default_tmpdir()).as_str()));
        let mut base = TestDir::new(&*root, Sstring::from(format!("base-{}", dist()).as_str()));
        base.random_fill(0, levels);
        base.populate().get()?;

        recursive_remove_directory(base.path()).get()?;
        assert!(!file_exists(&native(&base.path())).get()?);
        Ok(())
    })
});

engine_test_case!(test_make_tmp_dir, {
    make_tmp_dir(default_tmpdir(), FilePermissions::DEFAULT_DIR_PERMISSIONS).then(|td: TmpDir| {
        let mut td = td;
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            let tmp_path = native(td.get_path());
            assert!(file_exists(&tmp_path).get()?);
            td.remove().get()?;
            assert!(!file_exists(&tmp_path).get()?);
            Ok(())
        })
    })
});

fn tmp_dir_and_file(
    path_template: PathBuf,
    expected: Rc<Cell<usize>>,
    actual: Rc<Cell<usize>>,
) -> Future<()> {
    TmpDir::do_with(
        path_template,
        move |td: &mut TmpDir| {
            TmpFile::do_with(
                td.get_path().to_path_buf(),
                move |tf: &mut TmpFile| {
                    let f: *mut File = tf.get_file();
                    write_one_buffer(f, expected, actual)
                },
                OpenFlags::RW,
                FileOpenOptions::default(),
            )
        },
        FilePermissions::DEFAULT_DIR_PERMISSIONS,
    )
}

engine_test_case!(test_tmp_dir, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let expected = Rc::new(Cell::new(0usize));
        let actual = Rc::new(Cell::new(0usize));
        tmp_dir_and_file(default_tmpdir(), expected.clone(), actual.clone()).get()?;
        assert_eq!(expected.get(), actual.get());
        Ok(())
    })
});

engine_test_case!(test_tmp_dir_with_path, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let expected = Rc::new(Cell::new(0usize));
        let actual = Rc::new(Cell::new(0usize));
        tmp_dir_and_file(PathBuf::from("."), expected.clone(), actual.clone()).get()?;
        assert_eq!(expected.get(), actual.get());
        Ok(())
    })
});

engine_test_case!(test_tmp_dir_with_non_existing_path, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let r = TmpDir::do_with(
            PathBuf::from("/tmp/this_name_should_not_exist"),
            |_td: &mut TmpDir| make_ready_future(()),
            FilePermissions::DEFAULT_DIR_PERMISSIONS,
        )
        .get();
        let e = r.expect_err("a SystemError");
        assert!(e.is::<SystemError>());
        assert!(message_contains("No such file or directory")(&e));
        Ok(())
    })
});

engine_test_case!(tmp_dir_with_thread_test, {
    TmpDir::do_with_thread(|td: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let mut tf =
            make_tmp_file(td.get_path().to_path_buf(), OpenFlags::RW, FileOpenOptions::default())
                .get()?;
        let f = tf.get_file();
        let buf = get_init_buffer(f);
        let expected = buf.size();
        // SAFETY: `buf` owns `buf.size()` readable bytes and lives, untouched, until `get()`
        // returns the result of the write; there is no intent.
        let actual =
            unsafe { f.dma_write(0, buf.get(), buf.size(), std::ptr::null_mut()) }.get()?;
        assert_eq!(expected, actual);
        // SAFETY: the temporary file stays in place and unused until get() returns, right here.
        unsafe { tf.close() }.get()?;
        // SAFETY: the temporary file stays in place and unused until get() returns, right here.
        unsafe { tf.remove() }.get()?;
        Ok(())
    })
});

engine_test_case!(tmp_dir_with_leftovers_test, {
    TmpDir::do_with_thread(|td: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let path = td.get_path().join("testfile.tmp");
        touch_file(&native(&path), OpenFlags::RW | OpenFlags::CREATE).get()?;
        assert!(file_exists(&native(&path)).get()?);
        Ok(())
    })
});

engine_test_case!(tmp_dir_do_with_fail_func_test, {
    TmpDir::do_with_thread(|_outer: &mut TmpDir| {
        let r = TmpDir::do_with_default(|_inner: &mut TmpDir| {
            make_exception_future::<()>(ExceptionPtr::new(ExpectedException::new()))
        })
        .get();
        assert!(matches!(r, Err(ref e) if e.is::<ExpectedException>()));
    })
});

engine_test_case!(tmp_dir_do_with_fail_remove_test, {
    TmpDir::do_with_thread(|outer: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let saved_default_tmpdir = default_tmpdir();
        let outer_path = native(outer.get_path());
        let inner_path = Rc::new(std::cell::RefCell::new(String::new()));
        let inner_path_renamed = Rc::new(std::cell::RefCell::new(String::new()));
        set_default_tmpdir(PathBuf::from(&outer_path));
        let (ip, ipr) = (inner_path.clone(), inner_path_renamed.clone());
        let r = TmpDir::do_with_default(move |inner: &mut TmpDir| {
            *ip.borrow_mut() = native(inner.get_path());
            *ipr.borrow_mut() = format!("{}.renamed", ip.borrow());
            rename_file(&ip.borrow(), &ipr.borrow())
        })
        .get();
        assert!(matches!(r, Err(ref e) if e.is::<SystemError>()));
        assert!(!file_exists(&inner_path.borrow()).get()?);
        assert!(file_exists(&inner_path_renamed.borrow()).get()?);
        set_default_tmpdir(saved_default_tmpdir);
        Ok(())
    })
});

engine_test_case!(tmp_dir_do_with_thread_fail_func_test, {
    TmpDir::do_with_thread(|_outer: &mut TmpDir| {
        let r = TmpDir::do_with_thread(|_inner: &mut TmpDir| -> Result<(), ExceptionPtr> {
            Err(ExceptionPtr::new(ExpectedException::new()))
        })
        .get();
        assert!(matches!(r, Err(ref e) if e.is::<ExpectedException>()));
    })
});

engine_test_case!(tmp_dir_do_with_thread_fail_remove_test, {
    TmpDir::do_with_thread(|outer: &mut TmpDir| -> Result<(), ExceptionPtr> {
        let saved_default_tmpdir = default_tmpdir();
        let outer_path = native(outer.get_path());
        let inner_path = Rc::new(std::cell::RefCell::new(String::new()));
        let inner_path_renamed = Rc::new(std::cell::RefCell::new(String::new()));
        set_default_tmpdir(PathBuf::from(&outer_path));
        let (ip, ipr) = (inner_path.clone(), inner_path_renamed.clone());
        let r = TmpDir::do_with_thread(move |inner: &mut TmpDir| {
            *ip.borrow_mut() = native(inner.get_path());
            *ipr.borrow_mut() = format!("{}.renamed", ip.borrow());
            rename_file(&ip.borrow(), &ipr.borrow())
        })
        .get();
        assert!(matches!(r, Err(ref e) if e.is::<SystemError>()));
        assert!(!file_exists(&inner_path.borrow()).get()?);
        assert!(file_exists(&inner_path_renamed.borrow()).get()?);
        set_default_tmpdir(saved_default_tmpdir);
        Ok(())
    })
});

engine_test_case!(test_read_entire_file_contiguous, {
    TmpFile::do_with_default(|tf: &mut TmpFile| {
        let tfp: *mut TmpFile = tf;
        thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
            // SAFETY: do_with keeps the tmp_file alive until the thread ends.
            let tf = unsafe { &mut *tfp };
            let path = tf.get_path().to_path_buf();
            let f = tf.get_file();
            let a = f.memory_dma_alignment() as usize;
            let size = a * (1 + (dist() % 1000) as usize);
            let mut wbuf = TemporaryBuffer::<u8>::aligned(a, size);
            const CHARS: &[u8; 37] = b"abcdefghijklmnopqrstuvwxyz0123456789\0";
            for i in 0..size {
                // SAFETY: the buffer has `size` writable bytes.
                unsafe { *wbuf.get_write().add(i) = CHARS[(dist() as usize) % CHARS.len()] };
            }
            // SAFETY: `wbuf` owns `wbuf.size()` readable bytes and lives, untouched, until
            // `get()` returns the result of the write; there is no intent.
            let written =
                unsafe { f.dma_write(0, wbuf.get(), wbuf.size(), std::ptr::null_mut()) }.get()?;
            assert_eq!(written, wbuf.size());
            f.flush().get()?;

            let res = read_entire_file_contiguous(path).get()?;
            assert_eq!(res.as_bytes(), wbuf.as_slice());
            Ok(())
        })
    })
});
