//! Temporary files and directories.
//!
//! [`TmpFile`] and [`TmpDir`] create a uniquely named file or directory (from a template
//! whose run of `X` characters is replaced with random hex digits, created exclusively so
//! that no existing entry is reused) and remove it when asked. The `with_*` helpers create
//! one, run a function with it and always remove it afterwards. The default location is
//! `$TMPDIR` or `/tmp`, overridable with [`set_default_tmpdir`].
//!
//! Names are drawn from a per-thread minimal-standard generator seeded from
//! `/dev/urandom`.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use crate::engine::core::api::open_file_dma_with_options;
use crate::engine::core::do_with::do_with;
use crate::engine::core::file::{File, FileOpenOptions, FilePermissions};
use crate::engine::core::file_types::OpenFlags;
use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::thread::async_;
use crate::engine::util::file::{recursive_remove_directory, remove_file, touch_directory};
use crate::engine::util::sampler::RandomDevice;

const DEFAULT_TMP_NAME_TEMPLATE: &str = "XXXXXX.tmp";

/// The minimal-standard linear congruential generator `x = x * 16807 mod 2147483647`.
struct MinstdRand0 {
    state: u32,
}

impl MinstdRand0 {
    const A: u64 = 16807;
    const M: u64 = 2147483647;

    fn new(seed: u32) -> Self {
        let s = (seed as u64 % Self::M) as u32;
        MinstdRand0 { state: if s == 0 { 1 } else { s } }
    }

    #[inline]
    fn next(&mut self) -> u32 {
        self.state = ((self.state as u64 * Self::A) % Self::M) as u32;
        self.state
    }
}

/// A uniform integer in `[0, hi]` from `engine`, by rejection sampling: the generator's
/// range is split into `hi + 1` equal buckets and draws past the last full bucket are
/// discarded.
#[inline]
fn uniform_int(engine: &mut MinstdRand0, hi: u32) -> u32 {
    const URNG_MIN: u32 = 1;
    const URNG_RANGE: u32 = 2147483646 - 1; // max() - min()
    let urange = hi;
    let uerange = urange + 1;
    let scaling = (URNG_RANGE + 1) / uerange;
    let past = uerange * scaling;
    loop {
        let ret = engine.next() - URNG_MIN;
        if ret < past {
            return ret / scaling;
        }
    }
}

thread_local! {
    static TMP_NAME_ENGINE: RefCell<MinstdRand0> =
        RefCell::new(MinstdRand0::new(RandomDevice::new().call()));
}

fn generate_tmp_name(path_template: &Path) -> PathBuf {
    let mut parent = path_template.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut filename: Vec<u8> =
        path_template.file_name().map(|f| f.as_encoded_bytes().to_vec()).unwrap_or_default();
    if parent.as_os_str().is_empty() {
        parent = PathBuf::from(".");
    }
    let mut pos = filename.windows(2).position(|w| w == b"XX");
    if pos.is_none() {
        parent = path_template.to_path_buf();
        filename = DEFAULT_TMP_NAME_TEMPLATE.as_bytes().to_vec();
        pos = filename.windows(2).position(|w| w == b"XX");
        crate::engine_assert!(pos.is_some());
    }
    let mut pos = pos.unwrap_or(0);
    let end = filename.len();
    const CHARSET: &[u8; 16] = b"0123456789abcdef";
    TMP_NAME_ENGINE.with(|engine| {
        let mut engine = engine.borrow_mut();
        while pos < end && filename[pos] == b'X' {
            filename[pos] = CHARSET[uniform_int(&mut engine, (CHARSET.len() - 1) as u32) as usize];
            pos += 1;
        }
    });
    // SAFETY: the bytes come from an OsStr (or ASCII) with ASCII digits substituted.
    parent.push(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&filename) });
    parent
}

/// The default directory for temporary files: one for the process, shared by every shard
/// and guarded by a mutex (it is off any hot path). Empty until first read or set.
static DEFAULT_TMPDIR_PATH: std::sync::Mutex<PathBuf> = std::sync::Mutex::new(PathBuf::new());

/// Locks the default directory; a panic while it was held leaves a valid path behind.
#[inline]
fn default_tmpdir_path() -> std::sync::MutexGuard<'static, PathBuf> {
    DEFAULT_TMPDIR_PATH.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The directory for temporary files: `$TMPDIR`, or `/tmp`.
pub fn default_tmpdir() -> PathBuf {
    let mut p = default_tmpdir_path();
    if p.as_os_str().is_empty() {
        *p = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    }
    p.clone()
}

/// Overrides the directory for temporary files.
pub fn set_default_tmpdir(path: PathBuf) {
    *default_tmpdir_path() = path;
}

/// A temporary file, created exclusively by [`open`](TmpFile::open) and removed by
/// [`remove`](TmpFile::remove).
#[derive(Default)]
pub struct TmpFile {
    _path: PathBuf,
    _file: File,
    _is_open: Cell<bool>,
}

impl TmpFile {
    /// An unopened temporary file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates and opens a uniquely named file from `path_template`.
    ///
    /// # Safety
    /// The object stays alive and in place, and is not used otherwise, until the returned
    /// future resolves (the future keeps a raw pointer to it).
    pub unsafe fn open(
        &mut self,
        path_template: PathBuf,
        oflags: OpenFlags,
        options: FileOpenOptions,
    ) -> Future<()> {
        crate::engine_assert!(!self.has_path());
        crate::engine_assert!(!self.is_open());
        let oflags = oflags | OpenFlags::CREATE | OpenFlags::EXCLUSIVE;
        let path = generate_tmp_name(&path_template);
        let this: *mut TmpFile = self;
        open_file_dma_with_options(&path.to_string_lossy(), oflags, options).then(move |f: File| {
            // SAFETY: the contract of open().
            let this = unsafe { &mut *this };
            this._path = path;
            this._file = f;
            this._is_open.set(true);
            make_ready_future(())
        })
    }

    /// Closes the file, if open.
    ///
    /// # Safety
    /// The object stays alive and in place, and is not used otherwise, until the returned
    /// future resolves (the future keeps a raw pointer to it).
    pub unsafe fn close(&mut self) -> Future<()> {
        if !self.is_open() {
            return make_ready_future(());
        }
        let this: *mut TmpFile = self;
        self._file.close().then(move |()| {
            // SAFETY: as in open().
            unsafe { (*this)._is_open.set(false) };
        })
    }

    /// Removes the file, if it has a path.
    ///
    /// # Safety
    /// The object stays alive and in place, and is not used otherwise, until the returned
    /// future resolves (the future keeps a raw pointer to it).
    pub unsafe fn remove(&mut self) -> Future<()> {
        if !self.has_path() {
            return make_ready_future(());
        }
        let this: *mut TmpFile = self;
        remove_file(&self.get_path().to_string_lossy()).then(move |()| {
            // SAFETY: as in open().
            unsafe { (*this)._path.clear() };
        })
    }

    /// Opens a temporary file, runs `func` on it, then closes and removes it.
    pub fn do_with<Func>(
        path_template: PathBuf,
        func: Func,
        oflags: OpenFlags,
        options: FileOpenOptions,
    ) -> Future<()>
    where
        Func: FnOnce(&mut TmpFile) -> Future<()> + 'static,
    {
        do_with(TmpFile::new(), move |t: &mut TmpFile| {
            // Every use goes through `tp`, so none follows a re-borrow of `t`.
            let tp: *mut TmpFile = t;
            // SAFETY: do_with keeps the tmp_file alive and in place until the
            // chain resolves, and the steps of the chain use it one at a time.
            unsafe { (*tp).open(path_template, oflags, options) }
                .then(move |()| func(unsafe { &mut *tp }))
                .finally(
                    // SAFETY: as above.
                    move || unsafe { (*tp).close() }.finally(move || unsafe { (*tp).remove() }),
                )
        })
    }

    /// [`do_with`](Self::do_with) in the default temporary directory, opened read-write.
    pub fn do_with_default<Func>(func: Func) -> Future<()>
    where
        Func: FnOnce(&mut TmpFile) -> Future<()> + 'static,
    {
        Self::do_with(default_tmpdir(), func, OpenFlags::RW, FileOpenOptions::default())
    }

    /// Whether the file has a path (was opened and not removed).
    pub fn has_path(&self) -> bool {
        !self._path.as_os_str().is_empty()
    }

    /// Whether the file is open.
    pub fn is_open(&self) -> bool {
        self._is_open.get()
    }

    /// The path of the file.
    pub fn get_path(&self) -> &Path {
        &self._path
    }

    /// The open file.
    pub fn get_file(&mut self) -> &mut File {
        &mut self._file
    }

    /// Moves `x`'s state into a new `TmpFile`; `x` is left closed and without a path.
    pub fn take(x: &mut TmpFile) -> TmpFile {
        let r = TmpFile {
            _path: std::mem::take(&mut x._path),
            _file: std::mem::take(&mut x._file),
            _is_open: Cell::new(false),
        };
        r._is_open.swap(&x._is_open);
        r
    }
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        crate::engine_assert!(!self.has_path());
        crate::engine_assert!(!self.is_open());
    }
}

/// Creates a temporary file exclusively and returns it opened.
///
/// - `path_template`: where the file is created, optionally with a template for the file
///   name;
/// - `oflags`: open flags (`CREATE | EXCLUSIVE` are always added);
/// - `options`: additional open options, e.g. the permissions of the created file.
///
/// The last component of `path_template` may be a file name template, marked by two or
/// more consecutive `X`s, which are replaced by random hex digits. If there is no template,
/// `path_template` is taken as the parent directory and `XXXXXX.tmp` is appended as the
/// name template. The parent directory must exist and be writable by the process.
///
/// # Errors
///
/// The future fails if the file cannot be created.
///
/// The caller must close and remove the returned file before dropping it.
pub fn make_tmp_file(
    path_template: PathBuf,
    oflags: OpenFlags,
    options: FileOpenOptions,
) -> Future<TmpFile> {
    do_with(TmpFile::new(), move |t: &mut TmpFile| {
        // Every use goes through `tp`, so none follows a re-borrow of `t`.
        let tp: *mut TmpFile = t;
        // SAFETY: do_with keeps the tmp_file alive and in place until the chain
        // resolves.
        unsafe { (*tp).open(path_template, oflags, options) }
            .then(move |()| make_ready_future(TmpFile::take(unsafe { &mut *tp })))
    })
}

/// A temporary directory, created exclusively by [`create`](TmpDir::create) and removed
/// recursively by [`remove`](TmpDir::remove). Dropping it while it still has a path aborts
/// the process.
#[derive(Default)]
pub struct TmpDir {
    _path: PathBuf,
}

impl TmpDir {
    /// A directory not yet created.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a uniquely named directory from `path_template`.
    ///
    /// # Safety
    /// The object stays alive and in place, and is not used otherwise, until the returned
    /// future resolves (the future keeps a raw pointer to it).
    pub unsafe fn create(
        &mut self,
        path_template: PathBuf,
        create_permissions: FilePermissions,
    ) -> Future<()> {
        crate::engine_assert!(!self.has_path());
        let path = generate_tmp_name(&path_template);
        let this: *mut TmpDir = self;
        touch_directory(&path.to_string_lossy(), create_permissions).then(move |()| {
            // SAFETY: the contract of create().
            unsafe { (*this)._path = path };
            make_ready_future(())
        })
    }

    /// Removes the directory and its contents, if it has a path.
    pub fn remove(&mut self) -> Future<()> {
        if !self.has_path() {
            return make_ready_future(());
        }
        recursive_remove_directory(std::mem::take(&mut self._path))
    }

    /// Creates a temporary directory, runs `func` on it, then removes it.
    pub fn do_with<Func>(
        path_template: PathBuf,
        func: Func,
        create_permissions: FilePermissions,
    ) -> Future<()>
    where
        Func: FnOnce(&mut TmpDir) -> Future<()> + 'static,
    {
        do_with(TmpDir::new(), move |t: &mut TmpDir| {
            // Every use goes through `tp`, so none follows a re-borrow of `t`.
            let tp: *mut TmpDir = t;
            // SAFETY: do_with keeps the tmp_dir alive and in place until the
            // chain resolves, and the steps use it one at a time.
            unsafe { (*tp).create(path_template, create_permissions) }
                .then(move |()| func(unsafe { &mut *tp }))
                .finally(move || unsafe { (*tp).remove() })
        })
    }

    /// [`do_with`](Self::do_with) in the default temporary directory, with the default
    /// directory permissions.
    pub fn do_with_default<Func>(func: Func) -> Future<()>
    where
        Func: FnOnce(&mut TmpDir) -> Future<()> + 'static,
    {
        Self::do_with(default_tmpdir(), func, FilePermissions::DEFAULT_DIR_PERMISSIONS)
    }

    /// Creates a temporary directory in the default location, runs `func` with it inside an
    /// engine thread (so `func` may block on futures with `.get()`), then removes it.
    pub fn do_with_thread<Func, R>(func: Func) -> Future<()>
    where
        Func: FnOnce(&mut TmpDir) -> R + 'static,
        R: crate::engine::core::future::Futurize + 'static,
    {
        async_(move || -> Result<(), ExceptionPtr> {
            let mut t = TmpDir::new();
            // SAFETY: `t` lives on this thread's stack until get() returns.
            unsafe { t.create(default_tmpdir(), FilePermissions::DEFAULT_DIR_PERMISSIONS) }
                .get()?;
            // Both the call and the removal go through `tp`, so the second use does not
            // follow a re-borrow of `t` that would invalidate the pointer.
            let tp: *mut TmpDir = &mut t;
            // SAFETY: `t` lives on this thread's stack until get() returns; the reference
            // lasts for the call only.
            crate::engine::core::future::futurize_invoke(|| func(unsafe { &mut *tp }))
                // SAFETY: as above.
                .finally(move || unsafe { (*tp).remove() })
                .get()?;
            Ok(())
        })
    }

    /// Whether the directory has a path (was created and not removed).
    pub fn has_path(&self) -> bool {
        !self._path.as_os_str().is_empty()
    }

    /// The path of the directory.
    pub fn get_path(&self) -> &Path {
        &self._path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        crate::engine_assert!(!self.has_path());
    }
}

/// Creates a temporary directory exclusively and returns it.
///
/// - `path_template`: where the directory is created, optionally with a template for its
///   name;
/// - `create_permissions`: the permissions of the new directory.
///
/// The last component of `path_template` may be a name template, marked by two or more
/// consecutive `X`s, which are replaced by random hex digits. If there is no template,
/// `path_template` is taken as the parent directory and `XXXXXX.tmp` is appended as the
/// name template. The parent directory must exist and be writable by the process.
///
/// # Errors
///
/// The future fails if the directory cannot be created.
///
/// The caller must remove the returned directory before dropping it.
pub fn make_tmp_dir(path_template: PathBuf, create_permissions: FilePermissions) -> Future<TmpDir> {
    do_with(TmpDir::new(), move |t: &mut TmpDir| {
        // Every use goes through `tp`, so none follows a re-borrow of `t`.
        let tp: *mut TmpDir = t;
        // SAFETY: do_with keeps the tmp_dir alive and in place until the chain
        // resolves.
        unsafe { (*tp).create(path_template, create_permissions) }.then(move |()| {
            // SAFETY: do_with keeps the tmp_dir alive and in place until the chain resolves; create() has resolved, so nothing else references it.
            make_ready_future(TmpDir { _path: std::mem::take(unsafe { &mut (*tp)._path }) })
        })
    })
}
