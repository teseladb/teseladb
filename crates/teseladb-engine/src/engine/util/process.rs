//! Spawning and talking to subprocesses.
//!
//! [`experimental::Process::spawn`] starts a program with `posix_spawn(3)` through the
//! reactor, connecting its standard input, output and error to pipes. The pipes are
//! exposed as engine streams: [`cin`](experimental::Process::cin) is an output stream
//! whose writes go through the shard's I/O queue, and [`cout`](experimental::Process::cout)
//! / [`cerr`](experimental::Process::cerr) are input streams polled by the reactor.
//! [`wait`](experimental::Process::wait) resolves when the child exits. All calls must be
//! made on the reactor thread of the shard that spawned the process.

/// The subprocess API; it may still change.
pub mod experimental {
    use std::path::Path;

    use crate::engine::core::do_with::do_with;
    use crate::engine::core::fstream::FileInputStreamOptions;
    use crate::engine::core::future::{Future, make_exception_future, make_ready_future};
    use crate::engine::core::internal::buffer_allocator::BufferAllocator;
    use crate::engine::core::internal::io_request::IoRequest;
    use crate::engine::core::internal::pollable_fd::PollableFd;
    use crate::engine::core::io_priority_class::internal::MaybePriorityClassRef;
    use crate::engine::core::io_queue::IoQueue;
    use crate::engine::core::io_queue::internal::PriorityClass;
    use crate::engine::core::iostream::{
        DataSink, DataSinkImpl, DataSource, DataSourceImpl, InputStream, OutputStream,
    };
    use crate::engine::core::loop_::do_for_each;
    use crate::engine::core::memory::MALLOC_ALLOCATOR;
    use crate::engine::core::polymorphic_temporary_buffer::make_temporary_buffer;
    use crate::engine::core::posix::FileDesc;
    use crate::engine::core::reactor::engine;
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::net::packet::Packet;

    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;

    /// The optional parameters for spawning a subprocess.
    ///
    /// See `execve(2)` for the meaning of `argv` and `env`.
    #[derive(Default, Clone)]
    pub struct SpawnParameters {
        /// The arguments passed to the program (`argv[0]` included).
        pub argv: Vec<Sstring>,
        /// The environment variables for the program, as `NAME=value`.
        pub env: Vec<Sstring>,
    }

    struct CreateTag;

    /// A spawned subprocess and the pipes to its standard streams.
    ///
    /// The subprocess should always be [`wait`](Self::wait)ed for; otherwise it stays a
    /// zombie after it exits, until the application exits.
    pub struct Process {
        _pid: i32,
        _stdin: Option<FileDesc>,
        _stdout: Option<FileDesc>,
        _stderr: Option<FileDesc>,
    }

    /// The child exited normally.
    pub struct WaitExited {
        /// The exit status passed to `exit` (0 to 255).
        pub exit_code: i32,
    }

    /// The child was terminated by a signal.
    pub struct WaitSignaled {
        /// The number of the signal that terminated the child.
        pub terminating_signal: i32,
    }

    /// How a child process ended.
    pub enum WaitStatus {
        /// It exited normally.
        Exited(WaitExited),
        /// It was terminated by a signal.
        Signaled(WaitSignaled),
    }

    /// Reads a pipe through the reactor's poller.
    struct PipeDataSourceImpl {
        _fd: PollableFd,
        _ba: PipeBufferAllocator,
    }

    /// Allocates the read buffers of a pipe (8 KiB each, with the system allocator).
    struct PipeBufferAllocator;

    impl PipeDataSourceImpl {
        const BUFFER_SIZE: usize = 8192;

        fn new(fd: PollableFd) -> Self {
            PipeDataSourceImpl { _fd: fd, _ba: PipeBufferAllocator }
        }

        fn from_fd(fd: FileDesc) -> Box<PipeDataSourceImpl> {
            Box::new(PipeDataSourceImpl::new(PollableFd::from_file_desc(fd)))
        }
    }

    impl BufferAllocator for PipeBufferAllocator {
        fn allocate_buffer(&mut self) -> TemporaryBuffer<u8> {
            make_temporary_buffer(MALLOC_ALLOCATOR, PipeDataSourceImpl::BUFFER_SIZE)
        }
    }

    impl DataSourceImpl for PipeDataSourceImpl {
        fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
            // SAFETY: the allocator is a member of the data source, which outlives its get() calls.
            unsafe {
                self._fd.read_some(
                    &mut self._ba as *mut PipeBufferAllocator as *mut dyn BufferAllocator,
                )
            }
        }

        fn close(&mut self) -> Future<()> {
            self._fd.close();
            make_ready_future(())
        }
    }

    /// Writes a pipe through the shard's I/O queue, retrying short writes.
    struct PipeDataSinkImpl {
        _fd: FileDesc,
        _io_queue: *mut IoQueue,
        _buffer_size: usize,
    }

    impl PipeDataSinkImpl {
        fn new(fd: FileDesc) -> Self {
            PipeDataSinkImpl {
                _fd: fd,
                // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                _io_queue: unsafe { (*engine()).get_io_queue(0) },
                _buffer_size: FileInputStreamOptions::default().buffer_size,
            }
        }

        fn from_fd(fd: FileDesc) -> Box<PipeDataSinkImpl> {
            Box::new(PipeDataSinkImpl::new(fd))
        }
    }

    impl DataSinkImpl for PipeDataSinkImpl {
        fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
            let buf_size = buf.size();
            let req = IoRequest::make_write(
                self._fd.get(),
                0,
                buf.get() as *const std::ffi::c_void,
                buf_size,
                false,
            );
            let this = self as *mut PipeDataSinkImpl;
            let mut buf = buf;
            // SAFETY: the io_queue belongs to the engine, which outlives the sink; the pointer
            // is the queue's root pointer (`get_io_queue`), with no reference to it live.
            unsafe {
                IoQueue::submit_io_write(
                    self._io_queue,
                    PriorityClass::from_maybe(MaybePriorityClassRef),
                    buf_size,
                    req,
                    std::ptr::null_mut(),
                    Vec::new(),
                )
            }
            .then(move |written: usize| {
                if written < buf_size {
                    buf.trim_front(written);
                    // SAFETY: the data_sink keeps this impl alive while a put is pending.
                    return unsafe { (*this).put_buffer(buf) };
                }
                make_ready_future(())
            })
        }

        fn put(&mut self, mut data: Packet) -> Future<()> {
            let this = self as *mut PipeDataSinkImpl;
            do_with(data.release(), move |bufs: &mut Vec<TemporaryBuffer<u8>>| {
                let bufs: *mut Vec<TemporaryBuffer<u8>> = bufs;
                // SAFETY: do_with keeps the vector alive until the loop's future resolves.
                do_for_each(unsafe { (*bufs).iter_mut() }, move |buf: &mut TemporaryBuffer<u8>| {
                    // SAFETY: the data sink keeps this impl alive while a put is pending, and
                    // do_with keeps `bufs` alive until the loop finishes.
                    unsafe { (*this).put_buffer(buf.share()) }
                })
            })
        }

        fn close(&mut self) -> Future<()> {
            // A close error is reported through the returned future.
            if let Err(e) = self._fd.close() {
                return make_exception_future(e);
            }
            make_ready_future(())
        }

        fn buffer_size(&self) -> usize {
            self._buffer_size
        }
    }

    impl Process {
        fn new(_: CreateTag, pid: i32, cin: FileDesc, cout: FileDesc, cerr: FileDesc) -> Self {
            Process { _pid: pid, _stdin: Some(cin), _stdout: Some(cout), _stderr: Some(cerr) }
        }

        /// Waits until the child process exits or is terminated, and returns how it ended.
        ///
        /// # Errors
        ///
        /// The future fails with the error of `waitpid`.
        pub fn wait(&mut self) -> Future<WaitStatus> {
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            unsafe { (*engine()).waitpid(self._pid) }.then(
                |wstatus: i32| -> Result<WaitStatus, crate::engine::core::future::ExceptionPtr> {
                    // WIFEXITED / WEXITSTATUS / WIFSIGNALED / WTERMSIG
                    let wifexited = (wstatus & 0x7f) == 0;
                    if wifexited {
                        Ok(WaitStatus::Exited(WaitExited { exit_code: (wstatus >> 8) & 0xff }))
                    } else {
                        let wifsignaled = (((wstatus & 0x7f) + 1) as i8 >> 1) > 0;
                        crate::engine_assert!(wifsignaled);
                        Ok(WaitStatus::Signaled(WaitSignaled {
                            terminating_signal: wstatus & 0x7f,
                        }))
                    }
                },
            )
        }

        /// Asks the process to stop by sending it `SIGTERM`.
        ///
        /// # Errors
        ///
        /// Returns the error of `kill(2)`.
        pub fn terminate(&mut self) -> Result<(), crate::engine::core::future::ExceptionPtr> {
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            unsafe { (*engine()).kill(self._pid, SIGTERM) }
        }

        /// Forces the process to exit by sending it `SIGKILL`.
        ///
        /// # Errors
        ///
        /// Returns the error of `kill(2)`.
        pub fn kill(&mut self) -> Result<(), crate::engine::core::future::ExceptionPtr> {
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            unsafe { (*engine()).kill(self._pid, SIGKILL) }
        }

        /// Spawns `pathname` (the full path to the executable) with `posix_spawn(3)`, using
        /// the arguments and environment in `params`.
        ///
        /// # Errors
        ///
        /// The future fails if the pipes cannot be created or the spawn fails.
        ///
        /// # Panics
        ///
        /// Aborts the process (via [`engine_assert!`](crate::engine_assert)) if
        /// `params.argv` is empty.
        pub fn spawn_with(pathname: &Path, params: SpawnParameters) -> Future<Process> {
            crate::engine_assert!(!params.argv.is_empty());
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            unsafe { (*engine()).spawn(&pathname.to_string_lossy(), params.argv, params.env) }.then(
                |(pid, stdin_pipe, stdout_pipe, stderr_pipe): (
                    i32,
                    FileDesc,
                    FileDesc,
                    FileDesc,
                )| {
                    make_ready_future(Process::new(
                        CreateTag,
                        pid,
                        stdin_pipe,
                        stdout_pipe,
                        stderr_pipe,
                    ))
                },
            )
        }

        /// Spawns `pathname` (the full path to the executable) with `posix_spawn(3)`, with
        /// `argv = [pathname]` and an empty environment.
        ///
        /// # Errors
        ///
        /// See [`spawn_with`](Self::spawn_with).
        pub fn spawn(pathname: &Path) -> Future<Process> {
            Self::spawn_with(
                pathname,
                SpawnParameters {
                    argv: vec![Sstring::from(pathname.to_string_lossy().as_ref())],
                    env: vec![],
                },
            )
        }

        /// Returns a writable stream connected to the child's standard input.
        ///
        /// # Panics
        ///
        /// Panics if called more than once.
        pub fn cin(&mut self) -> OutputStream<u8> {
            let fd = self._stdin.take().expect("stdin already taken");
            OutputStream::new(DataSink::new(PipeDataSinkImpl::from_fd(fd)))
        }

        /// Returns a readable stream of the child's standard output.
        ///
        /// # Panics
        ///
        /// Panics if called more than once.
        pub fn cout(&mut self) -> InputStream<u8> {
            let fd = self._stdout.take().expect("stdout already taken");
            InputStream::new(DataSource::new(PipeDataSourceImpl::from_fd(fd)))
        }

        /// Returns a readable stream of the child's standard error.
        ///
        /// # Panics
        ///
        /// Panics if called more than once.
        pub fn cerr(&mut self) -> InputStream<u8> {
            let fd = self._stderr.take().expect("stderr already taken");
            InputStream::new(DataSource::new(PipeDataSourceImpl::from_fd(fd)))
        }
    }

    /// Free-function form of [`Process::spawn_with`].
    pub fn spawn_process_with(pathname: &Path, params: SpawnParameters) -> Future<Process> {
        Process::spawn_with(pathname, params)
    }

    /// Free-function form of [`Process::spawn`].
    pub fn spawn_process(pathname: &Path) -> Future<Process> {
        Process::spawn(pathname)
    }
}
